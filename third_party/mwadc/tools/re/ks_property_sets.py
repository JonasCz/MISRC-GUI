"""List KSPROPERTY_SET tables (GUID, item IDs, MinProperty/MinData, Get/Set) in a 64-bit AVStream .sys.

usage: ks_property_sets.py MWProCapture.sys   (scans .rdata for set descriptors;
the Magewell main set {3A45B930-...} has 90 items incl. hidden 0x40000000+ IDs in driver 4323)
"""
import sys, struct, uuid
# Find KSPROPERTY_SET entries {GUID*, ULONG count, PKSPROPERTY_ITEM, ULONG fastcount, fastio*}; dump each set's items.
d=open(sys.argv[1],'rb').read()
pe=struct.unpack_from('<I',d,0x3c)[0]; nsec=struct.unpack_from('<H',d,pe+6)[0]; optsz=struct.unpack_from('<H',d,pe+20)[0]
base=struct.unpack_from('<Q',d,pe+24+24)[0]
secs=[]
for i in range(nsec):
    o=pe+24+optsz+40*i; vs,va,rs,ro=struct.unpack_from('<IIII',d,o+8); secs.append((d[o:o+8].rstrip(b'\0').decode(),va,rs,ro))
def v2o(v):
    rva=v-base
    for n,va,rs,ro in secs:
        if va<=rva<va+rs: return ro+rva-va
for n,va,rs,ro in secs:
    if n!='.rdata': continue
    for o in range(ro,ro+rs-40,8):
        g,cnt,items,fc,fio,_=struct.unpack_from('<QI4xQI4xQQ',d,o)
        if not(1<=cnt<=400 and fc==0 and fio==0): continue
        go,io=v2o(g),v2o(items)
        if go is None or io is None: continue
        ok=True; rows=[]
        for k in range(cnt):
            pid,get,minp,mind,setp=struct.unpack_from('<I4xQIIQ',d,io+k*0x48)
            if minp>0x1000: ok=False;break
            rows.append(f"{pid:#x}:{minp}/{mind}{'G' if get else ''}{'S' if setp else ''}")
        if ok: print(uuid.UUID(bytes_le=d[go:go+16]), cnt, ' '.join(rows))
