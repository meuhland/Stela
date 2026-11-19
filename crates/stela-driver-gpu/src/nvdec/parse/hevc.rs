//! HEVC (Rec. ITU-T H.265): sequence and picture parameter sets (with
//! their scaling lists), the slice segment header up to the reference
//! picture set, the picture order count (8.3.1), the reference picture sets
//! (8.3.2) and the output order (C.5.2), for Main and Main 10.
//!
//! What NVDEC's setup takes besides the parameter sets: the POC difference
//! of every picture the current one keeps (`RefDiffPicOrderCnts[slot]`), the
//! initial reference lists as slots (`initreflistidxl0/l1`), and two bit
//! counts of the slice header, the bits from `slice_pic_order_cnt_lsb` to
//! the end of the long-term references (`sw_hdr_skip_length`: the engine
//! skips what the driver parsed) and those of the slice's own
//! `st_ref_pic_set()`.
//!
//! Surface slots and output order are FFmpeg's ([`super::super::dpb`]):
//! an IDR drops every reference first; the picture joins FFmpeg's picture
//! array at its first free place; the pictures its reference picture set
//! drops and that wait for nothing give their slots back, in array order;
//! pictures are shown (lowest order count first) while more than
//! `sps_max_num_reorder_pics` wait or the array holds more than
//! `sps_max_dec_pic_buffering`, a shown picture that is no reference
//! giving its slot back at once; and only then does the picture take the
//! slot pool's next slot, which may be one just shown (hevc-main-bf's
//! POC 8 takes POC 3's slot as POC 3 is shown, and NVIDIA's driver moves
//! that slot to another surface: [`super::super::dpb::Surfaces`]).
use super::super::dpb::{Output, Pool};
use super::{Bits, Error, Nal, Result, ceil_log2};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

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
/// GA10x, which Stela's layouts do not reach).
pub const MAX_SIDE: u32 = 4096;
/// The most surface slots a stream gets: `SET_PICTURE_LUMA_OFFSET0..15`.
pub const MAX_SLOTS: u8 = 16;
/// The places of FFmpeg's HEVC picture array.
const PICTURE_PLACES: u8 = 32;

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

/// Scaling lists (7.3.4, 7.4.5), each in raster order: per `sizeId` (4x4,
/// 8x8, 16x16, 32x32) and `matrixId` (0..5; 32x32 uses 0 and 3) the 4x4 or
/// 8x8 grid of coefficients, and the 16x16 and 32x32 DC coefficients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalingList {
    pub lists: [[[u8; 64]; 6]; 4],
    pub dc: [[u8; 6]; 2],
}

/// Table 7-6, in up-right diagonal order: the intra and inter default 8x8
/// lists (sizeId 1 to 3).
const DEFAULT_INTRA: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20,
    21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
    29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
const DEFAULT_INTER: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20,
    20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
    28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

/// The up-right diagonal scan of an `n` x `n` block (6.5.3): the raster
/// position of each coded coefficient.
fn diagonal(n: usize) -> Vec<usize> {
    let mut scan = Vec::with_capacity(n * n);
    let (mut x, mut y) = (0i32, 0i32);
    while scan.len() < n * n {
        while y >= 0 {
            if (x as usize) < n && (y as usize) < n {
                scan.push(y as usize * n + x as usize);
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    scan
}

impl ScalingList {
    /// Table 7-5 and 7-6: flat 4x4 lists, the default 8x8 ones, DC 16.
    pub fn default_lists() -> Self {
        let scan8 = diagonal(8);
        let mut out = Self {
            lists: [[[16; 64]; 6]; 4],
            dc: [[16; 6]; 2],
        };
        for size in 1..4 {
            for matrix in 0..6 {
                let coded = if matrix < 3 {
                    &DEFAULT_INTRA
                } else {
                    &DEFAULT_INTER
                };
                for (k, &value) in coded.iter().enumerate() {
                    out.lists[size][matrix][scan8[k]] = value;
                }
            }
        }
        out
    }

    /// The 1 KiB buffer NVIDIA's driver puts at the setup page's +0x200
    /// (`HEVC_SET_SCALING_LIST_OFFSET`), read off hevc-main-slices-1080p:
    /// the six 16x16 DC coefficients, the two 32x32 ones (matrixId 0, 3),
    /// 8 zero bytes, then the 4x4 lists (16 bytes each), the 8x8, 16x16 and
    /// 32x32 ones (64 bytes each, 32x32 for matrixId 0 and 3), all in
    /// raster order, and 16 zero bytes.
    pub fn buffer(&self) -> [u8; 1024] {
        let mut out = [0u8; 1024];
        out[..6].copy_from_slice(&self.dc[0]);
        out[6] = self.dc[1][0];
        out[7] = self.dc[1][3];
        let mut at = 16;
        for matrix in 0..6 {
            out[at..at + 16].copy_from_slice(&self.lists[0][matrix][..16]);
            at += 16;
        }
        for size in 1..3 {
            for matrix in 0..6 {
                out[at..at + 64].copy_from_slice(&self.lists[size][matrix]);
                at += 64;
            }
        }
        for matrix in [0, 3] {
            out[at..at + 64].copy_from_slice(&self.lists[3][matrix]);
            at += 64;
        }
        out
    }
}

/// `scaling_list_data()` (7.3.4).
fn scaling_list_data(b: &mut Bits) -> Result<ScalingList> {
    let defaults = ScalingList::default_lists();
    let mut out = defaults;
    let scan4 = diagonal(4);
    let scan8 = diagonal(8);
    for size in 0..4 {
        let step = if size == 3 { 3 } else { 1 };
        let mut matrix = 0;
        while matrix < 6 {
            if !b.flag()? {
                let delta =
                    b.ue_max((matrix / step) as u32, "scaling_list_pred_matrix_id_delta")? as usize;
                if delta == 0 {
                    out.lists[size][matrix] = defaults.lists[size][matrix];
                    if size > 1 {
                        out.dc[size - 2][matrix] = 16;
                    }
                } else {
                    let reference = matrix - delta * step;
                    out.lists[size][matrix] = out.lists[size][reference];
                    if size > 1 {
                        out.dc[size - 2][matrix] = out.dc[size - 2][reference];
                    }
                }
            } else {
                let count = if size == 0 { 16 } else { 64 };
                let mut next = 8i32;
                if size > 1 {
                    next = b.se_in(-7..=247, "scaling_list_dc_coef_minus8")? + 8;
                    out.dc[size - 2][matrix] = next as u8;
                }
                for k in 0..count {
                    let delta = b.se_in(-128..=127, "scaling_list_delta_coef")?;
                    next = (next + delta + 256) % 256;
                    let at = if size == 0 { scan4[k] } else { scan8[k] };
                    out.lists[size][matrix][at] = next as u8;
                }
            }
            matrix += step;
        }
    }
    Ok(out)
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
    /// `sps_max_num_reorder_pics` of the highest sub-layer.
    pub max_num_reorder: u8,
    pub log2_min_cb: u8,
    pub log2_max_cb: u8,
    pub log2_min_tb: u8,
    pub log2_max_tb: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled: bool,
    /// The sequence's lists when enabled: its own, else the defaults.
    pub scaling: Option<ScalingList>,
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

    /// `sps_max_dec_pic_buffering`.
    pub fn max_dec_pic_buffering(&self) -> u32 {
        u32::from(self.max_dec_pic_buffering_minus1) + 1
    }

    /// The surface slots a stream of this sequence may hold at once: the
    /// picture buffer and the one being decoded (bumping keeps the buffer
    /// within `sps_max_dec_pic_buffering` while any picture waits).
    pub fn slots(&self) -> u8 {
        (self.max_dec_pic_buffering() + 1).min(u32::from(MAX_SLOTS)) as u8
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
    /// `pps_scaling_list_data()`, when the set has its own.
    pub scaling: Option<ScalingList>,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u8,
    pub slice_segment_header_extension_present: bool,
}

/// The slice segment header fields the decoder needs (7.3.6.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slice {
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u8,
    pub dependent: bool,
    /// 0 B, 1 P, 2 I.
    pub slice_type: u8,
    pub pic_output: bool,
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
        sps.max_num_reorder = b.ue_max(
            u32::from(sps.max_dec_pic_buffering_minus1),
            "sps_max_num_reorder_pics out of range",
        )? as u8;
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
        sps.scaling = Some(if b.flag()? {
            scaling_list_data(&mut b)?
        } else {
            ScalingList::default_lists()
        });
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
        pps.scaling = Some(scaling_list_data(&mut b)?);
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
        pic_output: true,
        ..Slice::default()
    };
    if (16..=23).contains(&kind) {
        slice.no_output_of_prior_pics = b.flag()?;
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
        slice.pic_output = b.flag()?;
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

/// A picture the decoded-picture buffer keeps as a reference.
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
    /// The slots the stream may hold at once ([`Sps::slots`]).
    pub slots: u8,
    /// The highest slot used so far, this picture's included.
    pub high_water: u8,
    /// What the engine conceals a damaged picture from (`ERROR_FRM_IDX`):
    /// the first slot of the initial reference list 0 (0 without
    /// references), as NVIDIA's driver gives it. For P-only streams that
    /// is the picture decoded before; hevc-main-bf's B-frames show the
    /// difference.
    pub previous_slot: u8,
    pub poc: i32,
    /// The references kept for this one, before it is added.
    pub dpb: Vec<Kept>,
    /// `RefPicSetStCurrBefore`, `RefPicSetStCurrAfter`, `RefPicSetLtCurr`
    /// as slots.
    pub st_curr_before: Vec<u8>,
    pub st_curr_after: Vec<u8>,
    pub lt_curr: Vec<u8>,
    /// The scaling lists in force, when enabled: the PPS's, else the SPS's.
    pub scaling: Option<ScalingList>,
    /// Which decode of the stream this is (0 for the first).
    pub decoded: u32,
    /// The pictures shown at this picture's start, in output order (this
    /// one may be among them).
    pub outputs: Vec<Output>,
    /// References of its reference picture set that were not in the
    /// buffer (never decoded: a seek, a splice, a loop, a dropped picture),
    /// each stood in for by the reference nearest in order count, as FFmpeg
    /// generates the missing ones: the picture is decoded, its errors
    /// concealed. Where the buffer holds no reference at all, an inter
    /// picture's stand-in is the nearest picture still decoded there (kept
    /// as a reference from then on), and an intra picture's missing
    /// references are left out (it reads none).
    pub concealed: u8,
}

/// What a refused unit was, for the refusal's log line: its NAL unit, order
/// count and reference picture set as far as they were read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Note {
    nal_type: u8,
    temporal_id: u8,
    lsb: u32,
    poc: Option<i32>,
    pic_output: bool,
    kind: char,
    /// The order count as if the stream went on, and whether the IRAP
    /// starts afresh.
    going_on: Option<i32>,
    restart_cra: bool,
    no_rasl_output: bool,
    before: Vec<i32>,
    after: Vec<i32>,
    follow: Vec<i32>,
    /// Long-term: the order count (or its LSBs), used, whole.
    long: Vec<(i32, bool, bool)>,
}

/// A reference picture set's entry to find in the buffer.
#[derive(Clone, Copy, Debug)]
enum Wanted {
    Short(i32),
    /// The order count, or its LSBs when not `full`.
    Long {
        target: i32,
        full: bool,
        max_lsb: i32,
    },
}

impl Wanted {
    fn poc(self) -> i32 {
        match self {
            Self::Short(poc) => poc,
            Self::Long { target, .. } => target,
        }
    }

    fn matches(self, frame: &Frame) -> bool {
        match self {
            Self::Short(poc) => frame.short && !frame.long && frame.poc == poc,
            Self::Long {
                target,
                full,
                max_lsb,
            } => {
                frame.long
                    && if full {
                        frame.poc == target
                    } else {
                        frame.poc & (max_lsb - 1) == target
                    }
            }
        }
    }
}

/// What a stream's parser did about pictures it could not decode as
/// coded, since it began.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Events {
    /// RASL pictures of a CRA or BLA that started decoding afresh, skipped
    /// (8.1.3): they may refer to pictures never decoded, and are not shown.
    pub skipped: u32,
    /// References stood in for ([`Picture::concealed`]).
    pub concealed: u32,
    /// CRA pictures that started afresh mid-stream (`HandleCraAsBlaFlag`):
    /// after a flush, or whose reference picture set names pictures not in
    /// the buffer, as at a splice or a seek with no flush.
    pub restarted: u32,
}

impl Picture {
    /// `NumPicTotalCurr`.
    pub fn num_pic_total_curr(&self) -> usize {
        self.st_curr_before.len() + self.st_curr_after.len() + self.lt_curr.len()
    }
}

/// A picture of FFmpeg's picture array: a reference, or waiting to be
/// shown, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    place: u8,
    /// `None` only for the picture being started, before its slot is taken.
    slot: Option<u8>,
    poc: i32,
    short: bool,
    long: bool,
    output: bool,
    decoded: u32,
}

impl Frame {
    fn reference(&self) -> bool {
        self.short || self.long
    }

    fn held(&self) -> bool {
        self.reference() || self.output
    }
}

/// A stream's parameter sets and reference state, one access unit at a time.
#[derive(Clone, Debug)]
pub struct Parser {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    frames: Vec<Frame>,
    pool: Pool,
    prev_tid0_poc: i32,
    /// The next IRAP has `NoRaslOutputFlag` (the first, or after an end of
    /// sequence).
    starting: bool,
    /// The stream was flushed (its end, `finishDelayedFrames`) since the
    /// last picture: a CRA next starts afresh, as after an end of sequence,
    /// but a picture of any other kind goes on with the references kept.
    flushed: bool,
    /// The last IRAP started afresh (`NoRaslOutputFlag`): its RASL pictures
    /// are skipped.
    skip_rasl: bool,
    active_sps: Option<Sps>,
    decoded: u32,
    events: Events,
    /// The last IRAP taken: its NAL unit type, order count and decode.
    last_irap: Option<(u8, i32, u32)>,
    /// The decode count at the last flush, and the resets so far.
    flushed_at: Option<u32>,
    resets: u32,
    /// What the unit being taken is, as far as read ([`Note`]).
    note: Option<Note>,
    /// Why the last unit refused was refused, with the stream's state.
    refusal: Option<String>,
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
            frames: Vec::new(),
            pool: Pool::new(MAX_SLOTS),
            prev_tid0_poc: 0,
            starting: true,
            flushed: false,
            skip_rasl: false,
            active_sps: None,
            decoded: 0,
            events: Events::default(),
            last_irap: None,
            flushed_at: None,
            resets: 0,
            note: None,
            refusal: None,
        }
    }

    pub fn active_sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    /// Why the last unit refused was refused: its NAL unit type, order
    /// count, `pic_output_flag`, reference picture set (current before,
    /// after, following, long-term), the buffer's pictures before it, the
    /// last IRAP, and whether it came after a flush or a reset.
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }

    /// The refusal of `error`, with the unit's `note` and this (the state
    /// before the unit, restored).
    fn describe_refusal(&self, error: &Error, note: Option<&Note>) -> String {
        let mut line = String::new();
        let _ = write!(line, "{} ({})", error.describe(), error.kind());
        match note {
            Some(note) => {
                let _ = write!(
                    line,
                    "; nal {} tid {} lsb {} poc {} pic_output {} kind {} going_on {} restart_cra {} \
                     no_rasl_output {}; rps curr_before {:?} curr_after {:?} foll {:?} long \
                     (poc, used, msb) {:?}",
                    note.nal_type,
                    note.temporal_id,
                    note.lsb,
                    note.poc
                        .map_or(String::from("?"), |poc| alloc::format!("{poc}")),
                    note.pic_output,
                    note.kind,
                    note.going_on
                        .map_or(String::from("?"), |poc| alloc::format!("{poc}")),
                    note.restart_cra,
                    note.no_rasl_output,
                    note.before,
                    note.after,
                    note.follow,
                    note.long,
                );
            }
            None => line.push_str("; no slice header read"),
        }
        line.push_str("; dpb [");
        let mut frames: Vec<&Frame> = self.frames.iter().collect();
        frames.sort_by_key(|f| f.poc);
        for (k, frame) in frames.iter().enumerate() {
            let _ = write!(
                line,
                "{}{}@{}{}{}{}",
                if k > 0 { " " } else { "" },
                frame.poc,
                frame.slot.map_or(-1, i32::from),
                if frame.short { "S" } else { "" },
                if frame.long { "L" } else { "" },
                if frame.output { "O" } else { "" },
            );
        }
        let _ = write!(
            line,
            "]; last irap {}; prev_tid0_poc {}; decoded {}; {}; {}; resets {}; skip_rasl {}",
            self.last_irap
                .map_or(String::from("none"), |(nal, poc, decoded)| alloc::format!(
                    "nal {nal} poc {poc} decode {decoded}"
                )),
            self.prev_tid0_poc,
            self.decoded,
            if self.flushed {
                "right after a flush"
            } else {
                "no flush before it"
            },
            match self.flushed_at {
                Some(at) => alloc::format!("last flush at decode {at}"),
                None => String::from("never flushed"),
            },
            self.resets,
            self.skip_rasl,
        );
        if self.starting {
            line.push_str("; the stream starts afresh (its start, a reset or an end of sequence)");
        }
        line
    }

    /// The slot of the reference `wanted` names. One not in the buffer is
    /// stood in for (`concealed` counts it): by the reference nearest in
    /// order count; with no reference at all, an intra picture leaves it
    /// out (`None`), and an inter picture takes the nearest picture still
    /// decoded in the buffer, which stays a reference from then on (as
    /// FFmpeg's generated picture does). With no decoded picture at all, an
    /// inter picture is refused with `what`.
    fn reference_slot(
        &mut self,
        wanted: Wanted,
        intra: bool,
        concealed: &mut u8,
        what: &'static str,
    ) -> Result<Option<u8>> {
        if let Some(slot) = self
            .frames
            .iter()
            .find(|f| f.slot.is_some() && wanted.matches(f))
            .and_then(|f| f.slot)
        {
            return Ok(Some(slot));
        }
        let poc = wanted.poc();
        let nearest = |frames: &[Frame], any: bool| {
            frames
                .iter()
                .enumerate()
                .filter(|(_, f)| f.slot.is_some() && (any || f.reference()))
                .min_by_key(|(_, f)| (f.poc.abs_diff(poc), f.poc > poc))
                .map(|(k, _)| k)
        };
        if let Some(k) = nearest(&self.frames, false) {
            *concealed = concealed.saturating_add(1);
            return Ok(self.frames[k].slot);
        }
        if intra {
            *concealed = concealed.saturating_add(1);
            return Ok(None);
        }
        let k = nearest(&self.frames, true).ok_or(Error::Missing(what))?;
        *concealed = concealed.saturating_add(1);
        self.frames[k].short = true;
        Ok(self.frames[k].slot)
    }

    /// What the parser did about pictures it could not decode as coded.
    pub fn events(&self) -> Events {
        self.events
    }

    /// Takes a parameter set NAL outside an access unit (from an `hvcC`):
    /// VPS are skipped.
    pub fn parameter_set(&mut self, nal: &Nal<'_>) -> Result<()> {
        if nal.bytes.len() < 2 {
            return Err(Error::Truncated);
        }
        match nal.hevc_type() {
            NAL_VPS => {}
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
            _ => return Err(Error::Invalid("not a parameter set")),
        }
        Ok(())
    }

    /// Drops the array's pictures that are neither references nor waiting
    /// to be shown, giving their slots back in array order.
    fn release_unheld(&mut self) {
        self.frames.sort_by_key(|f| f.place);
        let mut kept = Vec::with_capacity(self.frames.len());
        for frame in core::mem::take(&mut self.frames) {
            if frame.held() {
                kept.push(frame);
            } else if let Some(slot) = frame.slot {
                self.pool.give(slot);
            }
        }
        self.frames = kept;
    }

    /// FFmpeg's `ff_hevc_output_frames()`: shows the lowest order count
    /// while more than `reorder` wait, or (with any waiting) the array
    /// holds more than `buffering`.
    fn bump(&mut self, reorder: u32, buffering: u32, outputs: &mut Vec<Output>, discard: bool) {
        loop {
            let waiting = self.frames.iter().filter(|f| f.output).count() as u32;
            let held = self.frames.iter().filter(|f| f.held()).count() as u32;
            if !(waiting > reorder || (waiting > 0 && held > buffering)) {
                return;
            }
            let mut best: Option<usize> = None;
            for (k, frame) in self.frames.iter().enumerate() {
                if frame.output && best.is_none_or(|b| frame.poc < self.frames[b].poc) {
                    best = Some(k);
                }
            }
            let Some(k) = best else { return };
            self.frames[k].output = false;
            let frame = self.frames[k];
            if !discard {
                outputs.push(Output {
                    slot: frame.slot.unwrap_or(u8::MAX),
                    surface: frame.slot.unwrap_or(u8::MAX),
                    poc: frame.poc,
                    decoded: frame.decoded,
                });
            }
            if !frame.held() {
                self.frames.remove(k);
                if let Some(slot) = frame.slot {
                    self.pool.give(slot);
                }
            }
        }
    }

    /// Takes one access unit's NAL units: the parameter sets it carries,
    /// and its picture, if it has slices. The reference state moves on as
    /// if the picture was decoded. None for a unit without a picture, and
    /// for a RASL picture skipped ([`Events::skipped`]). A unit refused
    /// leaves the reference state as it was (the parameter sets it carried
    /// stay), so the stream goes on with the next one.
    pub fn access_unit(&mut self, nals: &[Nal<'_>]) -> Result<Option<Picture>> {
        let saved = self.reference_state();
        self.note = None;
        let result = self.unit(nals);
        let note = self.note.take();
        if let Err(error) = &result {
            let sps = core::mem::take(&mut self.sps);
            let pps = core::mem::take(&mut self.pps);
            *self = Self { sps, pps, ..saved };
            // The counts a refused unit made are rolled back with it: a
            // refusal's log line says what it was instead.
            self.refusal = Some(self.describe_refusal(error, note.as_ref()));
        }
        result
    }

    /// Everything but the parameter sets, which a refused unit keeps.
    fn reference_state(&mut self) -> Self {
        let sps = core::mem::take(&mut self.sps);
        let pps = core::mem::take(&mut self.pps);
        let saved = self.clone();
        self.sps = sps;
        self.pps = pps;
        saved
    }

    /// Whether a reference picture set names a picture the buffer does not
    /// hold, as a CRA's does where the stream was cut or spliced before it.
    fn names_missing(&self, slice: &Slice, poc: i32, max_lsb: i32) -> bool {
        let held = |found: &dyn Fn(&Frame) -> bool| {
            self.frames
                .iter()
                .any(|f| f.slot.is_some() && f.reference() && found(f))
        };
        let short = slice
            .st_rps
            .negative
            .iter()
            .chain(&slice.st_rps.positive)
            .any(|&(delta, _)| !held(&|f| f.poc == poc + delta));
        let long = slice.long_term.iter().any(|&(value, _, msb_present)| {
            if msb_present {
                let target = (poc - (poc & (max_lsb - 1))) + value;
                !held(&|f| f.poc == target)
            } else {
                !held(&|f| f.poc & (max_lsb - 1) == value)
            }
        });
        short || long
    }

    fn unit(&mut self, nals: &[Nal<'_>]) -> Result<Option<Picture>> {
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
                NAL_VPS | NAL_SPS | NAL_PPS => self.parameter_set(nal)?,
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
        // `I`, `P` or `B`, and the slice segments as NVDEC reads them.
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
        self.note = Some(Note {
            nal_type,
            temporal_id: first_nal.hevc_temporal_id(),
            lsb: first.pic_order_cnt_lsb,
            pic_output: first.pic_output,
            kind,
            ..Note::default()
        });
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
        // 8.1.3: the RASL pictures of an IRAP that started afresh are not
        // decoded (they may refer to pictures before it, never decoded
        // here) nor shown. FFmpeg skips them the same way.
        if matches!(nal_type, 8 | 9) && self.skip_rasl {
            self.events.skipped += 1;
            return Ok(None);
        }
        // 8.3.1: the picture order count, as if the picture went on from
        // the one before.
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let lsb = first.pic_order_cnt_lsb as i32;
        let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
        let prev_msb = self.prev_tid0_poc - prev_lsb;
        let going_on = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        } + lsb;
        // A CRA starts afresh (`HandleCraAsBlaFlag`) after a flush, and
        // where its reference picture set names pictures the buffer does not
        // hold: a stream cut or spliced before it (a loop, a seek with no
        // flush), whose RASL pictures would refer to pictures never decoded.
        let restart_cra = nal_type == NAL_CRA
            && !self.starting
            && (self.flushed || self.names_missing(&first, going_on, max_lsb));
        if restart_cra {
            self.events.restarted += 1;
        }
        let no_rasl_output =
            irap && (idr || (16..=18).contains(&nal_type) || self.starting || restart_cra);
        let poc = if no_rasl_output { lsb } else { going_on };
        if let Some(note) = self.note.as_mut() {
            note.poc = Some(poc);
            note.going_on = Some(going_on);
            note.restart_cra = restart_cra;
            note.no_rasl_output = no_rasl_output;
        }
        self.pool.widen(sps.slots());
        let mut outputs = Vec::new();
        // An IDR drops every reference as its header is read; an IRAP
        // that starts afresh then shows (or with no_output_of_prior_pics
        // drops) everything still waiting.
        if idr {
            for frame in &mut self.frames {
                frame.short = false;
                frame.long = false;
            }
            self.release_unheld();
        }
        if irap && no_rasl_output && self.decoded > 0 {
            let discard = first.no_output_of_prior_pics || (nal_type == NAL_CRA && self.starting);
            self.bump(0, 0, &mut outputs, discard);
        }
        // The new picture's place in the array.
        let place = (0..PICTURE_PLACES)
            .find(|p| !self.frames.iter().any(|f| f.place == *p))
            .ok_or(Error::Invalid("every picture place is held"))?;
        self.frames.push(Frame {
            place,
            slot: None,
            poc,
            short: true,
            long: false,
            output: first.pic_output,
            decoded: self.decoded,
        });
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
        if let Some(note) = self.note.as_mut() {
            note.before = before.clone();
            note.after = after.clone();
            note.follow = follow.clone();
            note.long = long_curr
                .iter()
                .map(|&(poc, full)| (poc, true, full))
                .chain(long_follow.iter().map(|&(poc, full)| (poc, false, full)))
                .collect();
        }
        let matches_long = |frame_poc: i32, &(target, full): &(i32, bool)| {
            if full {
                frame_poc == target
            } else {
                frame_poc & (max_lsb - 1) == target
            }
        };
        if !idr {
            let restart = irap && no_rasl_output;
            for frame in self.frames.iter_mut().filter(|f| f.slot.is_some()) {
                let was_reference = frame.reference();
                let was_long = frame.long;
                frame.short = false;
                frame.long = false;
                if restart || !was_reference {
                    continue;
                }
                let long = long_curr
                    .iter()
                    .chain(&long_follow)
                    .any(|lt| matches_long(frame.poc, lt));
                let short = !was_long
                    && !long
                    && (before.contains(&frame.poc)
                        || after.contains(&frame.poc)
                        || follow.contains(&frame.poc));
                frame.long = long;
                frame.short = short;
            }
        }
        // 8.3.3's "no reference picture": a reference never decoded (a
        // seek, a loop or a splice not seen as one, a picture dropped or
        // refused). FFmpeg makes a grey picture in its place; the nearest
        // reference in order count stands in here, and the engine decodes
        // with it. A picture whose whole set names pictures the buffer does
        // not hold (a jump in the stream with no IRAP at it) finds no
        // reference left after the marking: an inter one then takes the
        // nearest picture still decoded, before the unheld are let go.
        let intra = kind == 'I';
        let mut concealed = 0u8;
        const SHORT_MISSING: &str =
            "a short-term reference is not in the buffer, nor any decoded picture";
        let mut st_curr_before = Vec::new();
        for &p in &before {
            if let Some(slot) =
                self.reference_slot(Wanted::Short(p), intra, &mut concealed, SHORT_MISSING)?
            {
                st_curr_before.push(slot);
            }
        }
        let mut st_curr_after = Vec::new();
        for &p in &after {
            if let Some(slot) =
                self.reference_slot(Wanted::Short(p), intra, &mut concealed, SHORT_MISSING)?
            {
                st_curr_after.push(slot);
            }
        }
        let mut lt_curr = Vec::new();
        for &(target, full) in &long_curr {
            let wanted = Wanted::Long {
                target,
                full,
                max_lsb,
            };
            if let Some(slot) = self.reference_slot(
                wanted,
                intra,
                &mut concealed,
                "a long-term reference is not in the buffer, nor any decoded picture",
            )? {
                lt_curr.push(slot);
            }
        }
        if !idr {
            self.release_unheld();
        }
        let references: Vec<Frame> = self
            .frames
            .iter()
            .filter(|f| f.slot.is_some() && f.reference())
            .copied()
            .collect();
        if kind != 'I' && st_curr_before.len() + st_curr_after.len() + lt_curr.len() == 0 {
            return Err(Error::Invalid("an inter picture with no reference"));
        }
        // Bumping (C.5.2.2), then the picture's slot.
        self.bump(
            u32::from(sps.max_num_reorder),
            sps.max_dec_pic_buffering(),
            &mut outputs,
            false,
        );
        let slot = self
            .pool
            .take()
            .ok_or(Error::Invalid("every surface holds a picture"))?;
        let current = self.decoded;
        if let Some(frame) = self.frames.iter_mut().find(|f| f.decoded == current) {
            frame.slot = Some(slot);
        }
        for output in &mut outputs {
            if output.decoded == current {
                output.slot = slot;
                output.surface = slot;
            }
        }
        let high_water = self.pool.high_water().unwrap_or(slot);
        let dpb = references
            .iter()
            .map(|f| Kept {
                slot: f.slot.unwrap_or(0),
                poc: f.poc,
                long_term: f.long,
            })
            .collect();
        let scaling = if sps.scaling_list_enabled {
            pps.scaling.or(sps.scaling)
        } else {
            None
        };
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
            slots: self.pool.most(),
            high_water,
            previous_slot: st_curr_before
                .iter()
                .chain(&st_curr_after)
                .chain(&lt_curr)
                .next()
                .copied()
                .unwrap_or(0),
            poc,
            dpb,
            st_curr_before,
            st_curr_after,
            lt_curr,
            scaling,
            decoded: current,
            outputs,
            concealed,
        };
        // As if decoded.
        if irap {
            self.skip_rasl = no_rasl_output;
            self.last_irap = Some((nal_type, poc, current));
        }
        self.starting = false;
        self.flushed = false;
        self.events.concealed += u32::from(concealed);
        self.decoded += 1;
        let sub_layer_non_reference = nal_type <= 14 && nal_type % 2 == 0;
        if first_nal.hevc_temporal_id() == 0
            && !matches!(nal_type, 6..=9)
            && !sub_layer_non_reference
        {
            self.prev_tid0_poc = poc;
        }
        Ok(Some(picture))
    }

    /// The pictures still waiting to be shown, in output order, as at the
    /// end of the stream. The references stay; a CRA next starts afresh, as
    /// after an end of sequence (a client flushes before it seeks).
    pub fn flush(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        self.bump(0, 0, &mut outputs, false);
        self.flushed = true;
        self.flushed_at = Some(self.decoded);
        outputs
    }

    /// Forgets every picture (a seek): no reference, nothing to show, every
    /// slot free; the parameter sets and the events stay, and the next
    /// picture must be an IRAP.
    pub fn reset(&mut self) {
        let sps = core::mem::take(&mut self.sps);
        let pps = core::mem::take(&mut self.pps);
        let refusal = self.refusal.take();
        *self = Self {
            sps,
            pps,
            events: self.events,
            resets: self.resets + 1,
            refusal,
            ..Self::new()
        };
    }

    /// The slots held now: references and pictures waiting to be shown.
    pub fn held_slots(&self) -> Vec<u8> {
        self.frames.iter().filter_map(|f| f.slot).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_diagonal_scan_goes_up_right() {
        assert_eq!(
            diagonal(4),
            [0, 4, 1, 8, 5, 2, 12, 9, 6, 3, 13, 10, 7, 14, 11, 15]
        );
        assert_eq!(diagonal(8).len(), 64);
    }

    #[test]
    fn the_default_lists_fill_nvidias_buffer_as_captured() {
        // hevc-main-slices-1080p's buffer (x265 scaling-list=default).
        let buffer = ScalingList::default_lists().buffer();
        assert_eq!(buffer[..8], [16; 8]);
        assert_eq!(buffer[8..16], [0; 8]);
        assert!(buffer[16..0x70].iter().all(|&b| b == 16));
        assert_eq!(buffer[0x70..0x78], [16, 16, 16, 16, 17, 18, 21, 24]);
        assert_eq!(buffer[0xa8..0xb0], [24, 25, 29, 36, 47, 65, 88, 115]);
        assert_eq!(buffer[0x130..0x138], [16, 16, 16, 16, 17, 18, 20, 24]);
        assert_eq!(buffer[0x3e8..0x3f0], [24, 25, 28, 33, 41, 54, 71, 91]);
        assert_eq!(buffer[0x3f0..], [0; 16]);
    }
}
