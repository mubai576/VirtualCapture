# tune_pc1_nic.ps1 — query + optimize Realtek 2.5G NIC on PC1
$nic = Get-NetAdapter | Where-Object { $_.InterfaceDescription -like '*Realtek*' -and $_.LinkSpeed -like '*2.5*' }
if (-not $nic) { Write-Error "Realtek 2.5G adapter not found"; exit 1 }
$name = $nic.Name
Write-Output "NIC: $name ($($nic.InterfaceDescription))"

# show current values
Write-Output "`n--- BEFORE ---"
Get-NetAdapterAdvancedProperty -Name $name |
  Where-Object { $_.RegistryKeyword -match 'Flow|Interrupt|ITR|EEE|Jumbo|Idle' } |
  Select-Object RegistryKeyword, RegistryValue, DisplayValue |
  Format-Table -AutoSize

# apply optimizations
$applied = @()
try {
    Set-NetAdapterAdvancedProperty -Name $name -RegistryKeyword '*FlowControl' -RegistryValue 0 -ErrorAction Stop
    $applied += 'FlowControl=0'
} catch { Write-Output "FlowControl set failed: $_" }
try {
    Set-NetAdapterAdvancedProperty -Name $name -RegistryKeyword '*InterruptModeration' -RegistryValue 0 -ErrorAction Stop
    $applied += 'InterruptModeration=0'
} catch { Write-Output "InterruptModeration set failed: $_" }
try {
    Set-NetAdapterAdvancedProperty -Name $name -RegistryKeyword '*EEE' -RegistryValue 0 -ErrorAction Stop
    $applied += 'EEE=0'
} catch { Write-Output "EEE set failed: $_" }
# Realtek may use 'ITR' or different keyword; try common ones
foreach ($kw in @('ITR','*ITR','ReceiveThrottle')) {
    try {
        Set-NetAdapterAdvancedProperty -Name $name -RegistryKeyword $kw -RegistryValue 0 -ErrorAction Stop
        $applied += "$kw=0"
        break
    } catch { }
}

Write-Output "Applied: $($applied -join ', ')"

# show after
Write-Output "`n--- AFTER ---"
Get-NetAdapterAdvancedProperty -Name $name |
  Where-Object { $_.RegistryKeyword -match 'Flow|Interrupt|ITR|EEE|Jumbo|Idle' } |
  Select-Object RegistryKeyword, RegistryValue, DisplayValue |
  Format-Table -AutoSize
