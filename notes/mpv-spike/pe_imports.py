import struct, sys

path = sys.argv[1]
data = open(path, 'rb').read()

e_lfanew = struct.unpack_from('<I', data, 0x3C)[0]
assert data[e_lfanew:e_lfanew+4] == b'PE\0\0'
coff = e_lfanew + 4
machine, num_sections = struct.unpack_from('<HH', data, coff)
opt_size = struct.unpack_from('<H', data, coff + 16)[0]
opt_off = coff + 20
magic = struct.unpack_from('<H', data, opt_off)[0]
pe32plus = magic == 0x20B
dd_off = opt_off + (112 if pe32plus else 96)
num_dd = struct.unpack_from('<I', data, dd_off - 4)[0]
import_rva, import_size = struct.unpack_from('<II', data, dd_off + 8)

sec_off = opt_off + opt_size
sections = []
for i in range(num_sections):
    s = sec_off + 40 * i
    name = data[s:s+8].rstrip(b'\0').decode('ascii', 'replace')
    vsize, va, rawsize, raw = struct.unpack_from('<IIII', data, s + 8)
    sections.append((name, va, vsize, raw, rawsize))

def rva_to_off(rva):
    for name, va, vsize, raw, rawsize in sections:
        if va <= rva < va + max(vsize, rawsize):
            return raw + (rva - va)
    return None

def read_cstr(off):
    end = data.index(b'\0', off)
    return data[off:end].decode('ascii', 'replace')

print(f'machine=0x{machine:X} pe32plus={pe32plus} num_dd={num_dd} import_rva=0x{import_rva:X} size={import_size}')
off = rva_to_off(import_rva)
dlls = []
while True:
    fields = struct.unpack_from('<IIIII', data, off)
    ilt, tds, fwd, name_rva, ft = fields
    if ilt == 0 and name_rva == 0:
        break
    # if ilt is 0, use ft for lookup table
    lookup = ilt or ft
    name = read_cstr(rva_to_off(name_rva))
    fns = []
    fo = rva_to_off(lookup)
    j = 0
    while True:
        if pe32plus:
            entry = struct.unpack_from('<Q', data, fo + 8*j)[0]
            if entry == 0: break
            if entry & (1 << 63):
                fns.append(f'#{entry & 0xFFFF}')
            else:
                fns.append(read_cstr(rva_to_off(entry & 0x7FFFFFFF) + 2))
        else:
            entry = struct.unpack_from('<I', data, fo + 4*j)[0]
            if entry == 0: break
            if entry & (1 << 31):
                fns.append(f'#{entry & 0xFFFF}')
            else:
                fns.append(read_cstr(rva_to_off(entry & 0x7FFFFFFF) + 2))
        j += 1
    dlls.append((name, fns))
    off += 20

print(f'{len(dlls)} imported DLLs:')
for name, fns in dlls:
    print(f'  {name}  ({len(fns)} funcs)')
    print(f'    {", ".join(fns[:12])}{" ..." if len(fns) > 12 else ""}')
