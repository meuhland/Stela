//! Picture setups (`nvenc_h264_drv_pic_setup_s`, `nvenc_h265_drv_pic_setup_s`),
//! built from the ones NVIDIA's driver was captured writing.
//!
//! The IDR starts from NVIDIA's frame 0 setup and every P frame from its
//! frame 1 setup (`{h264,hevc}_nvenc/stop6`, `nvenc-capture.md`), trimmed of
//! their zero tails in `fixtures/`. As nvenc-test does, only what NVIDIA's
//! driver itself changes from frame to frame is patched per frame:
//!
//! - H.264: `frame_num` and `pic_order_cnt_lsb` (k and 2k for the k-th
//!   picture since the IDR, POC type 2), `l0[]` (twice the DPB slot of each
//!   reference, newest first; frame m sits in slot m % 4), `temp_dist_l0[]`,
//!   `dist_scale_factor`, `diff_pic_order_cnt_zero[]`, the slice's
//!   reference count and its override flag, and `idr_pic_id` for IDRs after
//!   the first.
//! - HEVC: `pic_order_cnt_lsb` (k), `l0[0]` (twice the reconstruction slot of
//!   the previous frame, (n - 1) % 5) and `short_term_rps.num_negative_pics`
//!   (min(k, 4)).
//!
//! Once per session ([`Templates::new`]) both templates get the session's
//! size, which equals the captured 1920x1080 field for field, and its rate
//! control: the frame rate, the bitrate (NVIDIA's H.264 setups drift it from
//! frame to frame; the session's own is kept) or a constant QP.
use super::fields::{h264 as f4, hevc as f5};
use super::layout::{Geometry, Layout};
use super::{Codec, Config, Rate};
use alloc::vec;
use alloc::vec::Vec;

/// One bitfield of a setup or status: a byte offset and length, the shift of
/// its lowest bit and its width in bits, little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    offset: u16,
    bytes: u8,
    shift: u8,
    bits: u8,
}

const fn mask(bits: u8) -> u64 {
    if bits >= 64 { !0 } else { (1 << bits) - 1 }
}

impl Field {
    pub const fn new(offset: u16, bytes: u8, shift: u8, bits: u8) -> Self {
        Self {
            offset,
            bytes,
            shift,
            bits,
        }
    }

    fn raw(self, buffer: &[u8]) -> u64 {
        let start = usize::from(self.offset);
        buffer[start..start + usize::from(self.bytes)]
            .iter()
            .rev()
            .fold(0, |raw, byte| raw << 8 | u64::from(*byte))
    }

    pub fn get(self, buffer: &[u8]) -> u64 {
        (self.raw(buffer) >> self.shift) & mask(self.bits)
    }

    /// The field as a two's-complement number of its width.
    pub fn signed(self, buffer: &[u8]) -> i64 {
        let value = self.get(buffer);
        let unused = 64 - u32::from(self.bits);
        ((value << unused) as i64) >> unused
    }

    pub fn put(self, buffer: &mut [u8], value: u64) {
        let field = mask(self.bits) << self.shift;
        let raw = (self.raw(buffer) & !field) | ((value << self.shift) & field);
        let start = usize::from(self.offset);
        for (index, byte) in buffer[start..start + usize::from(self.bytes)]
            .iter_mut()
            .enumerate()
        {
            *byte = (raw >> (8 * index)) as u8;
        }
    }
}

/// `NV_NVENC_7_3`'s driver magic, every setup's first word.
pub const MAGIC: u64 = 0xc7b7_0006;
/// The captured H.264 IDR level, 4.2, and the bound of macroblocks it allows.
const H264_LEVEL_4_2_MBS: u32 = 8704;
/// Level 5.2, for a larger picture.
const H264_LEVEL_5_2: u64 = 52;

fn captured(codec: Codec, idr: bool) -> &'static [u8] {
    match (codec, idr) {
        (Codec::H264, true) => include_bytes!("fixtures/h264-idr-setup.bin"),
        (Codec::H264, false) => include_bytes!("fixtures/h264-p-setup.bin"),
        (Codec::Hevc, true) => include_bytes!("fixtures/hevc-idr-setup.bin"),
        (Codec::Hevc, false) => include_bytes!("fixtures/hevc-p-setup.bin"),
    }
}

/// NVIDIA's captured setup, whole: its zero tail put back.
pub fn template(codec: Codec, idr: bool) -> Vec<u8> {
    let mut setup = vec![0; codec.setup_bytes()];
    let captured = captured(codec, idr);
    setup[..captured.len()].copy_from_slice(captured);
    setup
}

/// Whether `setup` is laid out as the captured ones are: the magic, and
/// the control structures at their offsets.
pub fn layout_ok(codec: Codec, setup: &[u8]) -> bool {
    match codec {
        Codec::H264 => {
            f4::MAGIC.get(setup) == MAGIC
                && f4::PIC_CONTROL_ME_CONTROL_OFFSET.get(setup) == 0x300
                && f4::PIC_CONTROL_MD_CONTROL_OFFSET.get(setup) == 0x400
                && f4::PIC_CONTROL_Q_CONTROL_OFFSET.get(setup) == 0x500
                && f4::PIC_CONTROL_SLICE_CONTROL_OFFSET.get(setup) == 0x600
        }
        Codec::Hevc => {
            f5::MAGIC.get(setup) == MAGIC
                && f5::PIC_CONTROL_ME_CONTROL_OFFSET.get(setup) == 0x400
                && f5::PIC_CONTROL_MD_CONTROL_OFFSET.get(setup) == 0x3400
                && f5::PIC_CONTROL_Q_CONTROL_OFFSET.get(setup) == 0x5400
                && f5::PIC_CONTROL_SLICE_CONTROL_OFFSET.get(setup) == 0x5500
                && f5::PIC_CONTROL_WP_CONTROL_OFFSET.get(setup) == 0x6500
        }
    }
}

/// A session's IDR and P templates, its size and rate control applied.
#[derive(Clone, Debug)]
pub struct Templates {
    pub codec: Codec,
    pub idr: Vec<u8>,
    pub p: Vec<u8>,
    /// The most L0 references a P frame takes: the P template's PPS default
    /// (3 for H.264, 1 for HEVC).
    pub max_refs: u32,
}

/// The fields of one `nvenc_*_surface_cfg_s` the session's size sets.
struct SurfaceCfg {
    width_minus1: Field,
    height_minus1: Field,
    pitch: Field,
    pitch_chroma: Field,
    chroma_top: Field,
    luma_bottom: Field,
    chroma_bottom: Field,
}

/// The reference, input and output configurations of each codec's setup.
const H264_SURFACES: [SurfaceCfg; 3] = [
    SurfaceCfg {
        width_minus1: f4::REFPIC_CFG_FRAME_WIDTH_MINUS1,
        height_minus1: f4::REFPIC_CFG_FRAME_HEIGHT_MINUS1,
        pitch: f4::REFPIC_CFG_SFC_PITCH,
        pitch_chroma: f4::REFPIC_CFG_SFC_PITCH_CHROMA,
        chroma_top: f4::REFPIC_CFG_CHROMA_TOP_FRM_OFFSET,
        luma_bottom: f4::REFPIC_CFG_LUMA_BOT_OFFSET,
        chroma_bottom: f4::REFPIC_CFG_CHROMA_BOT_OFFSET,
    },
    SurfaceCfg {
        width_minus1: f4::INPUT_CFG_FRAME_WIDTH_MINUS1,
        height_minus1: f4::INPUT_CFG_FRAME_HEIGHT_MINUS1,
        pitch: f4::INPUT_CFG_SFC_PITCH,
        pitch_chroma: f4::INPUT_CFG_SFC_PITCH_CHROMA,
        chroma_top: f4::INPUT_CFG_CHROMA_TOP_FRM_OFFSET,
        luma_bottom: f4::INPUT_CFG_LUMA_BOT_OFFSET,
        chroma_bottom: f4::INPUT_CFG_CHROMA_BOT_OFFSET,
    },
    SurfaceCfg {
        width_minus1: f4::OUTPUTPIC_CFG_FRAME_WIDTH_MINUS1,
        height_minus1: f4::OUTPUTPIC_CFG_FRAME_HEIGHT_MINUS1,
        pitch: f4::OUTPUTPIC_CFG_SFC_PITCH,
        pitch_chroma: f4::OUTPUTPIC_CFG_SFC_PITCH_CHROMA,
        chroma_top: f4::OUTPUTPIC_CFG_CHROMA_TOP_FRM_OFFSET,
        luma_bottom: f4::OUTPUTPIC_CFG_LUMA_BOT_OFFSET,
        chroma_bottom: f4::OUTPUTPIC_CFG_CHROMA_BOT_OFFSET,
    },
];

const HEVC_SURFACES: [SurfaceCfg; 3] = [
    SurfaceCfg {
        width_minus1: f5::REFPIC_CFG_FRAME_WIDTH_MINUS1,
        height_minus1: f5::REFPIC_CFG_FRAME_HEIGHT_MINUS1,
        pitch: f5::REFPIC_CFG_SFC_PITCH,
        pitch_chroma: f5::REFPIC_CFG_SFC_PITCH_CHROMA,
        chroma_top: f5::REFPIC_CFG_CHROMA_TOP_FRM_OFFSET,
        luma_bottom: f5::REFPIC_CFG_LUMA_BOT_OFFSET,
        chroma_bottom: f5::REFPIC_CFG_CHROMA_BOT_OFFSET,
    },
    SurfaceCfg {
        width_minus1: f5::INPUT_CFG_FRAME_WIDTH_MINUS1,
        height_minus1: f5::INPUT_CFG_FRAME_HEIGHT_MINUS1,
        pitch: f5::INPUT_CFG_SFC_PITCH,
        pitch_chroma: f5::INPUT_CFG_SFC_PITCH_CHROMA,
        chroma_top: f5::INPUT_CFG_CHROMA_TOP_FRM_OFFSET,
        luma_bottom: f5::INPUT_CFG_LUMA_BOT_OFFSET,
        chroma_bottom: f5::INPUT_CFG_CHROMA_BOT_OFFSET,
    },
    SurfaceCfg {
        width_minus1: f5::OUTPUTPIC_CFG_FRAME_WIDTH_MINUS1,
        height_minus1: f5::OUTPUTPIC_CFG_FRAME_HEIGHT_MINUS1,
        pitch: f5::OUTPUTPIC_CFG_SFC_PITCH,
        pitch_chroma: f5::OUTPUTPIC_CFG_SFC_PITCH_CHROMA,
        chroma_top: f5::OUTPUTPIC_CFG_CHROMA_TOP_FRM_OFFSET,
        luma_bottom: f5::OUTPUTPIC_CFG_LUMA_BOT_OFFSET,
        chroma_bottom: f5::OUTPUTPIC_CFG_CHROMA_BOT_OFFSET,
    },
];

impl Templates {
    pub fn new(
        config: &Config,
        geometry: &Geometry,
        layout: &Layout,
    ) -> Result<Self, &'static str> {
        let codec = config.codec;
        let mut idr = template(codec, true);
        let mut p = template(codec, false);
        if !layout_ok(codec, &idr) || !layout_ok(codec, &p) {
            return Err("the captured setups are not laid out as nvenc_drv.h says");
        }
        // NVIDIA's IDR bitrate, which its P setups drift away from.
        let captured_bitrate = match codec {
            Codec::H264 => f4::RATE_CONTROL_NAL_BITRATE.get(&idr),
            Codec::Hevc => f5::RATE_CONTROL_NAL_BITRATE.get(&idr),
        };
        for setup in [&mut idr, &mut p] {
            size(codec, setup, geometry, layout);
            rate(codec, setup, config, captured_bitrate);
        }
        let max_refs = match codec {
            Codec::H264 => f4::PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1.get(&p),
            Codec::Hevc => f5::PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1.get(&p),
        } as u32
            + 1;
        Ok(Self {
            codec,
            idr,
            p,
            max_refs,
        })
    }
}

/// The geometry-dependent fields: each surface configuration, the history
/// and bitstream buffer sizes, the picture's macroblocks or CTBs and, for
/// a larger picture than level 4.2 allows, the H.264 level.
fn size(codec: Codec, setup: &mut [u8], geometry: &Geometry, layout: &Layout) {
    let luma = geometry.luma_bytes();
    let chroma_top = luma / 256;
    let surfaces = match codec {
        Codec::H264 => &H264_SURFACES,
        Codec::Hevc => &HEVC_SURFACES,
    };
    for cfg in surfaces {
        cfg.width_minus1
            .put(setup, u64::from(geometry.coded_width - 1));
        cfg.height_minus1
            .put(setup, u64::from(geometry.coded_height - 1));
        cfg.pitch.put(setup, u64::from(geometry.pitch));
        cfg.pitch_chroma.put(setup, u64::from(geometry.pitch));
        cfg.chroma_top.put(setup, chroma_top);
        // The field offsets HEVC's configurations carry (frame pictures do
        // not use them): half the luma, and half the chroma past its top.
        if cfg.luma_bottom.get(setup) != 0 {
            cfg.luma_bottom.put(setup, luma / 2 / 256);
        }
        if cfg.chroma_bottom.get(setup) != 0 {
            cfg.chroma_bottom.put(setup, chroma_top + luma / 4 / 256);
        }
    }
    let history = u64::from(geometry.mb_width()) * 384;
    let bitstream = layout.bitstream_bytes();
    match codec {
        Codec::H264 => {
            f4::PIC_CONTROL_HIST_BUF_SIZE.put(setup, history);
            f4::PIC_CONTROL_BITSTREAM_BUF_SIZE.put(setup, bitstream);
            f4::SLICE_NUM_MB.put(setup, u64::from(geometry.macroblocks()));
            if geometry.macroblocks() > H264_LEVEL_4_2_MBS {
                f4::SPS_DATA_LEVEL_IDC.put(setup, H264_LEVEL_5_2);
            }
        }
        Codec::Hevc => {
            f5::PIC_CONTROL_HIST_BUF_SIZE.put(setup, history);
            f5::PIC_CONTROL_BITSTREAM_BUF_SIZE.put(setup, bitstream);
            let ctb = 8u32 << f5::SPS_DATA_MAX_CU_SIZE.get(setup);
            let ctbs = geometry.coded_width.div_ceil(ctb) * geometry.coded_height.div_ceil(ctb);
            f5::SLICE_NUM_CTUS.put(setup, u64::from(ctbs));
            f5::PIC_CONTROL_CEIL_LOG2_PIC_SIZE_IN_CTBS
                .put(setup, u64::from(ctbs.next_power_of_two().trailing_zeros()));
        }
    }
}

/// The session's rate control: its frame rate, and its bitrate or QP.
fn rate(codec: Codec, setup: &mut [u8], config: &Config, captured_bitrate: u64) {
    let (framerate, qp, slice_qp, bitrate, vcl_bitrate, cpbs) = match codec {
        Codec::H264 => (
            f4::RATE_CONTROL_FRAMERATE,
            f4::RATE_CONTROL_QP,
            f4::SLICE_QP_AVR,
            f4::RATE_CONTROL_NAL_BITRATE,
            f4::RATE_CONTROL_VCL_BITRATE,
            [
                f4::RATE_CONTROL_NAL_CPB_SIZE,
                f4::RATE_CONTROL_VCL_CPB_SIZE,
                f4::RATE_CONTROL_BUFFERSIZE,
            ],
        ),
        Codec::Hevc => (
            f5::RATE_CONTROL_FRAMERATE,
            f5::RATE_CONTROL_QP,
            f5::SLICE_QP_AVR,
            f5::RATE_CONTROL_NAL_BITRATE,
            f5::RATE_CONTROL_VCL_BITRATE,
            [
                f5::RATE_CONTROL_NAL_CPB_SIZE,
                f5::RATE_CONTROL_VCL_CPB_SIZE,
                f5::RATE_CONTROL_BUFFERSIZE,
            ],
        ),
    };
    framerate.put(setup, u64::from(config.fps) << 8);
    match config.rate {
        Rate::ConstQp(value) => {
            bitrate.put(setup, captured_bitrate);
            for field in qp {
                field.put(setup, u64::from(value));
            }
            slice_qp.put(setup, u64::from(value));
        }
        Rate::Cbr { bitrate: wanted } => {
            bitrate.put(setup, u64::from(wanted));
            // NVIDIA's other sizes are for its 20 Mb/s: a two-second CPB.
            // Another bitrate scales them; the rest of its rate control
            // (Bmin, Ravg, R, rhopbi) is kept as captured.
            if u64::from(wanted) != captured_bitrate {
                vcl_bitrate.put(setup, u64::from(wanted));
                for field in cpbs {
                    field.put(setup, 2 * u64::from(wanted));
                }
            }
        }
    }
}

/// What one frame's setup needs beyond its session's templates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picture {
    /// The session's picture index, which picks every rotating buffer.
    pub index: u32,
    /// Pictures since the last IDR, zero for an IDR.
    pub since_idr: u32,
    /// IDRs before this one in the session.
    pub idrs_before: u32,
    /// L0 references: min(since_idr, the template's most).
    pub refs: u32,
}

impl Picture {
    pub fn idr(&self) -> bool {
        self.since_idr == 0
    }
}

/// The DPB slots NVIDIA's H.264 setups name: frame m in slot m % 4.
pub const H264_DPB_SLOTS: u32 = 4;
/// The reconstructed pictures, used round-robin.
pub const RECON_SLOTS: u32 = 5;

/// Writes `picture`'s setup into `out`, a setup slot's bytes.
pub fn build(templates: &Templates, picture: &Picture, out: &mut [u8]) {
    let template = if picture.idr() {
        &templates.idr
    } else {
        &templates.p
    };
    out[..template.len()].copy_from_slice(template);
    let setup = &mut out[..template.len()];
    match templates.codec {
        Codec::H264 => h264(setup, picture),
        Codec::Hevc => hevc(setup, picture),
    }
}

fn h264(setup: &mut [u8], picture: &Picture) {
    let max_frame_num = 1u64 << (f4::SPS_DATA_LOG2_MAX_FRAME_NUM_MINUS4.get(setup) + 4);
    let max_poc_lsb = 1u64 << (f4::SPS_DATA_LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4.get(setup) + 4);
    let k = u64::from(picture.since_idr);
    if picture.idr() {
        f4::PIC_CONTROL_FRAME_NUM.put(setup, 0);
        f4::PIC_CONTROL_PIC_ORDER_CNT_LSB.put(setup, 0);
        // Consecutive IDRs need different idr_pic_id; the first keeps 0.
        f4::PIC_CONTROL_IDR_PIC_ID.put(setup, u64::from(picture.idrs_before & 0xffff));
        return;
    }
    let refs = picture.refs;
    let pps_refs = f4::PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1.get(setup);
    let zero = u64::from((0xffu32 << refs) & 0xff);
    f4::PIC_CONTROL_FRAME_NUM.put(setup, k % max_frame_num);
    f4::PIC_CONTROL_PIC_ORDER_CNT_LSB.put(setup, (2 * k) % max_poc_lsb);
    for i in 0..8u32 {
        let used = i < refs;
        let l0 = if used {
            2 * u64::from((picture.index - 1 - i) % H264_DPB_SLOTS)
        } else {
            0xfe
        };
        f4::PIC_CONTROL_L0[i as usize].put(setup, l0);
        f4::PIC_CONTROL_TEMP_DIST_L0[i as usize]
            .put(setup, if used { u64::from(i) + 1 } else { 0xff });
        for row in f4::PIC_CONTROL_DIST_SCALE_FACTOR {
            row[i as usize].put(setup, if used { 0x100 } else { 0xffff });
        }
    }
    for field in f4::PIC_CONTROL_DIFF_PIC_ORDER_CNT_ZERO {
        field.put(setup, zero * 0x0101_0101);
    }
    f4::SLICE_NUM_REF_IDX_L0_ACTIVE_MINUS1.put(setup, u64::from(refs) - 1);
    f4::SLICE_NUM_REF_IDX_ACTIVE_OVERRIDE_FLAG
        .put(setup, u64::from(u64::from(refs) - 1 != pps_refs));
}

fn hevc(setup: &mut [u8], picture: &Picture) {
    let max_poc_lsb = 1u64 << (f5::SPS_DATA_LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4.get(setup) + 4);
    if picture.idr() {
        // IDR_W_RADL, POC 0, an empty RPS: the template as captured.
        f5::PIC_CONTROL_PIC_ORDER_CNT_LSB.put(setup, 0);
        return;
    }
    let k = u64::from(picture.since_idr);
    f5::PIC_CONTROL_PIC_ORDER_CNT_LSB.put(setup, k % max_poc_lsb);
    f5::PIC_CONTROL_L0[0].put(setup, 2 * u64::from((picture.index - 1) % RECON_SLOTS));
    f5::PIC_CONTROL_SHORT_TERM_RPS_NUM_NEGATIVE_PICS.put(setup, k.min(u64::from(RECON_SLOTS - 1)));
}

/// The setup's picture type: 3 for an IDR, 0 for a P frame.
pub fn picture_type(codec: Codec, setup: &[u8]) -> u64 {
    match codec {
        Codec::H264 => f4::PIC_CONTROL_PIC_TYPE.get(setup),
        Codec::Hevc => f5::PIC_CONTROL_PIC_TYPE.get(setup),
    }
}

/// Where the setup asks the engine to start the bitstream.
pub fn bitstream_start(codec: Codec, setup: &[u8]) -> u64 {
    match codec {
        Codec::H264 => f4::PIC_CONTROL_BITSTREAM_START_POS.get(setup),
        Codec::Hevc => f5::PIC_CONTROL_BITSTREAM_START_POS.get(setup),
    }
}
