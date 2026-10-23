# check_bins.py — check binary timestamps and sizes on PC1
import os, time
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
build = os.path.join(root, "build", "Release")
for name in ["vc-sender.exe", "vc-receiver.exe", "vc-testwin.exe"]:
    p = os.path.join(build, name)
    if os.path.exists(p):
        mtime = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(os.path.getmtime(p)))
        print(f"{name}: {os.path.getsize(p)//1024}KB  mtime={mtime}")
    else:
        print(f"{name}: MISSING")
