# Aleph0 hypervisor roadmap

This is a learning-oriented Intel VT-x hypervisor booted as a UEFI application. Each milestone should leave a small result that can be checked before adding the next piece. The first goal is a controlled guest instruction and VM exit, not an operating system boot.

## Current state

- [x] Build a `x86_64-unknown-uefi` application with `cargo build`.
- [x] Boot the EFI application with the Hyper-V Gen 2 runner (`run-hyperv.ps1`); nested virtualization exposes VMX. The runner requires an elevated PowerShell session.
- [x] Check CPUID VMX support and firmware's `IA32_FEATURE_CONTROL` settings.
- [x] Set `CR4.VMXE`, apply the VMX CR0/CR4 fixed bits, allocate a VMXON region, and enter VMX root operation.
- [x] Allocate and load a VMCS; verify a `VMWRITE`/`VMREAD` round trip.
- [x] Read and print current selectors, descriptor-table registers, and FS/GS/TR bases.
- [x] Establish a usable host task register. The tested Hyper-V boot reported a nonzero `TR`; activation checks GDTR, TR, and the decoded TSS base against the prepared values.

There is no guest execution, VM-exit handler, EPT, or OS boot yet. VMCS constants for guest RIP/RSP, host RIP/RSP, and exit reason are defined but not yet used for entry.

## 1. Establish a host TSS

- [x] Allocate a long-lived 64-bit TSS and a GDT that preserves the descriptors used by the current CS/SS and other active selectors; append a present 16-byte, 64-bit available-TSS descriptor.
- [x] Load that GDT and use `LTR` with its TSS selector. Keep the GDT and TSS alive while the CPU uses them; do not modify the firmware's GDT in place.
- [x] Re-read `STR`, GDTR, and the TSS base. Confirm `TR` is nonzero and its descriptor resolves to the allocated TSS before copying this state into the VMCS.
- [x] Check firmware interactions on the current boot path: after VMXON, the VMCS self-test, and UEFI calls, GDTR, TR, TSS base, and IDTR still match the activated state.

**Checkpoint:** The reported Hyper-V boot had no failures and printed a nonzero TR selector; activation verified its descriptor's base against the allocated TSS, and the post-VMX check reported the tables preserved. No `VMLAUNCH` yet. Recheck if new firmware call paths are added.

## 2. Prepare a minimal VMCS

- [ ] First, read `IA32_VMX_BASIC` and check bit 55. Read the true-control MSRs if available, otherwise the ordinary control MSRs, for pin-based, primary processor-based, VM-exit, and VM-entry controls. Print each MSR's low and high 32-bit halves. This only discovers the CPU's rules: do not write VMCS controls or launch a guest yet. Checkpoint: a Hyper-V boot prints all four values without faulting.
- [ ] Next, derive legal values from those MSRs: the low halves specify bits that must be 1, and the high halves specify bits allowed to be 1. Reject requested features the CPU cannot enable; do not hard-code arbitrary control values. Write the four controls to the VMCS and read them back. A successful `VMWRITE`/`VMREAD` proves storage, not that VM entry will succeed.
- [ ] Build a known, simple 64-bit guest context: controlled code and stack, valid CR0/CR3/CR4, segment selectors/bases/limits/access rights, GDTR/IDTR, RIP/RSP, RFLAGS bit 1 set, and a VMCS link pointer of all ones. Keep backing memory alive and accessible.
- [ ] Fill required host fields using the actual root-mode state: selectors, CRs, FS/GS/TR bases, GDTR/IDTR bases, and a dedicated VM-exit stack and handler RIP. Check the host selectors and addresses against VM-entry rules.
- [ ] Specify how UEFI memory and address translation are used before attempting entry. Initial experiments can use a controlled identity-mapped environment without EPT; that is not guest memory isolation.

**Checkpoint:** After capability discovery, required VMCS fields are written and selected ones read back. An invalid-field `VMWRITE` is reported with the VM-instruction error code, rather than silently ignored. No `VMLAUNCH` yet.

## 3. Launch and observe one guest exit

- [ ] Execute `VMLAUNCH` into a tiny guest that deliberately executes `VMCALL`.
- [ ] On instruction failure, distinguish VMfailInvalid, VMfailValid (read `VM_INSTRUCTION_ERROR`), and VM-entry failure reported as a VM exit. Do not treat every return as a successful guest run.
- [ ] In a VM-exit entry stub, preserve guest general-purpose registers before using Rust code. Read `VM_EXIT_REASON` and record the result; use a known-good host stack.
- [ ] Handle only the expected `VMCALL` at first. Define a controlled stopping/return path; do not blindly `VMRESUME` at the same guest RIP.

**Checkpoint:** A Hyper-V run confirms that the guest executed and records the expected `VMCALL` exit reason, not merely a successful VMCS write.

## 4. Make guest execution repeatable

- [ ] Advance guest RIP by the VM-exit instruction length when appropriate, then `VMRESUME` and handle a second known exit.
- [ ] Handle unexpected exits and failed resume paths explicitly. Add clean VMCS/VMX shutdown and restore host state before returning control to firmware.
- [ ] Plan for interrupts, exceptions, and firmware calls while in VMX root operation; avoid assuming a UEFI print call is safe inside a low-level exit stub.

**Checkpoint:** A short guest sequence produces predictable multiple exits and terminates cleanly.

## 5. Add isolation and devices incrementally

- [ ] Check CPU support for secondary execution controls and EPT; allocate and validate EPT tables before enabling them. Verify allowed guest physical accesses and an intentional EPT-violation exit.
- [ ] Add only the necessary exits first (for example CPUID and I/O), then define the virtual devices needed by a larger guest.
- [ ] Decide how the hypervisor will own memory and persist after `ExitBootServices`. Loader-data pages and UEFI identity mapping are only assumptions for the current boot-services experiment.
- [ ] Expand to multiple vCPUs, scheduling, and an OS guest only after the single-vCPU lifecycle is reliable.

## How to verify each milestone

- Run `cargo build` after each change. It checks compilation, not VM-entry validity.
- Run `run-hyperv.ps1` from an elevated PowerShell session and inspect the VM console. The runner turns off the named test VM and recreates its ESP VHDX; do not keep irreplaceable data there.
- Record observed VMX instruction errors and VM-exit reasons as milestones are reached. QEMU without nested VMX can still test non-VMX diagnostics, but cannot validate `VMLAUNCH`.

Relevant code: `src/main.rs`, `src/vmx/segment.rs`, `src/vmx/vmxon.rs`, `src/vmx/vmcs.rs`, and `run-hyperv.ps1`.