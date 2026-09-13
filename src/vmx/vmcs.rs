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

use uefi::boot::MemoryType;
use core::arch::asm;
use super::vmxon::vmcs_revision_id;

/// VMCS region size. Architecturally at most 4 KiB; one page is the norm.
pub const VMCS_REGION_SIZE: usize = 4096;

// Encoding layout: bit 0 = access type (0 = full, 1 = high half of a 64-bit
// field), bits 9:1 = index, bits 11:10 = type (0 control, 1 read-only data,
// 2 guest state, 3 host state), bits 14:13 = width (0 = 16-bit, 1 = 64-bit,
// 2 = 32-bit, 3 = natural).

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
}

/// Decodes the RFLAGS value a VMX instruction leaves behind.
///
/// Every VMX instruction reports its outcome the same way: all flags clear on
/// success, CF set for VMfailInvalid, ZF set for VMfailValid. Call this right
/// after the `asm!` block instead of repeating the bit tests.
pub fn check_rflags(rflags: u64) -> Result<(), VmcsError> {
    /// RFLAGS.CF, bit 0.
    const CF: u64 = 1 << 0;
    /// RFLAGS.ZF, bit 6.
    const ZF: u64 = 1 << 6;

    if rflags & CF != 0 {
        Err(VmcsError::VmFailInvalid)
    } else if rflags & ZF != 0 {
        Err(VmcsError::VmFailValid)
    } else {
        Ok(())
    }
}

/// A 4 KiB, 4 KiB-aligned VMCS region.
pub struct VmcsRegion {
    /// Physical address of the region. Equal to the virtual address under UEFI.
    phys_addr: u64,
}

impl VmcsRegion {
    /// Allocates and zeroes one page through UEFI Boot Services.
    pub unsafe fn allocate() -> Result<Self, VmcsError> {
        let ptr = uefi::boot::allocate_pages(
            uefi::boot::AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            1
        ).map_err(|_| VmcsError::AllocationFailed)?;

        unsafe {
            ptr.write_bytes(0, VMCS_REGION_SIZE);
        }

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

        check_rflags(rflags)

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

        check_rflags(rflags)
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

    check_rflags(rflags)
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
