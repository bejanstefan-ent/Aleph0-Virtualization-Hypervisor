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
- [x] Discover VMX control MSRs, select legal control values, and write/read back the four VMCS control fields.
- [x] Write/read back host selectors, CRs, segment/table bases, SYSENTER state, and `HOST_RSP`/`HOST_RIP`. The Hyper-V run reported host PAT and EFER load controls both false, so those conditional fields were skipped.
- [x] Allocate a dedicated four-page VM-exit stack and compile an inactive entry stub that saves general-purpose registers and calls a non-returning Rust handler.
- [x] Add a firmware-independent COM1 serial logger and stream it from the Hyper-V VM to the host console and `run\serial.log` (see section 0).

There is no guest execution, observed VM exit, EPT, or OS boot yet. The VM-exit handler is written to print the exit over serial and halt, but has never run; the stored host entry addresses have only been read back, not used by a VM exit. Guest RIP/RSP and exit-reason constants exist, but guest state and `VMLAUNCH` are not configured.

## 0. Debug output that works without firmware

`uefi::println!` calls the firmware console, which is unsafe in the VM-exit handler (interrupts off, arbitrary firmware state) and gone after `ExitBootServices`. Serial output drives the UART directly with `in`/`out`, so it works in both. Background and usage: [docs/SERIAL_LOGGING.md](docs/SERIAL_LOGGING.md).

- [x] Drive the 16550 UART at COM1 (`0x3F8`) from `src/serial.rs`: 115200 8N1, polled, no interrupts. Probe with loopback before enabling output, and bound every wait so a missing UART cannot hang the hypervisor.
- [x] Provide `serial_print!`/`serial_println!` through `core::fmt` with no allocation, for use in exit context.
- [x] Route bring-up messages through `log!`, which writes each line to both the firmware console and COM1.
- [x] Attach COM1 to `\\.\pipe\aleph0-com1` in `run-hyperv.ps1` and stream it with `read-serial.ps1`, which also saves `run\serial.log`. The reader was tested against a local pipe server, not yet against Hyper-V.
- [ ] Confirm on Hyper-V: the console prints `Serial output enabled on COM1`, and the PowerShell window and `run\serial.log` show the same bring-up lines as the VM console.

**Checkpoint:** A Hyper-V run produces a `run\serial.log` that matches the VM console. Only then rely on serial as the observation path for the first VM exit.

## 1. Establish a host TSS

- [x] Allocate a long-lived 64-bit TSS and a GDT that preserves the descriptors used by the current CS/SS and other active selectors; append a present 16-byte, 64-bit available-TSS descriptor.
- [x] Load that GDT and use `LTR` with its TSS selector. Keep the GDT and TSS alive while the CPU uses them; do not modify the firmware's GDT in place.
- [x] Re-read `STR`, GDTR, and the TSS base. Confirm `TR` is nonzero and its descriptor resolves to the allocated TSS before copying this state into the VMCS.
- [x] Check firmware interactions on the current boot path: after VMXON, the VMCS self-test, and UEFI calls, GDTR, TR, TSS base, and IDTR still match the activated state.

**Checkpoint:** The reported Hyper-V boot had no failures and printed a nonzero TR selector; activation verified its descriptor's base against the allocated TSS, and the post-VMX check reported the tables preserved. No `VMLAUNCH` yet. Recheck if new firmware call paths are added.

## 2. Prepare a minimal VMCS

- [x] Read `IA32_VMX_BASIC` and check bit 55. Read the true-control MSRs if available, otherwise the ordinary control MSRs, for pin-based, primary processor-based, VM-exit, and VM-entry controls. Print each MSR's low and high 32-bit halves. The Hyper-V boot printed all four values without faulting.
- [x] Derive legal values from those MSRs: the low halves specify bits that must be 1, and the high halves specify bits allowed to be 1. Reject requested features the CPU cannot enable. Write the four controls to the VMCS and read them back. A successful `VMWRITE`/`VMREAD` proves storage, not that VM entry will succeed.
- [ ] Add a guest-memory owner (for example, `src/vmx/guest.rs`) that allocates and retains separate guest code and guest stack pages. Start with tiny guest code whose first test instruction is `VMCALL`; keep both pages alive for the entire VMX experiment.
- [ ] Decide and verify guest address translation before using those pages. With EPT disabled, guest virtual addresses translate through guest CR3; confirm the code page is executable and the separate stack page is writable under those page tables. Do not assume a UEFI allocation address is automatically usable by the guest.
- [ ] Build a known, simple 64-bit guest context: valid CR0/CR3/CR4, segment selectors/bases/limits/access rights, GDTR/IDTR, RIP pointing at the guest code, RSP pointing at the guest stack, RFLAGS bit 1 set, and a VMCS link pointer of all ones. Initialize all other guest fields required by the selected controls and retain their backing memory.
- [ ] Add a guest-state writer and read back the fields it writes. This checks VMCS storage only; the CPU's VM-entry checks happen when entry is attempted.
- [x] Write/read back host fields from the current root-mode state: selectors, CRs, FS/GS/TR and GDTR/IDTR bases, SYSENTER MSRs, and the dedicated VM-exit stack top and stub address. Load host PAT/EFER fields only if the selected exit controls require them.
- [ ] Complete VM-entry validation of host selectors, CRs, and addresses; write/readback alone does not prove the host state will pass VM-entry checks.
- [ ] Document the UEFI memory and address-translation assumptions for this experiment. A shared/identity-mapped address space without EPT is only a learning setup, not guest memory isolation.

**Checkpoint:** Hyper-V reported successful control, host-state, SYSENTER, and `HOST_RSP`/`HOST_RIP` write/readback. Both host PAT and EFER load controls were false, so those conditional writes were not exercised. Guest memory, guest fields, and VM-entry validation remain; no `VMLAUNCH` yet.

## 3. Launch and observe one guest exit

- [x] Assemble and link an inactive VM-exit entry stub that saves guest general-purpose registers, prepares the UEFI x64 call frame, and calls a non-returning handler. Do not mistake compilation or `HOST_RIP` readback for a tested exit.
- [x] Replace the spin-only handler with a deliberate first-exit diagnostic: read `VM_EXIT_REASON`, record it through a mechanism that can be observed, and stop safely. Do not assume UEFI printing is safe in the low-level exit handler; there is no `VMRESUME` path yet. `vmexit_handler` reads the exit reason (basic reason plus the VM-entry-failure bit), exit qualification, guest RIP, instruction length and the saved guest RAX-RDX, prints them with `serial_println!`, and halts with `cli; hlt`. Written and compiled only; it runs for the first time after `VMLAUNCH`.
- [ ] After guest memory/state readback and the observable exit path are ready, execute `VMLAUNCH` to enter the tiny guest. `VMLAUNCH` enters the guest for the first time; the guest then executes `VMCALL`, which causes the VM exit.
- [ ] On instruction failure, distinguish VMfailInvalid, VMfailValid (read `VM_INSTRUCTION_ERROR`), and VM-entry failure reported as a VM exit. Do not treat every return as a successful guest run.
- [ ] Handle only the expected `VMCALL` at first. Define a controlled stop/observation path; do not blindly `VMRESUME` at the same guest RIP.

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

- Run `cargo build --target x86_64-unknown-uefi` after each change. It checks compilation, not VM-entry validity.
- Run `cargo test-host` for the host unit tests of pure logic (RFLAGS decoding, control selection, descriptor encoding). They do not execute VMX instructions.
- Run `run-hyperv.ps1` from an elevated PowerShell session and inspect the VM console and the serial stream in the PowerShell window (also saved to `run\serial.log`). The runner turns off the named test VM and recreates its ESP VHDX; do not keep irreplaceable data there. VM-exit diagnostics appear only on serial.
- Record observed VMX instruction errors and VM-exit reasons as milestones are reached. QEMU without nested VMX can still test non-VMX diagnostics, but cannot validate `VMLAUNCH`.

Relevant code: `src/main.rs`, `src/serial.rs`, `src/vmx/segment.rs`, `src/vmx/vmxon.rs`, `src/vmx/vmcs.rs`, `src/vmx/vmexit.rs`, `run-hyperv.ps1`, and `read-serial.ps1`.