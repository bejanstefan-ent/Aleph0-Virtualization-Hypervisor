//! VM-exit host resources. Entry addresses may be stored in the VMCS, but no guest is launched yet.

use core::ptr::NonNull;

use uefi::boot::MemoryType;

use super::page::PAGE_SIZE;

pub const VM_EXIT_STACK_PAGES: usize = 4;
pub const VM_EXIT_STACK_SIZE: usize = VM_EXIT_STACK_PAGES * PAGE_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmExitStackError {
    /// Failed to allocate memory for the VM-exit stack.
    AllocationFailed,
}

/// Owns a contiguous stack allocation that must outlive every use of HOST_RSP.
pub struct VmExitStack {
    base: NonNull<u8>,
}

impl VmExitStack {
    /// Allocate a zeroed, page-aligned stack for the host VM-exit path.
    pub fn allocate() -> Result<Self, VmExitStackError> {
        let base = uefi::boot::allocate_pages(
            uefi::boot::AllocateType::AnyPages, 
            MemoryType::LOADER_DATA, 
            VM_EXIT_STACK_PAGES)
        .map_err(|_| VmExitStackError::AllocationFailed)?;

        // Verify page alignment
        assert_eq!(base.as_ptr() as usize % PAGE_SIZE, 0);

        // Zero the stack
        unsafe {
            core::ptr::write_bytes(base.as_ptr(), 0, VM_EXIT_STACK_SIZE);
        }

        Ok(Self { base })
    }

    /// Top of the downward-growing stack for HOST_RSP; VM entry is not attempted yet.
    pub fn top(&self) -> u64 {
        // Compute base + VM_EXIT_STACK_SIZE, check 16-byte alignment, and
        // return the address as u64. The stack grows toward lower addresses.
        let top = unsafe { self.base.as_ptr().add(VM_EXIT_STACK_SIZE) } as usize;
        assert_eq!(top % 16, 0);
        top as u64
    }
}

#[repr(C)]
struct RegisterFrame {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8:  u64,
    r9:  u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

core::arch::global_asm!(
    r#"
    .globl vmexit_entry
vmexit_entry:
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    mov rcx, rsp
    sub rsp, 40
    cld
    call vmexit_handler
    ud2
"#
);

#[unsafe(no_mangle)]
extern "efiapi" fn vmexit_handler(_frame: *const RegisterFrame) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

unsafe extern "C" {
    fn vmexit_entry() -> !;
}

pub fn entry_address() -> u64 {
    vmexit_entry as *const () as usize as u64
}