//! Guest memory for the first VM entry: one code page and one stack page.
//!
//! The guest's instructions are assembled into the `.efi` image between
//! `guest_code_start` and `guest_code_end`, then copied into a page of their
//! own, so the guest never executes from host code pages. The copy only
//! works because the code is position-independent: no absolute addresses,
//! only relative jumps.
//!
//! Guest RIP is [`GuestMemory::code_base`] and guest RSP
//! [`GuestMemory::stack_top`]; `GuestState::write` (in `guest_state`) writes
//! both to the VMCS. [`GuestMemory::check_mappings`] confirms that the
//! current page tables, which the guest will share, map the code page
//! executable and the stack page writable.

use core::ptr::NonNull;

use uefi::boot::MemoryType;

use super::cr::{self, read_cr4};
use super::page::{allocate_zeroed_pages_of_type, PAGE_SIZE};
use super::paging::{self, Mapping, WalkError};

/// Value the guest loads into EAX before its first VMCALL.
///
/// The VM-exit handler prints the guest's RAX, so seeing this value on
/// serial shows the guest's own instructions ran, not just that an exit
/// happened. `mov eax` zero-extends, so RAX reads back as this value. The
/// guest adds one before every later VMCALL, so exit N shows
/// `GUEST_MARKER + N - 1`.
pub const GUEST_MARKER: u32 = 0xA1E0;

core::arch::global_asm!(
    r#"
    .globl guest_code_start
    .globl guest_code_end
guest_code_start:
    mov eax, {marker}
guest_vmcall_loop:
    vmcall
    // Only reached when the host advanced RIP past the VMCALL and resumed
    // the guest. The increment shows on the next exit: RAX one higher
    // proves these instructions ran, rather than the VMCALL running again.
    inc eax
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
    /// The code page is a user page and CR4.SMEP is set, so the ring-0
    /// guest could not execute it.
    CodeBlockedBySmep,
    /// The stack page is read-only at some level.
    StackNotWritable,
    /// The stack page is a user page and CR4.SMAP is set, so the ring-0
    /// guest (RFLAGS.AC = 0) could not push to it.
    StackBlockedBySmap,
}

/// Checks that a ring-0 guest running with `cr4` can execute from `code`
/// and write to `stack`.
///
/// Assumes the guest gets this same CR4 and RFLAGS.AC = 0, as planned for
/// the guest state.
fn check_permissions(code: &Mapping, stack: &Mapping, cr4: u64) -> Result<(), GuestMemoryError> {
    if code.execute_disable {
        return Err(GuestMemoryError::CodeNotExecutable);
    }
    if code.user && cr4 & cr::CR4_SMEP != 0 {
        return Err(GuestMemoryError::CodeBlockedBySmep);
    }
    if !stack.writable {
        return Err(GuestMemoryError::StackNotWritable);
    }
    if stack.user && cr4 & cr::CR4_SMAP != 0 {
        return Err(GuestMemoryError::StackBlockedBySmap);
    }
    Ok(())
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
    /// a set XD bit is reserved and would fault anyway. The guest runs in
    /// ring 0, so a user page is also refused where CR4.SMEP (code) or
    /// CR4.SMAP (stack) would block it; see [`check_permissions`].
    ///
    /// # Safety
    ///
    /// Boot services must still be active; see [`paging::walk_current`].
    pub unsafe fn check_mappings(&self) -> Result<GuestMappings, GuestMemoryError> {
        let code = unsafe { paging::walk_current(self.code_base()) }
            .map_err(GuestMemoryError::CodeNotMapped)?;
        let stack = unsafe { paging::walk_current(self.stack.as_ptr() as u64) }
            .map_err(GuestMemoryError::StackNotMapped)?;

        check_permissions(&code, &stack, unsafe { read_cr4() })?;
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
                0xFF, 0xC0,                   // inc eax
                0xEB, 0xF9,                   // jmp rel8 -7: back to vmcall
            ],
        );
    }

    #[test]
    fn guest_code_fits_in_one_page() {
        assert!(guest_code().len() <= PAGE_SIZE);
    }

    /// A supervisor, writable, executable 4 KiB page.
    fn kernel_page() -> Mapping {
        Mapping {
            physical: 0x7eb1_9000,
            size: paging::PageSize::Size4K,
            writable: true,
            user: false,
            execute_disable: false,
        }
    }

    const SMEP_AND_SMAP: u64 = cr::CR4_SMEP | cr::CR4_SMAP;

    #[test]
    fn kernel_pages_pass_even_with_smep_and_smap() {
        assert_eq!(check_permissions(&kernel_page(), &kernel_page(), SMEP_AND_SMAP), Ok(()));
    }

    #[test]
    fn code_with_xd_is_refused() {
        let code = Mapping { execute_disable: true, ..kernel_page() };
        assert_eq!(check_permissions(&code, &kernel_page(), 0), Err(GuestMemoryError::CodeNotExecutable));
    }

    #[test]
    fn user_code_is_refused_only_under_smep() {
        let code = Mapping { user: true, ..kernel_page() };
        assert_eq!(check_permissions(&code, &kernel_page(), 0), Ok(()));
        assert_eq!(
            check_permissions(&code, &kernel_page(), cr::CR4_SMEP),
            Err(GuestMemoryError::CodeBlockedBySmep),
        );
    }

    #[test]
    fn read_only_stack_is_refused() {
        let stack = Mapping { writable: false, ..kernel_page() };
        assert_eq!(check_permissions(&kernel_page(), &stack, 0), Err(GuestMemoryError::StackNotWritable));
    }

    #[test]
    fn user_stack_is_refused_only_under_smap() {
        let stack = Mapping { user: true, ..kernel_page() };
        assert_eq!(check_permissions(&kernel_page(), &stack, 0), Ok(()));
        assert_eq!(
            check_permissions(&kernel_page(), &stack, cr::CR4_SMAP),
            Err(GuestMemoryError::StackBlockedBySmap),
        );
    }
}
