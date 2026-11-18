//! HEVC (Rec. ITU-T H.265): sequence and picture parameter sets, the slice
//! segment header up to the reference picture set, the picture order count
//! (8.3.1) and the reference picture sets (8.3.2), for Main and Main 10.
//!
//! What NVDEC's setup takes besides the parameter sets: the POC difference
//! of every picture the current one keeps (`RefDiffPicOrderCnts[slot]`), the
//! initial reference lists as slots (`initreflistidxl0/l1`), and two bit
//! counts of the slice header, the bits from `slice_pic_order_cnt_lsb` to
//! the end of the long-term references (`sw_hdr_skip_length`: the engine
//! skips what the driver parsed) and those of the slice's own
//! `st_ref_pic_set()`. Each picture is written to the lowest slot no kept
//! picture holds, out of `sps_max_dec_pic_buffering_minus1 + 1`.
use super::{Bits, Error, Nal, Result, ceil_log2};
use alloc::vec::Vec;

pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_IDR_N_LP: u8 = 20;
pub const NAL_CRA: u8 = 21;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;
pub const NAL_EOS: u8 = 36;
pub const NAL_PREFIX_SEI: u8 = 39;

/// The largest pictures NVDEC takes for HEVC here: 4096x4096 (8192 on
/// GA10x, which the self-test's layouts never reach).
pub const MAX_SIDE: u32 = 4096;

/// A short-term reference picture set (7.3.7, 7.4.8): the POC deltas
/// before and after the current picture, nearest first, and whether the
/// current picture uses each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StRps {
    pub negative: Vec<(i32, bool)>,
    pub positive: Vec<(i32, bool)>,
}

impl StRps {
    pub fn num_delta_pocs(&self) -> usize {
        self.negative.len() + self.positive.len()
    }
}

/// The conformance window, in luma samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// A sequence parameter set (7.3.2.2), VUI and extensions left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sps {
    pub id: u8,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub max_sub_layers: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub window: Window,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_max_poc_lsb: u8,
    /// `sps_max_dec_pic_buffering_minus1` of the highest sub-layer.
    pub max_dec_pic_buffering_minus1: u8,
    pub log2_min_cb: u8,
    pub log2_max_cb: u8,
    pub log2_min_tb: u8,
    pub log2_max_tb: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled: bool,
    pub amp: bool,
    pub sample_adaptive_offset: bool,
    pub pcm: bool,
    pub pcm_bit_depth_luma: u8,
    pub pcm_bit_depth_chroma: u8,
    pub log2_min_pcm_cb: u8,
    pub log2_max_pcm_cb: u8,
    pub pcm_loop_filter_disabled: bool,
    pub st_rps: Vec<StRps>,
    pub long_term_ref_pics_present: bool,
    /// `lt_ref_pic_poc_lsb_sps` and `used_by_curr_pic_lt_sps_flag`.
    pub lt_ref_pics: Vec<(u32, bool)>,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
}

impl Sps {
    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_max_cb
    }

    pub fn width_ctbs(&self) -> u32 {
        self.width.div_ceil(self.ctb_size())
    }

    pub fn height_ctbs(&self) -> u32 {
        self.height.div_ceil(self.ctb_size())
    }

    /// The displayed size: the coded size inside the conformance window.
    pub fn display_size(&self) -> (u32, u32) {
        (
            self.width - self.window.left - self.window.right,
            self.height - self.window.top - self.window.bottom,
        )
    }
}

/// A picture parameter set (7.3.2.3), extensions left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_minus1: u8,
    pub num_ref_idx_l1_default_minus1: u8,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub loop_filter_across_tiles_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_control_present: bool,
    pub deblocking_filter_override_enabled: bool,
    pub pps_deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u8,
    pub slice_segment_header_extension_present: bool,
}

/// The slice segment header fields the decoder needs (7.3.6.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slice {
    pub first_slice_segment_in_pic: bool,
    pub pps_id: u8,
    pub dependent: bool,
    /// 0 B, 1 P, 2 I.
    pub slice_type: u8,
    pub pic_order_cnt_lsb: u32,
    /// The slice's own short-term set, or the SPS's it names.
    pub st_rps: StRps,
    /// Whether the set was the slice's own.
    pub st_rps_in_slice: bool,
    /// `NumDeltaPocs[RefRpsIdx]` when the slice's own set was predicted.
    pub num_delta_pocs_of_ref_rps: u32,
    /// Bits of the slice's `st_ref_pic_set()`.
    pub st_rps_bits: u32,
    /// Bits from `slice_pic_order_cnt_lsb` through the long-term pictures.
    pub skip_bits: u32,
    /// Long-term pictures: POC (LSBs, or the full POC when `msb`), used.
    pub long_term: Vec<(i32, bool, bool)>,
    pub temporal_mvp: bool,
}

/// Skips `profile_tier_level(1, max_sub_layers_minus1)`; the general
/// profile and level.
fn profile_tier_level(b: &mut Bits, max_sub_layers_minus1: u32) -> Result<(u8, u8)> {
    b.skip(3)?; // general_profile_space, general_tier_flag
    let profile = b.u(5)? as u8;
    let compatible = b.u(32)?;
    b.skip(4 + 43 + 1)?;
    let level = b.u(8)? as u8;
    let mut present = [(false, false); 8];
    for slot in present.iter_mut().take(max_sub_layers_minus1 as usize) {
        *slot = (b.flag()?, b.flag()?);
    }
    if max_sub_layers_minus1 > 0 {
        b.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for &(profile_present, level_present) in present.iter().take(max_sub_layers_minus1 as usize) {
        if profile_present {
            b.skip(88)?;
        }
        if level_present {
            b.skip(8)?;
        }
    }
    // A Main or Main 10 stream may say so by compatibility alone.
    let profile = if profile == 0 && compatible & (1 << 30) != 0 {
        1
    } else if profile == 0 && compatible & (1 << 29) != 0 {
        2
    } else {
        profile
    };
    Ok((profile, level))
}

/// `st_ref_pic_set(idx)` (7.3.7) and its derivation (7.4.8): the set, and
/// `NumDeltaPocs[RefRpsIdx]` when it was predicted from another.
fn st_ref_pic_set(b: &mut Bits, idx: usize, count: usize, sets: &[StRps]) -> Result<(StRps, u32)> {
    let predicted = idx != 0 && b.flag()?;
    if predicted {
        let delta_idx = if idx == count {
            b.ue_max(idx as u32 - 1, "delta_idx_minus1 out of range")? as usize + 1
        } else {
            1
        };
        let reference = sets
            .get(idx - delta_idx)
            .ok_or(Error::Invalid("a predicted RPS names a missing set"))?;
        let sign = b.flag()?;
        let abs = b.ue_max(1 << 15, "abs_delta_rps_minus1 out of range")? as i32 + 1;
        let delta_rps = if sign { -abs } else { abs };
        let n = reference.num_delta_pocs();
        let mut used = Vec::with_capacity(n + 1);
        let mut use_delta = Vec::with_capacity(n + 1);
        for _ in 0..=n {
            let u = b.flag()?;
            used.push(u);
            use_delta.push(if u { true } else { b.flag()? });
        }
        let neg = reference.negative.len();
        let mut set = StRps::default();
        for j in (0..reference.positive.len()).rev() {
            let d = reference.positive[j].0 + delta_rps;
            if d < 0 && use_delta[neg + j] {
                set.negative.push((d, used[neg + j]));
            }
        }
        if delta_rps < 0 && use_delta[n] {
            set.negative.push((delta_rps, used[n]));
        }
        for j in 0..neg {
            let d = reference.negative[j].0 + delta_rps;
            if d < 0 && use_delta[j] {
                set.negative.push((d, used[j]));
            }
        }
        for j in (0..neg).rev() {
            let d = reference.negative[j].0 + delta_rps;
            if d > 0 && use_delta[j] {
                set.positive.push((d, used[j]));
            }
        }
        if delta_rps > 0 && use_delta[n] {
            set.positive.push((delta_rps, used[n]));
        }
        for j in 0..reference.positive.len() {
            let d = reference.positive[j].0 + delta_rps;
            if d > 0 && use_delta[neg + j] {
                set.positive.push((d, used[neg + j]));
            }
        }
        if set.num_delta_pocs() > 16 {
            return Err(Error::Invalid("a predicted RPS of more than 16 pictures"));
        }
        return Ok((set, n as u32));
    }
    let negatives = b.ue_max(16, "num_negative_pics above 16")?;
    let positives = b.ue_max(16 - negatives, "num_positive_pics out of range")?;
    let mut set = StRps::default();
    let mut previous = 0i32;
    for _ in 0..negatives {
        previous -= b.ue_max(1 << 15, "delta_poc_s0_minus1 out of range")? as i32 + 1;
        set.negative.push((previous, b.flag()?));
    }
    previous = 0;
    for _ in 0..positives {
        previous += b.ue_max(1 << 15, "delta_poc_s1_minus1 out of range")? as i32 + 1;
        set.positive.push((previous, b.flag()?));
    }
    Ok((set, 0))
}

/// Parses a sequence parameter set's RBSP (after the 2-byte NAL header).
pub fn sps(escaped: &[u8]) -> Result<Sps> {
    let mut b = Bits::new(escaped);
    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.u(3)?;
    if max_sub_layers_minus1 > 6 {
        return Err(Error::Invalid("sps_max_sub_layers_minus1 above 6"));
    }
    b.skip(1)?;
    let (profile_idc, level_idc) = profile_tier_level(&mut b, max_sub_layers_minus1)?;
    let mut sps = Sps {
        id: b.ue_max(15, "sps_seq_parameter_set_id above 15")? as u8,
        profile_idc,
        level_idc,
        max_sub_layers: max_sub_layers_minus1 as u8 + 1,
        ..Sps::default()
    };
    sps.chroma_format_idc = b.ue_max(3, "chroma_format_idc above 3")? as u8;
    if sps.chroma_format_idc == 3 {
        sps.separate_colour_plane = b.flag()?;
    }
    sps.width = b.ue_max(16384, "pic_width_in_luma_samples above 16384")?;
    sps.height = b.ue_max(16384, "pic_height_in_luma_samples above 16384")?;
    if sps.width == 0 || sps.height == 0 {
        return Err(Error::Invalid("an empty picture"));
    }
    if b.flag()? {
        let (unit_x, unit_y) = match sps.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let mut sides = [0u32; 4];
        for side in &mut sides {
            *side = b.ue_max(8192, "conformance window offset out of range")?;
        }
        sps.window = Window {
            left: sides[0] * unit_x,
            right: sides[1] * unit_x,
            top: sides[2] * unit_y,
            bottom: sides[3] * unit_y,
        };
        if sps.window.left + sps.window.right >= sps.width
            || sps.window.top + sps.window.bottom >= sps.height
        {
            return Err(Error::Invalid("the conformance window leaves no picture"));
        }
    }
    sps.bit_depth_luma = 8 + b.ue_max(8, "bit_depth_luma_minus8 above 8")? as u8;
    sps.bit_depth_chroma = 8 + b.ue_max(8, "bit_depth_chroma_minus8 above 8")? as u8;
    sps.log2_max_poc_lsb = 4 + b.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4 above 12")? as u8;
    let all = b.flag()?;
    let first = if all { 0 } else { max_sub_layers_minus1 };
    for _ in first..=max_sub_layers_minus1 {
        sps.max_dec_pic_buffering_minus1 =
            b.ue_max(15, "sps_max_dec_pic_buffering_minus1 above 15")? as u8;
        b.ue()?; // sps_max_num_reorder_pics
        b.ue()?; // sps_max_latency_increase_plus1
    }
    sps.log2_min_cb = 3 + b.ue_max(3, "log2_min_luma_coding_block_size_minus3 above 3")? as u8;
    sps.log2_max_cb =
        sps.log2_min_cb + b.ue_max(3, "log2_diff_max_min_luma_coding_block_size")? as u8;
    sps.log2_min_tb = 2 + b.ue_max(3, "log2_min_luma_transform_block_size_minus2 above 3")? as u8;
    sps.log2_max_tb =
        sps.log2_min_tb + b.ue_max(3, "log2_diff_max_min_luma_transform_block_size")? as u8;
    if sps.log2_max_cb > 6 || sps.log2_max_tb > 5 || sps.log2_max_tb > sps.log2_max_cb {
        return Err(Error::Invalid("coding or transform blocks out of range"));
    }
    sps.max_transform_hierarchy_depth_inter =
        b.ue_max(4, "max_transform_hierarchy_depth_inter")? as u8;
    sps.max_transform_hierarchy_depth_intra =
        b.ue_max(4, "max_transform_hierarchy_depth_intra")? as u8;
    sps.scaling_list_enabled = b.flag()?;
    if sps.scaling_list_enabled {
        return Err(Error::Unsupported("HEVC scaling lists"));
    }
    sps.amp = b.flag()?;
    sps.sample_adaptive_offset = b.flag()?;
    sps.pcm = b.flag()?;
    if sps.pcm {
        sps.pcm_bit_depth_luma = b.u(4)? as u8 + 1;
        sps.pcm_bit_depth_chroma = b.u(4)? as u8 + 1;
        sps.log2_min_pcm_cb = 3 + b.ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")? as u8;
        sps.log2_max_pcm_cb = sps.log2_min_pcm_cb + b.ue_max(2, "log2_diff_max_min_pcm")? as u8;
        sps.pcm_loop_filter_disabled = b.flag()?;
    }
    let sets = b.ue_max(64, "num_short_term_ref_pic_sets above 64")? as usize;
    for idx in 0..sets {
        let (set, _) = st_ref_pic_set(&mut b, idx, sets, &sps.st_rps)?;
        sps.st_rps.push(set);
    }
    sps.long_term_ref_pics_present = b.flag()?;
    if sps.long_term_ref_pics_present {
        let count = b.ue_max(32, "num_long_term_ref_pics_sps above 32")?;
        for _ in 0..count {
            let lsb = b.u(u32::from(sps.log2_max_poc_lsb))?;
            sps.lt_ref_pics.push((lsb, b.flag()?));
        }
    }
    sps.temporal_mvp = b.flag()?;
    sps.strong_intra_smoothing = b.flag()?;
    Ok(sps)
}

/// Parses a picture parameter set's RBSP; `sps` are the sequence sets
/// seen so far, by id.
pub fn pps(escaped: &[u8], sps: &[Option<Sps>]) -> Result<Pps> {
    let mut b = Bits::new(escaped);
    let mut pps = Pps {
        id: b.ue_max(63, "pps_pic_parameter_set_id above 63")? as u8,
        sps_id: b.ue_max(15, "pps_seq_parameter_set_id above 15")? as u8,
        ..Pps::default()
    };
    let seq = sps
        .get(usize::from(pps.sps_id))
        .and_then(Option::as_ref)
        .ok_or(Error::Missing(
            "a picture parameter set names a missing sequence set",
        ))?;
    pps.dependent_slice_segments_enabled = b.flag()?;
    pps.output_flag_present = b.flag()?;
    pps.num_extra_slice_header_bits = b.u(3)? as u8;
    pps.sign_data_hiding = b.flag()?;
    pps.cabac_init_present = b.flag()?;
    pps.num_ref_idx_l0_default_minus1 = b.ue_max(14, "num_ref_idx_l0_default_active_minus1")? as u8;
    pps.num_ref_idx_l1_default_minus1 = b.ue_max(14, "num_ref_idx_l1_default_active_minus1")? as u8;
    let qp_low = -26 - 6 * (i32::from(seq.bit_depth_luma) - 8);
    pps.init_qp_minus26 = b.se_in(qp_low..=25, "init_qp_minus26 out of range")?;
    pps.constrained_intra_pred = b.flag()?;
    pps.transform_skip_enabled = b.flag()?;
    pps.cu_qp_delta_enabled = b.flag()?;
    if pps.cu_qp_delta_enabled {
        pps.diff_cu_qp_delta_depth = b.ue_max(
            u32::from(seq.log2_max_cb - seq.log2_min_cb),
            "diff_cu_qp_delta_depth",
        )? as u8;
    }
    pps.cb_qp_offset = b.se_in(-12..=12, "pps_cb_qp_offset out of range")?;
    pps.cr_qp_offset = b.se_in(-12..=12, "pps_cr_qp_offset out of range")?;
    pps.slice_chroma_qp_offsets_present = b.flag()?;
    pps.weighted_pred = b.flag()?;
    pps.weighted_bipred = b.flag()?;
    pps.transquant_bypass_enabled = b.flag()?;
    pps.tiles_enabled = b.flag()?;
    pps.entropy_coding_sync_enabled = b.flag()?;
    if pps.tiles_enabled {
        return Err(Error::Unsupported("HEVC tiles"));
    }
    // Inferred 1 when absent (7.4.3.3.1).
    pps.loop_filter_across_tiles_enabled = true;
    pps.loop_filter_across_slices_enabled = b.flag()?;
    pps.deblocking_filter_control_present = b.flag()?;
    if pps.deblocking_filter_control_present {
        pps.deblocking_filter_override_enabled = b.flag()?;
        pps.pps_deblocking_filter_disabled = b.flag()?;
        if !pps.pps_deblocking_filter_disabled {
            pps.beta_offset_div2 = b.se_in(-6..=6, "pps_beta_offset_div2 out of range")?;
            pps.tc_offset_div2 = b.se_in(-6..=6, "pps_tc_offset_div2 out of range")?;
        }
    }
    if b.flag()? {
        return Err(Error::Unsupported("HEVC scaling lists"));
    }
    pps.lists_modification_present = b.flag()?;
    pps.log2_parallel_merge_level = 2 + b.ue_max(
        u32::from(seq.log2_max_cb) - 2,
        "log2_parallel_merge_level_minus2",
    )? as u8;
    pps.slice_segment_header_extension_present = b.flag()?;
    if b.flag()? && b.flag()? {
        return Err(Error::Unsupported("HEVC range extensions"));
    }
    Ok(pps)
}

/// `first_slice_segment_in_pic_flag` of a slice NAL.
pub fn first_in_picture(nal: &Nal<'_>) -> bool {
    nal.bytes.get(2).is_some_and(|b| b & 0x80 != 0)
}

fn is_idr(kind: u8) -> bool {
    kind == NAL_IDR_W_RADL || kind == NAL_IDR_N_LP
}

/// Parses the header of the slice segment `nal`.
pub fn slice(nal: &Nal<'_>, sps: &[Option<Sps>], pps: &[Option<Pps>]) -> Result<Slice> {
    let kind = nal.hevc_type();
    if nal.bytes.len() < 3 {
        return Err(Error::Truncated);
    }
    let mut b = Bits::new(&nal.bytes[2..]);
    let mut slice = Slice {
        first_slice_segment_in_pic: b.flag()?,
        ..Slice::default()
    };
    if (16..=23).contains(&kind) {
        b.skip(1)?; // no_output_of_prior_pics_flag
    }
    slice.pps_id = b.ue_max(63, "slice_pic_parameter_set_id above 63")? as u8;
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
    if !slice.first_slice_segment_in_pic {
        if pic.dependent_slice_segments_enabled {
            slice.dependent = b.flag()?;
        }
        let ctbs = seq.width_ctbs() * seq.height_ctbs();
        b.skip(ceil_log2(ctbs) as usize)?;
    }
    if slice.dependent {
        return Ok(slice);
    }
    b.skip(usize::from(pic.num_extra_slice_header_bits))?;
    slice.slice_type = b.ue_max(2, "slice_type above 2")? as u8;
    if pic.output_flag_present {
        b.skip(1)?;
    }
    if seq.separate_colour_plane {
        b.skip(2)?;
    }
    if !is_idr(kind) {
        let start = b.position();
        slice.pic_order_cnt_lsb = b.u(u32::from(seq.log2_max_poc_lsb))?;
        let from_sps = b.flag()?;
        let sets = seq.st_rps.len();
        if !from_sps {
            let rps_start = b.position();
            let (set, ref_deltas) = st_ref_pic_set(&mut b, sets, sets, &seq.st_rps)?;
            slice.st_rps_bits = (b.position() - rps_start) as u32;
            slice.st_rps = set;
            slice.st_rps_in_slice = true;
            slice.num_delta_pocs_of_ref_rps = ref_deltas;
        } else {
            if sets == 0 {
                return Err(Error::Invalid(
                    "a slice names an SPS RPS though there is none",
                ));
            }
            let idx = if sets > 1 {
                b.u(ceil_log2(sets as u32))? as usize
            } else {
                0
            };
            slice.st_rps = seq
                .st_rps
                .get(idx)
                .cloned()
                .ok_or(Error::Invalid("short_term_ref_pic_set_idx out of range"))?;
        }
        if seq.long_term_ref_pics_present {
            let from_sps_count = seq.lt_ref_pics.len() as u32;
            let num_sps = if from_sps_count > 0 {
                b.ue_max(from_sps_count, "num_long_term_sps out of range")?
            } else {
                0
            };
            let num_pics = b.ue_max(16, "num_long_term_pics above 16")?;
            let max_lsb = 1i64 << seq.log2_max_poc_lsb;
            let mut cycle = 0i64;
            for i in 0..num_sps + num_pics {
                let (lsb, used) = if i < num_sps {
                    let index = if from_sps_count > 1 {
                        b.u(ceil_log2(from_sps_count))? as usize
                    } else {
                        0
                    };
                    seq.lt_ref_pics[index]
                } else {
                    let lsb = b.u(u32::from(seq.log2_max_poc_lsb))?;
                    (lsb, b.flag()?)
                };
                let msb_present = b.flag()?;
                let mut poc = i64::from(lsb);
                if msb_present {
                    let delta = i64::from(b.ue()?);
                    cycle = if i == 0 || i == num_sps {
                        delta
                    } else {
                        cycle + delta
                    };
                    // Made whole once the current POC is known: carry the
                    // cycle in the value, the full POC computed later.
                    poc = i64::from(lsb) - cycle * max_lsb;
                }
                slice.long_term.push((poc as i32, used, msb_present));
            }
        }
        slice.skip_bits = (b.position() - start) as u32;
        if seq.temporal_mvp {
            slice.temporal_mvp = b.flag()?;
        }
    }
    Ok(slice)
}

/// A picture the decoded-picture buffer keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kept {
    pub slot: u8,
    pub poc: i32,
    pub long_term: bool,
}

/// One picture, ready for NVDEC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub sps: Sps,
    pub pps: Pps,
    /// The first slice segment's header.
    pub slice: Slice,
    pub nal_type: u8,
    pub idr: bool,
    /// An intra random access point (`nal_unit_type` 16 to 23).
    pub irap: bool,
    /// `I`, `P` or `B`.
    pub kind: char,
    /// The slice segments as NVDEC reads them: each `00 00 01` and its NAL.
    pub bitstream: Vec<u8>,
    pub slices: usize,
    pub slot: u8,
    /// The slots the stream uses: `sps_max_dec_pic_buffering_minus1 + 1`.
    pub slots: u8,
    /// The highest slot used so far, this picture's included.
    pub high_water: u8,
    /// The slot of the picture decoded before this one (0 for the first).
    pub previous_slot: u8,
    pub poc: i32,
    /// The pictures kept for this one, before it is added.
    pub dpb: Vec<Kept>,
    /// `RefPicSetStCurrBefore`, `RefPicSetStCurrAfter`, `RefPicSetLtCurr`
    /// as slots.
    pub st_curr_before: Vec<u8>,
    pub st_curr_after: Vec<u8>,
    pub lt_curr: Vec<u8>,
}

impl Picture {
    /// `NumPicTotalCurr`.
    pub fn num_pic_total_curr(&self) -> usize {
        self.st_curr_before.len() + self.st_curr_after.len() + self.lt_curr.len()
    }
}

/// A stream's parameter sets and reference state, one access unit at a time.
#[derive(Clone, Debug)]
pub struct Parser {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    dpb: Vec<Kept>,
    prev_tid0_poc: i32,
    high_water: Option<u8>,
    previous_slot: u8,
    /// The next IRAP has `NoRaslOutputFlag` (the first, or after an end of
    /// sequence).
    starting: bool,
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
            sps: alloc::vec![None; 16],
            pps: alloc::vec![None; 64],
            dpb: Vec::new(),
            prev_tid0_poc: 0,
            high_water: None,
            previous_slot: 0,
            starting: true,
            active_sps: None,
        }
    }

    pub fn active_sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    /// Takes one access unit's NAL units: the parameter sets it carries,
    /// and its picture, if it has slices. The reference state moves on as
    /// if the picture was decoded.
    pub fn access_unit(&mut self, nals: &[Nal<'_>]) -> Result<Option<Picture>> {
        let mut slices: Vec<(Nal<'_>, Slice)> = Vec::new();
        for nal in nals {
            let kind = nal.hevc_type();
            if nal.bytes.len() < 2 {
                return Err(Error::Truncated);
            }
            if nal.bytes[1] >> 3 != 0 || nal.bytes[0] & 1 != 0 {
                return Err(Error::Unsupported("HEVC layers above the base"));
            }
            match kind {
                NAL_SPS => {
                    let sps = sps(&nal.bytes[2..])?;
                    let id = usize::from(sps.id);
                    self.sps[id] = Some(sps);
                }
                NAL_PPS => {
                    let pps = pps(&nal.bytes[2..], &self.sps)?;
                    let id = usize::from(pps.id);
                    self.pps[id] = Some(pps);
                }
                NAL_EOS => self.starting = true,
                0..=9 | 16..=21 => {
                    let header = slice(nal, &self.sps, &self.pps)?;
                    slices.push((*nal, header));
                }
                10..=15 | 22..=31 => return Err(Error::Invalid("a reserved VCL NAL type")),
                _ => {}
            }
        }
        let Some((first_nal, first)) = slices.first().cloned() else {
            return Ok(None);
        };
        if !first.first_slice_segment_in_pic {
            return Err(Error::Missing("a picture without its first slice segment"));
        }
        let nal_type = first_nal.hevc_type();
        for (nal, header) in &slices {
            if nal.hevc_type() != nal_type || header.pps_id != first.pps_id {
                return Err(Error::Invalid("the slices of a picture disagree"));
            }
        }
        let pps = self.pps[usize::from(first.pps_id)]
            .clone()
            .ok_or(Error::Missing("pps"))?;
        let sps = self.sps[usize::from(pps.sps_id)]
            .clone()
            .ok_or(Error::Missing("sps"))?;
        match &self.active_sps {
            Some(active)
                if active.width != sps.width
                    || active.height != sps.height
                    || active.bit_depth_luma != sps.bit_depth_luma
                    || active.max_dec_pic_buffering_minus1 != sps.max_dec_pic_buffering_minus1 =>
            {
                return Err(Error::Unsupported(
                    "a new sequence of another size mid-stream",
                ));
            }
            _ => self.active_sps = Some(sps.clone()),
        }
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma != sps.bit_depth_chroma {
            return Err(Error::Unsupported("HEVC other than 4:2:0 at one bit depth"));
        }
        if !matches!(sps.bit_depth_luma, 8 | 10) {
            return Err(Error::Unsupported("HEVC other than 8 or 10 bits"));
        }
        if sps.width > MAX_SIDE || sps.height > MAX_SIDE {
            return Err(Error::Unsupported("HEVC larger than 4096x4096"));
        }
        let idr = is_idr(nal_type);
        let irap = (16..=23).contains(&nal_type);
        if self.starting && !irap {
            return Err(Error::Missing(
                "the stream does not start with an IRAP picture",
            ));
        }
        let no_rasl_output = irap && (idr || (16..=18).contains(&nal_type) || self.starting);
        if matches!(nal_type, 8 | 9) && self.starting {
            return Err(Error::Unsupported("RASL pictures after a starting CRA"));
        }
        // 8.3.1: the picture order count.
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let lsb = first.pic_order_cnt_lsb as i32;
        let msb = if no_rasl_output {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = msb + lsb;
        // 8.3.2: the reference picture set, and what the buffer keeps.
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut follow = Vec::new();
        if !idr {
            for &(delta, used) in &first.st_rps.negative {
                if used {
                    before.push(poc + delta)
                } else {
                    follow.push(poc + delta)
                }
            }
            for &(delta, used) in &first.st_rps.positive {
                if used {
                    after.push(poc + delta)
                } else {
                    follow.push(poc + delta)
                }
            }
        }
        let mut long_curr = Vec::new();
        let mut long_follow = Vec::new();
        for &(value, used, msb_present) in &first.long_term {
            // A full POC (value carries -cycle * MaxLsb) or LSBs alone.
            let target = if msb_present {
                (poc - (poc & (max_lsb - 1))) + value
            } else {
                value
            };
            if used {
                long_curr.push((target, msb_present));
            } else {
                long_follow.push((target, msb_present));
            }
        }
        let matches_long = |kept: &Kept, &(target, full): &(i32, bool)| {
            if full {
                kept.poc == target
            } else {
                kept.poc & (max_lsb - 1) == target
            }
        };
        let old = core::mem::take(&mut self.dpb);
        if !(irap && no_rasl_output) {
            for kept in old {
                let long = long_curr
                    .iter()
                    .chain(&long_follow)
                    .any(|lt| matches_long(&kept, lt));
                let short = !kept.long_term
                    && !long
                    && (before.contains(&kept.poc)
                        || after.contains(&kept.poc)
                        || follow.contains(&kept.poc));
                if long || short {
                    self.dpb.push(Kept {
                        long_term: long,
                        ..kept
                    });
                }
            }
        }
        let slot_of = |dpb: &[Kept], poc: i32, long: bool| {
            dpb.iter()
                .find(|k| k.poc == poc && k.long_term == long)
                .map(|k| k.slot)
        };
        let mut st_curr_before = Vec::new();
        for &p in &before {
            st_curr_before.push(slot_of(&self.dpb, p, false).ok_or(Error::Missing(
                "a short-term reference is not in the buffer",
            ))?);
        }
        let mut st_curr_after = Vec::new();
        for &p in &after {
            st_curr_after.push(slot_of(&self.dpb, p, false).ok_or(Error::Missing(
                "a short-term reference is not in the buffer",
            ))?);
        }
        let mut lt_curr = Vec::new();
        for lt in &long_curr {
            let slot = self
                .dpb
                .iter()
                .find(|k| k.long_term && matches_long(k, lt))
                .map(|k| k.slot)
                .ok_or(Error::Missing("a long-term reference is not in the buffer"))?;
            lt_curr.push(slot);
        }
        let mut kind = 'I';
        let mut bitstream = Vec::new();
        for (nal, header) in &slices {
            bitstream.extend_from_slice(&[0, 0, 1]);
            bitstream.extend_from_slice(nal.bytes);
            if !header.dependent {
                kind = match (kind, header.slice_type) {
                    (_, 0) | ('B', _) => 'B',
                    (_, 1) | ('P', _) => 'P',
                    _ => 'I',
                };
            }
        }
        if kind != 'I' && st_curr_before.len() + st_curr_after.len() + lt_curr.len() == 0 {
            return Err(Error::Invalid("an inter picture with no reference"));
        }
        let slots = sps.max_dec_pic_buffering_minus1 + 1;
        let slot = (0..slots)
            .find(|s| !self.dpb.iter().any(|k| k.slot == *s))
            .ok_or(Error::Invalid("every surface holds a kept picture"))?;
        let high_water = self.high_water.map_or(slot, |h| h.max(slot));
        self.high_water = Some(high_water);
        let picture = Picture {
            sps: sps.clone(),
            pps,
            slice: first.clone(),
            nal_type,
            idr,
            irap,
            kind,
            bitstream,
            slices: slices.len(),
            slot,
            slots,
            high_water,
            previous_slot: self.previous_slot,
            poc,
            dpb: self.dpb.clone(),
            st_curr_before,
            st_curr_after,
            lt_curr,
        };
        // As if decoded.
        self.starting = false;
        self.previous_slot = slot;
        let sub_layer_non_reference = nal_type <= 14 && nal_type % 2 == 0;
        if first_nal.hevc_temporal_id() == 0
            && !matches!(nal_type, 6..=9)
            && !sub_layer_non_reference
        {
            self.prev_tid0_poc = poc;
        }
        self.dpb.push(Kept {
            slot,
            poc,
            long_term: false,
        });
        Ok(Some(picture))
    }
}

/// Splits an Annex B stream into access units (7.4.2.4.4).
pub fn access_units<'a>(nals: &[Nal<'a>]) -> Vec<Vec<Nal<'a>>> {
    let mut units: Vec<Vec<Nal<'a>>> = Vec::new();
    let mut current: Vec<Nal<'a>> = Vec::new();
    let mut has_slice = false;
    for nal in nals {
        let kind = nal.hevc_type();
        let vcl = kind < 32;
        let starts = if vcl {
            has_slice && first_in_picture(nal)
        } else {
            has_slice
                && matches!(kind, NAL_VPS | NAL_SPS | NAL_PPS | NAL_AUD | NAL_PREFIX_SEI | 41..=44 | 48..=55)
        };
        if starts {
            units.push(core::mem::take(&mut current));
            has_slice = false;
        }
        has_slice |= vcl;
        current.push(*nal);
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

/// Every picture of an Annex B stream, in decoding order.
pub fn pictures(stream: &[u8]) -> Result<Vec<Picture>> {
    let nals = super::nal_units(stream);
    let mut parser = Parser::new();
    let mut pictures = Vec::new();
    for unit in access_units(&nals) {
        if let Some(picture) = parser.access_unit(&unit)? {
            pictures.push(picture);
        }
    }
    Ok(pictures)
}
