//! amdgpu as a stack's kernel driver: what the boot asks about an AMD GPU
//! before Linux sees it, answered from amdgpu's own PCI ID table, the list
//! of devices it leaves to radeon and its family rules as the build read
//! them (`build/amdgpu.rs`).
use crate::{Adapter, stack::Control};

include!(concat!(env!("OUT_DIR"), "/amdgpu_devices.rs"));

/// What amdgpu's probe answers for one device of its table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Support {
    /// The probe goes on to the device's IP blocks.
    Probed,
    /// `amdgpu_support_enabled` refuses the family: this build's Kconfig
    /// leaves its support out.
    NotBuilt(&'static str),
}

/// The table's entry for `device`, if it names the device by ID.
fn listed(device: u16) -> Option<(u8, Support)> {
    DEVICES
        .binary_search_by_key(&device, |(id, ..)| *id)
        .ok()
        .map(|index| (DEVICES[index].1, DEVICES[index].2))
}

/// Whether the table's class entries hold the adapter: any AMD function of
/// one of these classes, which the driver then identifies by IP discovery.
fn discovered(adapter: &Adapter) -> bool {
    let class = u32::from_be_bytes([0, adapter.class[0], adapter.class[1], adapter.class[2]]);
    CLASSES.contains(&class)
}

/// Whether amdgpu's probe takes the adapter: its table holds it, by ID or by
/// class, the driver does not leave it to radeon, and this build holds its
/// family's support.
pub fn admits(adapter: &Adapter) -> Result<(), &'static str> {
    if LEFT_TO_RADEON.binary_search(&adapter.device).is_ok() {
        return Err(
            "amdgpu leaves this GPU to an older driver (radeon, r128 or mach64), which is not ported to Stela",
        );
    }
    match listed(adapter.device) {
        Some((_, Support::Probed)) => Ok(()),
        Some((_, Support::NotBuilt(reason))) => Err(reason),
        None if discovered(adapter) => Ok(()),
        None => Err("amdgpu's PCI ID table does not hold this device"),
    }
}

/// The DMA mask amdgpu sets on the adapter, or the narrowest it may set: a
/// device named by ID has its family's memory controller's mask; one found
/// by class sets its mask after IP discovery, and gets the narrowest mask of
/// the memory controllers discovery can choose.
pub fn dma_bits(adapter: &Adapter) -> u8 {
    listed(adapter.device).map_or(DISCOVERY_BITS, |(bits, _)| bits)
}

/// The name amdgpu's interface gives the family `family`
/// (`AMDGPU_FAMILY_*` without its prefix), which its device query answers.
pub fn family_name(family: u32) -> Option<&'static str> {
    FAMILIES
        .binary_search_by_key(&family, |(number, _)| *number)
        .ok()
        .map(|index| FAMILIES[index].1)
}

/// amdgpu's query ioctl, `DRM_IOCTL_AMDGPU_INFO`.
pub mod query {
    use super::Control;

    /// `struct drm_amdgpu_info`: where the answer goes, its size, the query
    /// and the query's own arguments, which these queries leave zero.
    #[repr(C)]
    struct Info {
        return_pointer: u64,
        return_size: u32,
        query: u32,
        arguments: [u32; 4],
    }

    /// `DRM_IOW(DRM_COMMAND_BASE + DRM_AMDGPU_INFO, struct drm_amdgpu_info)`.
    const INFO: u32 = 0x4020_6445;
    /// `AMDGPU_INFO_DEV_INFO`, answering `struct drm_amdgpu_info_device`.
    const DEV_INFO: u32 = 0x16;
    /// `AMDGPU_INFO_MEMORY`, answering `struct drm_amdgpu_memory_info`.
    const MEMORY: u32 = 0x19;
    /// `AMDGPU_IDS_FLAGS_FUSION`.
    const FUSION: u64 = 1;

    /// The first `N` words of the answer to `query`. The driver copies no
    /// more than the size asked.
    fn get<const N: usize>(control: Control, query: u32) -> Option<[u64; N]> {
        let mut answer = [0_u64; N];
        let mut record = Info {
            return_pointer: answer.as_mut_ptr() as u64,
            return_size: size_of::<[u64; N]>() as u32,
            query,
            arguments: [0; 4],
        };
        (control(INFO, (&raw mut record).cast()) == 0).then_some(answer)
    }

    /// The device's family (`AMDGPU_FAMILY_*`), the fifth 32-bit field of
    /// `struct drm_amdgpu_info_device` after the device ID and revisions.
    pub fn family(control: Control) -> Option<u32> {
        get::<3>(control, DEV_INFO).map(|words| words[2] as u32)
    }

    /// Whether the device is built into the processor: amdgpu sets
    /// `AMDGPU_IDS_FLAGS_FUSION` in `ids_flags`, the 64-bit field at byte 136
    /// of `struct drm_amdgpu_info_device`, for a device it flags
    /// `AMD_IS_APU`, from its ID table or, for a device its table matches by
    /// class, from the graphics core IP discovery reports.
    pub fn integrated(control: Control) -> Option<bool> {
        get::<18>(control, DEV_INFO).map(|words| words[17] & FUSION != 0)
    }

    /// The memory the driver hands out: the usable bytes of its VRAM heap
    /// and of its GTT heap, which on an integrated GPU is system memory.
    /// `struct drm_amdgpu_memory_info` is three heaps (VRAM, its
    /// CPU-accessible part, GTT) of four 64-bit fields, the second usable.
    pub fn memory_bytes(control: Control) -> Option<u64> {
        let heaps = get::<12>(control, MEMORY)?;
        heaps[1].checked_add(heaps[9]).filter(|bytes| *bytes != 0)
    }

    /// `DRM_IOCTL_AMDGPU_CTX`: `DRM_IOWR(DRM_COMMAND_BASE + 0x02, union
    /// drm_amdgpu_ctx)`.
    const CTX: u32 = 0xc010_6442;
    const CTX_OP_ALLOC_CTX: u32 = 1;
    const CTX_OP_QUERY_STATE2: u32 = 4;
    /// `AMDGPU_CTX_QUERY2_FLAGS_RESET`: the GPU was reset since the context
    /// was made.
    const CTX_RESET: u64 = 1 << 0;
    /// `AMDGPU_CTX_QUERY2_FLAGS_RESET_IN_PROGRESS`: amdgpu is resetting it.
    const CTX_RESET_IN_PROGRESS: u64 = 1 << 5;

    /// `union drm_amdgpu_ctx`: `drm_amdgpu_ctx_in`, over which the kernel
    /// writes `drm_amdgpu_ctx_out` (`alloc.ctx_id` or `state.flags`).
    #[repr(C)]
    #[derive(Default)]
    struct Context {
        op: u32,
        flags: u32,
        ctx_id: u32,
        priority: i32,
    }

    /// Makes an amdgpu context, answering its `ctx_id`.
    pub fn reset_watch(control: Control) -> Option<u32> {
        let mut context = Context {
            op: CTX_OP_ALLOC_CTX,
            ..Context::default()
        };
        // `alloc.ctx_id` is the output's first word.
        (control(CTX, (&raw mut context).cast()) == 0).then_some(context.op)
    }

    /// `AMDGPU_CTX_QUERY2_FLAGS_*` of the context `ctx_id`. Takes only the
    /// file's own context lock.
    pub fn reset_state(control: Control, ctx_id: u32) -> Option<crate::stack::ResetState> {
        use crate::stack::ResetState;
        let mut context = Context {
            op: CTX_OP_QUERY_STATE2,
            ctx_id,
            ..Context::default()
        };
        if control(CTX, (&raw mut context).cast()) != 0 {
            return None;
        }
        // `state.flags` is the output's first eight bytes.
        let flags = u64::from(context.op) | u64::from(context.flags) << 32;
        Some(if flags & CTX_RESET_IN_PROGRESS != 0 {
            ResetState::Resetting
        } else if flags & CTX_RESET != 0 {
            ResetState::Reset
        } else {
            ResetState::Running
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Vendor, bus::pci::Address};
    use core::ffi::c_void;

    fn adapter(device: u16, class: [u8; 3]) -> Adapter {
        Adapter {
            address: Address {
                segment: 0,
                bus: 0x10,
                device: 0,
                function: 0,
            },
            vendor: Vendor::from_pci(0x1002),
            device,
            class,
        }
    }

    #[test]
    fn the_table_is_amdgpus_own_with_each_familys_mask() {
        assert!(DEVICES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(LEFT_TO_RADEON.windows(2).all(|pair| pair[0] < pair[1]));
        // Polaris 10 (GMC 8): 40 bits. Vega 10, Navi 10 and Sienna Cichlid
        // (GMC 9, 10 and 11 or later): the narrowest mask of those.
        assert_eq!(listed(0x67df), Some((40, Support::Probed)));
        for device in [0x6860, 0x731f, 0x73bf] {
            assert_eq!(listed(device), Some((DISCOVERY_BITS, Support::Probed)));
        }
        assert_eq!(DISCOVERY_BITS, 44);
        // VGA, other display and processing accelerator.
        assert_eq!(CLASSES, [0x03_0000, 0x03_8000, 0x12_0000]);
    }

    #[test]
    fn families_are_named_as_amdgpus_interface_names_them() {
        assert!(FAMILIES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        // Navi 10, Raphael and Granite Ridge graphics, GFX12.
        assert_eq!(family_name(143), Some("NV"));
        assert_eq!(family_name(149), Some("GC_10_3_6"));
        assert_eq!(family_name(152), Some("GC_12_0_0"));
        assert_eq!(family_name(0), None);
        assert_eq!(family_name(147), None);
    }

    #[test]
    fn a_device_is_admitted_as_the_probe_would_take_it() {
        // A GPU the table does not name is found by its class and gets the
        // mask every memory controller of discovery allows.
        let unnamed = adapter(0x0001, [3, 0, 0]);
        assert_eq!(listed(unnamed.device), None);
        assert_eq!(admits(&unnamed), Ok(()));
        assert_eq!(dma_bits(&unnamed), 44);
        // An unnamed function of another class is not amdgpu's.
        assert!(admits(&adapter(0x0001, [4, 3, 0])).is_err());
        // Polaris 10, by ID.
        let polaris = adapter(0x67df, [3, 0, 0]);
        assert_eq!(admits(&polaris), Ok(()));
        assert_eq!(dma_bits(&polaris), 40);
        // A device of the list amdgpu leaves to radeon, VGA class or not.
        let radeon = adapter(LEFT_TO_RADEON[0], [3, 0, 0]);
        assert!(admits(&radeon).unwrap_err().contains("radeon"));
        // Families whose support the profile leaves out are refused with the
        // driver's own message: Tahiti (SI) and Bonaire (CIK).
        for (device, generation) in [(0x6798, "SI"), (0x6649, "CIK")] {
            let refusal = admits(&adapter(device, [3, 0, 0])).unwrap_err();
            assert_eq!(
                refusal,
                alloc::format!("amdgpu built without {generation} support")
            );
        }
    }

    #[test]
    fn queries_ask_the_info_ioctl_and_keep_refusals() {
        let mut asked = alloc::vec::Vec::new();
        let mut fused = false;
        let mut answer = |request: u32, argument: *mut c_void| {
            // SAFETY: the query passes its own record and answer buffer.
            let record = unsafe { &*argument.cast::<[u64; 4]>() };
            let (size, query) = (record[1] as u32, (record[1] >> 32) as u32);
            asked.push((request, query, size));
            // SAFETY: as above; the buffer holds `size` bytes.
            let words = unsafe {
                core::slice::from_raw_parts_mut(record[0] as *mut u64, size as usize / 8)
            };
            if query == 0x16 {
                // Device ID and chip revision, then the family in the low
                // half of the third word.
                words[2] = 0x1234_5678_0000_0091;
                if size > 136 {
                    // `ids_flags`: preemption, then fusion on the second ask.
                    words[17] = if fused { 3 } else { 2 };
                    fused = true;
                }
            } else {
                words[1] = 512 << 20;
                words[9] = 16 << 30;
            }
            0
        };
        assert_eq!(query::family(&mut answer), Some(0x91));
        assert_eq!(
            query::memory_bytes(&mut answer),
            Some((512 << 20) + (16 << 30))
        );
        assert_eq!(query::integrated(&mut answer), Some(false));
        assert_eq!(query::integrated(&mut answer), Some(true));
        assert_eq!(
            asked,
            [
                (0x4020_6445, 0x16, 24),
                (0x4020_6445, 0x19, 96),
                (0x4020_6445, 0x16, 144),
                (0x4020_6445, 0x16, 144)
            ]
        );
        let mut refuse = |_: u32, _: *mut c_void| -22;
        assert_eq!(query::family(&mut refuse), None);
        assert_eq!(query::integrated(&mut refuse), None);
        assert_eq!(query::memory_bytes(&mut refuse), None);
        let mut empty = |_: u32, _: *mut c_void| 0;
        assert_eq!(query::memory_bytes(&mut empty), None);
    }
}
