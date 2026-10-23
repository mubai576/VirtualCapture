import json, sys
with open('run/selftest3.json', encoding='utf-8', errors='replace') as f:
    d = json.loads(f.read())
out = str(d.get('stdout','')) + str(d.get('stderr',''))
sys.stdout.write('dispatch: %s\n' % ('dispatch' in out.lower()))
sys.stdout.write('dda-gpu: %s\n' % ('dda-gpu' in out.lower()))
sys.stdout.write('exit: %s\n' % d.get('exit_code'))
for l in out.splitlines()[-8:]:
    sys.stdout.write(l + '\n')
