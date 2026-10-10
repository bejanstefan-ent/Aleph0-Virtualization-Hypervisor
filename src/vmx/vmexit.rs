//! VM-exit host resources: the exit stack, the assembly entry stub, and the
//! first-exit diagnostic handler.
//!
//! On every VM exit the CPU loads host state from the VMCS and jumps to
//! HOST_RIP ([`entry_address`]) with RSP = HOST_RSP ([`VmExitStack::top`]).
//! Nothing else is set up for us: interrupts are off (host RFLAGS is forced
//! to 0x2), and the guest's general-purpose registers are still live in the
//! CPU, so the stub saves them before any Rust code can clobber them.
//!
//! No guest is launched yet, so none of this has executed. When it does, the
//! handler prints the exit over serial and halts; there is no VMRESUME path.

use core::ptr::NonNull;

use crate::output::halt_forever;

use super::page::{allocate_zeroed_pages, PAGE_SIZE};
use super::vmcs::{self, VmcsError};
use super::vmcs::fields::{EXIT_QUALIFICATION, GUEST_RIP, VM_EXIT_INSTRUCTION_LEN, VM_EXIT_REASON};

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
        let base = allocate_zeroed_pages(VM_EXIT_STACK_PAGES, VmExitStackError::AllocationFailed)?;
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

/// VM-exit reason, split into the fields of the VM_EXIT_REASON VMCS field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitReason {
    /// Bits 15:0: which event caused the exit (Intel SDM Vol. 3D, Appendix C).
    pub basic: u16,
    /// Bit 31: VM entry itself failed. The guest never ran; `basic` says
    /// why (33 = invalid guest state, 34 = MSR loading, 41 = machine check).
    pub entry_failure: bool,
}

impl ExitReason {
    pub fn from_raw(raw: u32) -> Self {
        Self {
            basic: raw as u16,
            entry_failure: raw & (1 << 31) != 0,
        }
    }
}

/// Human-readable name for a basic exit reason. Only the reasons this
/// hypervisor is likely to meet early on are spelled out.
pub fn exit_reason_name(basic: u16) -> &'static str {
    match basic {
        0 => "exception or NMI",
        1 => "external interrupt",
        2 => "triple fault",
        3 => "INIT signal",
        7 => "interrupt window",
        9 => "task switch",
        10 => "CPUID",
        12 => "HLT",
        18 => "VMCALL",
        28 => "control-register access",
        30 => "I/O instruction",
        31 => "RDMSR",
        32 => "WRMSR",
        33 => "VM-entry failure: invalid guest state",
        34 => "VM-entry failure: MSR loading",
        41 => "VM-entry failure: machine-check event",
        48 => "EPT violation",
        49 => "EPT misconfiguration",
        _ => "unnamed exit reason",
    }
}

/// Reads one VMCS field for the diagnostic, printing instead of failing.
fn read_or_report(name: &str, field: u32) -> Option<u64> {
    match unsafe { vmcs::vmread(field) } {
        Ok(value) => Some(value),
        Err(error) => {
            report_unreadable(name, error);
            None
        }
    }
}

fn report_unreadable(name: &str, error: VmcsError) {
    crate::serial_println!("{} VM exit: {name} unreadable ({error:?})", crate::TAG);
}

/// First-exit diagnostic: report what happened over serial, then halt.
///
/// Runs on the VM-exit stack with interrupts off. It may only use things
/// that need no firmware: VMREAD (the VMCS is still current) and the serial
/// port. `uefi::println!` must not be called here; `log!` and the panic
/// handler skip the console once the phase below is set.
///
/// Once VMRESUME exists, this becomes a dispatcher that handles the exit,
/// advances guest RIP by VM_EXIT_INSTRUCTION_LEN, and resumes the guest.
#[unsafe(no_mangle)]
extern "efiapi" fn vmexit_handler(frame: *const RegisterFrame) -> ! {
    // First, so every line below (and any panic) skips the firmware console.
    // Never switched back: after VMLAUNCH the host runs only in exit handlers.
    crate::output::set_phase(crate::output::Phase::VmExit);

    let tag = crate::TAG;

    if let Some(raw) = read_or_report("VM_EXIT_REASON", VM_EXIT_REASON) {
        let reason = ExitReason::from_raw(raw as u32);
        crate::serial_println!(
            "{tag} VM exit: reason={} ({}){}",
            reason.basic,
            exit_reason_name(reason.basic),
            if reason.entry_failure { " [VM entry failed; guest did not run]" } else { "" },
        );
    }
    if let Some(qualification) = read_or_report("EXIT_QUALIFICATION", EXIT_QUALIFICATION) {
        crate::serial_println!("{tag} VM exit: qualification={qualification:#018x}");
    }
    if let (Some(rip), Some(length)) = (
        read_or_report("GUEST_RIP", GUEST_RIP),
        read_or_report("VM_EXIT_INSTRUCTION_LEN", VM_EXIT_INSTRUCTION_LEN),
    ) {
        crate::serial_println!("{tag} VM exit: guest rip={rip:#018x} instruction length={length}");
    }

    // The stub pushed these just below HOST_RSP; they are the guest's values.
    let frame = unsafe { &*frame };
    crate::serial_println!(
        "{tag} VM exit: guest rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx,
    );

    crate::serial_println!("{tag} VM exit: halting; there is no VMRESUME path yet.");
    halt_forever();
}

unsafe extern "C" {
    fn vmexit_entry() -> !;
}

pub fn entry_address() -> u64 {
    vmexit_entry as *const () as usize as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_reason_splits_basic_and_entry_failure() {
        assert_eq!(ExitReason::from_raw(18), ExitReason { basic: 18, entry_failure: false });
        assert_eq!(
            ExitReason::from_raw(0x8000_0021),
            ExitReason { basic: 33, entry_failure: true },
        );
    }

    #[test]
    fn exit_reason_ignores_other_high_bits() {
        // Bits 30:16 carry unrelated flags (e.g. bit 27, enclave mode).
        assert_eq!(ExitReason::from_raw(0x0800_000A).basic, 10);
    }
}
