//! Virtio over a DeviceTree register page: the virtio-mmio version 2
//! transport (virtio 1.2, 4.2) of the devices at `/arm-io/stela-virtio<n>`,
//! as the runtime models them (`stela_soc::virtio`) and the StelaVirtIO kext
//! drives them. The kext's `stela/virtio.h` is generated from this module.
//!
//! The register page's offsets and values are Linux's
//! `include/uapi/linux/virtio_mmio.h`; the feature bits and device IDs are
//! the specification's (6, 5). A device is one 16 KiB page and one AIC
//! input, a level the device holds while `INTERRUPT_STATUS` has a bit set.
//! Its virtqueues are split (2.7), each in guest RAM at the three guest
//! physical addresses the driver programs: descriptor table, available
//! ring and used ring.

/// Defines a block of 64-bit layout constants and `LAYOUT` listing them,
/// from which the kext's header is generated.
macro_rules! layout {
    ($($(#[doc = $doc:literal])* $name:ident = $value:expr;)*) => {
        $($(#[doc = $doc])* pub const $name: u64 = $value;)*
        /// Every constant above, by name.
        pub const LAYOUT: &[(&str, u64)] = &[$((stringify!($name), $name)),*];
    };
}

/// The register page (`virtio_mmio.h`). Every register below `CONFIG` is
/// 32-bit; the device's configuration space from `CONFIG` on is read in
/// 8-, 16- and 32-bit accesses.
pub mod mmio {
    layout! {
        /// The register page: the guest's page, so only the kernel maps it.
        BYTES = 0x4000;

        /// R: [`MAGIC`].
        MAGIC_VALUE = 0x000;
        /// R: [`VERSION_2`]: no legacy interface.
        VERSION = 0x004;
        /// R: the device type (`ids`); 0 for none.
        DEVICE_ID = 0x008;
        /// R: [`VENDOR`].
        VENDOR_ID = 0x00c;
        /// R: 32 bits of the device's features, those `DEVICE_FEATURES_SEL`
        /// selects (0 for bits 0-31, 1 for bits 32-63).
        DEVICE_FEATURES = 0x010;
        /// W: which 32 bits `DEVICE_FEATURES` shows.
        DEVICE_FEATURES_SEL = 0x014;
        /// W: 32 bits of the features the driver accepts.
        DRIVER_FEATURES = 0x020;
        /// W: which 32 bits `DRIVER_FEATURES` writes.
        DRIVER_FEATURES_SEL = 0x024;
        /// W: the queue the `QUEUE_` registers below name.
        QUEUE_SEL = 0x030;
        /// R: the most entries the selected queue takes; 0 for no queue.
        QUEUE_NUM_MAX = 0x034;
        /// W: the selected queue's entries, a power of two.
        QUEUE_NUM = 0x038;
        /// RW: 1 the selected queue runs on the rings programmed; 0 stops it.
        QUEUE_READY = 0x044;
        /// W: the index of a queue with new available buffers.
        QUEUE_NOTIFY = 0x050;
        /// R: `INT_` bits raised and not yet acknowledged.
        INTERRUPT_STATUS = 0x060;
        /// W: clears the `INT_` bits written; with none left the line falls.
        INTERRUPT_ACK = 0x064;
        /// RW: the `status` bits; 0 resets the device.
        STATUS = 0x070;
        /// W: the selected queue's descriptor table, low and high words.
        QUEUE_DESC_LOW = 0x080;
        QUEUE_DESC_HIGH = 0x084;
        /// W: its available ring (the driver area).
        QUEUE_AVAIL_LOW = 0x090;
        QUEUE_AVAIL_HIGH = 0x094;
        /// W: its used ring (the device area).
        QUEUE_USED_LOW = 0x0a0;
        QUEUE_USED_HIGH = 0x0a4;
        /// W: the shared memory region `SHM_` reads describe.
        SHM_SEL = 0x0ac;
        /// R: a region's length and base, all ones for none (none here).
        SHM_LEN_LOW = 0x0b0;
        SHM_LEN_HIGH = 0x0b4;
        SHM_BASE_LOW = 0x0b8;
        SHM_BASE_HIGH = 0x0bc;
        /// R: changes whenever the configuration space does.
        CONFIG_GENERATION = 0x0fc;
        /// The device's configuration space.
        CONFIG = 0x100;

        /// `MAGIC_VALUE`: "virt" in little-endian.
        MAGIC = 0x7472_6976;
        /// `VERSION`: the virtio 1 interface.
        VERSION_2 = 2;
        /// `VENDOR_ID`: "STLA" in little-endian.
        VENDOR = 0x414c_5453;

        /// `INTERRUPT_STATUS`: a queue has new used buffers.
        INT_VRING = 1 << 0;
        /// `INTERRUPT_STATUS`: the configuration, or the device's status
        /// (`DEVICE_NEEDS_RESET`), changed.
        INT_CONFIG = 1 << 1;
    }
}

/// The device status bits (virtio 1.2, 2.1).
pub mod status {
    layout! {
        /// The guest found the device.
        ACKNOWLEDGE = 1;
        /// The guest has a driver for it.
        DRIVER = 2;
        /// The driver is ready: the device may use its queues.
        DRIVER_OK = 4;
        /// The device accepted the driver's features.
        FEATURES_OK = 8;
        /// The device stopped on an error; the driver must reset it.
        DEVICE_NEEDS_RESET = 0x40;
        /// The driver gave up on the device.
        FAILED = 0x80;
    }
}

/// Feature bits every device type shares (virtio 1.2, 6).
pub mod features {
    layout! {
        /// Indirect descriptor tables: never offered.
        INDIRECT_DESC = 1 << 28;
        /// `used_event` and `avail_event` suppress notifications.
        EVENT_IDX = 1 << 29;
        /// Virtio 1 semantics, which a version 2 page requires.
        VERSION_1 = 1 << 32;
        /// The device sits behind an IOMMU: never offered, its DMA is
        /// guest physical.
        ACCESS_PLATFORM = 1 << 33;
        /// Packed virtqueues: never offered.
        RING_PACKED = 1 << 34;

        /// What every device offers on top of its own bits.
        TRANSPORT = VERSION_1 | EVENT_IDX;
        /// What a driver must accept.
        REQUIRED = VERSION_1;
    }
}

/// Device types (virtio 1.2, 5; `virtio_ids.h`).
pub mod ids {
    layout! {
        NET = 1;
        BLOCK = 2;
        CONSOLE = 3;
        /// The entropy source.
        RNG = 4;
        SOUND = 25;
    }
}

/// The network device (virtio 1.2, 5.1; `virtio_net.h`): its feature bits,
/// configuration space and the `virtio_net_hdr_v1` every buffer starts with,
/// as `stela_soc::virtio::net` models it and StelaVirtIONet drives it. The
/// feature bits are StelaNet's (`crate::net::features`), which numbers them
/// as virtio-net does.
pub mod net {
    /// Feature bits (5.1.3).
    pub mod features {
        layout! {
            /// The device completes partial transmit checksums.
            CSUM = 1 << 0;
            /// The driver takes receive frames whose checksums the device
            /// vouched for (`HDR_F_DATA_VALID`).
            GUEST_CSUM = 1 << 1;
            /// `CONFIG_MAC` holds the station address.
            MAC = 1 << 5;
            /// The driver takes coalesced TCPv4 and TCPv6 receive segments.
            GUEST_TSO4 = 1 << 7;
            GUEST_TSO6 = 1 << 8;
            /// The device segments TCPv4 and TCPv6: never offered.
            HOST_TSO4 = 1 << 11;
            HOST_TSO6 = 1 << 12;
            /// A receive frame may span buffers (`num_buffers`).
            MRG_RXBUF = 1 << 15;
            /// `CONFIG_STATUS` holds the link state.
            STATUS = 1 << 16;
            /// A control queue: never offered, so the device filters nothing
            /// a driver could set (promiscuous and multicast lists).
            CTRL_VQ = 1 << 17;

            /// What the device always offers, on top of the transport's.
            OFFERED = MAC | STATUS;
        }
    }

    layout! {
        /// The configuration space: the station address, 6 bytes.
        CONFIG_MAC = 0;
        /// The link state, 16 bits of `STATUS_` bits.
        CONFIG_STATUS = 6;
        /// The queue pairs, 16 bits: always 1.
        CONFIG_MAX_VIRTQUEUE_PAIRS = 8;
        /// The configuration space's bytes.
        CONFIG_BYTES = 10;
        /// `CONFIG_STATUS`: the link is up.
        STATUS_LINK_UP = 1;

        /// The receive queue's index, and the transmit queue's.
        RX_QUEUE = 0;
        TX_QUEUE = 1;

        /// The `virtio_net_hdr_v1` every buffer starts with (5.1.6), its
        /// `num_buffers` included whatever the features.
        HEADER_BYTES = 12;
        /// Its fields' offsets: flags, gso_type, hdr_len, gso_size,
        /// csum_start, csum_offset and num_buffers.
        HDR_FLAGS = 0;
        HDR_GSO_TYPE = 1;
        HDR_HDR_LEN = 2;
        HDR_GSO_SIZE = 4;
        HDR_CSUM_START = 6;
        HDR_CSUM_OFFSET = 8;
        HDR_NUM_BUFFERS = 10;
        /// `flags`: the checksum from `csum_start` is partial, its field at
        /// `csum_offset` past it.
        HDR_F_NEEDS_CSUM = 1;
        /// `flags`: the device verified the frame's checksums.
        HDR_F_DATA_VALID = 2;
        /// `gso_type`: no segmentation.
        HDR_GSO_NONE = 0;

        /// The longest Ethernet frame without FCS, the header excluded, and
        /// the shortest: an Ethernet header.
        FRAME_MAX = 1514;
        FRAME_MIN = 14;
    }
}

/// A split virtqueue's three areas (virtio 1.2, 2.7), each at the address
/// the driver programs.
pub mod ring {
    /// A descriptor: address, length, flags, next.
    pub const DESCRIPTOR_BYTES: u64 = 16;
    /// `flags`: the chain goes on at `next`.
    pub const DESC_F_NEXT: u16 = 1;
    /// `flags`: the device writes this buffer.
    pub const DESC_F_WRITE: u16 = 2;
    /// `flags`: an indirect table (not offered).
    pub const DESC_F_INDIRECT: u16 = 4;
    /// Available ring `flags`: no interrupt wanted (without EVENT_IDX).
    pub const AVAIL_F_NO_INTERRUPT: u16 = 1;
    /// Used ring `flags`: no notification wanted (without EVENT_IDX).
    pub const USED_F_NO_NOTIFY: u16 = 1;
    /// The areas' alignments.
    pub const DESC_ALIGN: u64 = 16;
    pub const AVAIL_ALIGN: u64 = 2;
    pub const USED_ALIGN: u64 = 4;
    /// The most entries a queue has (a 16-bit index's half, as 2.7 allows).
    pub const QUEUE_MAX: u64 = 32768;

    /// The descriptor table's bytes.
    pub const fn desc_bytes(size: u64) -> u64 {
        DESCRIPTOR_BYTES * size
    }

    /// The available ring's: `flags`, `idx`, `ring[size]`, `used_event`.
    pub const fn avail_bytes(size: u64) -> u64 {
        4 + 2 * size + 2
    }

    /// The used ring's: `flags`, `idx`, `ring[size]` of (id, len),
    /// `avail_event`.
    pub const fn used_bytes(size: u64) -> u64 {
        4 + 8 * size + 2
    }
}

/// The block device's own layout.
pub mod blk;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_are_distinct_words_below_the_configuration() {
        let registers: alloc::vec::Vec<u64> = mmio::LAYOUT
            .iter()
            .take_while(|(name, _)| *name != "CONFIG")
            .filter(|(name, _)| *name != "BYTES")
            .map(|&(_, value)| value)
            .collect();
        assert_eq!(registers.len(), 28);
        for (index, register) in registers.iter().enumerate() {
            assert_eq!(register % 4, 0);
            assert!(*register < mmio::CONFIG);
            assert!(!registers[..index].contains(register));
        }
        // As virtio_mmio.h numbers them.
        assert_eq!(mmio::QUEUE_READY, 0x44);
        assert_eq!(mmio::QUEUE_USED_HIGH, 0xa4);
        assert_eq!(mmio::CONFIG_GENERATION, 0xfc);
        assert_eq!(&mmio::MAGIC.to_le_bytes()[..4], b"virt");
        assert_eq!(&mmio::VENDOR.to_le_bytes()[..4], b"STLA");
    }

    #[test]
    fn the_ring_areas_have_the_specifications_sizes() {
        assert_eq!(ring::desc_bytes(256), 4096);
        assert_eq!(ring::avail_bytes(256), 518);
        assert_eq!(ring::used_bytes(256), 2054);
        // The descriptor layout is the one StelaNet's rings use.
        assert_eq!(ring::DESCRIPTOR_BYTES, crate::net::ring::DESCRIPTOR_BYTES);
        assert_eq!(ring::DESC_F_WRITE, crate::net::ring::DESC_F_WRITE);
        assert_eq!(features::TRANSPORT & features::REQUIRED, features::REQUIRED);
        assert_eq!(ids::RNG, 4);
    }

    #[test]
    fn the_network_device_numbers_its_bits_and_header_as_stelanet_does() {
        use crate::net::{features as stelanet, window};
        use net::features::*;
        for (virtio, stelanet) in [
            (CSUM, stelanet::CSUM),
            (GUEST_CSUM, stelanet::GUEST_CSUM),
            (MAC, stelanet::MAC),
            (GUEST_TSO4, stelanet::GUEST_TSO4),
            (GUEST_TSO6, stelanet::GUEST_TSO6),
            (HOST_TSO4, stelanet::HOST_TSO4),
            (HOST_TSO6, stelanet::HOST_TSO6),
            (MRG_RXBUF, stelanet::MRG_RXBUF),
            (super::features::EVENT_IDX, stelanet::EVENT_IDX),
            (super::features::VERSION_1, stelanet::VERSION_1),
        ] {
            assert_eq!(virtio, stelanet);
        }
        // virtio_net.h's numbering of what StelaNet has no bit for.
        assert_eq!((STATUS, CTRL_VQ), (1 << 16, 1 << 17));
        assert_eq!(net::HEADER_BYTES, window::HEADER_BYTES);
        assert_eq!(net::FRAME_MAX, window::FRAME_MAX);
        assert_eq!(net::HDR_NUM_BUFFERS + 2, net::HEADER_BYTES);
        assert_eq!(net::CONFIG_MAX_VIRTQUEUE_PAIRS + 2, net::CONFIG_BYTES);
        assert_eq!(OFFERED & super::features::TRANSPORT, 0);
    }
}
