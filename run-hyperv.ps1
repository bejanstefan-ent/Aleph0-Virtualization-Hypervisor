<#
.SYNOPSIS
    Builds the hypervisor and boots it in a Hyper-V Gen 2 VM with nested
    virtualization enabled, so VMX is actually exposed to the guest.

    Requires an elevated PowerShell session and the Hyper-V role installed.

.PARAMETER VMName
    Name of the Hyper-V VM to create/reuse.

.PARAMETER MemoryMB
    Static startup memory. Nested virtualization requires Dynamic Memory off.

.PARAMETER Release
    Builds the release profile instead of debug.
#>
param(
    [string]$VMName   = "Aleph0Test",
    [int]$MemoryMB    = 2048,
    [switch]$Release
)

# See run.ps1 for why $ErrorActionPreference is not set to "Stop" globally.

$espType = '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}'   # EFI System Partition GPT type

$root         = $PSScriptRoot
$buildProfile = if ($Release) { "release" } else { "debug" }
$efiSrc       = Join-Path $root "target\x86_64-unknown-uefi\$buildProfile\aleph0hypervisor.efi"
$runDir       = Join-Path $root "run"
$vhdPath      = Join-Path $runDir "aleph0-esp.vhdx"

# 0. Preconditions
$isAdmin = ([System.Security.Principal.WindowsPrincipal]::new(
    [System.Security.Principal.WindowsIdentity]::GetCurrent())
).IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Error "This script must run from an elevated PowerShell session."
    exit 1
}
if (-not (Get-Command Get-VM -ErrorAction SilentlyContinue)) {
    Write-Error "Hyper-V role not installed. Run: Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All"
    exit 1
}

# 1. Build
Write-Host "==> cargo build $(if ($Release) { '--release' })" -ForegroundColor Cyan
Push-Location $root
if ($Release) { cargo build --release } else { cargo build }
$buildExit = $LASTEXITCODE
Pop-Location
if ($buildExit -ne 0) {
    Write-Error "cargo build failed (exit code $buildExit)."
    exit $buildExit
}
if (-not (Test-Path $efiSrc)) {
    Write-Error "Compiled binary not found at $efiSrc"
    exit 1
}

try {
    # 2. Stop the VM and detach the old disk so the VHDX can be rebuilt
    $vm = Get-VM -Name $VMName -ErrorAction SilentlyContinue
    if ($vm -and $vm.State -ne 'Off') {
        Write-Host "==> Stopping $VMName" -ForegroundColor Cyan
        Stop-VM -Name $VMName -TurnOff -Force -ErrorAction Stop
    }
    if ($vm) {
        Get-VMHardDiskDrive -VMName $VMName | Remove-VMHardDiskDrive -ErrorAction Stop
    }

    # 3. Rebuild the ESP VHDX from scratch. Simplest way to refresh BOOTX64.EFI:
    #    once the partition carries the ESP GPT type it no longer takes a drive letter.
    New-Item -ItemType Directory -Force -ErrorAction Stop -Path $runDir | Out-Null
    if (Test-Path $vhdPath) {
        Dismount-VHD -Path $vhdPath -ErrorAction SilentlyContinue
        Remove-Item -Force -ErrorAction Stop $vhdPath
    }

    Write-Host "==> Creating ESP disk $vhdPath" -ForegroundColor Cyan
    New-VHD -Path $vhdPath -SizeBytes 256MB -Dynamic -ErrorAction Stop | Out-Null

    $disk = Mount-VHD -Path $vhdPath -Passthru -ErrorAction Stop | Get-Disk
    try {
        Initialize-Disk -Number $disk.Number -PartitionStyle GPT -ErrorAction Stop
        # Formatted as basic data first: Format-Volume needs a drive letter, and an
        # ESP-typed partition will not take one. The type is set afterwards.
        $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter -ErrorAction Stop
        Format-Volume -DriveLetter $part.DriveLetter -FileSystem FAT32 `
            -NewFileSystemLabel "ESP" -Confirm:$false -ErrorAction Stop | Out-Null

        $bootDir = "$($part.DriveLetter):\EFI\BOOT"
        New-Item -ItemType Directory -Force -ErrorAction Stop -Path $bootDir | Out-Null
        Copy-Item -Force -ErrorAction Stop $efiSrc (Join-Path $bootDir "BOOTX64.EFI")

        $part | Set-Partition -GptType $espType -ErrorAction Stop
    } finally {
        Dismount-VHD -Path $vhdPath -ErrorAction SilentlyContinue
    }

    # 4. Create or reconfigure the VM
    if (-not $vm) {
        Write-Host "==> Creating VM $VMName" -ForegroundColor Cyan
        New-VM -Name $VMName -Generation 2 -MemoryStartupBytes ($MemoryMB * 1MB) `
            -NoVHD -ErrorAction Stop | Out-Null
        # No network adapter: nothing here needs one, and it keeps the nested setup simple.
        Get-VMNetworkAdapter -VMName $VMName | Remove-VMNetworkAdapter -ErrorAction Stop
    }

    Add-VMHardDiskDrive -VMName $VMName -Path $vhdPath -ErrorAction Stop
    # Secure Boot off: the .efi is unsigned. Boot straight off the ESP disk.
    Set-VMFirmware -VMName $VMName -EnableSecureBoot Off -ErrorAction Stop
    Set-VMFirmware -VMName $VMName -FirstBootDevice (Get-VMHardDiskDrive -VMName $VMName) -ErrorAction Stop
    # Nested virtualization requires static memory.
    Set-VMMemory -VMName $VMName -DynamicMemoryEnabled $false -StartupBytes ($MemoryMB * 1MB) -ErrorAction Stop
    Set-VMProcessor -VMName $VMName -Count 2 -ExposeVirtualizationExtensions $true -ErrorAction Stop
} catch {
    Write-Error "Setup failed: $_"
    exit 1
}

# 5. Boot it and open the console (uefi::println writes to ConOut, i.e. the video console)
Write-Host "==> Starting $VMName" -ForegroundColor Cyan
Start-VM -Name $VMName
vmconnect.exe localhost $VMName
