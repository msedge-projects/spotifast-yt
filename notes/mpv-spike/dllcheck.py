import ctypes, os, sys

cands = [
    'vulkan-1.dll',
    'api-ms-win-core-winrt-l1-1-0.dll',
    'api-ms-win-core-winrt-error-l1-1-1.dll',
    'api-ms-win-crt-private-l1-1-0.dll',
    'api-ms-win-core-synch-l1-2-0.dll',
    'api-ms-win-core-path-l1-1-0.dll',
    'd2d1.dll', 'DWrite.dll', 'AVICAP32.dll', 'AVRT.dll', 'bcryptprimitives.dll',
]
k32 = ctypes.WinDLL('kernel32', use_last_error=True)
for name in cands:
    h = k32.LoadLibraryExW(ctypes.c_wchar_p(name), None, 0)
    err = ctypes.get_last_error()
    print(f'{name:<45} load={"OK" if h else f"FAIL err={err}"}')
    if h:
        k32.FreeLibrary(h)

exe = sys.argv[1] if len(sys.argv) > 1 else r'C:\Users\DELL\AppData\Local\Programs\mpv\mpv.exe'
h = k32.LoadLibraryExW(ctypes.c_wchar_p(exe), None, 0)
print(f'\nmpv.exe: load={"OK" if h else f"FAIL err={ctypes.get_last_error()}"}')
