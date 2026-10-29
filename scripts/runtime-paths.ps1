# Shared by repository and portable-package launch helpers.
function Get-VcBinaryPath {
    param([Parameter(Mandatory = $true)][string]$Name)
    $vcRoot = Split-Path -Parent $PSScriptRoot
    foreach ($vcRelative in @("build\$Name", "build\Release\$Name", $Name)) {
        $vcCandidate = Join-Path $vcRoot $vcRelative
        if (Test-Path -LiteralPath $vcCandidate -PathType Leaf) { return $vcCandidate }
    }
    throw "找不到程序 $Name（已检查根目录、build、build\Release）"
}
