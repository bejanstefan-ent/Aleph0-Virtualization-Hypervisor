#![no_main]
#![no_std]

use uefi::prelude::*;

mod vmx;
use vmx::VmxCapabilities;

#[entry]
fn main() -> Status {
    uefi::println!("[Aleph0 Virtualization Hypervisor] Initializing UEFI helpers...");

    uefi::helpers::init().unwrap();

    uefi::println!("[Aleph0 Virtualization Hypervisor] UEFI helpers initialized successfully.");

    // Bound at this scope on purpose: the VMXON region must stay alive for as
    // long as the CPU is in VMX operation, i.e. until the loop below.
    let _vmxon_region = match unsafe { vmx::detect() } {
        VmxCapabilities::Supported => {
            uefi::println!("[Aleph0 Virtualization Hypervisor] VMX supported and enabled by firmware.");

            match unsafe { vmx::vmxon::enter_vmx_root_operation() } {
                Ok(region) => {
                    uefi::println!("[Aleph0 Virtualization Hypervisor] Entered VMX root operation.");
                    Some(region)
                }
                Err(e) => {
                    uefi::println!("[Aleph0 Virtualization Hypervisor] VMXON failed: {:?}", e);
                    None
                }
            }
        }
        VmxCapabilities::NotSupportedByCpu => {
            uefi::println!("[Aleph0 Virtualization Hypervisor] VMX not supported by this CPU.");
            None
        }
        VmxCapabilities::DisabledByFirmware => {
            uefi::println!("[Aleph0 Virtualization Hypervisor] VMX supported by CPU but disabled by firmware.");
            None
        }
    };

    loop { }
}
