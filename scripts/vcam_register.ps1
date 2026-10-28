# vcam_register.ps1 -- (re)register ROOT\vc-vcam.dll for the CURRENT user (HKCU, no admin) and report state.
# Run as the same user that runs the DShow client. Usage: -File vcam_register.ps1 [-Unregister]
param([switch]$Unregister)
$root = Split-Path -Parent $PSScriptRoot
$dll = Join-Path (Join-Path $root 'build') 'vc-vcam.dll'   # OTA live location (agent BUILD_DIR)
if (-not (Test-Path $dll)) { $dll = Join-Path $root 'vc-vcam.dll' }
if (-not (Test-Path $dll)) { Write-Output "FAIL dll missing: $dll"; exit 2 }
# processes that still hold an old vc-vcam.dll (must be restarted to load the new one)
$holders = Get-Process -ErrorAction SilentlyContinue | Where-Object {
  try { $_.Modules | Where-Object { $_.ModuleName -ieq 'vc-vcam.dll' } } catch { $null } } |
  ForEach-Object { "$($_.Name)($($_.Id))" }
Write-Output ("holders: " + ($(if ($holders) { $holders -join ',' } else { 'none' })))
$args = @('/s'); if ($Unregister) { $args += '/u' }; $args += "`"$dll`""
$p = Start-Process -FilePath "$env:SystemRoot\System32\regsvr32.exe" -ArgumentList $args -Wait -PassThru
Write-Output "regsvr32 exit=$($p.ExitCode) unregister=$([bool]$Unregister)"
$key = 'HKCU:\Software\Classes\CLSID\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}\InprocServer32'
if (Test-Path $key) { Write-Output ("InprocServer32=" + (Get-ItemProperty $key).'(default)') } else { Write-Output "InprocServer32=(none)" }
Write-Output ("dll sha256=" + (Get-FileHash $dll -Algorithm SHA256).Hash)
exit $p.ExitCode
