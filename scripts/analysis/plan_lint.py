# 查PLAN格式
import re
lines = open("PLAN.md", encoding="utf-8").read().splitlines()
heads = {}
for i, l in enumerate(lines):
    m = re.match(r"## (\d+)\.", l)
    if m:
        heads.setdefault(int(m.group(1)), []).append(i + 1)
dup = {k: v for k, v in heads.items() if len(v) > 1}
print("sections:", sorted(heads), "duplicates:", dup)
import itertools
tail = "\n".join(lines[430:])
refs = sorted(set(int(x) for x in re.findall(r"S(\d+)", tail)))
missing = [r for r in refs if r not in heads]
print("S-refs in tail:", refs, "missing targets:", missing)
