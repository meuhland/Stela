use super::*;
use crate::metal::{open::PcieLink, vulkan::physical::PciBusInfo};

fn radv(device_type: PhysicalDeviceType) -> Identity {
    Identity {
        api_version: (1 << 22) | (4 << 12),
        driver_id: DriverId::MesaRadv,
        driver_version: 0,
        vendor_id: VENDOR_ID,
        device_id: 0x13c0,
        pipeline_cache_uuid: [0xad; 16],
        device_type,
        device_name: String::from("AMD Radeon Graphics (RADV RAPHAEL_MENDOCINO)"),
        conformance_version: [1, 4, 3, 0],
        pci_bus: Some(PciBusInfo {
            domain: 0,
            bus: 0x7a,
            device: 0,
            function: 0,
        }),
    }
}

fn attachment(external: bool) -> Attachment {
    Attachment {
        link: PcieLink {
            speed: 4,
            width: 16,
        },
        external,
        location_number: 3,
    }
}

/// GC 10.3.6, the graphics of Ryzen 7000 and 9000 processors.
const RAPHAEL: u32 = 149;

#[test]
fn an_integrated_gpu_is_built_in_with_amdgpus_family_name() {
    let platform = admit_family(
        &radv(PhysicalDeviceType::IntegratedGpu),
        RAPHAEL,
        attachment(false),
    )
    .unwrap();
    assert_eq!(platform.architecture, "GC_10_3_6");
    assert_eq!(platform.location, DeviceLocation::BuiltIn);
    assert_eq!(platform.max_transfer_rate, 0);
    assert!(!platform.headless && !platform.removable);
    assert_eq!(platform.location_number, 3);
    assert_eq!(platform.linear_row_alignment, 256);
    assert_eq!(platform.linear_offset_alignment, 256);
    assert_eq!(platform.sparse_page_bytes, 64 << 10);
}

#[test]
fn a_discrete_gpu_reports_its_link_and_where_it_is() {
    let navi = 143;
    let slot = admit_family(
        &radv(PhysicalDeviceType::DiscreteGpu),
        navi,
        attachment(false),
    )
    .unwrap();
    assert_eq!(slot.architecture, "NV");
    assert_eq!(slot.location, DeviceLocation::Slot);
    assert_eq!(
        slot.max_transfer_rate,
        attachment(false).link.bytes_per_second()
    );
    let external = admit_family(
        &radv(PhysicalDeviceType::DiscreteGpu),
        navi,
        attachment(true),
    )
    .unwrap();
    assert_eq!(external.location, DeviceLocation::External);
    assert!(external.removable);
}

#[test]
fn other_vendors_drivers_families_and_devices_are_refused() {
    let mut nvidia = radv(PhysicalDeviceType::DiscreteGpu);
    nvidia.vendor_id = 0x10de;
    assert_eq!(
        admit_family(&nvidia, RAPHAEL, attachment(false)),
        Err(Refusal::NotAmd { vendor_id: 0x10de })
    );
    let mut venus = radv(PhysicalDeviceType::DiscreteGpu);
    venus.driver_id = DriverId::MesaVenus;
    assert_eq!(
        admit_family(&venus, RAPHAEL, attachment(false)),
        Err(Refusal::NotRadv(DriverId::MesaVenus))
    );
    assert_eq!(
        admit_family(&radv(PhysicalDeviceType::DiscreteGpu), 0, attachment(false)),
        Err(Refusal::UnknownFamily { family: 0 })
    );
    assert_eq!(
        admit_family(&radv(PhysicalDeviceType::Cpu), RAPHAEL, attachment(false)),
        Err(Refusal::NotGpu(PhysicalDeviceType::Cpu))
    );
}
