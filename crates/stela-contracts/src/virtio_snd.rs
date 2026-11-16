//! The virtio sound device (virtio 1.2, 5.14; Linux's
//! `include/uapi/linux/virtio_snd.h`), as Stela's model
//! (`stela_soc::virtio::snd`) serves it and the StelaVirtIO kext's sound
//! driver drives it. The kext's `stela/virtio_snd.h` is generated from this
//! module.
//!
//! Every message is little-endian. A control request is one readable
//! request and one writable response, which starts with a status code; a
//! PCM transfer on the TX queue is a readable `virtio_snd_pcm_xfer` header,
//! the frames, and a writable `virtio_snd_pcm_status`.

/// Defines a block of 64-bit layout constants and `LAYOUT` listing them,
/// from which the kext's header is generated.
macro_rules! layout {
    ($($(#[doc = $doc:literal])* $name:ident = $value:expr;)*) => {
        $($(#[doc = $doc])* pub const $name: u64 = $value;)*
        /// Every constant above, by name.
        pub const LAYOUT: &[(&str, u64)] = &[$((stringify!($name), $name)),*];
    };
}

/// The queues, the configuration and the messages' codes.
pub mod codes {
    layout! {
        /// The queues, by index.
        VQ_CONTROL = 0;
        VQ_EVENT = 1;
        VQ_TX = 2;
        VQ_RX = 3;

        /// `virtio_snd_config`: jacks, streams and channel maps, each a
        /// 32-bit count.
        CONFIG_JACKS = 0;
        CONFIG_STREAMS = 4;
        CONFIG_CHMAPS = 8;
        CONFIG_BYTES = 12;

        /// Directions.
        D_OUTPUT = 0;
        D_INPUT = 1;

        /// Control requests.
        R_JACK_INFO = 1;
        R_JACK_REMAP = 2;
        R_PCM_INFO = 0x100;
        R_PCM_SET_PARAMS = 0x101;
        R_PCM_PREPARE = 0x102;
        R_PCM_RELEASE = 0x103;
        R_PCM_START = 0x104;
        R_PCM_STOP = 0x105;
        R_CHMAP_INFO = 0x200;

        /// Events.
        EVT_JACK_CONNECTED = 0x1000;
        EVT_JACK_DISCONNECTED = 0x1001;
        EVT_PCM_PERIOD_ELAPSED = 0x1100;
        EVT_PCM_XRUN = 0x1101;

        /// Status codes.
        S_OK = 0x8000;
        S_BAD_MSG = 0x8001;
        S_NOT_SUPP = 0x8002;
        S_IO_ERR = 0x8003;

        /// Sample formats, by bit of `virtio_snd_pcm_info::formats`.
        FMT_S16 = 5;
        FMT_S32 = 17;
        /// Frame rates, by bit of `virtio_snd_pcm_info::rates`.
        RATE_44100 = 6;
        RATE_48000 = 7;

        /// Channel positions.
        CHMAP_FL = 3;
        CHMAP_FR = 4;
        /// The positions a channel map holds.
        CHMAP_MAX_SIZE = 18;
    }
}

/// The messages' sizes and fields' offsets.
pub mod messages {
    layout! {
        /// `virtio_snd_hdr`: `code`.
        HDR_BYTES = 4;
        /// `virtio_snd_query_info`: `hdr`, `start_id`, `count`, `size`.
        QUERY_INFO_BYTES = 16;
        QUERY_START_ID = 4;
        QUERY_COUNT = 8;
        QUERY_SIZE = 12;
        /// `virtio_snd_pcm_hdr`: `hdr`, `stream_id`.
        PCM_HDR_BYTES = 8;
        PCM_STREAM_ID = 4;
        /// `virtio_snd_pcm_set_params`: `hdr` (a PCM header),
        /// `buffer_bytes`, `period_bytes`, `features`, `channels`, `format`,
        /// `rate`, `padding`.
        SET_PARAMS_BYTES = 24;
        SET_PARAMS_BUFFER_BYTES = 8;
        SET_PARAMS_PERIOD_BYTES = 12;
        SET_PARAMS_FEATURES = 16;
        SET_PARAMS_CHANNELS = 20;
        SET_PARAMS_FORMAT = 21;
        SET_PARAMS_RATE = 22;
        /// `virtio_snd_jack_info`: `hdr.hda_fn_nid`, `features`,
        /// `hda_reg_defconf`, `hda_reg_caps`, `connected`, `padding[7]`.
        JACK_INFO_BYTES = 24;
        /// `virtio_snd_pcm_info`: `hdr.hda_fn_nid`, `features`, `formats`
        /// (64-bit), `rates` (64-bit), `direction`, `channels_min`,
        /// `channels_max`, `padding[5]`.
        PCM_INFO_BYTES = 32;
        PCM_INFO_FEATURES = 4;
        PCM_INFO_FORMATS = 8;
        PCM_INFO_RATES = 16;
        PCM_INFO_DIRECTION = 24;
        PCM_INFO_CHANNELS_MIN = 25;
        PCM_INFO_CHANNELS_MAX = 26;
        /// `virtio_snd_chmap_info`: `hdr.hda_fn_nid`, `direction`,
        /// `channels`, `positions[18]`.
        CHMAP_INFO_BYTES = 24;
        CHMAP_INFO_DIRECTION = 4;
        CHMAP_INFO_CHANNELS = 5;
        CHMAP_INFO_POSITIONS = 6;
        /// `virtio_snd_pcm_xfer`: `stream_id`.
        PCM_XFER_BYTES = 4;
        /// `virtio_snd_pcm_status`: `status`, `latency_bytes`.
        PCM_STATUS_BYTES = 8;
        /// `virtio_snd_event`: `hdr`, `data`.
        EVENT_BYTES = 8;
    }
}

/// A sample format's bytes per sample, for the formats offered.
pub const fn sample_bytes(format: u8) -> Option<u64> {
    match format as u64 {
        codes::FMT_S16 => Some(2),
        codes::FMT_S32 => Some(4),
        _ => None,
    }
}

/// A rate code's frames a second, for the rates offered.
pub const fn rate_hz(rate: u8) -> Option<u64> {
    match rate as u64 {
        codes::RATE_44100 => Some(44_100),
        codes::RATE_48000 => Some(48_000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_virtio_snd_hs() {
        assert_eq!(codes::R_PCM_STOP, 0x105);
        assert_eq!(codes::S_IO_ERR, 0x8003);
        assert_eq!(codes::FMT_S16, 5);
        assert_eq!(codes::FMT_S32, 17);
        assert_eq!(codes::RATE_48000, 7);
        assert_eq!(sample_bytes(5), Some(2));
        assert_eq!(sample_bytes(6), None);
        assert_eq!(rate_hz(7), Some(48_000));
        assert_eq!(rate_hz(6), Some(44_100));
        assert_eq!(rate_hz(8), None);
        // The info structures each fill their size.
        assert_eq!(
            messages::CHMAP_INFO_POSITIONS + codes::CHMAP_MAX_SIZE,
            messages::CHMAP_INFO_BYTES
        );
        assert_eq!(
            messages::PCM_INFO_CHANNELS_MAX + 1 + 5,
            messages::PCM_INFO_BYTES
        );
        assert_eq!(messages::SET_PARAMS_RATE + 2, messages::SET_PARAMS_BYTES);
    }
}
