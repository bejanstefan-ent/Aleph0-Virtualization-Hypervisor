//! The Virtual Machine Control Structure (VMCS).
//!
//! A VMCS describes one virtual CPU: the guest's register state, the host
//! state to return to on a VM exit, and which events cause those exits.
//! One VMCS per vCPU; one "current" VMCS per logical processor.
//!
//! The region looks exactly like the VMXON region — 4 KiB, 4 KiB-aligned,
//! revision identifier in the first four bytes — but it is used differently.
//! Apart from that revision identifier, **never touch it with ordinary loads
//! and stores**: the internal layout is undocumented and the CPU caches
//! fields internally. [`vmread`] and [`vmwrite`] are the only legal access.
//!
//! Lifecycle:
//!
//! 1. [`VmcsRegion::allocate`] + [`VmcsRegion::write_revision_id`] — same as
//!    the VMXON region.
//! 2. [`VmcsRegion::vmclear`] — initialise it and flush any cached data the
//!    CPU holds for that physical address. Required once before first use,
//!    even on a freshly zeroed page.
//! 3. [`VmcsRegion::vmptrld`] — make it the *current* VMCS. From here on
//!    [`vmread`]/[`vmwrite`] act on it implicitly, with no address operand.
//! 4. [`vmwrite`] the fields, then (a later step) VMLAUNCH.
//!
//! Fields are addressed by a 32-bit **encoding**, not by an offset. The
//! encoding packs the field's width, type (control / read-only / guest /
//! host) and index — see the constants below.
//!
//! Like VMXON, these instructions report failure through RFLAGS rather than
//! exceptions: CF=1 is VMfailInvalid, ZF=1 is VMfailValid. The difference is
//! that once a current VMCS exists, VMfailValid also stores a numeric reason
//! readable via [`vm_instruction_error`].

use core::arch::asm;
use crate::vmx::msr::{self, VmcsControlMsrs};
use crate::vmx::{cr, segment};
use crate::vmx::instruction::{check_rflags, VmxFail};

use super::page::{allocate_zeroed_page, PAGE_SIZE};
use super::vmxon::vmcs_revision_id;

/// VMCS region size. Architecturally at most 4 KiB; one page is the norm.
pub const VMCS_REGION_SIZE: usize = 4096;
const _: () = assert!(VMCS_REGION_SIZE == PAGE_SIZE);

// Encoding layout: bit 0 = access type (0 = full, 1 = high half of a 64-bit
// field), bits 9:1 = index, bits 11:10 = type (0 control, 1 read-only data,
// 2 guest state, 3 host state), bits 14:13 = width (0 = 16-bit, 1 = 64-bit,
// 2 = 32-bit, 3 = natural).

/// Pin-based VM-execution controls (32-bit VMCS field).
pub const PIN_BASED_VM_EXEC_CONTROL: u32 = 0x4000;
/// Primary processor-based VM-execution controls (32-bit VMCS field).
pub const PRIMARY_VM_EXEC_CONTROL: u32 = 0x4002;
/// VM-exit controls (32-bit VMCS field).
pub const VM_EXIT_CONTROLS: u32 = 0x400C;
/// VM-exit control bits that load host PAT and EFER from the VMCS.
pub const VM_EXIT_LOAD_IA32_PAT: u64 = 1 << 19;
pub const VM_EXIT_LOAD_IA32_EFER: u64 = 1 << 21;
/// VM-entry controls (32-bit VMCS field).
pub const VM_ENTRY_CONTROLS: u32 = 0x4012;

const HOST_ADDRESS_SPACE_SIZE: u32 = 1 << 9;
const IA32E_GUEST_MODE: u32 = 1 << 9;

/// Requested control bits; CPU-required bits are added by `configure_controls`.
#[derive(Debug, Clone, Copy)]
pub struct DesiredControls {
    pub pin: u32,
    pub primary: u32,
    pub exit: u32,
    pub entry: u32,
}

impl DesiredControls {
    /// Minimal requests for a 64-bit guest on this 64-bit host.
    pub const fn minimal_64_bit_guest() -> Self {
        Self {
            pin: 0,
            primary: 0,
            exit: HOST_ADDRESS_SPACE_SIZE,
            entry: IA32E_GUEST_MODE,
        }
    }
}

/// Read-only: why the last VMX instruction failed (valid after VMfailValid).
pub const VM_INSTRUCTION_ERROR: u32 = 0x4400;
/// Read-only: why the last VM exit happened.
pub const VM_EXIT_REASON: u32 = 0x4402;

/// Guest RSP (natural width).
pub const GUEST_RSP: u32 = 0x681C;
/// Guest RIP (natural width).
pub const GUEST_RIP: u32 = 0x681E;
/// Guest RFLAGS (natural width).
pub const GUEST_RFLAGS: u32 = 0x6820;

/// Host segment selectors (16-bit VMCS fields).
pub const HOST_ES_SELECTOR: u32 = 0x0C00;
pub const HOST_CS_SELECTOR: u32 = 0x0C02;
pub const HOST_SS_SELECTOR: u32 = 0x0C04;
pub const HOST_DS_SELECTOR: u32 = 0x0C06;
pub const HOST_FS_SELECTOR: u32 = 0x0C08;
pub const HOST_GS_SELECTOR: u32 = 0x0C0A;
pub const HOST_TR_SELECTOR: u32 = 0x0C0C;

/// Host PAT and EFER (64-bit VMCS fields, loaded when selected on VM exit).
pub const HOST_IA32_PAT: u32 = 0x2C00;
pub const HOST_IA32_EFER: u32 = 0x2C02;

/// Host SYSENTER code selector (32-bit VMCS field).
pub const HOST_IA32_SYSENTER_CS: u32 = 0x4C00;

/// Host control registers (natural-width VMCS fields).
pub const HOST_CR0: u32 = 0x6C00;
pub const HOST_CR3: u32 = 0x6C02;
pub const HOST_CR4: u32 = 0x6C04;

/// Host segment and descriptor-table bases (natural-width VMCS fields).
pub const HOST_FS_BASE: u32 = 0x6C06;
pub const HOST_GS_BASE: u32 = 0x6C08;
pub const HOST_TR_BASE: u32 = 0x6C0A;
pub const HOST_GDTR_BASE: u32 = 0x6C0C;
pub const HOST_IDTR_BASE: u32 = 0x6C0E;

/// Host SYSENTER stack and entry addresses (natural-width VMCS fields).
pub const HOST_IA32_SYSENTER_ESP: u32 = 0x6C10;
pub const HOST_IA32_SYSENTER_EIP: u32 = 0x6C12;

/// Host RSP: the stack the CPU switches to on every VM exit.
pub const HOST_RSP: u32 = 0x6C14;
/// Host RIP: the exit handler the CPU jumps to on every VM exit.
pub const HOST_RIP: u32 = 0x6C16;

/// Must be set to `!0` unless shadow VMCS is in use. A classic omission:
/// leaving it zero makes VM entry fail with an invalid-guest-state exit.
pub const VMCS_LINK_POINTER: u32 = 0x2800;

/// Why a VMCS operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsError {
    /// UEFI could not provide a page.
    AllocationFailed,
    /// CF=1: the instruction failed with no current VMCS to record why.
    VmFailInvalid,
    /// ZF=1: the instruction failed and stored a reason in the current VMCS;
    /// read it with [`vm_instruction_error`].
    VmFailValid,
    /// [`self_test`] read back something other than what it wrote. Both
    /// instructions reported success, so this is not a VMX failure.
    SelfTestMismatch,
    /// The requested control value is not supported by the processor.
    UnsupportedControlValue,
    /// A control field read back differently from the value written to it.
    ControlReadbackMismatch,
    /// Host state validation failed.
    HostStateValidation,
}

impl From<VmxFail> for VmcsError {
    fn from(fail: VmxFail) -> Self {
        match fail {
            VmxFail::Invalid => Self::VmFailInvalid,
            VmxFail::Valid => Self::VmFailValid,
        }
    }
}

/// A 4 KiB, 4 KiB-aligned VMCS region.
pub struct VmcsRegion {
    /// Physical address of the region. Equal to the virtual address under UEFI.
    phys_addr: u64,
}

impl VmcsRegion {
    /// Allocates and zeroes one page through UEFI Boot Services.
    pub fn allocate() -> Result<Self, VmcsError> {
        let ptr = allocate_zeroed_page(VmcsError::AllocationFailed)?;

        Ok(
            Self {
                phys_addr: ptr.as_ptr() as u64
            }
        )
    }

    /// Address of the field holding the physical address, for the memory
    /// operand that VMCLEAR and VMPTRLD take.
    pub fn phys_addr_ref(&self) -> &u64 {
        &self.phys_addr
    }

    /// Writes the VMCS revision identifier into the first four bytes.
    ///
    /// # Safety
    ///
    /// The region must not be in use as a VMCS when it is modified.
    pub unsafe fn write_revision_id(&mut self, revision_id: u32) {
        unsafe {
            (self.phys_addr as *mut u32).write_volatile(revision_id);
        }
    }

    /// Initialises the VMCS and flushes CPU-cached data for its address.
    ///
    /// Must run once before the region is first used, and again whenever a
    /// VMCS stops being current on this processor.
    pub unsafe fn vmclear(&self) -> Result<(), VmcsError> {
        let rflags: u64;
        unsafe {
            asm!(
                "vmclear [{0}]",
                "pushfq",
                "pop {1}",
                in(reg) self.phys_addr_ref(),
                lateout(reg) rflags
            );
        }

        Ok(check_rflags(rflags)?)

    }

    /// Makes this VMCS the current one for this logical processor.
    pub unsafe fn vmptrld(&self) -> Result<(), VmcsError> {
        let rflags: u64;
        unsafe {
            asm!(
                "vmptrld [{0}]",
                "pushfq",
                "pop {1}",
                in(reg) self.phys_addr_ref(),
                lateout(reg) rflags
            );
        }

        Ok(check_rflags(rflags)?)
    }
}

/// Reads a field from the current VMCS.
///
/// Operand order is the opposite of [`vmwrite`]: `VMREAD dest, field`, so the
/// destination register comes first and the encoding second.
pub unsafe fn vmread(field_encoding: u32) -> Result<u64, VmcsError> {
    let value: u64;
    let rflags: u64;

    unsafe {
        asm!(
            "vmread {value}, {field:r}",
            "pushfq",
            "pop {rflags}",
            value = out(reg) value,
            // Cast to u64 like vmwrite does: `{field:r}` names the full
            // 64-bit register, and a u32 input leaves its upper half
            // undefined, which the CPU would read as part of the encoding.
            field = in(reg) field_encoding as u64,
            rflags = lateout(reg) rflags,
        );
    }

    check_rflags(rflags)?;

    Ok(value)
}

/// Writes a field in the current VMCS.
///
/// Operand order is `VMWRITE field, value` — the encoding comes first, which
/// reads backwards compared to a normal `mov`.
pub unsafe fn vmwrite(field_encoding: u32, value: u64) -> Result<(), VmcsError> {
    let rflags: u64;

    unsafe {
        asm!(
            "vmwrite {field:r}, {value}",
            "pushfq",
            "pop {rflags}",
            value = in(reg) value,
            field = in(reg) field_encoding as u64,
            rflags = lateout(reg) rflags,
        );
    }

    Ok(check_rflags(rflags)?)
}

/// Derive legal pin, primary, exit, and entry controls from the CPU masks,
/// write them to the current VMCS, and verify each field by reading it back.
/// Always request 64-bit host mode on VM exit, regardless of `desired.exit`.
///
/// # Safety
///
/// VMX operation must be active and a VMCS must be current on this CPU.
pub unsafe fn configure_controls(capabilities: &VmcsControlMsrs, desired: DesiredControls) -> Result<(), VmcsError> {
    let pin = choose_control(capabilities.pinbased, desired.pin)?;
    let primary = choose_control(capabilities.primary, desired.primary)?;
    let exit = choose_control(capabilities.exit, desired.exit | HOST_ADDRESS_SPACE_SIZE)?;
    let entry = choose_control(capabilities.entry, desired.entry)?;

    unsafe {
        vmwrite(PIN_BASED_VM_EXEC_CONTROL, pin.into())?;
        vmwrite(PRIMARY_VM_EXEC_CONTROL, primary.into())?;
        vmwrite(VM_EXIT_CONTROLS, exit.into())?;
        vmwrite(VM_ENTRY_CONTROLS, entry.into())?;

        // Verify by reading back
        if vmread(PIN_BASED_VM_EXEC_CONTROL)? != pin as u64 {
            return Err(VmcsError::ControlReadbackMismatch);
        }
        if vmread(PRIMARY_VM_EXEC_CONTROL)? != primary as u64 {
            return Err(VmcsError::ControlReadbackMismatch);
        }
        if vmread(VM_EXIT_CONTROLS)? != exit as u64 {
            return Err(VmcsError::ControlReadbackMismatch);
        }
        if vmread(VM_ENTRY_CONTROLS)? != entry as u64 {
            return Err(VmcsError::ControlReadbackMismatch);
        }
    }

    Ok(())
}

/// Capture the active host state and write it to the current VMCS.
/// Do not use this as evidence that VM entry is ready until all host fields
/// required by the selected VM-exit controls have been populated.
///
/// # Safety
///
/// VMX operation must be active, a VMCS must be current on this CPU, and the
/// host GDT and TSS must already be active and remain valid on VM exit.
pub unsafe fn configure_host_state() -> Result<(), VmcsError> {
    // a. Read segment::read_all() and cr::read_cr0/cr3/cr4 after GDT/TSS activation.
    // b. Check the host selectors' VM-entry constraints before writing them.
    // c. Pair each HOST_* selector, CR, and base encoding with its live value;
    //    use vmwrite and vmread to verify every pair, as configure_controls does.
    // d. Load host PAT/EFER only when selected VM-exit controls require them.
    // e. Configure HOST_RSP/HOST_RIP separately with configure_host_entry; do not launch.
    let segments = unsafe { segment::read_all() };
    let cr0 = unsafe { cr::read_cr0() };
    let cr3 = unsafe { cr::read_cr3() };
    let cr4 = unsafe { cr::read_cr4() };
    let sysenter_cs = u64::from(unsafe { msr::rdmsr(msr::IA32_SYSENTER_CS) } as u32);
    let sysenter_esp = unsafe { msr::rdmsr(msr::IA32_SYSENTER_ESP) };
    let sysenter_eip = unsafe { msr::rdmsr(msr::IA32_SYSENTER_EIP) };

    {

        let selectors = [
            segments.es, segments.cs, segments.ss, segments.ds,
            segments.fs, segments.gs, segments.tr,
        ];

        if selectors.iter().any(|selector| selector & 0b111 != 0) {
            return Err(VmcsError::HostStateValidation);
        }
    }

    if segments.cs == 0 || segments.tr == 0 {
        return Err(VmcsError::HostStateValidation);
    }
    let exit_controls = unsafe { vmread(VM_EXIT_CONTROLS)? };
    
    unsafe {
        write_host_state(HOST_ES_SELECTOR, segments.es as u64)?;
        write_host_state(HOST_CS_SELECTOR, segments.cs as u64)?;
        write_host_state(HOST_SS_SELECTOR, segments.ss as u64)?;
        write_host_state(HOST_DS_SELECTOR, segments.ds as u64)?;
        write_host_state(HOST_FS_SELECTOR, segments.fs as u64)?;
        write_host_state(HOST_GS_SELECTOR, segments.gs as u64)?;
        write_host_state(HOST_TR_SELECTOR, segments.tr as u64)?;

        write_host_state(HOST_CR0, cr0)?;
        write_host_state(HOST_CR3, cr3)?;
        write_host_state(HOST_CR4, cr4)?;

        write_host_state(HOST_FS_BASE, segments.fs_base)?;
        write_host_state(HOST_GS_BASE, segments.gs_base)?;
        write_host_state(HOST_TR_BASE, segments.tr_base)?;
        write_host_state(HOST_GDTR_BASE, segments.gdtr.base)?;
        write_host_state(HOST_IDTR_BASE, segments.idtr.base)?;

        write_host_state(HOST_IA32_SYSENTER_CS, sysenter_cs)?;
        write_host_state(HOST_IA32_SYSENTER_ESP, sysenter_esp)?;
        write_host_state(HOST_IA32_SYSENTER_EIP, sysenter_eip)?;

        if exit_controls & VM_EXIT_LOAD_IA32_PAT != 0 {
            write_host_state(HOST_IA32_PAT, msr::rdmsr(msr::IA32_PAT))?;
        }
        if exit_controls & VM_EXIT_LOAD_IA32_EFER != 0 {
            write_host_state(HOST_IA32_EFER, msr::rdmsr(msr::IA32_EFER))?;
        }
    }

    Ok(())
}

/// Store the host stack top and VM-exit entry address without entering a guest.
///
/// # Safety
///
/// VMX operation must be active and a VMCS current on this CPU. The stack
/// and entry code must remain mapped and valid whenever a VM exit can occur.
pub unsafe fn configure_host_entry(stack_top: u64, entry_address: u64) -> Result<(), VmcsError> {
    if stack_top == 0 || stack_top & 15 != 0 || entry_address == 0 {
        return Err(VmcsError::HostStateValidation);
    }

    unsafe {
        write_host_state(HOST_RSP, stack_top)?;
        write_host_state(HOST_RIP, entry_address)?;
    }
    Ok(())
}

/// Write a host state field and verify it by reading it back.
unsafe fn write_host_state(field_encoding: u32, value: u64) -> Result<(), VmcsError> {
    unsafe {
        vmwrite(field_encoding, value)?;
        let read_back = vmread(field_encoding)?;
        if read_back != value {
            return Err(VmcsError::HostStateValidation);
        }
        Ok(())
    }
}

/// Choose a 32-bit VMCS control value using one raw capability MSR.
/// The low half forces bits to 1; the high half permits bits to be 1.
/// Reject an inconsistent mask or a requested bit the CPU cannot enable.
fn choose_control(capability: u64, desired: u32) -> Result<u32, VmcsError> {
    let required = capability as u32;
    let allowed = (capability >> 32) as u32;
    if required & !allowed != 0 || desired & !allowed != 0 {
        return Err(VmcsError::UnsupportedControlValue);
    }

    // Add mandatory bits without changing any other requested bits.
    Ok(desired | required)
}

/// The numeric reason the last VMX instruction failed.
///
/// Only meaningful straight after a [`VmcsError::VmFailValid`], and only
/// while a current VMCS exists. The field keeps the last value the CPU wrote,
/// so reading it at any other time returns something stale with no way to
/// tell — that is the caller's responsibility, not something the hardware
/// flags.
///
/// Never call this to diagnose a failed [`vmread`]: this *is* a `vmread`, so
/// it would fail the same way and recurse.
pub unsafe fn vm_instruction_error() -> Result<u32, VmcsError> {
    // VM_INSTRUCTION_ERROR is a 32-bit field; VMREAD zero-extends it.
    Ok(unsafe { vmread(VM_INSTRUCTION_ERROR)? } as u32)
}

/// Human-readable name for a [`vm_instruction_error`] code.
///
/// Numbers from Intel SDM Vol. 3, "VM-Instruction Error Numbers"; only the
/// ones reachable from this code are spelled out.
pub fn vm_instruction_error_name(error: u32) -> &'static str {
    match error {
        2 => "VMCLEAR with invalid physical address",
        3 => "VMCLEAR with VMXON pointer",
        4 => "VMLAUNCH with non-clear VMCS",
        5 => "VMRESUME with non-launched VMCS",
        7 => "VM entry with invalid control field(s)",
        8 => "VM entry with invalid host-state field(s)",
        9 => "VMPTRLD with invalid physical address",
        10 => "VMPTRLD with VMXON pointer",
        11 => "VMPTRLD with incorrect VMCS revision identifier",
        12 => "VMREAD/VMWRITE from/to unsupported VMCS component",
        13 => "VMWRITE to read-only VMCS component",
        15 => "VMXON executed in VMX root operation",
        _ => "unknown VM-instruction error",
    }
}

/// Proves the whole access path works: allocate, make current, write a field,
/// read it back.
///
/// Requires the CPU to already be in VMX operation (VMXON done).
pub unsafe fn self_test() -> Result<VmcsRegion, VmcsError> {
    // Bits set in both halves, so a write that only lands in the low 32 bits
    // is caught. Canonical (bits 63:47 clear), which GUEST_RIP will need once
    // VM entry starts checking it.
    const PATTERN: u64 = 0x0000_7FFF_DEAD_BEEF;

    unsafe {
        let mut vmcs_region = VmcsRegion::allocate()?;
        // Before VMPTRLD, not just before use: VMPTRLD rejects a region whose
        // revision identifier does not match the CPU (error 11).
        vmcs_region.write_revision_id(vmcs_revision_id());
        vmcs_region.vmclear()?;
        vmcs_region.vmptrld()?;

        vmwrite(GUEST_RIP, PATTERN)?;
        if vmread(GUEST_RIP)? != PATTERN {
            return Err(VmcsError::SelfTestMismatch);
        }

        Ok(vmcs_region)
    }
}
