//! StelaNet: the paravirtual network adapter at `/arm-io/stela-net0`, as the
//! runtime models it (`stela_soc::stela_net`) and the StelaNet front end of
//! the StelaGPU kext drives it.
//!
//! The device is one 16 KiB register page and one AIC input. Frames move
//! through two virtio 1.2 split virtqueues in guest RAM, receive and
//! transmit, which the kext allocates physically contiguous and names by
//! guest physical address: each holds its descriptor table, then its
//! available ring, then its used ring at [`ring::used`]. Every buffer starts
//! with a [`HEADER_BYTES`] `virtio_net_hdr_v1`. Notifications follow
//! `VIRTIO_F_EVENT_IDX`: the driver names in each available ring's
//! `used_event` when it wants an interrupt, and the device in each used
//! ring's `avail_event` when it wants a `KICK`.
//!
//! Every register is 64-bit.

/// Defines a block of 64-bit layout constants and `LAYOUT` listing them,
/// from which the kext's header is generated.
macro_rules! layout {
    ($($(#[doc = $doc:literal])* $name:ident = $value:expr;)*) => {
        $($(#[doc = $doc])* pub const $name: u64 = $value;)*
        /// Every constant above, by name.
        pub const LAYOUT: &[(&str, u64)] = &[$((stringify!($name), $name)),*];
    };
}

/// The register page, its values and the virtqueue limits.
pub mod window {
    layout! {
        /// The register page: the guest's page, so only the kernel maps it.
        BYTES = 0x4000;

        /// R: [`MAGIC`] in the high word, the layout version in the low.
        VERSION = 0x00;
        /// R: the device's feature bits, in virtio-net numbering.
        FEATURES = 0x08;
        /// RW: the features the driver uses, a subset of `FEATURES`, written
        /// before `ENABLE`.
        ACCEPT = 0x10;
        /// R: the station address, its first byte in bits 0-7.
        MAC = 0x18;
        /// R: bit 0 the link is up; bits 32-63 its speed in Mb/s.
        LINK = 0x20;
        /// RW: reserved for a control page (counters); 0 for none.
        CONTROL = 0x28;
        /// RW: guest physical address of the receive virtqueue.
        RX_RING = 0x30;
        /// RW: entries of the receive virtqueue, a power of two up to
        /// [`QUEUE_MAX`].
        RX_SIZE = 0x38;
        /// RW: guest physical address of the transmit virtqueue.
        TX_RING = 0x40;
        /// RW: entries of the transmit virtqueue.
        TX_SIZE = 0x48;
        /// RW: 1 starts the device on the rings named; 0 stops it, and once
        /// that store retires the device reaches no guest memory. Reads 0
        /// after the device stopped itself on an [`CAUSE_ERROR`].
        ENABLE = 0x50;
        /// W: [`KICK_TX`] frames are published, [`KICK_RX`] buffers posted.
        KICK = 0x58;
        /// R: the causes raised since the last `ACK`, `CAUSE_` bits.
        CAUSE = 0x60;
        /// W: clears the causes written and, with none left, lowers the line.
        ACK = 0x68;
        /// RW: the receive filter, `FILTER_` bits.
        FILTER = 0x70;
        /// R: frames the device gave the guest.
        RX_FRAMES = 0x78;
        /// R: frames the guest sent through the device.
        TX_FRAMES = 0x80;
        /// R: frames from the host dropped: no buffer, filtered or too
        /// large for the buffer posted.
        RX_DROPS = 0x88;
        /// R: transmit chains refused as malformed.
        TX_ERRORS = 0x90;

        /// `VERSION`'s high word: "SNET".
        MAGIC = 0x534e_4554;
        /// `VERSION`'s low word.
        LAYOUT_VERSION = 1;

        /// `KICK`: the transmit ring has new chains.
        KICK_TX = 1 << 0;
        /// `KICK`: the receive ring has new buffers.
        KICK_RX = 1 << 1;

        /// `CAUSE`: frames reached the receive ring.
        CAUSE_RX = 1 << 0;
        /// `CAUSE`: transmit chains completed (only when asked for).
        CAUSE_TX = 1 << 1;
        /// `CAUSE`: the link changed.
        CAUSE_LINK = 1 << 2;
        /// `CAUSE`: a ring or buffer was unusable; the device stopped.
        CAUSE_ERROR = 1 << 3;

        /// `FILTER`: frames to the station address.
        FILTER_DIRECTED = 1 << 0;
        /// `FILTER`: broadcast frames.
        FILTER_BROADCAST = 1 << 1;
        /// `FILTER`: every multicast frame.
        FILTER_ALL_MULTICAST = 1 << 2;
        /// `FILTER`: every frame.
        FILTER_PROMISCUOUS = 1 << 3;

        /// The most entries a virtqueue has.
        QUEUE_MAX = 1024;
        /// The `virtio_net_hdr_v1` every buffer starts with.
        HEADER_BYTES = 12;
        /// The longest Ethernet frame without FCS, a header excluded.
        FRAME_MAX = 1514;
        /// The shortest: an Ethernet header.
        FRAME_MIN = 14;
        /// The speed `LINK` reports, in Mb/s.
        LINK_MBPS = 1000;
    }
}

/// Feature bits, as virtio-net (5.1.3) and virtio (6) number them.
pub mod features {
    /// The device checksums partial transmit checksums.
    pub const CSUM: u64 = 1 << 0;
    /// The driver accepts receive frames with partial checksums.
    pub const GUEST_CSUM: u64 = 1 << 1;
    /// The driver takes coalesced TCPv4 receive segments.
    pub const GUEST_TSO4: u64 = 1 << 7;
    /// The driver takes coalesced TCPv6 receive segments.
    pub const GUEST_TSO6: u64 = 1 << 8;
    /// The device segments TCPv4.
    pub const HOST_TSO4: u64 = 1 << 11;
    /// The device segments TCPv6.
    pub const HOST_TSO6: u64 = 1 << 12;
    /// Receive frames may span buffers (`num_buffers`).
    pub const MRG_RXBUF: u64 = 1 << 15;
    /// `MAC` holds the station address.
    pub const MAC: u64 = 1 << 5;
    /// `used_event` and `avail_event` suppress notifications.
    pub const EVENT_IDX: u64 = 1 << 29;
    /// Virtio 1 semantics: a 12-byte header, little-endian rings.
    pub const VERSION_1: u64 = 1 << 32;

    /// What this device offers.
    pub const OFFERED: u64 = MAC | EVENT_IDX | VERSION_1;
    /// What a driver must accept.
    pub const REQUIRED: u64 = VERSION_1;
}

/// A split virtqueue's layout in its one physically contiguous block.
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
    /// Used ring `flags`: no kick wanted (without EVENT_IDX).
    pub const USED_F_NO_NOTIFY: u16 = 1;
    /// The used ring's alignment in the block.
    pub const USED_ALIGN: u64 = 64;

    /// Offset of the available ring: `flags`, `idx`, `ring[size]`,
    /// `used_event`.
    pub const fn avail(size: u64) -> u64 {
        DESCRIPTOR_BYTES * size
    }

    /// Offset of the used ring: `flags`, `idx`, `ring[size]` of (id, len),
    /// `avail_event`.
    pub const fn used(size: u64) -> u64 {
        let end = avail(size) + 4 + 2 * size + 2;
        end.div_ceil(USED_ALIGN) * USED_ALIGN
    }

    /// The block's bytes.
    pub const fn bytes(size: u64) -> u64 {
        used(size) + 4 + 8 * size + 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_are_distinct_and_in_the_page() {
        use window::*;
        let registers = [
            VERSION, FEATURES, ACCEPT, MAC, LINK, CONTROL, RX_RING, RX_SIZE, TX_RING, TX_SIZE,
            ENABLE, KICK, CAUSE, ACK, FILTER, RX_FRAMES, TX_FRAMES, RX_DROPS, TX_ERRORS,
        ];
        for (index, register) in registers.iter().enumerate() {
            assert_eq!(register % 8, 0);
            assert!(*register < BYTES);
            assert!(!registers[..index].contains(register));
        }
    }

    #[test]
    fn a_ring_block_holds_descriptors_avail_and_aligned_used() {
        assert_eq!(ring::avail(256), 4096);
        // 4096 + 4 + 512 + 2 = 4614, aligned up to 4672.
        assert_eq!(ring::used(256), 4672);
        assert_eq!(ring::bytes(256), 4672 + 4 + 2048 + 2);
        assert!(ring::bytes(window::QUEUE_MAX) <= 0x8000);
        assert!(features::OFFERED & features::REQUIRED == features::REQUIRED);
    }
}
