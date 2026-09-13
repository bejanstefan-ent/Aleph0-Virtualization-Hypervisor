//! Control register access needed for VMX.
use core::arch::asm;

/// CR4 bit 13: VMX-enable. VMXON raises #UD while this is clear.
pub const CR4_VMXE: u64 = 1 << 13;

/// Reads CR0.
pub unsafe fn read_cr0() -> u64 {
    let value: u64;

    unsafe {
        asm!(
            "mov {}, cr0",
            out(reg) value,
            options(nomem, nostack, preserves_flags),
        );
    }
    
    value
}

/// Writes CR0.
pub unsafe fn write_cr0(value: u64) {
    unsafe {
        asm!(
            "mov cr0, {}",
            in(reg) value,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads CR4.
pub unsafe fn read_cr4() -> u64 {
    let value: u64;

    unsafe {
        asm!(
            "mov {}, cr4",
            out(reg) value,
            options(nomem, nostack, preserves_flags),
        );
    }
    
    value
}

/// Writes CR4.
pub unsafe fn write_cr4(value: u64) {
    unsafe {
        asm!(
            "mov cr4, {}",
            in(reg) value,
            options(nostack, preserves_flags),
        );
    }
}

/// Sets CR4.VMXE, leaving every other bit untouched.
pub unsafe fn enable_vmxe() {
    let mut cr4: u64;
    unsafe {
        cr4 = read_cr4();
        cr4 |= CR4_VMXE;
        write_cr4(cr4);
    }
}
