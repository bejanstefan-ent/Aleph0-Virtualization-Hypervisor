//! VM-exit host resources: the exit stack, the assembly entry stub, and the
//! exit handler.
//!
//! On every VM exit the CPU loads host state from the VMCS and jumps to
//! HOST_RIP ([`entry_address`]) with RSP = HOST_RSP ([`VmExitStack::top`]).
//! Nothing else is set up for us: interrupts are off (host RFLAGS is forced
//! to 0x2), and the guest's general-purpose registers are still live in the
//! CPU, so the stub saves them before any Rust code can clobber them.
//!
//! `bring_up::run` ends with VMLAUNCH, so the guest's first VM exit lands
//! here. The handler prints each exit over serial. On the guest's first
//! VMCALL it advances guest RIP and returns, and the stub executes VMRESUME.
//! On the second it stops, as on any unexpected exit, and halts.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::output::halt_forever;

use super::guest::GUEST_MARKER;
use super::page::{allocate_zeroed_pages, PAGE_SIZE};
use super::vmcs::{self, VmcsError};
use super::vmcs::fields::{
    EXIT_QUALIFICATION, GUEST_RIP, VM_EXIT_INSTRUCTION_LEN, VM_EXIT_INTERRUPTION_INFO,
    VM_EXIT_REASON,
};

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

    /// Top of the downward-growing stack, for HOST_RSP.
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

// The entry stub pushes r15 first and rax last, so rax ends up at the lowest
// address. Tie the struct layout to that order, so reordering a field or a
// push breaks the build instead of silently mixing up guest registers.
const _: () = {
    assert!(core::mem::size_of::<RegisterFrame>() == 15 * 8);
    assert!(core::mem::offset_of!(RegisterFrame, rax) == 0);
    assert!(core::mem::offset_of!(RegisterFrame, r15) == 14 * 8);
};

// Every exit enters here with RSP = HOST_RSP, the top of the exit stack, so
// the stack starts empty on each exit and does not grow across exits.
//
// Stack alignment (Microsoft x64 ABI: RSP is a multiple of 16 at each
// `call`): HOST_RSP is 16-aligned; 15 pushes take 120 bytes, and 40 more
// (32 bytes of shadow space plus 8 of padding) make 160, a multiple of 16.
// After the 15 pops RSP is HOST_RSP again, so the failure path needs only
// the 32 bytes of shadow space.
//
// Only the 15 general-purpose registers are saved, because nothing here
// touches vector state: the x86_64-unknown-uefi target is soft-float with
// MMX/SSE disabled (only `fxsr` is among its vector features), so the handler
// and `core::fmt` never use x87, XMM or MXCSR. If SSE target features are ever
// enabled, the handler would clobber the guest's vector registers, and this
// stub would have to save them (for example with FXSAVE/FXRSTOR) before an OS
// guest can run.
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

    // The handler returned: resume the guest. Restore its registers in the
    // reverse order of the pushes above; RIP, RSP and RFLAGS come from the
    // VMCS, not from this frame.
    add rsp, 40
    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rsi
    pop rdi
    pop rbp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15
    vmresume

    // Reached only if VMRESUME failed; RFLAGS say how.
    pushfq
    pop rcx
    sub rsp, 32
    call vmresume_failed
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

/// Basic exit reason of an exception (or NMI) exit.
const EXIT_REASON_EXCEPTION_OR_NMI: u16 = 0;
/// Basic exit reason of a VMCALL exit.
const EXIT_REASON_VMCALL: u16 = 18;

/// How many VMCALL exits the guest is resumed through before the handler
/// stops: the first exit is resumed, the second ends the run.
const PLANNED_VMCALL_EXITS: u64 = 2;

/// Exits handled so far, including the current one, so the first exit is
/// number 1. Only the exit handler touches it, with interrupts off, on the
/// one CPU that runs the guest; the atomic just avoids `static mut`.
static EXIT_COUNT: AtomicU64 = AtomicU64::new(0);

/// The guest's RAX at its `number`th VMCALL exit (1-based): the marker, plus
/// one for each `inc eax` the resumed guest has run since. `number` is
/// always at least 1.
fn expected_guest_rax(number: u64) -> u64 {
    u64::from(GUEST_MARKER) + number - 1
}

/// Whether exit `number` is the VMCALL exit the guest is built to produce.
///
/// The reason alone is not enough. The marker in the guest's RAX shows that
/// the guest's own instructions ran up to the VMCALL. Its increase on later
/// exits shows that the guest resumed past the previous VMCALL instead of
/// running it again.
fn is_expected_vmcall_exit(number: u64, reason: ExitReason, guest_rax: u64) -> bool {
    !reason.entry_failure
        && reason.basic == EXIT_REASON_VMCALL
        && guest_rax == expected_guest_rax(number)
}

/// What the handler does after printing an exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitAction {
    /// Advance guest RIP past the VMCALL and return to the guest.
    Resume,
    /// The last planned exit arrived as expected; stop.
    Finished,
    /// Not an exit this hypervisor handles; stop without resuming.
    Unexpected,
}

fn exit_action(number: u64, reason: ExitReason, guest_rax: u64) -> ExitAction {
    if !is_expected_vmcall_exit(number, reason, guest_rax) {
        ExitAction::Unexpected
    } else if number < PLANNED_VMCALL_EXITS {
        ExitAction::Resume
    } else {
        ExitAction::Finished
    }
}

/// The exception vector in a VM_EXIT_INTERRUPTION_INFO value, if the field
/// is valid (bit 31). Bits 7:0 hold the vector.
fn exception_vector(info: u32) -> Option<u8> {
    (info & (1 << 31) != 0).then_some(info as u8)
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

/// VM-exit handler: prints the exit over serial, then either advances guest
/// RIP and returns (the entry stub then executes VMRESUME), or halts.
///
/// Runs on the VM-exit stack with interrupts off. It may only use things
/// that need no firmware: VMREAD/VMWRITE (the VMCS is still current) and the
/// serial port. `uefi::println!` must not be called here; `log!` and the
/// panic handler skip the console once the phase below is set.
///
/// Returns only to resume the guest. Every other path halts, because there
/// is no way back to firmware yet (ROADMAP section 4b).
#[unsafe(no_mangle)]
extern "efiapi" fn vmexit_handler(frame: *const RegisterFrame) {
    // First, so every line below (and any panic) skips the firmware console.
    // Never switched back: after VMLAUNCH the host runs only in exit handlers.
    crate::output::set_phase(crate::output::Phase::VmExit);

    let tag = crate::TAG;
    let number = EXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let reason = read_or_report("VM_EXIT_REASON", VM_EXIT_REASON)
        .map(|raw| ExitReason::from_raw(raw as u32));
    if let Some(reason) = reason {
        crate::serial_println!(
            "{tag} VM exit #{number}: reason={} ({}){}",
            reason.basic,
            exit_reason_name(reason.basic),
            if reason.entry_failure { " [VM entry failed; guest did not run]" } else { "" },
        );
        if reason.basic == EXIT_REASON_EXCEPTION_OR_NMI && !reason.entry_failure {
            report_exception(number);
        }
    }
    if let Some(qualification) = read_or_report("EXIT_QUALIFICATION", EXIT_QUALIFICATION) {
        crate::serial_println!("{tag} VM exit #{number}: qualification={qualification:#018x}");
    }
    let rip = read_or_report("GUEST_RIP", GUEST_RIP);
    let length = read_or_report("VM_EXIT_INSTRUCTION_LEN", VM_EXIT_INSTRUCTION_LEN);
    if let (Some(rip), Some(length)) = (rip, length) {
        crate::serial_println!("{tag} VM exit #{number}: guest rip={rip:#018x} instruction length={length}");
    }

    // The stub pushed these just below HOST_RSP; after a normal exit they are
    // the guest's values. After a VM-entry failure (exit reason bit 31) the
    // guest never ran, so they are whatever was live when entry was attempted:
    // the host's registers for VMLAUNCH, or the restored guest registers
    // (the previous exit's) for VMRESUME. Either way, not this exit's guest.
    let frame = unsafe { &*frame };
    crate::serial_println!(
        "{tag} VM exit #{number}: guest rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx,
    );

    let action = match reason {
        Some(reason) => exit_action(number, reason, frame.rax),
        None => ExitAction::Unexpected,
    };
    match action {
        ExitAction::Resume => match (rip, length) {
            (Some(rip), Some(length)) => match unsafe { vmcs::vmwrite(GUEST_RIP, rip + length) } {
                Ok(()) => {
                    crate::serial_println!(
                        "{tag} VM exit #{number}: expected VMCALL (rax={:#x}); resuming the guest at rip={:#018x}.",
                        frame.rax, rip + length,
                    );
                    // Back to the stub, which restores the guest registers
                    // and executes VMRESUME.
                    return;
                }
                Err(error) => crate::serial_println!(
                    "{tag} VM exit #{number}: could not advance guest RIP ({error:?}); not resuming."
                ),
            },
            _ => crate::serial_println!(
                "{tag} VM exit #{number}: guest RIP or instruction length unreadable; not resuming."
            ),
        },
        ExitAction::Finished => crate::serial_println!(
            "{tag} VM exit #{number}: expected VMCALL (rax={:#x}); all {PLANNED_VMCALL_EXITS} planned exits handled.",
            frame.rax,
        ),
        ExitAction::Unexpected => crate::serial_println!(
            "{tag} VM exit #{number}: NOT an expected guest VMCALL exit (expected rax={:#x}).",
            expected_guest_rax(number),
        ),
    }

    crate::serial_println!("{tag} VM exit #{number}: halting; returning to firmware is ROADMAP section 4b.");
    halt_forever();
}

/// Prints the vector of an exception exit. Exit qualification, printed by
/// the caller, holds the faulting address for a page fault (#PF, vector 14).
fn report_exception(number: u64) {
    let tag = crate::TAG;
    if let Some(info) = read_or_report("VM_EXIT_INTERRUPTION_INFO", VM_EXIT_INTERRUPTION_INFO) {
        match exception_vector(info as u32) {
            Some(vector) => crate::serial_println!(
                "{tag} VM exit #{number}: guest exception vector={vector} (interruption info={info:#010x})"
            ),
            None => crate::serial_println!(
                "{tag} VM exit #{number}: interruption info not valid ({info:#010x})"
            ),
        }
    }
}

/// Called by the entry stub when VMRESUME falls through instead of entering
/// the guest; `rflags` is RFLAGS right after it. Prints why, then halts.
#[unsafe(no_mangle)]
extern "efiapi" fn vmresume_failed(rflags: u64) -> ! {
    let tag = crate::TAG;
    match vmcs::resume_failure(rflags) {
        // VmFailValid implies a current VMCS, so the error number is readable.
        VmcsError::VmFailValid => match unsafe { vmcs::vm_instruction_error() } {
            Ok(code) => crate::serial_println!(
                "{tag} VMRESUME failed: {} ({code})",
                vmcs::vm_instruction_error_name(code),
            ),
            Err(error) => crate::serial_println!(
                "{tag} VMRESUME failed: VmFailValid, error unreadable ({error:?})"
            ),
        },
        error => crate::serial_println!("{tag} VMRESUME failed: {error:?}"),
    }
    crate::serial_println!("{tag} VMRESUME failed; halting.");
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

    const VMCALL_EXIT: ExitReason = ExitReason { basic: 18, entry_failure: false };
    const MARKER: u64 = GUEST_MARKER as u64;

    #[test]
    fn first_vmcall_with_marker_is_expected() {
        assert!(is_expected_vmcall_exit(1, VMCALL_EXIT, MARKER));
    }

    #[test]
    fn second_vmcall_with_marker_plus_one_is_expected() {
        // The resumed guest ran `inc eax` before its second VMCALL.
        assert!(is_expected_vmcall_exit(2, VMCALL_EXIT, MARKER + 1));
    }

    #[test]
    fn second_vmcall_with_unchanged_marker_is_not_expected() {
        // RAX unchanged means the guest re-ran the same VMCALL: guest RIP
        // was not advanced past it.
        assert!(!is_expected_vmcall_exit(2, VMCALL_EXIT, MARKER));
    }

    #[test]
    fn vmcall_without_marker_is_not_expected() {
        // Right reason, but RAX does not prove the guest's own code ran.
        assert!(!is_expected_vmcall_exit(1, VMCALL_EXIT, 0));
    }

    #[test]
    fn entry_failure_is_never_expected() {
        let invalid_guest_state = ExitReason { basic: 33, entry_failure: true };
        assert!(!is_expected_vmcall_exit(1, invalid_guest_state, MARKER));
    }

    #[test]
    fn entry_failure_alone_rules_out_an_expected_exit() {
        // Right basic reason and marker; only the entry-failure bit differs.
        let failed_vmcall = ExitReason { basic: 18, entry_failure: true };
        assert!(!is_expected_vmcall_exit(1, failed_vmcall, MARKER));
    }

    #[test]
    fn other_reason_alone_rules_out_an_expected_exit() {
        // Marker present and no entry failure; only the basic reason (CPUID)
        // differs.
        let cpuid_exit = ExitReason { basic: 10, entry_failure: false };
        assert!(!is_expected_vmcall_exit(1, cpuid_exit, MARKER));
    }

    #[test]
    fn first_expected_exit_resumes_the_guest() {
        assert_eq!(exit_action(1, VMCALL_EXIT, MARKER), ExitAction::Resume);
    }

    #[test]
    fn last_planned_exit_finishes() {
        assert_eq!(
            exit_action(PLANNED_VMCALL_EXITS, VMCALL_EXIT, MARKER + PLANNED_VMCALL_EXITS - 1),
            ExitAction::Finished,
        );
    }

    #[test]
    fn unexpected_exit_is_never_resumed() {
        let cpuid_exit = ExitReason { basic: 10, entry_failure: false };
        assert_eq!(exit_action(1, cpuid_exit, MARKER), ExitAction::Unexpected);
    }

    #[test]
    fn exception_vector_is_bits_7_to_0_of_valid_info() {
        // Valid (bit 31), error code (bit 11), hardware exception (type 3),
        // vector 14: a page fault.
        assert_eq!(exception_vector(0x8000_0B0E), Some(14));
    }

    #[test]
    fn exception_vector_needs_the_valid_bit() {
        assert_eq!(exception_vector(0x0000_0B0E), None);
    }
}
