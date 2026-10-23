import zipfile, hashlib, shutil, os, tempfile
src = "release/vc-0.3.7.zip"
tmp = os.path.join(tempfile.gettempdir(), "vc-0.3.7-restage-dryrun.zip")
shutil.copy2(src, tmp)
new_sender = open("build/vc-sender.exe", "rb").read()
# 换发端重打包（存而不压，避漂移争议；记方式）
zin = zipfile.ZipFile(tmp, "r")
items = [(i.filename, zin.read(i.filename)) for i in zin.infolist()]
zin.close()
out = [(n, new_sender if n == "vc-sender.exe" else b) for n, b in items]
zout = zipfile.ZipFile(tmp, "w", compression=zipfile.ZIP_DEFLATED)
for n, b in out:
    zout.writestr(n, b)
zout.close()
# 验：余项字节全同，发端等于现构建
z0 = zipfile.ZipFile(src, "r"); z1 = zipfile.ZipFile(tmp, "r")
assert z0.namelist() == z1.namelist(), "entry order/names differ"
for n in z0.namelist():
    a, b = z0.read(n), z1.read(n)
    want_new = (n == "vc-sender.exe")
    print("%s swapped=%s identical=%s" % (n, want_new, a == b if not want_new else b == new_sender))
print("DRYRUN-OK", tmp)
