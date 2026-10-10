//! Unit tests for `src/vmx/guest.rs`.
//!
//! Compiled only by `cargo test-host`. That file includes this one with
//! `#[path]` as its child module `tests`, so `use super::*` reaches its
//! private items.

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
