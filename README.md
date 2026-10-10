# Aleph0 Virtualization Hypervisor

A learning-oriented Intel VT-x hypervisor written in Rust and booted as a UEFI
application. It currently enters VMX root operation and prepares a VMCS; no
guest runs yet. See [ROADMAP.md](ROADMAP.md) for what has been verified and
what comes next.

## Requirements

- Rust stable (edition 2024, Rust 1.85+). `rust-toolchain.toml` installs the
  `x86_64-unknown-uefi` target automatically.
- An Intel CPU with VT-x. VMX instructions only run under a VMM that exposes
  nested virtualization (Hyper-V is the tested path).

## Build

```powershell
cargo build            # produces target\x86_64-unknown-uefi\debug\aleph0hypervisor.efi
```

`.cargo/config.toml` makes the UEFI target the default.

## Test

```powershell
cargo test-host        # alias for: cargo test --target x86_64-pc-windows-msvc
```

Unit tests cover the pure logic (RFLAGS decoding, control-value selection,
descriptor encoding/decoding) and run on the host. Nothing that executes VMX
instructions can be unit-tested; that still needs a boot under Hyper-V.

## Run

**Hyper-V (VMX available):** from an elevated PowerShell session,

```powershell
.\run-hyperv.ps1
```

This turns off and reconfigures the `Aleph0Test` VM and recreates its ESP disk
under `run/`; keep nothing important there. It then streams the VM's COM1
serial output into the PowerShell window and `run\serial.log`
(`-NoSerial` skips this; `.\read-serial.ps1` reattaches later). VM-exit
diagnostics only appear there, not on the VM console. See
[docs/SERIAL_LOGGING.md](docs/SERIAL_LOGGING.md).

**QEMU + OVMF (no VMX):** put `OVMF_CODE.fd` and `OVMF_VARS.fd` from
[rust-osdev/ovmf-prebuilt](https://github.com/rust-osdev/ovmf-prebuilt) in
`ovmf/`, then run `.\run.ps1`. Without nested VMX this only exercises the
diagnostics that run before VMXON.

## Layout

| Path | Purpose |
| --- | --- |
| `src/main.rs` | UEFI entry point; drives the bring-up sequence |
| `src/serial.rs` | COM1 serial logger usable without firmware |
| `src/vmx/cpuid.rs`, `msr.rs`, `cr.rs` | CPU feature, MSR and control-register access |
| `src/vmx/host_tables.rs`, `segment.rs` | Host GDT/TSS setup and segmentation state |
| `src/vmx/vmxon.rs` | Entering VMX root operation |
| `src/vmx/vmcs/mod.rs`, `fields.rs` | VMCS lifecycle, field access, controls and host state; every field encoding |
| `src/vmx/vmexit.rs` | VM-exit stack, entry stub and exit diagnostic |
| `run-hyperv.ps1`, `read-serial.ps1` | Hyper-V test VM runner and serial reader |
| `docs/` | Background explanations: [SERIAL_LOGGING.md](docs/SERIAL_LOGGING.md) (serial logger, beginner walkthrough), [PORT_IO.md](docs/PORT_IO.md) (`in`/`out` and the I/O port space) |
