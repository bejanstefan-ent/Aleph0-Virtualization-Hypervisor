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

## Run

**Hyper-V (VMX available):** from an elevated PowerShell session,

```powershell
.\run-hyperv.ps1
```

This turns off and reconfigures the `Aleph0Test` VM and recreates its ESP disk
under `run/`; keep nothing important there.

**QEMU + OVMF (no VMX):** put `OVMF_CODE.fd` and `OVMF_VARS.fd` from
[rust-osdev/ovmf-prebuilt](https://github.com/rust-osdev/ovmf-prebuilt) in
`ovmf/`, then run `.\run.ps1`. Without nested VMX this only exercises the
diagnostics that run before VMXON.

## Layout

| Path | Purpose |
| --- | --- |
| `src/main.rs` | UEFI entry point; drives the bring-up sequence |
| `src/vmx/cpuid.rs`, `msr.rs`, `cr.rs` | CPU feature, MSR and control-register access |
| `src/vmx/host_tables.rs`, `segment.rs` | Host GDT/TSS setup and segmentation state |
| `src/vmx/vmxon.rs` | Entering VMX root operation |
| `src/vmx/vmcs.rs` | VMCS lifecycle, field access, controls and host state |
| `src/vmx/vmexit.rs` | VM-exit stack and entry stub |
