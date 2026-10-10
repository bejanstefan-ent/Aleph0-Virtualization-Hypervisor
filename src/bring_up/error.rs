//! Why bring-up stopped, and how that gets printed.

use crate::vmx;
use crate::vmx::guest::GuestMemoryError;
use crate::vmx::guest_state::GuestStateError;
use crate::vmx::host_tables::{GdtError, TssError};
use crate::vmx::vmcs::VmcsError;
use crate::vmx::vmexit::VmExitStackError;
use crate::vmx::vmxon::VmxOnError;

/// Why bring-up stopped. Each step returns one of these, so the sequence
/// reads top to bottom with `?` and [`report_error`] prints the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BringUpError {
    Tss(TssError),
    Gdt(GdtError),
    VmExitStack(VmExitStackError),
    GuestMemory(GuestMemoryError),
    VmxNotSupportedByCpu,
    VmxDisabledByFirmware,
    VmxOn(VmxOnError),
    /// A VMCS step failed; `operation` names the step for the report.
    Vmcs { operation: &'static str, error: VmcsError },
    GuestState(GuestStateError),
}

impl From<TssError> for BringUpError {
    fn from(error: TssError) -> Self {
        Self::Tss(error)
    }
}

impl From<GdtError> for BringUpError {
    fn from(error: GdtError) -> Self {
        Self::Gdt(error)
    }
}

impl From<VmExitStackError> for BringUpError {
    fn from(error: VmExitStackError) -> Self {
        Self::VmExitStack(error)
    }
}

impl From<GuestMemoryError> for BringUpError {
    fn from(error: GuestMemoryError) -> Self {
        Self::GuestMemory(error)
    }
}

impl From<VmxOnError> for BringUpError {
    fn from(error: VmxOnError) -> Self {
        Self::VmxOn(error)
    }
}

impl From<GuestStateError> for BringUpError {
    fn from(error: GuestStateError) -> Self {
        Self::GuestState(error)
    }
}

/// Tags a VMCS failure with the step it came from, for use with `map_err`.
pub(super) fn vmcs_step(operation: &'static str) -> impl FnOnce(VmcsError) -> BringUpError {
    move |error| BringUpError::Vmcs { operation, error }
}

/// Prints why bring-up stopped.
pub fn report_error(error: BringUpError) {
    match error {
        BringUpError::Tss(error) => log!("Host TSS allocation failed: {error:?}"),
        // Only this variant comes after LGDT/LTR; every other GdtError is
        // returned before the CPU's tables are touched.
        BringUpError::Gdt(GdtError::ActivationVerificationFailed) => log!(
            "Host GDT/TSS activation failed verification; the CPU may already use the new tables."
        ),
        BringUpError::Gdt(error) => log!("Host GDT/TSS setup failed: {error:?}; GDTR/TR unchanged."),
        BringUpError::VmExitStack(error) => log!("VM-exit stack allocation failed: {error:?}"),
        BringUpError::GuestMemory(error) => log!("Guest memory setup failed: {error:?}"),
        BringUpError::VmxNotSupportedByCpu => log!("VMX not supported by this CPU."),
        BringUpError::VmxDisabledByFirmware => log!("VMX supported by CPU but disabled by firmware."),
        BringUpError::VmxOn(error) => log!("VMXON failed: {error:?}"),
        BringUpError::Vmcs { operation, error } => report_vmcs_error(operation, error),
        BringUpError::GuestState(GuestStateError::Vmcs { field, error }) => {
            log!("Guest-state field {field:#06x} failed:");
            report_vmcs_error("guest-state write", error);
        }
        BringUpError::GuestState(GuestStateError::ReadbackMismatch { field, written, read }) => log!(
            "Guest-state field {field:#06x} read back {read:#x}, but {written:#x} was written."
        ),
        BringUpError::GuestState(error) => log!("Guest state invalid: {error:x?}"),
    }
}

/// Prints a VMCS failure, decoding the VM-instruction error when one exists.
fn report_vmcs_error(operation: &str, error: VmcsError) {
    if error != VmcsError::VmFailValid {
        log!("VMCS {operation} failed: {error:?}");
        return;
    }

    // VmFailValid implies a current VMCS, so the error number is readable.
    match unsafe { vmx::vmcs::vm_instruction_error() } {
        Ok(code) => {
            let name = vmx::vmcs::vm_instruction_error_name(code);
            log!("VMCS {operation} failed: {name} ({code})");
        }
        Err(e) => {
            log!("VMCS {operation} failed: VmFailValid, error unreadable ({e:?})");
        }
    }
}
