// Host unit tests (`cargo test --target <host triple>`) build with std and
// the test harness; the UEFI build is no_std with a firmware entry point.
#![cfg_attr(not(test), no_main)]
#![cfg_attr(not(test), no_std)]

use uefi::prelude::*;

mod serial;
mod output;
mod vmx;
use vmx::VmxCapabilities;
use vmx::guest::{GuestMappings, GuestMemory, GuestMemoryError};
use vmx::host_tables::{GdtError, GdtRegion, TssError, TssRegion};
use vmx::segment::DescriptorTable;
use vmx::vmcs::{DesiredControls, VmcsError, VmcsRegion};
use vmx::vmexit::{VmExitStack, VmExitStackError, VM_EXIT_STACK_SIZE};
use vmx::vmxon::{VmxOnError, VmxOnRegion};

/// Prefix for every line this hypervisor prints, on screen and on COM1.
pub const TAG: &str = "[Aleph0 Virtualization Hypervisor]";

/// Prints a tagged line to COM1, and to the firmware console while still in
/// boot services.
///
/// Safe in every context: outside the boot-services phase (see
/// `output::Phase`) the console half is skipped and only serial is written.
macro_rules! log {
    ($($arg:tt)*) => {
        crate::output::log_line(format_args!($($arg)*))
    };
}

/// Why bring-up stopped. Each step returns one of these, so the sequence
/// reads top to bottom with `?` and [`report_error`] prints the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BringUpError {
    Tss(TssError),
    Gdt(GdtError),
    VmExitStack(VmExitStackError),
    GuestMemory(GuestMemoryError),
    VmxNotSupportedByCpu,
    VmxDisabledByFirmware,
    VmxOn(VmxOnError),
    /// A VMCS step failed; `operation` names the step for the report.
    Vmcs { operation: &'static str, error: VmcsError },
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

/// Tags a VMCS failure with the step it came from, for use with `map_err`.
fn vmcs_step(operation: &'static str) -> impl FnOnce(VmcsError) -> BringUpError {
    move |error| BringUpError::Vmcs { operation, error }
}

/// The host GDT and TSS, plus the descriptor-table state recorded right
/// after they were activated.
struct HostTables {
    _tss: TssRegion,
    _gdt: GdtRegion,
    gdtr: DescriptorTable,
    tr: u16,
    tss_base: u64,
    idtr: DescriptorTable,
}

/// Everything the CPU may still use while it stays in VMX operation.
struct Hypervisor {
    _host_tables: HostTables,
    _vmexit_stack: VmExitStack,
    _guest: GuestMemory,
    _vmxon: VmxOnRegion,
    _vmcs: VmcsRegion,
}

#[cfg_attr(not(test), entry)]
fn main() -> Status {
    // First, so every later line also reaches COM1. Needs no firmware.
    let serial = serial::init();

    log!("Initializing UEFI helpers...");

    uefi::helpers::init().unwrap();

    log!("UEFI helpers initialized successfully.");
    match serial {
        Ok(()) => log!("Serial output enabled on COM1 ({:#x}).", serial::COM1),
        Err(error) => log!("Serial output disabled: {error:?}; console only."),
    }

    // Held until the loop below: the CPU keeps using these regions for as
    // long as it stays in VMX operation, so none may be freed before then.
    let _hypervisor = match bring_up() {
        Ok(hypervisor) => Some(hypervisor),
        Err(error) => {
            report_error(error);
            None
        }
    };

    loop { }
}

/// Activates the host tables, enters VMX operation, and prepares the VMCS.
///
/// Stops at the first failure. Regions allocated before that point are
/// leaked rather than freed (none implement `Drop`), so memory the CPU may
/// still reference — such as the VMXON region — is never returned to UEFI.
fn bring_up() -> Result<Hypervisor, BringUpError> {
    let host_tables = activate_host_tables()?;

    // Independent of VMX, so this runs and prints even under QEMU/TCG.
    dump_segment_state();

    let vmexit_stack = VmExitStack::allocate()?;
    log!("VM-exit stack allocated: top={:#018x} size={VM_EXIT_STACK_SIZE}.", vmexit_stack.top());

    let guest = GuestMemory::allocate()?;
    log!(
        "Guest memory allocated: code={:#018x} ({} bytes copied) stack top={:#018x}.",
        guest.code_base(), guest.code_len(), guest.stack_top(),
    );
    let mappings = unsafe { guest.check_mappings() }?;
    report_guest_mappings(&guest, &mappings);

    let (vmxon, vmcs) = bring_up_vmx(&vmexit_stack)?;

    check_host_tables_preserved(&host_tables);

    Ok(Hypervisor {
        _host_tables: host_tables,
        _vmexit_stack: vmexit_stack,
        _guest: guest,
        _vmxon: vmxon,
        _vmcs: vmcs,
    })
}

/// Builds a GDT that adds a TSS descriptor to a copy of the firmware's GDT,
/// loads it with LTR, and records the resulting state for later checks.
fn activate_host_tables() -> Result<HostTables, BringUpError> {
    let tss = TssRegion::allocate()?;
    log!("Host TSS allocated at {:#018x}; TR not loaded yet.", tss.base());

    let mut gdt = unsafe { GdtRegion::copy_active() }?;
    log!("Host GDT copied at {:#018x}.", gdt.base());

    let selector = gdt.append_tss_descriptor(&tss)?;
    if !gdt.verify_tss_descriptor(&tss) {
        return Err(GdtError::TssVerificationFailed.into());
    }
    let prepared = gdt
        .prepared_gdtr()
        .filter(|prepared| prepared.base == gdt.base() && prepared.limit == selector + 15)
        .ok_or(GdtError::GdtrPreparationFailed)?;
    log!(
        "Host GDT prepared: base={:#018x} limit={:#06x} tr_selector={selector:#06x}.",
        prepared.base, prepared.limit,
    );

    unsafe { gdt.activate(&tss) }?;
    log!("Host GDT and TSS activated and verified.");

    Ok(HostTables {
        gdtr: prepared,
        tr: selector,
        tss_base: tss.base(),
        idtr: unsafe { vmx::segment::read_idtr() },
        _tss: tss,
        _gdt: gdt,
    })
}

/// Detects VMX, enters root operation, then prepares the VMCS controls and
/// host state. VM entry is not attempted.
fn bring_up_vmx(vmexit_stack: &VmExitStack) -> Result<(VmxOnRegion, VmcsRegion), BringUpError> {
    match unsafe { vmx::detect() } {
        VmxCapabilities::Supported => {
            log!("VMX supported and enabled by firmware.");
        }
        VmxCapabilities::NotSupportedByCpu => return Err(BringUpError::VmxNotSupportedByCpu),
        VmxCapabilities::DisabledByFirmware => return Err(BringUpError::VmxDisabledByFirmware),
    }

    let controls = unsafe { vmx::msr::read_vmcs_control_msrs() };
    log!(
        "VMX BASIC={:#018x}; true controls={}",
        controls.basic, controls.basic & (1u64 << 55) != 0,
    );
    for (name, value) in [
        ("pin", controls.pinbased),
        ("primary", controls.primary),
        ("exit", controls.exit),
        ("entry", controls.entry),
    ] {
        log!(
            "VMX {name}: required={:#010x} allowed={:#010x}",
            value as u32, (value >> 32) as u32,
        );
    }

    let vmxon = unsafe { vmx::vmxon::enter_vmx_root_operation() }?;
    log!("Entered VMX root operation.");

    // Only legal now: the VMCS instructions raise #UD outside VMX operation.
    let vmcs = unsafe { VmcsRegion::create_current() }.map_err(vmcs_step("creation"))?;
    unsafe { vmx::vmcs::self_test() }.map_err(vmcs_step("self-test"))?;
    log!("VMCS self-test passed: VMREAD returned what VMWRITE stored.");

    unsafe { vmx::vmcs::configure_controls(&controls, DesiredControls::minimal_64_bit_guest()) }
        .map_err(vmcs_step("control setup"))?;
    log!("VMCS controls written and read back successfully.");

    unsafe { vmx::vmcs::configure_host_state() }.map_err(vmcs_step("host-state setup"))?;
    let exit_controls = unsafe { vmx::vmcs::vmread(vmx::vmcs::VM_EXIT_CONTROLS) }
        .map_err(vmcs_step("exit-control readback"))?;
    log!(
        "VMCS host fields written and read back; host PAT load={} EFER load={}; VM entry not attempted.",
        exit_controls & vmx::vmcs::VM_EXIT_LOAD_IA32_PAT != 0,
        exit_controls & vmx::vmcs::VM_EXIT_LOAD_IA32_EFER != 0,
    );

    unsafe { vmx::vmcs::configure_host_entry(vmexit_stack.top(), vmx::vmexit::entry_address()) }
        .map_err(vmcs_step("host entry address setup"))?;
    log!("VMCS HOST_RSP/HOST_RIP written and read back; VM entry not attempted.");

    Ok((vmxon, vmcs))
}

/// Prints how the current page tables, which the guest will share, map the
/// guest pages. Only called once both permission checks have passed.
fn report_guest_mappings(guest: &GuestMemory, mappings: &GuestMappings) {
    let stack_base = guest.stack_top() - vmx::page::PAGE_SIZE as u64;
    for (name, virtual_address, mapping) in [
        ("code", guest.code_base(), mappings.code),
        ("stack", stack_base, mappings.stack),
    ] {
        log!(
            "Guest {name} page: virt={virtual_address:#018x} phys={:#018x} identity={} size={:?} writable={} xd={}",
            mapping.physical, mapping.physical == virtual_address, mapping.size,
            mapping.writable, mapping.execute_disable,
        );
    }

    // The guest shares these tables, so a guest EFER.NXE that differs from
    // the host's would turn any XD bit in them into a reserved-bit fault.
    let nxe = unsafe { vmx::msr::rdmsr(vmx::msr::IA32_EFER) } & vmx::paging::EFER_NXE != 0;
    log!("Guest code page executable and stack page writable; host EFER.NXE={nxe}.");
}

/// Confirms that VMXON, the VMCS work, and UEFI calls left the host GDTR,
/// TR, TSS base, and IDTR as they were after activation.
fn check_host_tables_preserved(expected: &HostTables) {
    let gdtr = unsafe { vmx::segment::read_gdtr() };
    let tr = unsafe { vmx::segment::read_tr() };
    let idtr = unsafe { vmx::segment::read_idtr() };
    let same_gdtr = gdtr.base == expected.gdtr.base && gdtr.limit == expected.gdtr.limit;
    let same_idtr = idtr.base == expected.idtr.base && idtr.limit == expected.idtr.limit;
    let same_tss_base = same_gdtr && tr == expected.tr
        && unsafe { vmx::segment::segment_base_from_gdt(&gdtr, tr) } == expected.tss_base;

    if same_gdtr && same_idtr && same_tss_base {
        log!("Host GDTR, TR, TSS base and IDTR preserved after VMX/UEFI calls.");
    } else {
        log!("WARNING: Host descriptor state changed after VMX/UEFI calls.");
        log!(
            "Expected GDTR={:?} TR={:#06x} TSS={:#018x} IDTR={:?}",
            expected.gdtr, expected.tr, expected.tss_base, expected.idtr,
        );
        log!("Actual GDTR={gdtr:?} TR={tr:#06x} IDTR={idtr:?} TSS base matches={same_tss_base}");
    }
}

/// Prints why bring-up stopped.
fn report_error(error: BringUpError) {
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

/// Prints the segmentation state that will go into the VMCS host fields.
///
/// TR is the one to watch: VM entry rejects a host TR selector of 0.
fn dump_segment_state() {
    let state = unsafe { vmx::segment::read_all() };

    log!(
        "segments: cs={:#06x} ss={:#06x} ds={:#06x} es={:#06x} fs={:#06x} gs={:#06x} tr={:#06x}",
        state.cs, state.ss, state.ds, state.es, state.fs, state.gs, state.tr,
    );
    log!(
        "bases:    fs={:#018x} gs={:#018x} tr={:#018x}",
        state.fs_base, state.gs_base, state.tr_base,
    );
    log!(
        "gdtr:     base={:#018x} limit={:#06x}",
        state.gdtr.base, state.gdtr.limit,
    );
    log!(
        "idtr:     base={:#018x} limit={:#06x}",
        state.idtr.base, state.idtr.limit,
    );

    if state.tr == 0 {
        log!("WARNING: TR is 0; VM entry would reject this host state.");
    }
}
