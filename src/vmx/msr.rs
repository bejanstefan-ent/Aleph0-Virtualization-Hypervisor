//! IA32_FEATURE_CONTROL MSR checks gating VMX enablement.
use core::arch::asm;

/// MSR address of IA32_FEATURE_CONTROL.
pub const IA32_FEATURE_CONTROL: u32 = 0x3A;

/// Bit 0: lock bit. Once set, the MSR is read-only until reset.
pub const LOCK_BIT: u64 = 1 << 0;

/// Bit 2: enable VMX outside SMX operation.
pub const VMX_OUTSIDE_SMX_BIT: u64 = 1 << 2;

pub unsafe fn read_feature_control() -> u64 {
    unsafe {
        rdmsr(IA32_FEATURE_CONTROL)
    }
}

/// Returns whether VMX is enabled for operation outside SMX.
///
/// The IA32_FEATURE_CONTROL MSR must be locked and the
/// VMX-outside-SMX bit (bit 2) must be set.
pub unsafe fn feature_control_vmx_enabled() -> bool {
    let value = unsafe { read_feature_control() };

    let locked = value & LOCK_BIT != 0;
    let vmx_enabled = value & VMX_OUTSIDE_SMX_BIT != 0;

    locked && vmx_enabled
}


/// IA32_VMX_BASIC. Bits 0..=30: VMCS revision identifier (bit 31 is always 0).
/// Bit 55: when set, the `IA32_VMX_TRUE_*_CTLS` MSRs exist and should be
/// preferred over the non-TRUE variants when configuring the VMCS later.
pub const IA32_VMX_BASIC: u32 = 0x480;

/// Bits that must be 1 in CR0 while in VMX operation.
pub const IA32_VMX_CR0_FIXED0: u32 = 0x486;
/// Bits that may be 1 in CR0 while in VMX operation (a 0 here means "must be 0").
pub const IA32_VMX_CR0_FIXED1: u32 = 0x487;
/// Bits that must be 1 in CR4 while in VMX operation.
pub const IA32_VMX_CR4_FIXED0: u32 = 0x488;
/// Bits that may be 1 in CR4 while in VMX operation (a 0 here means "must be 0").
pub const IA32_VMX_CR4_FIXED1: u32 = 0x489;

/// Reads an arbitrary MSR.
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);

    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }

    ((high as u64) << 32) | low as u64
}

/// Writes an MSR. `value` is split into EDX (high 32) : EAX (low 32).
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let (low, high): (u32, u32) = (value as u32, (value >> 32) as u32);
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") low,
            in("edx") high,
            options(nostack, preserves_flags),
        );
    }
}
