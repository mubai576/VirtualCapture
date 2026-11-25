"""Execute shipped PowerShell launcher with mocked HTTP/processes; no real service."""
import json
import os
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.mark.skipif(os.name != 'nt', reason='Windows PowerShell launcher')
@pytest.mark.parametrize('image,exists,expected', [
    (None, True, 'vc-agent.exe'),
    ('vc-agent-0_5_2.exe', True, 'vc-agent-0_5_2.exe'),
    ('vc-agent-0_5_2-12345-1.exe', True, 'vc-agent-0_5_2-12345-1.exe'),
    ('../other.exe', True, None),
    ('C:\\other.exe', True, None),
    ('vc-agent-missing.exe', False, None),
])
def test_launcher_uses_safe_version_pointer(tmp_path, image, exists, expected):
    scripts = tmp_path / 'scripts'
    scripts.mkdir()
    # Distribution adds BOM for Windows PowerShell 5.1.
    (scripts / 'start-agent.ps1').write_text((ROOT / 'scripts/start-agent.ps1').read_text(encoding='utf-8-sig'), encoding='utf-8-sig')
    (tmp_path / 'vc-agent.exe').write_bytes(b'old-image')
    metadata = {'version': 'new'}
    if image:
        metadata['exe'] = image
        if exists and '/' not in image and '\\' not in image:
            (tmp_path / image).write_bytes(b'new-image')
    (tmp_path / 'version.json').write_text(json.dumps(metadata), encoding='utf-8')
    wrapper = tmp_path / 'mock-launch.ps1'
    wrapper.write_text('''param([string]$Root)
$ErrorActionPreference='Stop'
function Invoke-RestMethod { throw 'mock unavailable' }
function Start-Process {
 param($FilePath,$ArgumentList,$WorkingDirectory,$WindowStyle,[switch]$PassThru,$RedirectStandardOutput,$RedirectStandardError)
 [IO.File]::WriteAllText((Join-Path $Root 'chosen.txt'), $FilePath)
 throw 'TEST_CAPTURED'
}
try { & (Join-Path $Root 'scripts/start-agent.ps1') -Root $Root -Role capture }
catch { Write-Output $_.Exception.Message; exit 0 }
throw 'Unexpected real launcher return'
''', encoding='utf-8-sig')
    result = subprocess.run(['powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', str(wrapper), str(tmp_path)], capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, result.stderr
    chosen = tmp_path / 'chosen.txt'
    if expected:
        assert chosen.read_text() == str(tmp_path / expected)
        assert 'TEST_CAPTURED' in result.stdout
    else:
        assert not chosen.exists(), 'bad/missing pointer must never fall back to old image'
