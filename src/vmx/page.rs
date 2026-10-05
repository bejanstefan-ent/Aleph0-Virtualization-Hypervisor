use core::ptr::NonNull;
use uefi::boot::{self, AllocateType, MemoryType};

pub const PAGE_SIZE: usize = 4096;

/// Allocate and zero one page, preserving the caller's allocation error type.
pub fn allocate_zeroed_page<E>(allocation_error: E) -> Result<NonNull<u8>, E> {
    allocate_zeroed_pages(1, allocation_error)
}

/// Allocate and zero `count` contiguous pages, preserving the caller's
/// allocation error type.
pub fn allocate_zeroed_pages<E>(count: usize, allocation_error: E) -> Result<NonNull<u8>, E> {
    let pages = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, count)
        .map_err(|_| allocation_error)?;

    assert_eq!(pages.as_ptr() as usize % PAGE_SIZE, 0, "UEFI returned an unaligned page");
    unsafe { pages.write_bytes(0, count * PAGE_SIZE) };
    Ok(pages)
}
