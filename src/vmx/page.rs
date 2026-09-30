use core::ptr::NonNull;
use uefi::boot::{self, AllocateType, MemoryType};

pub const PAGE_SIZE: usize = 4096;

/// Allocate and zero one page, preserving the caller's allocation error type.
pub fn allocate_zeroed_page<E>(allocation_error: E) -> Result<NonNull<u8>, E> {
    let page = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|_| allocation_error)?;

    assert_eq!(page.as_ptr() as usize % PAGE_SIZE, 0, "UEFI returned an unaligned page");
    unsafe { page.write_bytes(0, PAGE_SIZE) };
    Ok(page)
}