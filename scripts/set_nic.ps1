<#
.SYNOPSIS set_nic.ps1, data-NIC tuning for T-06 (Jumbo 9014 + EEE off + InterruptModeration off).
.DESCRIPTION Idempotent. Prints before/after advanced properties. Needs admin.
#>
[CmdletBinding(SupportsShouldProcess=$true)]
param([string]$Name = "", [int]$Jumbo = 9014)
$ErrorActionPreference = "Stop"
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { Write-Error "need admin (right-click Run as Administrator)"; exit 1 }
if (-not $Name) { Get-NetAdapter | Select-Object Name,Status,LinkSpeed | Format-Table -AutoSize | Out-String | Write-Host; Write-Error "pass -Name <adapter, e.g. Controller>"; exit 2 }
$cands = @(Get-NetAdapter | Where-Object { $_.Name -like "*$Name*" -or $_.InterfaceDescription -like "*$Name*" })
if ($cands.Count -ne 1) { $cands | Select-Object Name,Status,InterfaceDescription | Format-Table -AutoSize | Out-String | Write-Host; Write-Error "want 1 match, got $($cands.Count)"; exit 2 }
$nic = $cands[0]; Write-Host (("target: {0} [{1}]" -f $nic.Name, $nic.InterfaceDescription))
Write-Host "-- before:"
Get-NetAdapterAdvancedProperty -Name $nic.Name | Where-Object { $_.RegistryKeyword -match "Jumbo|EEE|Moderation" } | Select-Object DisplayName,DisplayValue | Format-Table -AutoSize | Out-String | Write-Host
if ($PSCmdlet.ShouldProcess($nic.Name, "apply NIC tuning")) {
  $ok = $true
  try { Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword "*JumboPacket" -RegistryValue $Jumbo }
  catch { Write-Warning ("Jumbo set failed"); $ok = $false }
  foreach ($kw in @("*EEE", "*InterruptModeration")) {
    try { Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword $kw -RegistryValue 0 }
    catch {
      try { Set-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword $kw -RegistryValue "Disabled" }
      catch { Write-Warning (($kw + " off failed")) }
    }
  }
  if (-not $ok) { exit 1 }
}
Write-Host "-- after:"
Get-NetAdapterAdvancedProperty -Name $nic.Name | Where-Object { $_.RegistryKeyword -match "Jumbo|EEE|Moderation" } | Select-Object DisplayName,DisplayValue | Format-Table -AutoSize | Out-String | Write-Host
$j = (Get-NetAdapterAdvancedProperty -Name $nic.Name -RegistryKeyword "*JumboPacket" -ErrorAction SilentlyContinue | Select-Object -First 1).RegistryValue
if ("$j" -ne "$Jumbo") { Write-Error (("Jumbo verify failed: got " + $j + ", want ") + $Jumbo); exit 1 }
Write-Host "set_nic PASS"
