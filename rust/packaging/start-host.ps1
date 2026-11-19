[CmdletBinding()]
param([string]$Root = '')
$ErrorActionPreference = 'Stop'
try {
    $packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
    if (-not $Root) { $Root = $packageRoot }
    $Root = [IO.Path]::GetFullPath($Root)
    [IO.Directory]::CreateDirectory($Root) | Out-Null
    $releaseRoot = $packageRoot
    $pointer = Join-Path $Root 'app/current.json'
    if (Test-Path -LiteralPath $pointer) {
        $active = Get-Content -LiteralPath $pointer -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($active.schema -ne 1 -or -not $active.build_id -or $active.build_id -notmatch '^[A-Za-z0-9._-]{1,64}$' -or $active.build_id -in @('.', '..')) { throw 'Invalid active release pointer' }
        $releaseRoot = Join-Path (Join-Path $Root 'app/releases') $active.build_id
    }
    if (-not (Test-Path -LiteralPath (Join-Path $releaseRoot 'vc-host.exe'))) { throw 'Active vc-host.exe not found' }
    $dataRoot = if (Test-Path -LiteralPath $pointer) { Join-Path $Root 'data' } else { $Root }
    $logRoot = Join-Path $dataRoot 'run'
    [IO.Directory]::CreateDirectory($logRoot) | Out-Null
    $tag = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
    $stdout = Join-Path $logRoot ("host-$tag.stdout.log")
    $stderr = Join-Path $logRoot ("host-$tag.stderr.log")
    # Remove trailing separators before Windows argv quoting (C:\ becomes C: only
    # temporarily, then append a dot so a volume root remains an absolute path).
    $rootArg = $Root.TrimEnd('\', '/') + '\.'
    $nativeArg = $releaseRoot.TrimEnd('\', '/') + '\.'
    $exeArgs = '--root "' + $rootArg + '" --native-dir "' + $nativeArg + '"'
    $child = Start-Process -FilePath (Join-Path $releaseRoot 'vc-host.exe') -ArgumentList $exeArgs -WorkingDirectory $releaseRoot -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
    Start-Sleep -Milliseconds 700
    if ($child.HasExited) {
        throw "vc-host exited early (code $($child.ExitCode)). See $stderr"
    }
    Write-Output "vc-host PID=$($child.Id) root=$Root"
} catch {
    Write-Error $_
    exit 1
}
