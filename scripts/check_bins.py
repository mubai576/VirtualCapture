# check_bins.py — check binary timestamps and sizes on PC1
import os, sys, time
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(root, "ctrl"))
from runtime_paths import find_binary
for name in ["vc-sender.exe", "vc-receiver.exe", "vc-testwin.exe"]:
    p = find_binary(name, root)
    if os.path.exists(p):
        mtime = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(os.path.getmtime(p)))
        print(f"{name}: {os.path.getsize(p)//1024}KB  mtime={mtime}")
    else:
        print(f"{name}: MISSING")
