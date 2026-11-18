//! H.264 (Rec. ITU-T H.264): sequence and picture parameter sets (with the
//! VUI's reorder bound and the scaling matrices), the slice header up to
//! `dec_ref_pic_marking()`, the picture order count (8.2.1), the decoded
//! reference pictures (8.2.5) and the output order, frame pictures only.
//!
//! The reference pictures sit at fixed places of NVDEC's `dpb[16]`, as
//! NVIDIA's driver keeps them (`nvdec-conventions.md`): a picture keeps its
//! place while it is a reference, and a new reference takes the first free
//! one once the sliding window or `memory_management_control_operation`s
//! freed theirs.
//!
//! Surface slots, order counts and output order are FFmpeg's, the
//! application NVIDIA's driver was captured under ([`super::super::dpb`]):
//!
//! - **Picture order count type 0 starts at 65536:** FFmpeg's `idr()` sets
//!   `prev_poc_msb = 1 << 16` and `prev_poc_lsb = -1`, so an IDR's POC is
//!   65536 + its LSBs and every later one follows. Types 1 and 2 count from
//!   `frame_num` and get no offset. Only differences matter to the engine.
//! - **Slots** come off a stack; a picture holds its slot while it is a
//!   reference or waits to be shown, and the slots no picture holds any
//!   more are given back at the start of the next picture, in the order of
//!   FFmpeg's picture array (a picture takes the array's first free place).
//! - **Output:** a picture waits until more than `has_b_frames` do (the
//!   VUI's `max_num_reorder_frames`, or FFmpeg's guess from the order
//!   counts without it); then the lowest order count is shown, never across
//!   an IDR or a `memory_management_control_operation` 5.
use super::super::dpb::{Output, Pool};
use super::{Bits, Error, Nal, Result};
use alloc::vec::Vec;

/// `nal_unit_type`s.
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
pub const NAL_END_OF_SEQUENCE: u8 = 10;

/// The largest pictures NVDEC takes for H.264: 4096x4096.
pub const MAX_MBS_SIDE: u32 = 256;
/// The most slices a picture may have: the slice offsets buffer's words.
pub const MAX_SLICES: usize = 256;
/// The most surface slots a stream gets.
pub const MAX_SLOTS: u8 = 16;
/// FFmpeg's `H264_MAX_PICTURE_COUNT`: the places of its picture array.
const PICTURE_PLACES: u8 = 36;
/// FFmpeg's `MAX_DELAYED_PIC_COUNT`.
const MAX_DELAYED: usize = 16;

/// The cropping of a picture's coded frame, in luma samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crop {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// The scaling matrices of a parameter set (7.4.2.1.1.1), in raster order:
/// the six 4x4 lists (intra Y, Cb, Cr, inter Y, Cb, Cr) and the two 8x8
/// lists of 4:2:0 (intra Y, inter Y).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scaling {
    pub list4x4: [[u8; 16]; 6],
    pub list8x8: [[u8; 64]; 2],
}

impl Scaling {
    /// `Flat_4x4_16` and `Flat_8x8_16`: no scaling.
    pub const FLAT: Self = Self {
        list4x4: [[16; 16]; 6],
        list8x8: [[16; 64]; 2],
    };
}

impl Default for Scaling {
    fn default() -> Self {
        Self::FLAT
    }
}

/// The 4x4 zig-zag scan: raster position of each coded coefficient.
const ZIGZAG4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
/// The 8x8 zig-zag (frame) scan.
const ZIGZAG8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
/// Table 7-3 and 7-4, in zig-zag order: `Default_4x4_Intra`/`_Inter`,
/// `Default_8x8_Intra`/`_Inter`.
const DEFAULT_4X4: [[u8; 16]; 2] = [
    [
        6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
    ],
    [
        10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
    ],
];
const DEFAULT_8X8: [[u8; 64]; 2] = [
    [
        6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25,
        25, 25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31,
        31, 31, 31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
    ],
    [
        9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22,
        22, 22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27,
        27, 27, 27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
    ],
];

/// A zig-zag list in raster order.
fn raster<const N: usize>(coded: &[u8; N], scan: &[usize; N]) -> [u8; N] {
    let mut out = [0u8; N];
    for (k, &value) in coded.iter().enumerate() {
        out[scan[k]] = value;
    }
    out
}

/// The VUI's `bitstream_restriction()` (E.1.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restriction {
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
}

/// A sequence parameter set (7.3.2.1.1), of the VUI only the bitstream
/// restriction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub qpprime_y_zero_transform_bypass: bool,
    pub scaling_matrix_present: bool,
    /// The sequence's matrices: flat without `seq_scaling_matrix_present`.
    pub scaling: Scaling,
    pub log2_max_frame_num: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_poc_lsb: u8,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offsets_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_allowed: bool,
    pub width_mbs: u32,
    pub height_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    pub crop: Crop,
    pub restriction: Option<Restriction>,
}

impl Sps {
    /// `FrameHeightInMbs`.
    pub fn height_mbs(&self) -> u32 {
        (2 - u32::from(self.frame_mbs_only)) * self.height_map_units
    }

    pub fn coded_width(&self) -> u32 {
        self.width_mbs * 16
    }

    pub fn coded_height(&self) -> u32 {
        self.height_mbs() * 16
    }

    /// The displayed size, the coded one cropped.
    pub fn display_size(&self) -> (u32, u32) {
        (
            self.coded_width() - self.crop.left - self.crop.right,
            self.coded_height() - self.crop.top - self.crop.bottom,
        )
    }

    fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }

    /// `MaxDpbFrames` of the level (A.3.1, Table A-1), at most 16.
    pub fn max_dpb_frames(&self) -> u32 {
        let max_dpb_mbs: u32 = match self.level_idc {
            9 | 10 => 396,
            11 if self.constraint_flags & 0x10 != 0 => 396,
            11 => 900,
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            51 | 52 => 184320,
            _ => 696320,
        };
        (max_dpb_mbs / (self.width_mbs * self.height_mbs()).max(1)).clamp(1, 16)
    }

    /// The pictures that may wait to be shown: the VUI's bound, else the
    /// level's.
    pub fn reorder_bound(&self) -> u32 {
        match self.restriction {
            Some(r) => r.max_num_reorder_frames.min(16),
            None => self.max_dpb_frames(),
        }
    }

    /// The surface slots a stream of this sequence may hold at once: its
    /// references, the pictures waiting to be shown and the one being
    /// decoded, plus one a picture waiting to be given back holds.
    pub fn slots(&self) -> u8 {
        let refs = u32::from(self.max_num_ref_frames.max(1));
        (refs + self.reorder_bound() + 2).min(u32::from(MAX_SLOTS)) as u8
    }
}

/// A picture parameter set (7.3.2.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_minus1: u8,
    pub num_ref_idx_l1_default_minus1: u8,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub scaling_matrix_present: bool,
    /// The matrices pictures of this set use: the set's own, else the
    /// sequence's.
    pub scaling: Scaling,
    pub second_chroma_qp_index_offset: i32,
}

/// A `memory_management_control_operation` with its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mmco {
    /// 1: a short-term picture is no longer a reference.
    ForgetShort { difference_of_pic_nums_minus1: u32 },
    /// 2: a long-term picture is no longer a reference.
    ForgetLong { long_term_pic_num: u32 },
    /// 3: a short-term picture becomes long-term.
    ShortToLong {
        difference_of_pic_nums_minus1: u32,
        long_term_frame_idx: u32,
    },
    /// 4: the long-term indices above this go.
    MaxLong { max_long_term_frame_idx_plus1: u32 },
    /// 5: no picture is a reference any more.
    ForgetAll,
    /// 6: the current picture becomes long-term.
    CurrentToLong { long_term_frame_idx: u32 },
}

/// The slice header fields the decoder needs (7.3.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slice {
    pub first_mb: u32,
    /// `slice_type % 5`: 0 P, 1 B, 2 I, 3 SP, 4 SI.
    pub slice_type: u8,
    pub pps_id: u8,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    /// `adaptive_ref_pic_marking_mode_flag` and its operations.
    pub adaptive: bool,
    pub mmcos: Vec<Mmco>,
}

pub const P: u8 = 0;
pub const B: u8 = 1;
pub const I: u8 = 2;
pub const SP: u8 = 3;
pub const SI: u8 = 4;

fn profile_has_chroma_info(profile: u8) -> bool {
    matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// A `scaling_list()` of `N` coefficients (7.3.2.1.1.1), in zig-zag order;
/// `None` when it says `useDefaultScalingMatrixFlag`.
fn scaling_list<const N: usize>(bits: &mut Bits) -> Result<Option<[u8; N]>> {
    let mut list = [0u8; N];
    let mut last = 8i32;
    let mut next = 8i32;
    for (j, entry) in list.iter_mut().enumerate() {
        if next != 0 {
            let delta = bits.se_in(-128..=127, "delta_scale out of range")?;
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return Ok(None);
            }
        }
        let value = if next == 0 { last } else { next };
        *entry = value as u8;
        last = value;
    }
    Ok(Some(list))
}

/// The scaling matrices a parameter set signals (`count` lists: 6, and 2
/// 8x8 ones when they are coded), each absent one per fall-back rule A
/// (`fallback` is `None`: the defaults) or B (the sequence's).
fn scaling_matrices(
    bits: &mut Bits,
    count: usize,
    fallback: Option<&Scaling>,
    eight_from: &Scaling,
) -> Result<Scaling> {
    let mut out = Scaling {
        list8x8: eight_from.list8x8,
        ..Scaling::FLAT
    };
    for i in 0..count {
        let present = bits.flag()?;
        if i < 6 {
            let inter = usize::from(i >= 3);
            let default = raster(&DEFAULT_4X4[inter], &ZIGZAG4);
            out.list4x4[i] = if present {
                match scaling_list::<16>(bits)? {
                    Some(list) => raster(&list, &ZIGZAG4),
                    None => default,
                }
            } else if i == 0 || i == 3 {
                fallback.map_or(default, |f| f.list4x4[i])
            } else {
                out.list4x4[i - 1]
            };
        } else if i < 8 {
            let inter = i - 6;
            let default = raster(&DEFAULT_8X8[inter], &ZIGZAG8);
            out.list8x8[inter] = if present {
                match scaling_list::<64>(bits)? {
                    Some(list) => raster(&list, &ZIGZAG8),
                    None => default,
                }
            } else {
                fallback.map_or(default, |f| f.list8x8[inter])
            };
        } else if present {
            // 4:4:4's chroma 8x8 lists: parsed past, the stream is refused
            // for its chroma format anyway.
            scaling_list::<64>(bits)?;
        }
    }
    Ok(out)
}

/// `hrd_parameters()` (E.1.2), skipped.
fn skip_hrd(b: &mut Bits) -> Result<()> {
    let count = b.ue_max(31, "cpb_cnt_minus1 above 31")? + 1;
    b.skip(8)?;
    for _ in 0..count {
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    b.skip(20)
}

/// `vui_parameters()` (E.1.1): the bitstream restriction, if any.
fn vui(b: &mut Bits) -> Result<Option<Restriction>> {
    if b.flag()? && b.u(8)? == 255 {
        b.skip(32)?;
    }
    if b.flag()? {
        b.skip(1)?;
    }
    if b.flag()? {
        b.skip(4)?;
        if b.flag()? {
            b.skip(24)?;
        }
    }
    if b.flag()? {
        b.ue()?;
        b.ue()?;
    }
    if b.flag()? {
        b.skip(65)?;
    }
    let nal_hrd = b.flag()?;
    if nal_hrd {
        skip_hrd(b)?;
    }
    let vcl_hrd = b.flag()?;
    if vcl_hrd {
        skip_hrd(b)?;
    }
    if nal_hrd || vcl_hrd {
        b.skip(1)?;
    }
    b.skip(1)?; // pic_struct_present_flag
    if !b.flag()? {
        return Ok(None);
    }
    b.skip(1)?;
    for _ in 0..4 {
        b.ue()?;
    }
    let max_num_reorder_frames = b.ue()?;
    let max_dec_frame_buffering = b.ue()?;
    Ok(Some(Restriction {
        max_num_reorder_frames,
        max_dec_frame_buffering,
    }))
}

/// Parses a sequence parameter set's RBSP (`escaped`, after the NAL header).
pub fn sps(escaped: &[u8]) -> Result<Sps> {
    let mut b = Bits::new(escaped);
    let mut sps = Sps {
        profile_idc: b.u(8)? as u8,
        constraint_flags: b.u(8)? as u8,
        level_idc: b.u(8)? as u8,
        id: b.ue_max(31, "seq_parameter_set_id above 31")? as u8,
        chroma_format_idc: 1,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        ..Sps::default()
    };
    if profile_has_chroma_info(sps.profile_idc) {
        sps.chroma_format_idc = b.ue_max(3, "chroma_format_idc above 3")? as u8;
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = b.flag()?;
        }
        sps.bit_depth_luma = 8 + b.ue_max(6, "bit_depth_luma_minus8 above 6")? as u8;
        sps.bit_depth_chroma = 8 + b.ue_max(6, "bit_depth_chroma_minus8 above 6")? as u8;
        sps.qpprime_y_zero_transform_bypass = b.flag()?;
        sps.scaling_matrix_present = b.flag()?;
        if sps.scaling_matrix_present {
            let lists = if sps.chroma_format_idc == 3 { 12 } else { 8 };
            sps.scaling = scaling_matrices(&mut b, lists, None, &Scaling::FLAT)?;
        }
    }
    sps.log2_max_frame_num = 4 + b.ue_max(12, "log2_max_frame_num_minus4 above 12")? as u8;
    sps.pic_order_cnt_type = b.ue_max(2, "pic_order_cnt_type above 2")? as u8;
    match sps.pic_order_cnt_type {
        0 => {
            sps.log2_max_poc_lsb =
                4 + b.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4 above 12")? as u8;
        }
        1 => {
            sps.delta_pic_order_always_zero = b.flag()?;
            sps.offset_for_non_ref_pic = b.se()?;
            sps.offset_for_top_to_bottom_field = b.se()?;
            let cycle = b.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle above 255")?;
            for _ in 0..cycle {
                sps.offsets_for_ref_frame.push(b.se()?);
            }
        }
        _ => {}
    }
    sps.max_num_ref_frames = b.ue_max(16, "max_num_ref_frames above 16")? as u8;
    sps.gaps_in_frame_num_allowed = b.flag()?;
    sps.width_mbs = b.ue_max(MAX_MBS_SIDE - 1, "the picture is wider than 4096")? + 1;
    sps.height_map_units = b.ue_max(MAX_MBS_SIDE - 1, "the picture is taller than 4096")? + 1;
    sps.frame_mbs_only = b.flag()?;
    if !sps.frame_mbs_only {
        sps.mb_adaptive_frame_field = b.flag()?;
    }
    sps.direct_8x8_inference = b.flag()?;
    if b.flag()? {
        let (unit_x, unit_y) = match (sps.chroma_format_idc, sps.separate_colour_plane) {
            (0, _) | (3, true) => (1, 2 - u32::from(sps.frame_mbs_only)),
            (1, _) => (2, 2 * (2 - u32::from(sps.frame_mbs_only))),
            (2, _) => (2, 2 - u32::from(sps.frame_mbs_only)),
            _ => (1, 2 - u32::from(sps.frame_mbs_only)),
        };
        let mut crop = [0u32; 4];
        for side in &mut crop {
            *side = b.ue_max(8192, "frame cropping offset out of range")?;
        }
        sps.crop = Crop {
            left: crop[0] * unit_x,
            right: crop[1] * unit_x,
            top: crop[2] * unit_y,
            bottom: crop[3] * unit_y,
        };
        if sps.crop.left + sps.crop.right >= sps.coded_width()
            || sps.crop.top + sps.crop.bottom >= sps.coded_height()
        {
            return Err(Error::Invalid("the cropping leaves no picture"));
        }
    }
    // A VUI that does not parse costs only the reorder bound, which then
    // comes from the level, as without one.
    if b.flag()? {
        sps.restriction = vui(&mut b).ok().flatten();
    }
    Ok(sps)
}

/// Parses a picture parameter set's RBSP; `sps` are the sequence parameter
/// sets seen so far, by id.
pub fn pps(escaped: &[u8], sps: &[Option<Sps>]) -> Result<Pps> {
    let mut b = Bits::new(escaped);
    let mut pps = Pps {
        id: b.ue_max(255, "pic_parameter_set_id above 255")? as u8,
        sps_id: b.ue_max(31, "seq_parameter_set_id above 31")? as u8,
        ..Pps::default()
    };
    let seq = sps
        .get(usize::from(pps.sps_id))
        .and_then(Option::as_ref)
        .ok_or(Error::Missing(
            "a picture parameter set names a missing sequence set",
        ))?;
    pps.entropy_coding_mode = b.flag()?;
    pps.bottom_field_pic_order_in_frame_present = b.flag()?;
    if b.ue()? != 0 {
        return Err(Error::Unsupported("slice groups (FMO)"));
    }
    pps.num_ref_idx_l0_default_minus1 = b.ue_max(31, "num_ref_idx_l0_default_active_minus1")? as u8;
    pps.num_ref_idx_l1_default_minus1 = b.ue_max(31, "num_ref_idx_l1_default_active_minus1")? as u8;
    pps.weighted_pred = b.flag()?;
    pps.weighted_bipred_idc = b.u(2)? as u8;
    if pps.weighted_bipred_idc == 3 {
        return Err(Error::Invalid("weighted_bipred_idc 3"));
    }
    let qp_low = -26 - 6 * (i32::from(seq.bit_depth_luma) - 8);
    pps.pic_init_qp_minus26 = b.se_in(qp_low..=25, "pic_init_qp_minus26 out of range")?;
    pps.pic_init_qs_minus26 = b.se_in(-26..=25, "pic_init_qs_minus26 out of range")?;
    pps.chroma_qp_index_offset = b.se_in(-12..=12, "chroma_qp_index_offset out of range")?;
    pps.deblocking_filter_control_present = b.flag()?;
    pps.constrained_intra_pred = b.flag()?;
    pps.redundant_pic_cnt_present = b.flag()?;
    pps.second_chroma_qp_index_offset = pps.chroma_qp_index_offset;
    pps.scaling = seq.scaling;
    if b.more_rbsp_data() {
        pps.transform_8x8_mode = b.flag()?;
        pps.scaling_matrix_present = b.flag()?;
        if pps.scaling_matrix_present {
            let lists = 6 + if seq.chroma_format_idc == 3 { 6 } else { 2 }
                * usize::from(pps.transform_8x8_mode);
            // Rule A without the sequence's matrices, rule B with them
            // (7.4.2.2, Table 7-2).
            let fallback = seq.scaling_matrix_present.then_some(&seq.scaling);
            pps.scaling = scaling_matrices(&mut b, lists, fallback, &seq.scaling)?;
        }
        pps.second_chroma_qp_index_offset =
            b.se_in(-12..=12, "second_chroma_qp_index_offset out of range")?;
    }
    Ok(pps)
}

/// `first_mb_in_slice` of a slice NAL, which starts a picture when zero.
pub fn first_mb(nal: &Nal<'_>) -> Result<u32> {
    Bits::new(&nal.bytes[1..]).ue()
}

/// Parses the header of the slice `nal`.
pub fn slice(nal: &Nal<'_>, sps: &[Option<Sps>], pps: &[Option<Pps>]) -> Result<Slice> {
    let kind = nal.h264_type();
    let idr = kind == NAL_IDR;
    let mut b = Bits::new(&nal.bytes[1..]);
    let mut slice = Slice {
        first_mb: b.ue()?,
        ..Slice::default()
    };
    let raw_type = b.ue_max(9, "slice_type above 9")?;
    slice.slice_type = (raw_type % 5) as u8;
    slice.pps_id = b.ue_max(255, "pic_parameter_set_id above 255")? as u8;
    let pic = pps
        .get(usize::from(slice.pps_id))
        .and_then(Option::as_ref)
        .ok_or(Error::Missing(
            "a slice names a missing picture parameter set",
        ))?;
    let seq = sps
        .get(usize::from(pic.sps_id))
        .and_then(Option::as_ref)
        .ok_or(Error::Missing(
            "a slice's picture set names a missing sequence set",
        ))?;
    if idr && !matches!(slice.slice_type, I | SI) {
        return Err(Error::Invalid("an IDR slice that is not intra"));
    }
    if seq.separate_colour_plane {
        b.skip(2)?;
    }
    slice.frame_num = b.u(u32::from(seq.log2_max_frame_num))?;
    if !seq.frame_mbs_only && b.flag()? {
        return Err(Error::Unsupported("field pictures"));
    }
    if seq.mb_adaptive_frame_field {
        return Err(Error::Unsupported("MBAFF frames"));
    }
    if idr {
        slice.idr_pic_id = b.ue_max(65535, "idr_pic_id above 65535")?;
    }
    if seq.pic_order_cnt_type == 0 {
        slice.pic_order_cnt_lsb = b.u(u32::from(seq.log2_max_poc_lsb))?;
        if pic.bottom_field_pic_order_in_frame_present {
            slice.delta_pic_order_cnt_bottom = b.se()?;
        }
    }
    if seq.pic_order_cnt_type == 1 && !seq.delta_pic_order_always_zero {
        slice.delta_pic_order_cnt[0] = b.se()?;
        if pic.bottom_field_pic_order_in_frame_present {
            slice.delta_pic_order_cnt[1] = b.se()?;
        }
    }
    if pic.redundant_pic_cnt_present && b.ue()? != 0 {
        return Err(Error::Unsupported("redundant pictures"));
    }
    if slice.slice_type == B {
        b.skip(1)?; // direct_spatial_mv_pred_flag
    }
    slice.num_ref_idx_l0_active_minus1 = pic.num_ref_idx_l0_default_minus1;
    slice.num_ref_idx_l1_active_minus1 = pic.num_ref_idx_l1_default_minus1;
    if matches!(slice.slice_type, P | SP | B) && b.flag()? {
        slice.num_ref_idx_l0_active_minus1 = b.ue_max(31, "num_ref_idx_l0_active_minus1")? as u8;
        if slice.slice_type == B {
            slice.num_ref_idx_l1_active_minus1 =
                b.ue_max(31, "num_ref_idx_l1_active_minus1")? as u8;
        }
    }
    // ref_pic_list_modification()
    let lists = match slice.slice_type {
        B => 2,
        P | SP => 1,
        _ => 0,
    };
    for _ in 0..lists {
        if b.flag()? {
            let mut count = 0;
            loop {
                match b.ue_max(5, "modification_of_pic_nums_idc above 5")? {
                    3 => break,
                    0..=2 => {
                        b.ue()?;
                    }
                    _ => return Err(Error::Unsupported("MVC reference list modification")),
                }
                count += 1;
                if count > 33 {
                    return Err(Error::Invalid("too many reference list modifications"));
                }
            }
        }
    }
    // pred_weight_table()
    let weighted = (pic.weighted_pred && matches!(slice.slice_type, P | SP))
        || (pic.weighted_bipred_idc == 1 && slice.slice_type == B);
    if weighted {
        b.ue_max(7, "luma_log2_weight_denom above 7")?;
        let chroma = seq.chroma_format_idc != 0 && !seq.separate_colour_plane;
        if chroma {
            b.ue_max(7, "chroma_log2_weight_denom above 7")?;
        }
        let counts = [
            u32::from(slice.num_ref_idx_l0_active_minus1) + 1,
            if slice.slice_type == B {
                u32::from(slice.num_ref_idx_l1_active_minus1) + 1
            } else {
                0
            },
        ];
        for count in counts {
            for _ in 0..count {
                if b.flag()? {
                    b.se()?;
                    b.se()?;
                }
                if chroma && b.flag()? {
                    for _ in 0..4 {
                        b.se()?;
                    }
                }
            }
        }
    }
    // dec_ref_pic_marking()
    if nal.h264_ref_idc() != 0 {
        if idr {
            slice.no_output_of_prior_pics = b.flag()?;
            slice.long_term_reference = b.flag()?;
        } else {
            slice.adaptive = b.flag()?;
            if slice.adaptive {
                loop {
                    let mmco = match b.ue_max(6, "memory_management_control_operation above 6")? {
                        0 => break,
                        1 => Mmco::ForgetShort {
                            difference_of_pic_nums_minus1: b.ue()?,
                        },
                        2 => Mmco::ForgetLong {
                            long_term_pic_num: b.ue()?,
                        },
                        3 => Mmco::ShortToLong {
                            difference_of_pic_nums_minus1: b.ue()?,
                            long_term_frame_idx: b.ue_max(15, "long_term_frame_idx above 15")?,
                        },
                        4 => Mmco::MaxLong {
                            max_long_term_frame_idx_plus1: b
                                .ue_max(16, "max_long_term_frame_idx")?,
                        },
                        5 => Mmco::ForgetAll,
                        _ => Mmco::CurrentToLong {
                            long_term_frame_idx: b.ue_max(15, "long_term_frame_idx above 15")?,
                        },
                    };
                    slice.mmcos.push(mmco);
                    if slice.mmcos.len() > 66 {
                        return Err(Error::Invalid("too many memory management operations"));
                    }
                }
            }
        }
    }
    Ok(slice)
}

/// A reference picture in the decoded-picture buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The surface slot it was decoded to.
    pub slot: u8,
    pub frame_num: u32,
    /// `LongTermFrameIdx` when a long-term reference.
    pub long_term: Option<u32>,
    /// Top and bottom field order counts.
    pub poc: [i32; 2],
}

/// One picture, ready for NVDEC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub sps: Sps,
    pub pps: Pps,
    /// The first slice's header.
    pub slice: Slice,
    pub idr: bool,
    pub nal_ref_idc: u8,
    /// `I`, `P` or `B`: B if a slice is, P if one is P or SP.
    pub kind: char,
    /// The slices as NVDEC reads them: each `00 00 01` and its NAL unit.
    pub bitstream: Vec<u8>,
    /// Where each slice's start code is in [`Picture::bitstream`].
    pub slice_offsets: Vec<u32>,
    pub slot: u8,
    /// The slots the stream may hold at once ([`Sps::slots`]).
    pub slots: u8,
    /// The highest slot used so far, this picture's included.
    pub high_water: u8,
    /// The slot of the last reference picture decoded before this one (0
    /// for the first): what the engine conceals a damaged picture from
    /// (`ERROR_FRM_IDX`), and what NVIDIA's driver names in the `dpb[]`
    /// entries no reference holds. Non-reference pictures do not count:
    /// h264-high-bf's B-frames name the reference before them.
    pub previous_slot: u8,
    pub frame_num: u32,
    pub poc: [i32; 2],
    /// The references, at their `dpb[]` places, before this picture.
    pub dpb: [Option<Reference>; 16],
    /// Which decode of the stream this is (0 for the first).
    pub decoded: u32,
    /// The pictures shown as this one starts, in output order, maybe
    /// itself.
    pub outputs: Vec<Output>,
}

/// A picture FFmpeg's picture array holds: its place there and its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    place: u8,
    slot: u8,
}

/// A picture waiting to be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    slot: u8,
    poc: i32,
    decoded: u32,
    /// An IDR, or a picture with `memory_management_control_operation` 5:
    /// no picture is shown across it.
    barrier: bool,
}

/// A stream's parameter sets and reference state, one access unit at a time.
#[derive(Clone, Debug)]
pub struct Parser {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    refs: [Option<Reference>; 16],
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
    prev_ref_frame_num: u32,
    max_long_term_frame_idx: Option<u32>,
    previous_slot: u8,
    seen_idr: bool,
    /// The sequence the stream started with: a new one needs a new session.
    active_sps: Option<Sps>,
    pool: Pool,
    held: Vec<Held>,
    waiting: Vec<Waiting>,
    has_b_frames: u32,
    next_outputed_poc: i32,
    last_pocs: [i32; MAX_DELAYED],
    decoded: u32,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            sps: alloc::vec![None; 32],
            pps: alloc::vec![None; 256],
            refs: [None; 16],
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            prev_frame_num_offset: 0,
            prev_frame_num: 0,
            prev_ref_frame_num: 0,
            max_long_term_frame_idx: None,
            previous_slot: 0,
            seen_idr: false,
            active_sps: None,
            pool: Pool::new(MAX_SLOTS),
            held: Vec::new(),
            waiting: Vec::new(),
            has_b_frames: 0,
            next_outputed_poc: i32::MIN,
            last_pocs: [i32::MIN; MAX_DELAYED],
            decoded: 0,
        }
    }

    /// The sequence of the stream's first picture.
    pub fn active_sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    /// Takes a parameter set NAL outside an access unit (from an `avcC`).
    pub fn parameter_set(&mut self, nal: &Nal<'_>) -> Result<()> {
        match nal.h264_type() {
            NAL_SPS => {
                let sps = sps(&nal.bytes[1..])?;
                let id = usize::from(sps.id);
                self.sps[id] = Some(sps);
            }
            NAL_PPS => {
                let pps = pps(&nal.bytes[1..], &self.sps)?;
                let id = usize::from(pps.id);
                self.pps[id] = Some(pps);
            }
            _ => return Err(Error::Invalid("not a parameter set")),
        }
        Ok(())
    }

    /// Whether `slot` holds a reference.
    fn is_reference(&self, slot: u8) -> bool {
        self.refs.iter().flatten().any(|r| r.slot == slot)
    }

    /// Takes one access unit's NAL units: the parameter sets it carries,
    /// and its picture, if it has slices. The reference state moves on as
    /// if the picture was decoded.
    pub fn access_unit(&mut self, nals: &[Nal<'_>]) -> Result<Option<Picture>> {
        let mut slices: Vec<(Nal<'_>, Slice)> = Vec::new();
        for nal in nals {
            match nal.h264_type() {
                NAL_SPS | NAL_PPS => self.parameter_set(nal)?,
                NAL_SLICE | NAL_IDR => {
                    let header = slice(nal, &self.sps, &self.pps)?;
                    slices.push((*nal, header));
                }
                2..=4 => return Err(Error::Unsupported("data partitioning")),
                14 | 15 | 20 | 21 => return Err(Error::Unsupported("SVC or MVC")),
                _ => {}
            }
        }
        let Some((first_nal, first)) = slices.first().cloned() else {
            return Ok(None);
        };
        if slices.len() > MAX_SLICES {
            return Err(Error::Unsupported("more than 256 slices in a picture"));
        }
        let idr = first_nal.h264_type() == NAL_IDR;
        let nal_ref_idc = first_nal.h264_ref_idc();
        for (nal, header) in &slices {
            if (nal.h264_type() == NAL_IDR) != idr
                || header.frame_num != first.frame_num
                || header.pps_id != first.pps_id
                || (nal.h264_ref_idc() == 0) != (nal_ref_idc == 0)
            {
                return Err(Error::Invalid("the slices of a picture disagree"));
            }
        }
        let pps = self.pps[usize::from(first.pps_id)]
            .clone()
            .ok_or(Error::Missing("pps"))?;
        let sps = self.sps[usize::from(pps.sps_id)]
            .clone()
            .ok_or(Error::Missing("sps"))?;
        if !self.seen_idr && !idr {
            return Err(Error::Missing("the stream does not start with an IDR"));
        }
        match &self.active_sps {
            Some(active)
                if active.width_mbs != sps.width_mbs
                    || active.height_map_units != sps.height_map_units
                    || active.max_num_ref_frames != sps.max_num_ref_frames
                    || active.chroma_format_idc != sps.chroma_format_idc
                    || active.bit_depth_luma != sps.bit_depth_luma =>
            {
                return Err(Error::Unsupported(
                    "a new sequence of another size mid-stream",
                ));
            }
            _ => self.active_sps = Some(sps.clone()),
        }
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 {
            return Err(Error::Unsupported("H.264 other than 8-bit 4:2:0"));
        }
        let max_frame_num = sps.max_frame_num();
        if idr {
            if first.frame_num != 0 {
                return Err(Error::Invalid("an IDR with a frame_num"));
            }
            // FFmpeg's idr(): no reference left, and its POC origin.
            self.seen_idr = true;
            self.refs = [None; 16];
            self.max_long_term_frame_idx = None;
            self.prev_poc_msb = 1 << 16;
            self.prev_poc_lsb = -1;
            self.prev_frame_num_offset = 0;
            self.prev_frame_num = 0;
            self.prev_ref_frame_num = 0;
            self.last_pocs = [i32::MIN; MAX_DELAYED];
        } else if first.frame_num != self.prev_ref_frame_num
            && first.frame_num != (self.prev_ref_frame_num + 1) % max_frame_num
        {
            return Err(Error::Unsupported("a gap in frame_num"));
        }
        // 8.2.1: the picture order count.
        let reference = nal_ref_idc != 0;
        let frame_num_offset = if idr {
            0
        } else if self.prev_frame_num > first.frame_num {
            self.prev_frame_num_offset + max_frame_num as i32
        } else {
            self.prev_frame_num_offset
        };
        let (poc, poc_msb) = match sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let lsb = first.pic_order_cnt_lsb as i32;
                let msb = if lsb < self.prev_poc_lsb && self.prev_poc_lsb - lsb >= max_lsb / 2 {
                    self.prev_poc_msb + max_lsb
                } else if lsb > self.prev_poc_lsb && lsb - self.prev_poc_lsb > max_lsb / 2 {
                    self.prev_poc_msb - max_lsb
                } else {
                    self.prev_poc_msb
                };
                let top = msb + lsb;
                ([top, top + first.delta_pic_order_cnt_bottom], msb)
            }
            1 => {
                let cycle = sps.offsets_for_ref_frame.len() as i32;
                let mut abs = if cycle != 0 {
                    frame_num_offset + first.frame_num as i32
                } else {
                    0
                };
                if !reference && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i32;
                if abs > 0 {
                    let per_cycle: i32 = sps.offsets_for_ref_frame.iter().sum();
                    let count = (abs - 1) / cycle;
                    let within = ((abs - 1) % cycle) as usize;
                    expected = count * per_cycle
                        + sps.offsets_for_ref_frame[..=within].iter().sum::<i32>();
                }
                if !reference {
                    expected += sps.offset_for_non_ref_pic;
                }
                let top = expected + first.delta_pic_order_cnt[0];
                let bottom =
                    top + sps.offset_for_top_to_bottom_field + first.delta_pic_order_cnt[1];
                ([top, bottom], 0)
            }
            _ => {
                let temp = if idr {
                    0
                } else if !reference {
                    2 * (frame_num_offset + first.frame_num as i32) - 1
                } else {
                    2 * (frame_num_offset + first.frame_num as i32)
                };
                ([temp, temp], 0)
            }
        };
        // FFmpeg's frame start: the pictures neither a reference nor
        // waiting to be shown give their slots back, in picture-array
        // order; the new picture takes the array's first free place and
        // the pool's next slot.
        self.pool.widen(sps.slots());
        self.held.sort_by_key(|h| h.place);
        let mut kept = Vec::with_capacity(self.held.len());
        for held in core::mem::take(&mut self.held) {
            let waiting = self.waiting.iter().any(|w| w.slot == held.slot);
            if waiting || self.is_reference(held.slot) {
                kept.push(held);
            } else {
                self.pool.give(held.slot);
            }
        }
        self.held = kept;
        let place = (0..PICTURE_PLACES)
            .find(|p| !self.held.iter().any(|h| h.place == *p))
            .ok_or(Error::Invalid("every picture place is held"))?;
        let slot = self
            .pool
            .take()
            .ok_or(Error::Invalid("every surface holds a picture"))?;
        self.held.push(Held { place, slot });
        let high_water = self.pool.high_water().unwrap_or(slot);
        let mut bitstream = Vec::new();
        let mut slice_offsets = Vec::with_capacity(slices.len());
        let mut kind = 'I';
        for (nal, header) in &slices {
            slice_offsets.push(bitstream.len() as u32);
            bitstream.extend_from_slice(&[0, 0, 1]);
            bitstream.extend_from_slice(nal.bytes);
            kind = match (kind, header.slice_type) {
                (_, B) | ('B', _) => 'B',
                (_, P | SP) | ('P', _) => 'P',
                _ => 'I',
            };
        }
        let mut picture = Picture {
            sps: sps.clone(),
            pps,
            slice: first.clone(),
            idr,
            nal_ref_idc,
            kind,
            bitstream,
            slice_offsets,
            slot,
            slots: self.pool.most(),
            high_water,
            previous_slot: self.previous_slot,
            frame_num: first.frame_num,
            poc,
            dpb: self.refs,
            decoded: self.decoded,
            outputs: Vec::new(),
        };
        // As if decoded: the marking (8.2.5) and the order count's state.
        let mut forget_all = false;
        if reference {
            let current = Reference {
                slot,
                frame_num: first.frame_num,
                long_term: None,
                poc,
            };
            forget_all = self.mark(&sps, &first, idr, current)?;
            self.prev_ref_frame_num = if forget_all { 0 } else { first.frame_num };
        }
        if sps.pic_order_cnt_type == 0 && reference {
            if forget_all {
                self.prev_poc_msb = 0;
                self.prev_poc_lsb = poc[0] - poc[0].min(poc[1]);
            } else {
                self.prev_poc_msb = poc_msb;
                self.prev_poc_lsb = first.pic_order_cnt_lsb as i32;
            }
        }
        self.prev_frame_num_offset = if forget_all { 0 } else { frame_num_offset };
        self.prev_frame_num = if forget_all { 0 } else { first.frame_num };
        if forget_all {
            self.last_pocs = [i32::MIN; MAX_DELAYED];
        }
        if reference {
            self.previous_slot = slot;
        }
        // FFmpeg shows a frame by the lower of its field order counts.
        let shown_poc = poc[0].min(poc[1]);
        if let Some(output) = self.select_output(&sps, kind, slot, shown_poc, idr || forget_all) {
            picture.outputs.push(output);
        }
        self.decoded += 1;
        Ok(Some(picture))
    }

    /// FFmpeg's `h264_select_output_frame()` for the picture just started:
    /// it joins the pictures waiting to be shown, and one of them may be
    /// shown.
    fn select_output(
        &mut self,
        sps: &Sps,
        kind: char,
        slot: u8,
        poc: i32,
        barrier: bool,
    ) -> Option<Output> {
        if let Some(restriction) = sps.restriction {
            self.has_b_frames = self
                .has_b_frames
                .max(restriction.max_num_reorder_frames.min(MAX_DELAYED as u32));
        }
        // The guess at the reorder depth from the order counts, which only
        // counts without a bitstream restriction.
        let mut i = 0;
        loop {
            if i == MAX_DELAYED || poc < self.last_pocs[i] {
                if i > 0 {
                    self.last_pocs[i - 1] = poc;
                }
                break;
            } else if i > 0 {
                self.last_pocs[i - 1] = self.last_pocs[i];
            }
            i += 1;
        }
        let mut out_of_order = (MAX_DELAYED - i) as u32;
        let last = self.last_pocs[MAX_DELAYED - 1];
        let before = self.last_pocs[MAX_DELAYED - 2];
        if kind == 'B' || (before > i32::MIN && i64::from(last) - i64::from(before) > 2) {
            out_of_order = out_of_order.max(1);
        }
        let mut barrier = barrier;
        if out_of_order == MAX_DELAYED as u32 {
            self.last_pocs = [i32::MIN; MAX_DELAYED];
            self.last_pocs[0] = poc;
            barrier = true;
        } else if self.has_b_frames < out_of_order && sps.restriction.is_none() {
            self.has_b_frames = out_of_order;
        }
        self.waiting.push(Waiting {
            slot,
            poc,
            decoded: self.decoded,
            barrier,
        });
        let pictures = self.waiting.len() as u32;
        let mut out = 0;
        for k in 1..self.waiting.len() {
            if self.waiting[k].barrier {
                break;
            }
            if self.waiting[k].poc < self.waiting[out].poc {
                out = k;
            }
        }
        if self.has_b_frames == 0 && self.waiting[0].barrier {
            self.next_outputed_poc = i32::MIN;
        }
        let shown = self.waiting[out];
        let out_of_order = shown.poc < self.next_outputed_poc;
        if out_of_order || pictures > self.has_b_frames {
            self.waiting.remove(out);
        }
        if !out_of_order && pictures > self.has_b_frames {
            self.next_outputed_poc = if out == 0 && self.waiting.first().is_some_and(|w| w.barrier)
            {
                i32::MIN
            } else {
                shown.poc
            };
            return Some(Output {
                slot: shown.slot,
                surface: shown.slot,
                poc: shown.poc,
                decoded: shown.decoded,
            });
        }
        None
    }

    /// The pictures still waiting to be shown, in output order, as at the
    /// end of the stream. Their slots stay held until the next picture.
    pub fn flush(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        while !self.waiting.is_empty() {
            let mut out = 0;
            for k in 1..self.waiting.len() {
                if self.waiting[k].barrier {
                    break;
                }
                if self.waiting[k].poc < self.waiting[out].poc {
                    out = k;
                }
            }
            let shown = self.waiting.remove(out);
            outputs.push(Output {
                slot: shown.slot,
                surface: shown.slot,
                poc: shown.poc,
                decoded: shown.decoded,
            });
        }
        self.next_outputed_poc = i32::MIN;
        outputs
    }

    /// Forgets every picture (a seek): no reference, nothing to show, every
    /// slot free; the parameter sets stay, and the next picture must be an
    /// IDR.
    pub fn reset(&mut self) {
        let sps = core::mem::take(&mut self.sps);
        let pps = core::mem::take(&mut self.pps);
        *self = Self {
            sps,
            pps,
            ..Self::new()
        };
    }

    /// The slots held now: references and pictures waiting to be shown.
    pub fn held_slots(&self) -> Vec<u8> {
        self.held.iter().map(|h| h.slot).collect()
    }

    /// Marks the reference pictures after `current` (8.2.5); whether it
    /// had `memory_management_control_operation` 5.
    fn mark(
        &mut self,
        sps: &Sps,
        slice: &Slice,
        idr: bool,
        mut current: Reference,
    ) -> Result<bool> {
        let max_frame_num = sps.max_frame_num() as i64;
        let pic_num = |r: &Reference| -> i64 {
            let n = i64::from(r.frame_num);
            if n > i64::from(slice.frame_num) {
                n - max_frame_num
            } else {
                n
            }
        };
        let mut forget_all = false;
        if idr {
            if slice.long_term_reference {
                current.long_term = Some(0);
                self.max_long_term_frame_idx = Some(0);
            } else {
                self.max_long_term_frame_idx = None;
            }
        } else if slice.adaptive {
            let curr = i64::from(slice.frame_num);
            for mmco in &slice.mmcos {
                match *mmco {
                    Mmco::ForgetShort {
                        difference_of_pic_nums_minus1,
                    } => {
                        let x = curr - i64::from(difference_of_pic_nums_minus1) - 1;
                        for place in &mut self.refs {
                            if place.is_some_and(|r| r.long_term.is_none() && pic_num(&r) == x) {
                                *place = None;
                            }
                        }
                    }
                    Mmco::ForgetLong { long_term_pic_num } => {
                        for place in &mut self.refs {
                            if place.is_some_and(|r| r.long_term == Some(long_term_pic_num)) {
                                *place = None;
                            }
                        }
                    }
                    Mmco::ShortToLong {
                        difference_of_pic_nums_minus1,
                        long_term_frame_idx,
                    } => {
                        let x = curr - i64::from(difference_of_pic_nums_minus1) - 1;
                        for place in &mut self.refs {
                            if place.is_some_and(|r| r.long_term == Some(long_term_frame_idx)) {
                                *place = None;
                            }
                        }
                        for r in self.refs.iter_mut().flatten() {
                            if r.long_term.is_none() && pic_num(r) == x {
                                r.long_term = Some(long_term_frame_idx);
                            }
                        }
                    }
                    Mmco::MaxLong {
                        max_long_term_frame_idx_plus1,
                    } => {
                        for place in &mut self.refs {
                            if place.is_some_and(|r| {
                                r.long_term
                                    .is_some_and(|i| i + 1 > max_long_term_frame_idx_plus1)
                            }) {
                                *place = None;
                            }
                        }
                        self.max_long_term_frame_idx = max_long_term_frame_idx_plus1.checked_sub(1);
                    }
                    Mmco::ForgetAll => {
                        self.refs = [None; 16];
                        self.max_long_term_frame_idx = None;
                        forget_all = true;
                    }
                    Mmco::CurrentToLong {
                        long_term_frame_idx,
                    } => {
                        for place in &mut self.refs {
                            if place.is_some_and(|r| r.long_term == Some(long_term_frame_idx)) {
                                *place = None;
                            }
                        }
                        current.long_term = Some(long_term_frame_idx);
                    }
                }
            }
        } else {
            // The sliding window (8.2.5.3).
            let held = self.refs.iter().flatten().count();
            if held >= usize::from(sps.max_num_ref_frames.max(1)) {
                let oldest = self
                    .refs
                    .iter()
                    .enumerate()
                    .filter_map(|(at, r)| r.filter(|r| r.long_term.is_none()).map(|r| (at, r)))
                    .min_by_key(|(_, r)| pic_num(r))
                    .map(|(at, _)| at);
                match oldest {
                    Some(at) => self.refs[at] = None,
                    None => {
                        return Err(Error::Invalid(
                            "the sliding window found only long-term pictures",
                        ));
                    }
                }
            }
        }
        if forget_all {
            // The current picture's order counts are relative to itself.
            let base = current.poc[0].min(current.poc[1]);
            current.poc = [current.poc[0] - base, current.poc[1] - base];
            current.frame_num = 0;
        }
        let Some(free) = self.refs.iter().position(Option::is_none) else {
            return Err(Error::Invalid(
                "no place left in the decoded-picture buffer",
            ));
        };
        self.refs[free] = Some(current);
        if self.refs.iter().flatten().count() > usize::from(sps.max_num_ref_frames.max(1)) {
            return Err(Error::Invalid("more references than max_num_ref_frames"));
        }
        Ok(forget_all)
    }
}

/// Splits an Annex B stream into access units (7.4.1.2.3): a new one starts
/// at an access unit delimiter, a parameter set, an SEI or a prefix after a
/// slice, and at each slice with `first_mb_in_slice` 0.
pub fn access_units<'a>(nals: &[Nal<'a>]) -> Result<Vec<Vec<Nal<'a>>>> {
    let mut units: Vec<Vec<Nal<'a>>> = Vec::new();
    let mut current: Vec<Nal<'a>> = Vec::new();
    let mut has_slice = false;
    for nal in nals {
        let kind = nal.h264_type();
        let starts = match kind {
            NAL_SLICE | NAL_IDR => has_slice && first_mb(nal)? == 0,
            NAL_SEI | NAL_SPS | NAL_PPS | NAL_AUD | 14..=18 => has_slice,
            _ => false,
        };
        if starts {
            units.push(core::mem::take(&mut current));
            has_slice = false;
        }
        has_slice |= matches!(kind, NAL_SLICE | NAL_IDR);
        current.push(*nal);
    }
    if !current.is_empty() {
        units.push(current);
    }
    Ok(units)
}

/// Every picture of an Annex B stream, in decoding order.
pub fn pictures(stream: &[u8]) -> Result<Vec<Picture>> {
    let nals = super::nal_units(stream);
    let mut parser = Parser::new();
    let mut pictures = Vec::new();
    for unit in access_units(&nals)? {
        if let Some(picture) = parser.access_unit(&unit)? {
            pictures.push(picture);
        }
    }
    Ok(pictures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_matrices_come_out_in_raster_order() {
        let intra = raster(&DEFAULT_4X4[0], &ZIGZAG4);
        assert_eq!(
            intra,
            [
                6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42
            ]
        );
        let intra8 = raster(&DEFAULT_8X8[0], &ZIGZAG8);
        assert_eq!(intra8[..8], [6, 10, 13, 16, 18, 23, 25, 27]);
        assert_eq!(intra8[63], 42);
    }

    #[test]
    fn a_list_whose_first_delta_reaches_zero_is_the_default() {
        // delta_scale -8 (se code 16: 0000 1 0001) takes nextScale to 0.
        let mut bits = Bits::new(&[0b0000_1000, 0b1000_0000]);
        assert_eq!(scaling_list::<16>(&mut bits), Ok(None));
        // All deltas 0 (se code 0, "1"): a flat list of 8s.
        let mut bits = Bits::new(&[0xff, 0xff, 0x80]);
        assert_eq!(scaling_list::<16>(&mut bits), Ok(Some([8; 16])));
    }
}
