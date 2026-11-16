// SPDX-License-Identifier: GPL-2.0-only
//! Copyright(c) 2008 Intel Corporation
//! Copyright (c) 2013 Anssi Hannula <anssi.hannula@iki.fi>
//! ELD header fields: Copyright © 2023 Intel Corporation (MIT).
//! See NOTICE for the MIT permission/warranty notice.
//! ELD/SAD constraints from Linux pcm_drm_eld.c and HDMI eld.c at
//! a804c5742bff224d4cb4d25dd383554d598be32b. See NOTICE.
use crate::{Error, PcmFormat};
use alloc::vec::Vec;
use core::fmt;

/// Which check of Linux snd_hdmi_get_eld stopped an ELD read through the
/// codec's command transport, with the values it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EldReadFault {
    /// The ELD size (get HDMI ELD data, parameter 8) is outside 20..=256
    /// bytes; a zero size is read as 128, as Linux does.
    Size { response: u32 },
    /// Byte `index`'s response has no ELD-valid bit 31: the graphics
    /// driver may be writing the ELD right now (Linux repolls).
    ByteNotValid { index: u8, response: u32 },
    /// Byte `index` was not answered: an all-ones response.
    ByteUnanswered { index: u8 },
    /// Byte 0, the ELD version byte, read zero.
    FirstByteZero { response: u32 },
}

impl fmt::Display for EldReadFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Size { response } => write!(
                f,
                "ELD size {response} ({response:#x}) is outside 20..=256 bytes"
            ),
            Self::ByteNotValid { index, response } => write!(
                f,
                "byte {index} response {response:#010x} has no ELD-valid bit 31 (the GPU may be \
                 writing the ELD)"
            ),
            Self::ByteUnanswered { index } => {
                write!(f, "byte {index} was not answered (response 0xffffffff)")
            }
            Self::FirstByteZero { response } => {
                write!(f, "byte 0 (ELD version) is zero: response {response:#010x}")
            }
        }
    }
}

/// Which check of the ELD's layout (Linux drm_eld / hda_eld) its bytes
/// failed, with the values it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EldParseFault {
    /// The ELD is not 20..=256 bytes.
    Length { bytes: u16 },
    /// ELD_Ver (byte 0 bits 7:3) is neither 2 (CEA-861-D) nor 31.
    Version { byte0: u8 },
    /// MNL (byte 4 bits 4:0), the monitor name's length, exceeds 16.
    MonitorNameLength { length: u8 },
    /// The baseline block (BLL, byte 2, in dwords after the 4-byte header)
    /// does not hold the 16 fixed bytes, the monitor name and the SADs, or
    /// passes the ELD's end.
    Baseline {
        baseline_dwords: u8,
        name_length: u8,
        sads: u8,
        bytes: u16,
    },
    /// Conn_Type (byte 5 bits 3:2) is neither 0 (HDMI) nor 1 (DP).
    Connection { conn_type: u8 },
    /// The ELD's own length, the 4-byte header and BLL (byte 2) dwords of
    /// baseline, passes the ELD buffer size the codec answered (get HDMI
    /// ELD data, parameter 8).
    BaselineBeyondBuffer { baseline_dwords: u8, buffer: u16 },
}

impl fmt::Display for EldParseFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Length { bytes } => write!(f, "{bytes} bytes, not 20..=256"),
            Self::Version { byte0 } => write!(
                f,
                "version {} (byte 0 {byte0:#04x}) is neither 2 nor 31",
                byte0 >> 3
            ),
            Self::MonitorNameLength { length } => {
                write!(f, "monitor name length {length} exceeds 16")
            }
            Self::Baseline {
                baseline_dwords,
                name_length,
                sads,
                bytes,
            } => write!(
                f,
                "baseline of {baseline_dwords} dwords ends at byte {}, but the monitor name \
                 ({name_length} bytes) and {sads} SADs end at byte {} of {bytes}",
                4 + u16::from(baseline_dwords) * 4,
                20 + u16::from(name_length) + 3 * u16::from(sads)
            ),
            Self::Connection { conn_type } => {
                write!(
                    f,
                    "connection type {conn_type} is neither 0 (HDMI) nor 1 (DP)"
                )
            }
            Self::BaselineBeyondBuffer {
                baseline_dwords,
                buffer,
            } => write!(
                f,
                "baseline of {baseline_dwords} dwords ends at byte {}, past the codec's ELD \
                 buffer of {buffer} bytes",
                eld_length(baseline_dwords)
            ),
        }
    }
}

/// The ELD's length from its header's BLL (byte 2): the 4-byte header and
/// that many dwords of baseline (Linux drm_eld_size). Vendor-specific
/// blocks after the baseline are not read.
pub const fn eld_length(baseline_dwords: u8) -> u16 {
    4 + 4 * baseline_dwords as u16
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Hdmi,
    DisplayPort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioDescriptor {
    pub coding_type: u8,
    pub channels: u8,
    pub rates: u8,
    /// LPCM SAD sample-size bits: 16, 20, 24 in bits 0, 1, 2.
    pub sample_bits: u8,
}

/// Codec ELD snapshot. Parsing bytes alone never grants a route, port,
/// sink identity or PCM capability; the controller reads and binds the snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eld {
    raw: Vec<u8>,
    pub version: u8,
    pub connection: Connection,
    pub port_id: u64,
    pub manufacturer: u16,
    pub product: u16,
    pub monitor_name: Vec<u8>,
    pub speakers: u8,
    pub audio_sync_delay_ms: u16,
    pub supports_hdcp: bool,
    pub supports_ai: bool,
    pub descriptors: Vec<AudioDescriptor>,
}

impl Eld {
    pub fn bytes(&self) -> &[u8] {
        &self.raw
    }
    pub fn parse(raw: &[u8]) -> Result<Self, Error> {
        let fault = |fault| Err(Error::EldParse(fault));
        if !(20..=256).contains(&raw.len()) {
            return fault(EldParseFault::Length {
                bytes: raw.len().min(usize::from(u16::MAX)) as u16,
            });
        }
        if !matches!(raw[0] >> 3, 2 | 31) {
            return fault(EldParseFault::Version { byte0: raw[0] });
        }
        let name = usize::from(raw[4] & 31);
        let count = usize::from(raw[5] >> 4);
        let baseline = 4 + usize::from(raw[2]) * 4;
        let end = 20 + name + 3 * count;
        if name > 16 {
            return fault(EldParseFault::MonitorNameLength { length: name as u8 });
        }
        if end > raw.len() || baseline < end || baseline > raw.len() {
            return fault(EldParseFault::Baseline {
                baseline_dwords: raw[2],
                name_length: name as u8,
                sads: count as u8,
                bytes: raw.len() as u16,
            });
        }
        let connection = match (raw[5] >> 2) & 3 {
            0 => Connection::Hdmi,
            1 => Connection::DisplayPort,
            conn_type => return fault(EldParseFault::Connection { conn_type }),
        };
        let descriptors = raw[20 + name..end]
            .as_chunks::<3>()
            .0
            .iter()
            .map(|sad| AudioDescriptor {
                coding_type: (sad[0] >> 3) & 15,
                channels: (sad[0] & 7) + 1,
                rates: sad[1] & 127,
                sample_bits: if (sad[0] >> 3) & 15 == 1 {
                    sad[2] & 7
                } else {
                    0
                },
            })
            .collect();
        Ok(Self {
            raw: raw.to_vec(),
            version: raw[0] >> 3,
            connection,
            port_id: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
            manufacturer: u16::from_le_bytes(raw[16..18].try_into().unwrap()),
            product: u16::from_le_bytes(raw[18..20].try_into().unwrap()),
            monitor_name: raw[20..20 + name].to_vec(),
            speakers: raw[7] & 127,
            audio_sync_delay_ms: u16::from(raw[6]) * 2,
            supports_hdcp: raw[5] & 1 != 0,
            supports_ai: raw[5] & 2 != 0,
            descriptors,
        })
    }
    /// Keep rate/channel/size restrictions from the SAME LPCM SAD. Compressed
    /// descriptors never widen PCM. Basic 2ch/16bit 32/44.1/48k is the donor's
    /// mandatory HDMI audio baseline, only after a real valid ELD was observed.
    pub fn supports(&self, format: PcmFormat) -> bool {
        if format.encode().is_err() || !matches!(format.channels, 2 | 4 | 6 | 8) {
            return false;
        }
        let rate = match format.rate {
            32000 => 0,
            44100 => 1,
            48000 => 2,
            88200 => 3,
            96000 => 4,
            176400 => 5,
            192000 => 6,
            _ => return false,
        };
        let bits = match format.bits {
            16 => 1,
            20 => 2,
            24 => 4,
            _ => return false,
        };
        (format.channels == 2 && bits == 1 && rate <= 2)
            || self.descriptors.iter().any(|sad| {
                sad.coding_type == 1
                    && sad.channels >= format.channels
                    && sad.rates & (1 << rate) != 0
                    && sad.sample_bits & bits != 0
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes() -> [u8; 32] {
        let mut b = [0; 32];
        b[0] = 0x10;
        b[2] = 7;
        b[5] = 0x20;
        b[7] = 0x4f;
        b[20] = 0x0f;
        b[21] = 0x40;
        b[22] = 4;
        b[23] = 0x09;
        b[24] = 4;
        b[25] = 1;
        b
    }
    #[test]
    fn sad_constraints_are_per_descriptor() {
        let eld = Eld::parse(&bytes()).unwrap();
        assert!(eld.supports(PcmFormat {
            rate: 192000,
            bits: 24,
            channels: 8
        }));
        assert!(!eld.supports(PcmFormat {
            rate: 48000,
            bits: 24,
            channels: 8
        }));
        assert!(eld.supports(PcmFormat {
            rate: 48000,
            bits: 16,
            channels: 2
        }));
        let mut b = bytes();
        b[20] = 0x17;
        let eld = Eld::parse(&b).unwrap();
        assert!(!eld.supports(PcmFormat {
            rate: 192000,
            bits: 24,
            channels: 8
        }));
    }
    #[test]
    fn malformed_lengths_versions_and_connection_are_rejected() {
        use EldParseFault::*;
        for index in 0..5 {
            let mut b = bytes();
            let expected = match index {
                0 => {
                    b[0] = 8;
                    Version { byte0: 8 }
                }
                1 => {
                    b[4] = 31;
                    MonitorNameLength { length: 31 }
                }
                2 => {
                    b[2] = 1;
                    Baseline {
                        baseline_dwords: 1,
                        name_length: 0,
                        sads: 2,
                        bytes: 32,
                    }
                }
                3 => {
                    b[5] = 0xf0;
                    Baseline {
                        baseline_dwords: 7,
                        name_length: 0,
                        sads: 15,
                        bytes: 32,
                    }
                }
                _ => {
                    b[5] |= 8;
                    Connection { conn_type: 2 }
                }
            };
            assert_eq!(Eld::parse(&b), Err(Error::EldParse(expected)));
        }
        assert_eq!(
            Eld::parse(&[0; 19]),
            Err(Error::EldParse(Length { bytes: 19 }))
        );
    }

    #[test]
    fn faults_name_their_check_and_values() {
        use alloc::string::ToString;
        assert_eq!(
            EldParseFault::Version { byte0: 0x08 }.to_string(),
            "version 1 (byte 0 0x08) is neither 2 nor 31"
        );
        assert_eq!(
            EldParseFault::Baseline {
                baseline_dwords: 1,
                name_length: 0,
                sads: 2,
                bytes: 32
            }
            .to_string(),
            "baseline of 1 dwords ends at byte 8, but the monitor name (0 bytes) and 2 SADs end \
             at byte 26 of 32"
        );
        assert_eq!(
            EldReadFault::ByteNotValid {
                index: 12,
                response: 0x41
            }
            .to_string(),
            "byte 12 response 0x00000041 has no ELD-valid bit 31 (the GPU may be writing the ELD)"
        );
        assert_eq!(
            EldReadFault::Size { response: 300 }.to_string(),
            "ELD size 300 (0x12c) is outside 20..=256 bytes"
        );
        assert_eq!(
            EldParseFault::BaselineBeyondBuffer {
                baseline_dwords: 8,
                buffer: 32
            }
            .to_string(),
            "baseline of 8 dwords ends at byte 36, past the codec's ELD buffer of 32 bytes"
        );
        assert_eq!(eld_length(8), 36);
    }
}
