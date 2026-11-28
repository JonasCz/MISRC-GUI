"""Dump a 3-byte (i2c_addr8, reg, value) register table from an ELF64 relocatable section.

usage: adv7842_regtable.py <ProCaptureLib_64.lib> [section] [offset] [length]
Defaults dump g_regItemsInit of driver 1.3.4458 (.rodata+0x2a80, 0xe1 bytes); get other tables'
offsets from `objdump -t <lib> | grep g_regItems`.
"""
import sys, struct

MAPS = {0x40: 'IO', 0x44: 'CP', 0x4c: 'AFE', 0x90: 'SDP', 0x94: 'SDPIO', 0x68: 'HDMI', 0x6c: 'EDID',
        0x64: 'RPT', 0x7c: 'INFO', 0x80: 'CEC', 0x84: 'AVL', 0x48: 'VDP'}

f = open(sys.argv[1], 'rb').read()
name = (sys.argv[2] if len(sys.argv) > 2 else '.rodata').encode()
off = int(sys.argv[3], 0) if len(sys.argv) > 3 else 0x2a80
ln = int(sys.argv[4], 0) if len(sys.argv) > 4 else 0xe1

shoff, = struct.unpack_from('<Q', f, 0x28)
shentsize, shnum, shstrndx = struct.unpack_from('<HHH', f, 0x3A)
secs = [struct.unpack_from('<IIQQQQIIQQ', f, shoff + i * shentsize) for i in range(shnum)]
so = secs[shstrndx][4]
for s in secs:
    if f[so + s[0]:f.index(b'\0', so + s[0])] == name:
        d = f[s[4] + off:s[4] + off + ln]
        for i in range(0, len(d) - 2, 3):
            a, r, v = d[i], d[i + 1], d[i + 2]
            print(f"{MAPS.get(a, hex(a)):6s} 0x{r:02X} = 0x{v:02X}")
        break
