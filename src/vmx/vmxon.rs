//! Entering VMX root operation (VMXON).
//!
//! Order matters; every step is a stub below:
//!
//! 1. [`cr::enable_vmxe`] — set CR4.VMXE. VMXON is #UD without it.
//! 2. [`apply_fixed_bits`] — make CR0/CR4 satisfy `IA32_VMX_CR{0,4}_FIXED{0,1}`.
//!    For each register: `value = (value | fixed0) & fixed1`. VMXON fails
//!    with VmFailInvalid if this is skipped.
//! 3. [`VmxonRegion::allocate`] — one 4 KiB page, 4 KiB-aligned, zeroed.
//!    `boot::allocate_pages` already guarantees page alignment, and UEFI
//!    identity-maps memory, so the pointer is also the physical address.
//! 4. [`vmcs_revision_id`] — bits 0..=30 of `IA32_VMX_BASIC`, written to
//!    the first 4 bytes of the region via [`VmxonRegion::write_revision_id`].
//! 5. [`vmxon`] — execute VMXON. The instruction takes a *memory* operand
//!    holding the 64-bit physical address, so pass a pointer to a local
//!    `u64`, not the address itself. Then read RFLAGS:
//!    CF=1 → VmFailInvalid (bad region, alignment or revision id),
//!    ZF=1 → VmFailValid (already in VMX operation, error number 15).
//!
//! [`enter_vmx_root_operation`] runs the sequence end to end.
//!
//! Memory type: `LOADER_DATA` is fine while Boot Services are alive. If the
//! hypervisor is meant to survive `ExitBootServices`, this must become
//! `RUNTIME_SERVICES_DATA` (or a reserved type) so the OS never reclaims it.

use uefi::boot::MemoryType;
use core::arch::asm;
use super::cr;
use super::msr;

/// VMXON region size. Architecturally at most 4 KiB; one page is the norm.
pub const VMXON_REGION_SIZE: usize = 4096;

/// A 4 KiB, 4 KiB-aligned region handed to VMXON.
pub struct VmxOnRegion {
    /// Physical address of the region. Equal to the virtual address under UEFI.
    phys_addr: u64,
}

/// Why entering VMX operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmxOnError {
    /// UEFI could not provide a page.
    AllocationFailed,
    /// VMXON reported VmFailInvalid (CF=1).
    VmFailInvalid,
    /// VMXON reported VmFailValid (ZF=1): typically already in VMX operation.
    VmFailValid,
}

impl VmxOnRegion {
    /// Allocates and zeroes one page through UEFI Boot Services.
    pub unsafe fn allocate() -> Result<Self, VmxOnError> {
        let ptr = match uefi::boot::allocate_pages(
            uefi::boot::AllocateType::AnyPages, 
            MemoryType::LOADER_DATA, 
            1
        ) {
            Ok(ptr) => ptr,
            Err(_) => return Err(VmxOnError::AllocationFailed)
        };

        unsafe {
            ptr.write_bytes(0, VMXON_REGION_SIZE);
        };

        Ok(
            Self {
                phys_addr: ptr.as_ptr() as u64
            }
        )
    } 

    /// Physical address to hand to VMXON.
    pub fn phys_addr_ref(&self) -> &u64 {
        &self.phys_addr
    }

    /// Writes the VMCS revision identifier into the first 4 bytes.
    pub unsafe fn write_revision_id(&mut self, revision_id: u32) {
        unsafe {
            (self.phys_addr as *mut u32).write_volatile(revision_id);
        }
    }
}

/// VMCS revision identifier from `IA32_VMX_BASIC` bits 0..=30.
pub unsafe fn vmcs_revision_id() -> u32 {
    unsafe {
        (msr::rdmsr(msr::IA32_VMX_BASIC) & 0x7FFF_FFFF) as u32
    }
}

/// Forces CR0 and CR4 into the ranges the CPU requires for VMX operation.
pub unsafe fn apply_fixed_bits() {
    unsafe {
      let cr0 = (cr::read_cr0() | msr::rdmsr(msr::IA32_VMX_CR0_FIXED0)) & msr::rdmsr(msr::IA32_VMX_CR0_FIXED1);
      let cr4 = (cr::read_cr4() | msr::rdmsr(msr::IA32_VMX_CR4_FIXED0)) & msr::rdmsr(msr::IA32_VMX_CR4_FIXED1);

      cr::write_cr0(cr0);
      cr::write_cr4(cr4);  
    }
}

/// Executes VMXON on `region`.
pub unsafe fn vmxon(region: &VmxOnRegion) -> Result<(), VmxOnError> {
    let rflags: u64;
    unsafe {
        asm!(
            "vmxon [{0}]",
            "pushfq",
            "pop {1}",
            in(reg) region.phys_addr_ref(),
            out(reg) rflags
        );
    }

    let zero_flag = (rflags & (1 << 6)) != 0;
    let carry_flag = (rflags & (1 << 0)) != 0;

    if carry_flag {
        return Err(VmxOnError::VmFailInvalid);
    }

    if zero_flag {
        return Err(VmxOnError::VmFailValid);
    }

    Ok(())
}

/// The region must stay allocated for as long as the CPU is in VMX
/// operation; dropping it while VMX is on is undefined behaviour.
pub unsafe fn enter_vmx_root_operation() -> Result<VmxOnRegion, VmxOnError> {
    unsafe {
        cr::enable_vmxe();
        apply_fixed_bits();
        let mut vmx_on_region = VmxOnRegion::allocate()?;
        vmx_on_region.write_revision_id(vmcs_revision_id());
        vmxon(&vmx_on_region)?;
        Ok(vmx_on_region)
    }
}
