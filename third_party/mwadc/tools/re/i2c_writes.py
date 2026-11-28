"""List every xi_i2c_master_write/read with constant map/register in Ghidra-decompiled C.

usage: i2c_writes.py <decompiled.c from DecompAll.java> <lo_hex> <hi_hex>
Only functions whose entry address is in [lo, hi) are scanned; the ADV7842 front-end of driver
1.3.4458 is 108a00..10e000.
"""
import sys, re

MAPS = {'0x40': 'IO', '0x44': 'CP', '0x4c': 'AFE', '0x90': 'SDP', '0x94': 'SDPIO', '0x68': 'HDMI',
        '0x6c': 'EDID', '0x64': 'RPT'}

src = open(sys.argv[1]).read()
lo, hi = int(sys.argv[2], 16), int(sys.argv[3], 16)
for blk in src.split('// ==== ')[1:]:
    hdr, body = blk.split('\n', 1)
    fn, _, addr = hdr.partition(' @ ')
    if not lo <= int(addr, 16) < hi:
        continue
    b = re.sub(r'\s+', ' ', body)
    for m in re.finditer(r'xi_i2c_master_(write|read)\([^,]+,[^,]+,(0x[0-9a-f]+),(0x[0-9a-f]+|\d+),([^;]*?)\);', b):
        rw, mp, reg, val = m.groups()
        val = val.strip()[:60] if rw == 'write' else ''
        print(f"{fn:40s} {rw[0].upper()} {MAPS.get(mp, mp):5s} 0x{int(reg, 0):02X} {val}")
