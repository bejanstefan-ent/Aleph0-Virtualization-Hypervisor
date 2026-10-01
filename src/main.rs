#![no_main]
#![no_std]

use uefi::prelude::*;
use vmx::host_tables::TssRegion;

mod vmx;
use vmx::VmxCapabilities;
use vmx::vmcs::{DesiredControls, VmcsError, VmcsRegion};
use vmx::vmexit::{VmExitStack, VM_EXIT_STACK_SIZE};
use vmx::vmxon::VmxOnRegion;

use crate::vmx::host_tables::GdtRegion;

const TAG: &str = "[Aleph0 Virtualization Hypervisor]";

#[entry]
fn main() -> Status {
    uefi::println!("{TAG} Initializing UEFI helpers...");

    uefi::helpers::init().unwrap();

    uefi::println!("{TAG} UEFI helpers initialized successfully.");

    let _tss_region = match TssRegion::allocate() {
        Ok(region) => {
            uefi::println!("{TAG} Host TSS allocated at {:#018x}; TR not loaded yet.", region.base());
            Some(region)
        }
        Err(error) => {
            uefi::println!("{TAG} Host TSS allocation failed: {error:?}");
            None
        }
    };

    let mut _gdt_region = match unsafe { GdtRegion::copy_active() } {
        Ok(region) => {
            uefi::println!("{TAG} Host GDT copied at {:#018x}.", region.base());
            Some(region)
        }
        Err(error) => {
            uefi::println!("{TAG} Host GDT copy failed: {error:?}");
            None
        }
    };

    let mut activated_tables = None;
    if let (Some(tss), Some(gdt)) = (_tss_region.as_ref(), _gdt_region.as_mut()) {
        match gdt.append_tss_descriptor(tss) {
            Ok(selector) => {
                if gdt.verify_tss_descriptor(tss) {
                    match gdt.prepared_gdtr() {
                        Some(prepared) if prepared.base == gdt.base() && prepared.limit == selector + 15 => {
                            uefi::println!(
                                "{TAG} Host GDT prepared: base={:#018x} limit={:#06x} tr_selector={selector:#06x}.",
                                prepared.base, prepared.limit,
                            );
                            match unsafe { gdt.activate(tss) } {
                                Ok(()) => {
                                    activated_tables = Some((prepared, selector, tss.base(), unsafe { vmx::segment::read_idtr() }));
                                    uefi::println!("{TAG} Host GDT and TSS activated and verified.");
                                }
                                Err(error) => uefi::println!(
                                    "{TAG} Host GDT/TSS activation failed: {error:?}; GDTR/TR may have changed."
                                ),
                            }
                        }
                        _ => uefi::println!("{TAG} Prepared GDTR verification failed; GDTR/TR unchanged."),
                    }
                } else {
                    uefi::println!("{TAG} TSS descriptor verification failed; GDTR/TR unchanged.");
                }
            }
            Err(error) => {
                uefi::println!("{TAG} TSS descriptor append failed: {error:?}");
            }
        }
    }

    // Independent of VMX, so this runs and prints even under QEMU/TCG.
    dump_segment_state();

    let _vmexit_stack = match VmExitStack::allocate() {
        Ok(stack) => {
            uefi::println!("{TAG} VM-exit stack allocated: top={:#018x} size={VM_EXIT_STACK_SIZE}; HOST_RSP not set.", stack.top());
            Some(stack)
        }
        Err(error) => {
            uefi::println!("{TAG} VM-exit stack allocation failed: {error:?}");
            None
        }
    };

    // Held until the loop below: the CPU keeps using both regions for as long
    // as it stays in VMX operation, so neither may be dropped before then.
    let _vmx_state = bring_up_vmx(activated_tables.is_some(), _vmexit_stack.as_ref());

    if let Some((expected_gdtr, expected_tr, expected_tss_base, expected_idtr)) = activated_tables {
        let gdtr = unsafe { vmx::segment::read_gdtr() };
        let tr = unsafe { vmx::segment::read_tr() };
        let idtr = unsafe { vmx::segment::read_idtr() };
        let same_gdtr = gdtr.base == expected_gdtr.base && gdtr.limit == expected_gdtr.limit;
        let same_idtr = idtr.base == expected_idtr.base && idtr.limit == expected_idtr.limit;
        let same_tss_base = same_gdtr && tr == expected_tr
            && unsafe { vmx::segment::segment_base_from_gdt(&gdtr, tr) } == expected_tss_base;

        if same_gdtr && same_idtr && same_tss_base {
            uefi::println!("{TAG} Host GDTR, TR, TSS base and IDTR preserved after VMX/UEFI calls.");
        } else {
            uefi::println!("{TAG} WARNING: Host descriptor state changed after VMX/UEFI calls.");
            uefi::println!("{TAG} Expected GDTR={expected_gdtr:?} TR={expected_tr:#06x} TSS={expected_tss_base:#018x} IDTR={expected_idtr:?}");
            uefi::println!("{TAG} Actual GDTR={gdtr:?} TR={tr:#06x} IDTR={idtr:?} TSS base matches={same_tss_base}");
        }
    }

    loop { }
}

/// Detects VMX, enters root operation, then exercises the VMCS access path.
///
/// Returns the regions so the caller can keep them alive. The VMCS is
/// optional: VMXON can succeed while the VMCS self-test fails.
fn bring_up_vmx(host_tables_active: bool, vmexit_stack: Option<&VmExitStack>) -> Option<(VmxOnRegion, Option<VmcsRegion>)> {
    match unsafe { vmx::detect() } {
        VmxCapabilities::Supported => {
            uefi::println!("{TAG} VMX supported and enabled by firmware.");
        }
        VmxCapabilities::NotSupportedByCpu => {
            uefi::println!("{TAG} VMX not supported by this CPU.");
            return None;
        }
        VmxCapabilities::DisabledByFirmware => {
            uefi::println!("{TAG} VMX supported by CPU but disabled by firmware.");
            return None;
        }
    }

    let controls = unsafe { vmx::msr::read_vmcs_control_msrs() };
    uefi::println!(
        "{TAG} VMX BASIC={:#018x}; true controls={}",
        controls.basic, controls.basic & (1u64 << 55) != 0,
    );
    for (name, value) in [
        ("pin", controls.pinbased),
        ("primary", controls.primary),
        ("exit", controls.exit),
        ("entry", controls.entry),
    ] {
        uefi::println!(
            "{TAG} VMX {name}: required={:#010x} allowed={:#010x}",
            value as u32, (value >> 32) as u32,
        );
    }

    let vmxon_region = match unsafe { vmx::vmxon::enter_vmx_root_operation() } {
        Ok(region) => {
            uefi::println!("{TAG} Entered VMX root operation.");
            region
        }
        Err(e) => {
            uefi::println!("{TAG} VMXON failed: {e:?}");
            return None;
        }
    };

    // Only legal now: the VMCS instructions raise #UD outside VMX operation.
    let vmcs_region = match unsafe { vmx::vmcs::self_test() } {
        Ok(region) => {
            uefi::println!("{TAG} VMCS self-test passed: VMREAD returned what VMWRITE stored.");
            match unsafe { vmx::vmcs::configure_controls(&controls, DesiredControls::minimal_64_bit_guest()) } {
                Ok(()) => {
                    uefi::println!("{TAG} VMCS controls written and read back successfully.");
                    if host_tables_active {
                        match unsafe { vmx::vmcs::configure_host_state() } {
                            Ok(()) => match unsafe { vmx::vmcs::vmread(vmx::vmcs::VM_EXIT_CONTROLS) } {
                                Ok(exit_controls) => {
                                    uefi::println!(
                                        "{TAG} VMCS host fields written and read back; host PAT load={} EFER load={}; VM entry not attempted.",
                                        exit_controls & vmx::vmcs::VM_EXIT_LOAD_IA32_PAT != 0,
                                        exit_controls & vmx::vmcs::VM_EXIT_LOAD_IA32_EFER != 0,
                                    );
                                    if let Some(stack) = vmexit_stack {
                                        match unsafe { vmx::vmcs::configure_host_entry(stack.top(), vmx::vmexit::entry_address()) } {
                                            Ok(()) => uefi::println!("{TAG} VMCS HOST_RSP/HOST_RIP written and read back; VM entry not attempted."),
                                            Err(error) => report_vmcs_error("host entry address setup", error),
                                        }
                                    } else {
                                        uefi::println!("{TAG} VMCS HOST_RSP/HOST_RIP setup skipped: VM-exit stack not allocated.");
                                    }
                                }
                                Err(error) => report_vmcs_error("exit-control readback", error),
                            },
                            Err(error) => report_vmcs_error("host-state setup", error),
                        }
                    } else {
                        uefi::println!("{TAG} VMCS host-state setup skipped: host GDT/TSS not activated.");
                    }
                }
                Err(error) => report_vmcs_error("control setup", error),
            }
            Some(region)
        }
        Err(e) => {
            report_vmcs_error("self-test", e);
            None
        }
    };

    Some((vmxon_region, vmcs_region))
}

/// Prints a VMCS failure, decoding the VM-instruction error when one exists.
fn report_vmcs_error(operation: &str, error: VmcsError) {
    if error != VmcsError::VmFailValid {
        uefi::println!("{TAG} VMCS {operation} failed: {error:?}");
        return;
    }

    // VmFailValid implies a current VMCS, so the error number is readable.
    match unsafe { vmx::vmcs::vm_instruction_error() } {
        Ok(code) => {
            let name = vmx::vmcs::vm_instruction_error_name(code);
            uefi::println!("{TAG} VMCS {operation} failed: {name} ({code})");
        }
        Err(e) => {
            uefi::println!("{TAG} VMCS {operation} failed: VmFailValid, error unreadable ({e:?})");
        }
    }
}

/// Prints the segmentation state that will go into the VMCS host fields.
///
/// TR is the one to watch: VM entry rejects a host TR selector of 0.
fn dump_segment_state() {
    let state = unsafe { vmx::segment::read_all() };

    uefi::println!(
        "{TAG} segments: cs={:#06x} ss={:#06x} ds={:#06x} es={:#06x} fs={:#06x} gs={:#06x} tr={:#06x}",
        state.cs, state.ss, state.ds, state.es, state.fs, state.gs, state.tr,
    );
    uefi::println!(
        "{TAG} bases:    fs={:#018x} gs={:#018x} tr={:#018x}",
        state.fs_base, state.gs_base, state.tr_base,
    );
    uefi::println!(
        "{TAG} gdtr:     base={:#018x} limit={:#06x}",
        state.gdtr.base, state.gdtr.limit,
    );
    uefi::println!(
        "{TAG} idtr:     base={:#018x} limit={:#06x}",
        state.idtr.base, state.idtr.limit,
    );

    if state.tr == 0 {
        uefi::println!("{TAG} WARNING: TR is 0; VM entry would reject this host state.");
    }
}
