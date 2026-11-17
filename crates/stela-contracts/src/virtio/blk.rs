//! The virtio block device (virtio 1.2, 5.2; Linux's
//! `include/uapi/linux/virtio_blk.h`), as `stela_soc::virtio::blk` models it
//! and the StelaVirtIO kext's StelaVirtIOBlock drives it: one request queue,
//! each request a chain of a 16-byte header the device reads, the data, and
//! one status byte the device writes last. Sectors are 512 bytes whatever
//! the device's block size. Generated into the kext's `stela/virtio_blk.h`.

/// Feature bits (5.2.3).
pub mod features {
    layout! {
        /// `size_max` is the most bytes one data buffer has.
        SIZE_MAX = 1 << 1;
        /// `seg_max` is the most data buffers one request has.
        SEG_MAX = 1 << 2;
        /// The device is read-only: writes fail with `IOERR`.
        RO = 1 << 5;
        /// `blk_size` is the device's block size, which requests align to.
        BLK_SIZE = 1 << 6;
        /// The device takes `FLUSH`: a completed write is durable only after it.
        FLUSH = 1 << 9;
        /// Discards and write-zeroes: never offered.
        DISCARD = 1 << 13;
        WRITE_ZEROES = 1 << 14;
    }
}

/// The configuration space (`struct virtio_blk_config`), byte offsets from
/// the transport's `CONFIG`.
pub mod config {
    layout! {
        /// u64: the capacity in 512-byte sectors.
        CAPACITY = 0;
        /// u32: with `SIZE_MAX`.
        SIZE_MAX = 8;
        /// u32: with `SEG_MAX`.
        SEG_MAX = 12;
        /// u32: with `BLK_SIZE`.
        BLK_SIZE = 20;
        /// The structure's bytes up to and through the write-zeroes fields;
        /// the device answers zeros for every field it does not offer.
        BYTES = 60;
    }
}

/// Requests (5.2.6): the header's `type`, and the status byte.
pub mod request {
    layout! {
        /// The header: le32 type, le32 reserved, le64 sector.
        HEADER_BYTES = 16;
        /// Bytes per sector, whatever the block size.
        SECTOR_BYTES = 512;
        /// The bytes of a `GET_ID` answer: the serial, NUL-padded.
        ID_BYTES = 20;

        /// Read from the device into the driver's buffers.
        T_IN = 0;
        /// Write the driver's buffers to the device.
        T_OUT = 1;
        T_FLUSH = 4;
        T_GET_ID = 8;
        T_DISCARD = 11;
        T_WRITE_ZEROES = 13;

        S_OK = 0;
        S_IOERR = 1;
        S_UNSUPP = 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As Linux's `virtio_blk.h` numbers them.
    #[test]
    fn the_constants_are_virtio_blk_hs() {
        assert_eq!(features::FLUSH, 1 << 9);
        assert_eq!(features::BLK_SIZE, 1 << 6);
        assert_eq!(config::BLK_SIZE, 20);
        assert_eq!(request::T_GET_ID, 8);
        assert_eq!(request::S_UNSUPP, 2);
        // None of the device's bits collides with the transport's.
        let own = features::LAYOUT.iter().fold(0, |bits, (_, bit)| bits | bit);
        assert_eq!(own & super::super::features::TRANSPORT, 0);
        assert!(own < 1 << 24);
    }
}
