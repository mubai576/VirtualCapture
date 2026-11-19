[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$Root)
$ErrorActionPreference = 'Stop'
try {
    $Root = [IO.Path]::GetFullPath($Root)
    $releaseRoot = $Root
    $releaseId = '.'
    $pointer = Join-Path $Root 'app/current.json'
    if (Test-Path -LiteralPath $pointer) {
        $active = Get-Content -LiteralPath $pointer -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($active.schema -ne 1 -or -not $active.build_id -or $active.build_id -notmatch '^[A-Za-z0-9._-]{1,64}$' -or $active.build_id -in @('.', '..')) { throw 'Invalid active release pointer' }
        $releaseId = $active.build_id
        $releaseRoot = Join-Path (Join-Path $Root 'app/releases') $releaseId
    }
    $exe = Join-Path $releaseRoot 'vc-capture.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw 'Active vc-capture.exe not found' }
    # Return only an ASCII ID. The CMD caller keeps its original Unicode root,
    # and executes the CLI directly to preserve native console shutdown.
    Write-Output $releaseId
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
