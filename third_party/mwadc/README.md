# mwadc

Raw multi-channel ADC capture with a **Magewell Pro Capture HDMI** (PCIe; Artix-7 + Analog Devices ADV7842) on its
**stock driver and firmware**, for [vhs-decode](https://github.com/oyvindln/vhs-decode) (FM RF from VCR head
amplifiers, hi-fi audio RF, composite). The Pro Capture DVI and AIO (same chip) are supported too, untested (doc 15).
MISRC-GUI uses it as the Magewell capture source (`misrc_tools/misrc_gui/input/gui_mwadc.c`, doc 12 §7).

- 3 synchronous channels × 10 bits, gapless, exactly 40 MSPS (up to ~100 MSPS for three channels, 160 MSPS for one),
  frames up to 2048 × 2048 samples, plus the card's line-in audio, resampled onto the RF sample clock (exactly 48000
  samples per 40 M RF samples, doc 12 §9).
- No external sync signal, no driver patch, nothing flashed; works with the signed Windows driver from a normal user
  account. The trick: the ADV7842 free-runs as a raw digitizer, and the driver's lock check is satisfied by moving the
  chip's CP register map and putting fake sync readbacks in its EDID RAM (doc 11).
- Usable RF swing on an unmodified card: ≈ 780 of 1024 steps on one channel, or ≈ 444 on every channel (doc 13).

## Layout

Rust workspace (Windows), GPL-3.0-or-later (`COPYING`):

| Crate | Output |
|---|---|
| `core/` (`mwadc-core`) | library: recipe, `CaptureConfig`, `Device` (open/arm/restore), threaded `Stream`, calibration |
| `cli/` (`mwadc`) | `mwadc.exe` |
| `ffi/` (`mwadc-ffi`) | `mwadc_ffi.dll` + `ffi/include/mwadc.h` (C API, used by MISRC-GUI); `ffi/tests/c_smoke.c` hardware smoke test |

`docs/` is the hardware and tool documentation (below), `tools/` has the WAV/label exporter, the interactive capture
viewer and the reverse-engineering scripts.

## Build

Needs Rust with the MSVC toolchain (<https://rustup.rs>).

```
scripts\build.cmd                         # Windows: builds the CLI, dist\mwadc.exe, stages LibMWCapture.dll
dist\mwadc.exe list
dist\mwadc.exe -b 0 probe                 # read-only survey
dist\mwadc.exe -b 0 calibrate             # silent inputs: store per-input centres/gains
dist\mwadc.exe -b 0 capture --seconds 10  # capture, then restore the card (-o key=value: mwadc options)
dist\mwadc.exe export <session>           # per-channel .s16 (any OS)
scripts/check.sh                          # macOS/Linux: Windows type-check + unit tests
```

At run time the Magewell driver and the Capture SDK runtime (`LibMWCapture.dll`) are needed; nothing from the SDK is
needed to build.

### MISRC-GUI

This repository is developed here and copied into MISRC-GUI, where these files are never edited: everything as
`third_party/mwadc`, except `misrc-gui/`, which holds the GUI's own Magewell files (`input/gui_mwadc.c/.h`, the
capture backend; `ui/gui_ui_mwadc.c/.h`, the settings panel with all its texts) and goes to `misrc_tools/misrc_gui/`.
The `ENABLE_MWADC` hooks in other GUI files and the meson options stay MISRC-GUI's. With a MISRC-GUI checkout next to
this one:

```
powershell -ExecutionPolicy Bypass -File scripts\build-all.ps1
```

builds `dist\mwadc.exe`, copies this working tree (committed or not) into `..\MISRC-GUI` (adding an
`AGENTS.md`/`CLAUDE.md` to `third_party/mwadc` that points back here), and builds the GUI with `mwadc_ffi.dll` (MSYS2
UCRT64 meson, `-Dmwadc=enabled`; meson runs `scripts/meson-cargo.py`, which builds only the C API crate). Nothing is
committed in MISRC-GUI. If a file there was changed since the last copy (an upstream merge, a maintainer's edit), the
script copies nothing, shows the diff and stops: bring the change in here first, or pass `-Force` to overwrite it.
`-MisrcGui <path>`, `-SkipCli`, `-SkipSync`, `-SkipGui` adjust it (see the script header).

## Documentation (`docs/`)

| # | File | Contents |
|---|---|---|
| 01 | `01-goal-and-requirements.md` | What vhs-decode needs, what the card delivers |
| 02 | `02-hardware-overview.md` | Card family, components, measured basics |
| 03 | `03-adv7842-raw-adc-path.md` | The ADV7842 CP as a free-running raw digitizer |
| 06 | `06-driver-and-register-map.md` | Driver structure, FPGA BAR0 blocks, DMA |
| 07 | `07-userspace-adv7842-access.md` | Hidden KS properties (Windows) and private ioctls (Linux) for I2C/BAR0 access |
| 09 | `09-open-questions.md` | What is still unknown and how to find out |
| 10 | `10-references.md` | Source material and external links |
| 11 | `11-stock-driver-raw-capture.md` | How the stock driver works and the capture sequence; rates, frame sizes, sample format |
| 12 | `12-mwadc.md` | The `mwadc` tool: build, commands, what it writes, output files, helpers |
| 13 | `13-analog-inputs.md` | Analog clamp, usable swing, one vs. two RF channels, board options, bandwidth |
| 14 | `14-front-end-gain.md` | Measured data path: CP gain (12-bit), analog range (AFE 0x0C), centre (manual clamp), anti-alias filters, calibration math |

## Conventions

- Register notation follows ADI UG-214: `NAME, Addr <map> (<block>), Address 0xRR[bits]`, with 8-bit I2C addresses
  (IO 0x40, CP 0x44, AFE 0x4C, …).
- "Verified" means read from a datasheet, driver source or decompiled binary; "measured" means observed on the card.
