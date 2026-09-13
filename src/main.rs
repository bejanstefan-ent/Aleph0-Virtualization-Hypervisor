#![no_main]
#![no_std]

use uefi::prelude::*;

mod vmx;
use vmx::VmxCapabilities;
use vmx::vmcs::{VmcsError, VmcsRegion};
use vmx::vmxon::VmxOnRegion;

const TAG: &str = "[Aleph0 Virtualization Hypervisor]";

#[entry]
fn main() -> Status {
    uefi::println!("{TAG} Initializing UEFI helpers...");

    uefi::helpers::init().unwrap();

    uefi::println!("{TAG} UEFI helpers initialized successfully.");

    // Held until the loop below: the CPU keeps using both regions for as long
    // as it stays in VMX operation, so neither may be dropped before then.
    let _vmx_state = bring_up_vmx();

    loop {}
}

/// Detects VMX, enters root operation, then exercises the VMCS access path.
///
/// Returns the regions so the caller can keep them alive. The VMCS is
/// optional: VMXON can succeed while the VMCS self-test fails.
fn bring_up_vmx() -> Option<(VmxOnRegion, Option<VmcsRegion>)> {
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
            Some(region)
        }
        Err(e) => {
            report_vmcs_error(e);
            None
        }
    };

    Some((vmxon_region, vmcs_region))
}

/// Prints a VMCS failure, decoding the VM-instruction error when one exists.
fn report_vmcs_error(error: VmcsError) {
    if error != VmcsError::VmFailValid {
        uefi::println!("{TAG} VMCS self-test failed: {error:?}");
        return;
    }

    // VmFailValid implies a current VMCS, so the error number is readable.
    match unsafe { vmx::vmcs::vm_instruction_error() } {
        Ok(code) => {
            let name = vmx::vmcs::vm_instruction_error_name(code);
            uefi::println!("{TAG} VMCS self-test failed: {name} ({code})");
        }
        Err(e) => {
            uefi::println!("{TAG} VMCS self-test failed: VmFailValid, error unreadable ({e:?})");
        }
    }
}
