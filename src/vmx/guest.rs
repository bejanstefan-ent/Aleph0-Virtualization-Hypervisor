//! Guest memory for the first VM entry: one code page and one stack page.
//!
//! The guest's instructions are assembled into the `.efi` image between
//! `guest_code_start` and `guest_code_end`, then copied into a page of their
//! own, so the guest never executes from host code pages. The copy only
//! works because the code is position-independent: no absolute addresses,
//! only relative jumps.
//!
//! Guest RIP will be [`GuestMemory::code_base`] and guest RSP
//! [`GuestMemory::stack_top`]. Neither is written to the VMCS yet.
//! [`GuestMemory::check_mappings`] confirms that the current page tables,
//! which the guest will share, map the code page executable and the stack
//! page writable.

use core::ptr::NonNull;

use uefi::boot::MemoryType;

use super::page::{allocate_zeroed_pages_of_type, PAGE_SIZE};
use super::paging::{self, Mapping, WalkError};

/// Value the guest loads into EAX before its first VMCALL.
///
/// The VM-exit handler prints the guest's RAX, so seeing this value on
/// serial shows the guest's own instructions ran, not just that an exit
/// happened. `mov eax` zero-extends, so RAX reads back as this value.
pub const GUEST_MARKER: u32 = 0xA1E0;

core::arch::global_asm!(
    r#"
    .globl guest_code_start
    .globl guest_code_end
guest_code_start:
    mov eax, {marker}
guest_vmcall_loop:
    vmcall
    // Only reached if the host resumes the guest; exits again instead of
    // running off the end of the page.
    jmp guest_vmcall_loop
guest_code_end:
"#,
    marker = const GUEST_MARKER,
);

unsafe extern "C" {
    static guest_code_start: u8;
    static guest_code_end: u8;
}

/// The guest's instruction bytes, as assembled into the image.
fn guest_code() -> &'static [u8] {
    let start = &raw const guest_code_start;
    let end = &raw const guest_code_end;
    // Both labels come from the one asm block above, start before end.
    unsafe { core::slice::from_raw_parts(start, end.offset_from(start) as usize) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestMemoryError {
    /// The assembled guest code does not fit in one page.
    CodeTooLarge,
    /// Failed to allocate the guest code page.
    CodeAllocationFailed,
    /// Failed to allocate the guest stack page.
    StackAllocationFailed,
    /// The page-table walk for the code page failed.
    CodeNotMapped(WalkError),
    /// The page-table walk for the stack page failed.
    StackNotMapped(WalkError),
    /// The code page is mapped with XD set at some level.
    CodeNotExecutable,
    /// The stack page is read-only at some level.
    StackNotWritable,
}

/// How the current page tables map the two guest pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestMappings {
    pub code: Mapping,
    pub stack: Mapping,
}

/// Owns the guest code and stack pages. Never freed: the guest may run from
/// them for as long as the CPU stays in VMX operation.
pub struct GuestMemory {
    code: NonNull<u8>,
    stack: NonNull<u8>,
}

impl GuestMemory {
    /// Allocate both pages and copy the guest code into the code page.
    ///
    /// The code page is `LOADER_CODE` because firmware may map
    /// `LOADER_DATA` no-execute; the stack page is `LOADER_DATA`.
    pub fn allocate() -> Result<Self, GuestMemoryError> {
        let code_bytes = guest_code();
        if code_bytes.len() > PAGE_SIZE {
            return Err(GuestMemoryError::CodeTooLarge);
        }

        let code = allocate_zeroed_pages_of_type(
            1,
            MemoryType::LOADER_CODE,
            GuestMemoryError::CodeAllocationFailed,
        )?;
        let stack = allocate_zeroed_pages_of_type(
            1,
            MemoryType::LOADER_DATA,
            GuestMemoryError::StackAllocationFailed,
        )?;

        unsafe { core::ptr::copy_nonoverlapping(code_bytes.as_ptr(), code.as_ptr(), code_bytes.len()) };
        Ok(Self { code, stack })
    }

    /// Address of the first guest instruction, for GUEST_RIP.
    pub fn code_base(&self) -> u64 {
        self.code.as_ptr() as u64
    }

    /// Number of guest code bytes copied into the code page.
    pub fn code_len(&self) -> usize {
        guest_code().len()
    }

    /// Top of the downward-growing guest stack, for GUEST_RSP.
    pub fn stack_top(&self) -> u64 {
        let top = unsafe { self.stack.as_ptr().add(PAGE_SIZE) } as usize;
        assert_eq!(top % 16, 0);
        top as u64
    }

    /// Walks the current page tables for both pages and checks that the
    /// code page is executable and the stack page writable.
    ///
    /// The guest will use these same tables (guest CR3 = host CR3, no EPT).
    /// Each page is 4 KiB-aligned, so one walk covers the whole page: a page
    /// can never straddle two mappings.
    ///
    /// Requiring XD clear is stricter than needed when EFER.NXE = 0, but then
    /// a set XD bit is reserved and would fault anyway.
    ///
    /// # Safety
    ///
    /// Boot services must still be active; see [`paging::walk_current`].
    pub unsafe fn check_mappings(&self) -> Result<GuestMappings, GuestMemoryError> {
        let code = unsafe { paging::walk_current(self.code_base()) }
            .map_err(GuestMemoryError::CodeNotMapped)?;
        let stack = unsafe { paging::walk_current(self.stack.as_ptr() as u64) }
            .map_err(GuestMemoryError::StackNotMapped)?;

        if code.execute_disable {
            return Err(GuestMemoryError::CodeNotExecutable);
        }
        if !stack.writable {
            return Err(GuestMemoryError::StackNotWritable);
        }
        Ok(GuestMappings { code, stack })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_code_is_marker_then_vmcall_loop() {
        assert_eq!(
            guest_code(),
            [
                0xB8, 0xE0, 0xA1, 0x00, 0x00, // mov eax, GUEST_MARKER
                0x0F, 0x01, 0xC1,             // vmcall
                0xEB, 0xFB,                   // jmp rel8 -5: back to vmcall
            ],
        );
    }

    #[test]
    fn guest_code_fits_in_one_page() {
        assert!(guest_code().len() <= PAGE_SIZE);
    }
}
