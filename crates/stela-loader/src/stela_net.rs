//! The StelaNet adapter in the guest's DeviceTree: `/arm-io/stela-net0`,
//! whose nub the StelaNet front end of the StelaGPU kext matches by
//! `compatible` and maps by `reg` (`stela_contracts::net`). It is placed as
//! the StelaGPU gates are ([`crate::stela_gpu::place_window`]): the last
//! free aligned gap of `/arm-io`, and the AIC input past every one in use,
//! so whichever of them is placed later takes the next gap and input.
use crate::Result;
use crate::apple_device_tree::DeviceTree;
use crate::stela_gpu::{Placement, place_window};

/// The node's name under `/arm-io`.
pub const NAME: &str = "stela-net0";
/// What the kext's personality matches (`IONameMatch`).
pub const COMPATIBLE: &[u8] = b"net,stela\0";
const DEVICE_TYPE: &[u8] = b"stela-net\0";

/// Adds `/arm-io/stela-net0` with its register page.
pub fn place_net(tree: &mut DeviceTree) -> Result<Placement> {
    place_window(
        tree,
        NAME,
        COMPATIBLE,
        DEVICE_TYPE,
        stela_contracts::net::window::BYTES,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apple_device_tree::Node;
    use crate::stela_gpu::place_gpu_gate;

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
    fn the_adapter_gets_its_node_page_and_interrupt() {
        let mut tree = tree();
        let placement = place_net(&mut tree).unwrap();
        // The last 16 KiB below the AIC's registers, the input past 0x40.
        assert_eq!(placement.physical, 0x2_0000_0000 + 0xffe_c000);
        assert_eq!(placement.irq, 0x41);
        let node = tree.node("/arm-io/stela-net0").unwrap();
        assert_eq!(node.property("compatible").unwrap().value, COMPATIBLE);
        assert_eq!(node.property("device_type").unwrap().value, DEVICE_TYPE);
        assert_eq!(node.property("interrupts").unwrap().as_u32(), Some(0x41));
        assert_eq!(
            node.property("reg").unwrap().value,
            [0xffe_c000u64, 0x4000].map(u64::to_le_bytes).concat()
        );
        assert!(place_net(&mut tree).is_err(), "one adapter");
    }

    #[test]
    fn the_adapter_after_the_gpu_gate_takes_the_next_gap_and_input() {
        // The boot places the GPU gates first, then the adapter.
        let apart = |net: Placement, gate: Placement| {
            net.physical + 0x4000 <= gate.physical || gate.physical + GATE <= net.physical
        };
        let mut tree = tree();
        let gate = place_gpu_gate(&mut tree, 0, GATE).unwrap();
        let net = place_net(&mut tree).unwrap();
        // The gate takes the aligned 4 MiB below the AIC; the adapter the
        // 16 KiB just below the AIC, above the gate.
        assert_eq!(gate.physical, 0x2_0f80_0000);
        assert_eq!(net.physical, 0x2_0ffe_c000);
        assert_eq!((gate.irq, net.irq), (0x41, 0x42));
        assert!(apart(net, gate));
        // The other order also gives each its own window and input.
        let mut tree = super::tests::tree();
        let net = place_net(&mut tree).unwrap();
        let gate = place_gpu_gate(&mut tree, 0, GATE).unwrap();
        assert_eq!((net.irq, gate.irq), (0x41, 0x42));
        assert!(apart(net, gate));
        assert!(tree.node("/arm-io/stela-net0").is_some());
        assert!(tree.node("/arm-io/stela-gpu0").is_some());
    }
}
