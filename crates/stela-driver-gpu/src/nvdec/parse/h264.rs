//! H.264 (Rec. ITU-T H.264): sequence and picture parameter sets, the slice
//! header up to `dec_ref_pic_marking()`, the picture order count (8.2.1)
//! and the decoded reference pictures (8.2.5), frame pictures only.
//!
//! The reference pictures sit at fixed places of NVDEC's `dpb[16]`, as
//! NVIDIA's driver keeps them (`nvdec-conventions.md`): a picture keeps its
//! place while it is a reference, and a new reference takes the first free
//! one once the sliding window or `memory_management_control_operation`s
//! freed theirs. Each picture is written to a surface slot: the lowest slot
//! no reference holds, out of `max_num_ref_frames + 1`.
use super::{Bits, Error, Nal, Result};
use alloc::vec::Vec;

/// `nal_unit_type`s.
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// The largest pictures NVDEC takes for H.264: 4096x4096.
pub const MAX_MBS_SIDE: u32 = 256;
/// The most slices a picture may have: the slice offsets buffer's words.
pub const MAX_SLICES: usize = 256;

/// The cropping of a picture's coded frame, in luma samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crop {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// A sequence parameter set (7.3.2.1.1), VUI left out.
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

/// Skips a `scaling_list()` of `size` coefficients.
fn skip_scaling_list(bits: &mut Bits, size: usize) -> Result<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = bits.se_in(-128..=127, "delta_scale out of range")?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
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
            for i in 0..lists {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
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
    if b.more_rbsp_data() {
        pps.transform_8x8_mode = b.flag()?;
        pps.scaling_matrix_present = b.flag()?;
        if pps.scaling_matrix_present {
            let lists = 6 + if seq.chroma_format_idc == 3 { 6 } else { 2 }
                * usize::from(pps.transform_8x8_mode);
            for i in 0..lists {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
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
    /// The slots the stream uses: `max_num_ref_frames + 1`.
    pub slots: u8,
    /// The highest slot used so far, this picture's included.
    pub high_water: u8,
    /// The slot of the picture decoded before this one, which NVIDIA's
    /// driver names in the `dpb[]` entries no reference holds.
    pub previous_slot: u8,
    pub frame_num: u32,
    pub poc: [i32; 2],
    /// The references, at their `dpb[]` places, before this picture.
    pub dpb: [Option<Reference>; 16],
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
    high_water: Option<u8>,
    seen_idr: bool,
    /// The sequence the stream started with: a new one needs a new session.
    active_sps: Option<Sps>,
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
            high_water: None,
            seen_idr: false,
            active_sps: None,
        }
    }

    /// The sequence of the stream's first picture.
    pub fn active_sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    /// Takes one access unit's NAL units: the parameter sets it carries,
    /// and its picture, if it has slices. The reference state moves on as
    /// if the picture was decoded.
    pub fn access_unit(&mut self, nals: &[Nal<'_>]) -> Result<Option<Picture>> {
        let mut slices: Vec<(Nal<'_>, Slice)> = Vec::new();
        for nal in nals {
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
        if sps.scaling_matrix_present || pps.scaling_matrix_present {
            return Err(Error::Unsupported("H.264 scaling matrices"));
        }
        let max_frame_num = sps.max_frame_num();
        if idr {
            self.seen_idr = true;
            self.refs = [None; 16];
            self.max_long_term_frame_idx = None;
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
            self.prev_frame_num_offset = 0;
            self.prev_frame_num = 0;
            self.prev_ref_frame_num = 0;
            if first.frame_num != 0 {
                return Err(Error::Invalid("an IDR with a frame_num"));
            }
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
        // The surface: the lowest slot no reference holds.
        let slots = sps.max_num_ref_frames.max(1) + 1;
        let slot = (0..slots)
            .find(|s| !self.refs.iter().flatten().any(|r| r.slot == *s))
            .ok_or(Error::Invalid("every surface holds a reference"))?;
        let high_water = self.high_water.map_or(slot, |h| h.max(slot));
        self.high_water = Some(high_water);
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
        let picture = Picture {
            sps: sps.clone(),
            pps,
            slice: first.clone(),
            idr,
            nal_ref_idc,
            kind,
            bitstream,
            slice_offsets,
            slot,
            slots,
            high_water,
            previous_slot: self.previous_slot,
            frame_num: first.frame_num,
            poc,
            dpb: self.refs,
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
        self.previous_slot = slot;
        Ok(Some(picture))
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
