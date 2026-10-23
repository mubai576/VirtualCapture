# 比两个OTA包差异
import zipfile, hashlib, os
z = zipfile.ZipFile("release/vc-0.3.7.zip")
print("entries:")
for n in z.namelist():
    print(" ", n, z.getinfo(n).file_size)
exe = z.read("vc-sender.exe")
print("zip vc-sender.exe sha256=", hashlib.sha256(exe).hexdigest())
cur = open("build/vc-sender.exe", "rb").read()
print("now vc-sender.exe sha256=", hashlib.sha256(cur).hexdigest())
print("identical=", exe == cur)
for other in ("vc-receiver.exe", "vc-agent.py", "version.json"):
    try:
        a = z.read(other)
        b = open(os.path.join("build" if other.endswith(".exe") else ("agent" if other.endswith(".py") else "."), other), "rb").read()
        print("%s identical=%s" % (other, a == b))
    except Exception as e:
        print(other, "ERR", e)
print("scripts:")
sok = True
for n in z.namelist():
    if not n.startswith("scripts/"):
        continue
    a = z.read(n)
    b = open(n.replace("/", "\\"), "rb").read()
    same = a == b
    sok = sok and same
    print(" %s identical=%s" % (n, same))
print("ALL-SCRIPTS-IDENTICAL=", sok)
