//! Stela's virtio-mmio devices in the guest's DeviceTree:
//! `/arm-io/stela-virtio<n>`, whose nub the StelaVirtIO kext's transport
//! matches by `compatible` and maps by `reg` (`stela_contracts::virtio`). Each
//! is placed as the StelaGPU gates and the StelaNet adapter are
//! ([`crate::stela_gpu::place_window`]): the last free aligned gap of
//! `/arm-io`, and the AIC input past every one in use, so whichever is
//! placed later takes the next gap and input.
use crate::Result;
use crate::apple_device_tree::DeviceTree;
use crate::stela_gpu::{Placement, place_window};
use alloc::format;

/// What the kext's transport matches (`IONameMatch`), then the Linux
/// binding's name for a virtio-mmio device, so other guests find it too.
pub const COMPATIBLE: &[u8] = b"stela,virtio-mmio\0virtio,mmio\0";
const DEVICE_TYPE: &[u8] = b"stela-virtio\0";

/// The node's name under `/arm-io` for device `index`.
pub fn name(index: usize) -> alloc::string::String {
    format!("stela-virtio{index}")
}

/// Adds `/arm-io/stela-virtio<index>` with its register page.
pub fn place_virtio(tree: &mut DeviceTree, index: usize) -> Result<Placement> {
    place_window(
        tree,
        &name(index),
        COMPATIBLE,
        DEVICE_TYPE,
        stela_contracts::virtio::mmio::BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple_device_tree::Node;
    use crate::stela_gpu::place_gpu_gate;
    use crate::stela_net::place_net;
    use alloc::vec::Vec;

    const GATE: u64 = 0x40_0000;
    const AIC_PHANDLE: u32 = 7;

    fn tree() -> DeviceTree {
        let mut arm_io = Node::named("arm-io").unwrap();
        for cells in ["#address-cells", "#size-cells"] {
            arm_io
                .set_property(cells, 2u32.to_le_bytes().to_vec())
                .unwrap();
        }
        let ranges = [0, 0x2_0000_0000, 0x1000_0000]
            .map(u64::to_le_bytes)
            .concat();
        arm_io.set_property("ranges", ranges).unwrap();
        let mut aic = Node::named("aic").unwrap();
        aic.set_property(
            "reg",
            [0xfff_0000u64, 0x1_0000].map(u64::to_le_bytes).concat(),
        )
        .unwrap();
        aic.set_property("AAPL,phandle", AIC_PHANDLE.to_le_bytes().to_vec())
            .unwrap();
        aic.set_property("interrupt-parent", AIC_PHANDLE.to_le_bytes().to_vec())
            .unwrap();
        aic.set_property("interrupts", 0x40u32.to_le_bytes().to_vec())
            .unwrap();
        arm_io.children.push(aic);
        let mut root = Node::named("device-tree").unwrap();
        root.children.push(arm_io);
        DeviceTree { root }
    }

    #[test]
    fn a_device_gets_its_node_page_and_interrupt() {
        let mut tree = tree();
        let placement = place_virtio(&mut tree, 0).unwrap();
        assert_eq!(placement.physical, 0x2_0000_0000 + 0xffe_c000);
        assert_eq!(placement.irq, 0x41);
        let node = tree.node("/arm-io/stela-virtio0").unwrap();
        assert_eq!(node.property("compatible").unwrap().value, COMPATIBLE);
        assert_eq!(node.property("device_type").unwrap().value, DEVICE_TYPE);
        assert_eq!(node.property("interrupts").unwrap().as_u32(), Some(0x41));
        assert_eq!(
            node.property("interrupt-parent").unwrap().as_u32(),
            Some(AIC_PHANDLE)
        );
        assert_eq!(
            node.property("reg").unwrap().value,
            [0xffe_c000u64, 0x4000].map(u64::to_le_bytes).concat()
        );
        assert!(place_virtio(&mut tree, 0).is_err(), "one node per index");
        // The next device takes the next page down and the next input.
        let next = place_virtio(&mut tree, 1).unwrap();
        assert_eq!(
            (next.physical, next.irq),
            (0x2_0000_0000 + 0xffe_8000, 0x42)
        );
    }

    #[test]
    fn devices_after_the_gpu_gate_and_stelanet_each_take_their_own_window_and_input() {
        // The boot's order: the GPU gates, StelaNet, then the virtio devices.
        let mut tree = tree();
        let gate = place_gpu_gate(&mut tree, 0, GATE).unwrap();
        let net = place_net(&mut tree).unwrap();
        let virtio = place_virtio(&mut tree, 0).unwrap();
        let windows = [(gate, GATE), (net, 0x4000), (virtio, 0x4000)];
        let inputs: Vec<u32> = windows.iter().map(|(placement, _)| placement.irq).collect();
        assert_eq!(inputs, [0x41, 0x42, 0x43]);
        for (index, (a, a_bytes)) in windows.iter().enumerate() {
            for (b, b_bytes) in &windows[index + 1..] {
                assert!(
                    a.physical + a_bytes <= b.physical || b.physical + b_bytes <= a.physical,
                    "{a:?} and {b:?} overlap"
                );
            }
        }
        assert_eq!(virtio.physical, 0x2_0ffe_8000);
        assert!(tree.node("/arm-io/stela-virtio0").is_some());
    }
}
