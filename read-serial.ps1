<#
.SYNOPSIS
    Streams the test VM's COM1 output to this console and to run\serial.log.

.DESCRIPTION
    Hyper-V exposes a VM's COM port as a named pipe (configured by
    run-hyperv.ps1 with Set-VMComPort). Hyper-V is the pipe server; this
    script connects as the client and prints each line the hypervisor sends.

    Output written while no client is connected is dropped, so connect as
    soon as the VM starts. run-hyperv.ps1 does that automatically; run this
    script on its own to reattach to a VM that is already running.

    Requires an elevated PowerShell session (the pipe is only open to
    administrators). Reading ends when the VM turns off; Ctrl+C also stops it
    once the next line arrives.

.PARAMETER PipeName
    Pipe name without the \\.\pipe\ prefix. Must match run-hyperv.ps1.

.PARAMETER TimeoutSeconds
    How long to keep retrying while the VM is starting and the pipe does
    not exist yet.
#>
param(
    [string]$PipeName      = "aleph0-com1",
    [int]$TimeoutSeconds   = 30
)

# See run.ps1 for why $ErrorActionPreference is not set to "Stop" globally.

$runDir  = Join-Path $PSScriptRoot "run"
$logPath = Join-Path $runDir "serial.log"
New-Item -ItemType Directory -Force -Path $runDir | Out-Null

$pipe = [System.IO.Pipes.NamedPipeClientStream]::new(
    ".", $PipeName, [System.IO.Pipes.PipeDirection]::InOut)

# The pipe only exists while the VM is running, so retry until it appears.
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
while (-not $pipe.IsConnected) {
    try {
        # Waits up to 1 s for the pipe to appear; throws TimeoutException if not.
        $pipe.Connect(1000)
    } catch {
        Start-Sleep -Milliseconds 250
    }
    if (-not $pipe.IsConnected -and (Get-Date) -gt $deadline) {
        Write-Error "Could not connect to \\.\pipe\$PipeName within $TimeoutSeconds s. Is the VM running with COM1 attached?"
        $pipe.Dispose()
        exit 1
    }
}

Write-Host "==> Connected to \\.\pipe\$PipeName; logging to $logPath" -ForegroundColor Cyan
Set-Content -Path $logPath -Value $null

$reader = [System.IO.StreamReader]::new($pipe, [System.Text.Encoding]::ASCII)
try {
    # ReadLine returns $null when Hyper-V closes the pipe (VM turned off).
    while ($null -ne ($line = $reader.ReadLine())) {
        Write-Host $line
        Add-Content -Path $logPath -Value $line
    }
    Write-Host "==> Serial pipe closed (VM stopped)." -ForegroundColor Cyan
} finally {
    $reader.Dispose()
    $pipe.Dispose()
}
