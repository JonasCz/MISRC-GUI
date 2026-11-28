//! Hidden diagnostic KS properties of MWProCapture.sys (doc 07), sent with
//! `DeviceIoControl(IOCTL_KS_PROPERTY)` to the channel's device interface, the same way
//! LibMWCapture.dll talks to the driver.
//!
//! Property set {3A45B930-FF5C-42AA-A5A2-43771B93EBEF} (MWCAP_PROPSETID_FILTER):
//! - 0x40000005: ADV7842 I2C. Request = KSPROPERTY + u32 channel + u8 dev8 + u8 reg (30 bytes),
//!   data = the register bytes.
//! - 0x40000006: BAR0 register. Request = KSPROPERTY + u32 offset (28 bytes), data = u32.
//!   The driver does no bounds check (see `bus::bar0_check`).

use std::ptr::{null, null_mut};

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};

use crate::bus::{bar0_check, RegBus};

const IOCTL_KS_PROPERTY: u32 = 0x002F_0003; // CTL_CODE(FILE_DEVICE_KS, 0, METHOD_NEITHER, FILE_ANY_ACCESS)
const KSPROPERTY_TYPE_GET: u32 = 1;
const KSPROPERTY_TYPE_SET: u32 = 2;
/// {3A45B930-FF5C-42AA-A5A2-43771B93EBEF}, little-endian GUID layout.
const PROPSETID_FILTER: [u8; 16] = [
    0x30, 0xB9, 0x45, 0x3A, 0x5C, 0xFF, 0xAA, 0x42, 0xA5, 0xA2, 0x43, 0x77, 0x1B, 0x93, 0xEB, 0xEF,
];
pub const PROP_I2C: u32 = 0x4000_0005;
pub const PROP_BAR0: u32 = 0x4000_0006;

/// Win32 event (auto-reset unless asked otherwise).
pub struct Event(pub HANDLE);

impl Event {
    pub fn new(manual_reset: bool) -> Result<Event> {
        let h = unsafe { CreateEventW(null(), manual_reset as i32, 0, null()) };
        if h.is_null() {
            bail!("CreateEventW failed: {}", unsafe { GetLastError() });
        }
        Ok(Event(h))
    }
    /// True if signalled within `ms`.
    pub fn wait(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub struct KsDevice {
    h: HANDLE,
    ev: Event,
}

// Plain kernel handles; every request is synchronous on the calling thread.
unsafe impl Send for KsDevice {}
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

/// I2C channel field of the property request (0 on the Pro Capture HDMI).
const I2C_CHANNEL: u32 = 0;

impl Drop for KsDevice {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.h) };
    }
}

impl KsDevice {
    /// Open our own filter handle on the channel's device interface path (NUL-terminated).
    pub fn open(path: &[u16]) -> Result<KsDevice> {
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            bail!(
                "CreateFileW on {} failed: Win32 error {}",
                super::sdk::wide_to_string(path),
                unsafe { GetLastError() }
            );
        }
        Ok(KsDevice { h, ev: Event::new(true)? })
    }

    /// One KS property request on the Magewell filter set. Returns bytes transferred.
    pub fn property(&self, id: u32, flags: u32, extra: &[u8], data: &mut [u8]) -> Result<u32> {
        // KS expects 8-byte aligned property/data buffers; build them in u64 storage.
        let req_len = 24 + extra.len();
        let mut req = vec![0u64; req_len.div_ceil(8)];
        let rb = unsafe { std::slice::from_raw_parts_mut(req.as_mut_ptr() as *mut u8, req.len() * 8) };
        rb[..16].copy_from_slice(&PROPSETID_FILTER);
        rb[16..20].copy_from_slice(&id.to_le_bytes());
        rb[20..24].copy_from_slice(&flags.to_le_bytes());
        rb[24..req_len].copy_from_slice(extra);

        let mut dat = vec![0u64; data.len().div_ceil(8).max(1)];
        let db = unsafe { std::slice::from_raw_parts_mut(dat.as_mut_ptr() as *mut u8, dat.len() * 8) };
        db[..data.len()].copy_from_slice(data);

        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.ev.0;
        unsafe { ResetEvent(self.ev.0) };
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.h,
                IOCTL_KS_PROPERTY,
                req.as_ptr() as *const _,
                req_len as u32,
                dat.as_mut_ptr() as *mut _,
                data.len() as u32,
                &mut ret,
                &mut ov,
            )
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                bail!("IOCTL_KS_PROPERTY id {id:#x} flags {flags} failed: Win32 error {e}{}", hint(e));
            }
            if unsafe { GetOverlappedResult(self.h, &ov, &mut ret, 1) } == 0 {
                let e = unsafe { GetLastError() };
                bail!("IOCTL_KS_PROPERTY id {id:#x} flags {flags} failed: Win32 error {e}{}", hint(e));
            }
        }
        data.copy_from_slice(&db[..data.len()]);
        Ok(ret)
    }

    fn i2c_req(&self, dev8: u8, reg: u8) -> [u8; 6] {
        let c = I2C_CHANNEL.to_le_bytes();
        [c[0], c[1], c[2], c[3], dev8, reg]
    }
}

fn hint(e: u32) -> &'static str {
    match e {
        1168 | 1170 => " (property not found: this driver build may not have the hidden diagnostic properties)",
        5 => " (access denied)",
        87 => " (invalid parameter: request/data size rejected)",
        _ => "",
    }
}

impl RegBus for KsDevice {
    fn i2c_read(&mut self, dev8: u8, reg: u8, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() || reg as usize + buf.len() > 256 {
            bail!("I2C read of {} bytes at {reg:#04x} crosses the 256-byte map", buf.len());
        }
        let req = self.i2c_req(dev8, reg);
        self.property(PROP_I2C, KSPROPERTY_TYPE_GET, &req, buf)?;
        Ok(())
    }

    fn i2c_write(&mut self, dev8: u8, reg: u8, data: &[u8]) -> Result<()> {
        if data.is_empty() || reg as usize + data.len() > 256 {
            bail!("I2C write of {} bytes at {reg:#04x} crosses the 256-byte map", data.len());
        }
        let req = self.i2c_req(dev8, reg);
        let mut d = data.to_vec();
        self.property(PROP_I2C, KSPROPERTY_TYPE_SET, &req, &mut d)?;
        Ok(())
    }

    fn bar0_read(&mut self, off: u32) -> Result<u32> {
        bar0_check(off)?;
        let mut d = [0u8; 4];
        self.property(PROP_BAR0, KSPROPERTY_TYPE_GET, &off.to_le_bytes(), &mut d)?;
        Ok(u32::from_le_bytes(d))
    }
}
