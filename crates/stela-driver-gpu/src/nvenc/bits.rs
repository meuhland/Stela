//! The parameter sets NVENC does not write, and reading Annex B back.
//!
//! The engine writes slice NALs only: in every captured H.264 status
//! `total_bit_count - type1_bit_count` is the 32 bits of the slice's one
//! start code, and the HEVC frames of 2026-11-16 held NAL 19 then NAL 1
//! alone. NVIDIA's driver builds SPS and PPS (and HEVC's VPS) on the CPU;
//! so does this module, from the IDR setup's `sps_data`/`pps_data`, as
//! nvenc-test's `h264.c` and `hevc.c` do.
use super::fields::{h264 as f4, hevc as f5};
use alloc::vec::Vec;

/// An RBSP being written, most significant bit first.
#[derive(Default)]
pub struct Bits {
    bytes: Vec<u8>,
    bits: usize,
}

impl Bits {
    /// `value`'s low `count` bits (at most 32).
    pub fn u(&mut self, count: u32, value: u64) {
        for bit in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if value >> bit & 1 != 0 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    pub fn flag(&mut self, value: bool) {
        self.u(1, u64::from(value));
    }

    /// Exp-Golomb, unsigned.
    pub fn ue(&mut self, value: u64) {
        let x = value + 1;
        let length = 63 - x.leading_zeros();
        self.u(length, 0);
        self.u(length + 1, x);
    }

    /// Exp-Golomb, signed.
    pub fn se(&mut self, value: i64) {
        self.ue(if value > 0 {
            2 * value as u64 - 1
        } else {
            2 * value.unsigned_abs()
        });
    }

    /// `rbsp_trailing_bits`.
    pub fn trailing(&mut self) {
        self.u(1, 1);
        while !self.bits.is_multiple_of(8) {
            self.u(1, 0);
        }
    }

    /// A 4-byte start code, the NAL header and the RBSP with emulation
    /// prevention, appended to `out`.
    pub fn nal(&self, header: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(header);
        let mut zeros = 0;
        for &byte in &self.bytes {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
    }
}

fn high_profile(profile: u64) -> bool {
    matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// The H.264 SPS and PPS for the stream `setup` (an IDR's) begins, of a
/// `width` x `height` picture: cropping the coded size to it, timing from
/// the setup's frame rate, no reordering (no B-frames).
pub fn h264_parameter_sets(setup: &[u8], width: u32, height: u32) -> Vec<u8> {
    let get = |field: super::setup::Field| field.get(setup);
    let profile = get(f4::SPS_DATA_PROFILE_IDC);
    let chroma_format = get(f4::SPS_DATA_CHROMA_FORMAT_IDC);
    let frame_mbs_only = get(f4::SPS_DATA_FRAME_MBS_ONLY);
    let mbw = (get(f4::INPUT_CFG_FRAME_WIDTH_MINUS1) + 16) / 16;
    let mbh = (get(f4::INPUT_CFG_FRAME_HEIGHT_MINUS1) + 16) / 16;
    let max_refs = get(f4::PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1) + 1;
    let fps8 = get(f4::RATE_CONTROL_FRAMERATE);
    let (width, height) = (u64::from(width), u64::from(height));
    let mut out = Vec::new();

    let mut b = Bits::default();
    b.u(8, profile);
    b.u(8, 0); // constraint_set flags
    b.u(8, get(f4::SPS_DATA_LEVEL_IDC));
    b.ue(0); // seq_parameter_set_id
    if high_profile(profile) {
        b.ue(chroma_format);
        if chroma_format == 3 {
            b.u(1, get(f4::SPS_DATA_SEPARATE_COLOUR_PLANE_FLAG));
        }
        b.ue(0); // bit_depth_luma_minus8
        b.ue(0); // bit_depth_chroma_minus8
        b.u(1, get(f4::SPS_DATA_LOSSLESS_QPPRIME_FLAG));
        b.u(1, 0); // seq_scaling_matrix_present_flag
    }
    b.ue(get(f4::SPS_DATA_LOG2_MAX_FRAME_NUM_MINUS4));
    let poc_type = get(f4::SPS_DATA_PIC_ORDER_CNT_TYPE);
    b.ue(poc_type);
    if poc_type == 0 {
        b.ue(get(f4::SPS_DATA_LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4));
    }
    b.ue(max_refs); // max_num_ref_frames
    b.u(1, 0); // gaps_in_frame_num_value_allowed_flag
    b.ue(mbw - 1);
    b.ue(if frame_mbs_only != 0 { mbh } else { mbh / 2 } - 1);
    b.u(1, frame_mbs_only);
    if frame_mbs_only == 0 {
        b.u(1, 0); // mb_adaptive_frame_field_flag
    }
    b.u(1, 1); // direct_8x8_inference_flag
    if mbw * 16 != width || mbh * 16 != height {
        b.u(1, 1); // frame_cropping_flag: right and bottom, 4:2:0 units of 2
        b.ue(0);
        b.ue((mbw * 16 - width) / 2);
        b.ue(0);
        b.ue((mbh * 16 - height) / if frame_mbs_only != 0 { 2 } else { 4 });
    } else {
        b.u(1, 0);
    }
    b.u(1, 1); // vui_parameters_present_flag
    b.u(1, 0); // aspect_ratio_info_present_flag
    b.u(1, 0); // overscan_info_present_flag
    b.u(1, 0); // video_signal_type_present_flag
    b.u(1, 0); // chroma_loc_info_present_flag
    b.flag(fps8 != 0); // timing_info_present_flag
    if fps8 != 0 {
        b.u(32, 256); // num_units_in_tick: fps = time_scale / (2 * 256)
        b.u(32, 2 * fps8); // time_scale
        b.u(1, 1); // fixed_frame_rate_flag
    }
    b.u(1, 0); // nal_hrd_parameters_present_flag
    b.u(1, 0); // vcl_hrd_parameters_present_flag
    b.u(1, 0); // pic_struct_present_flag
    b.u(1, 1); // bitstream_restriction_flag: no reordering
    b.u(1, 1); // motion_vectors_over_pic_boundaries_flag
    b.ue(0); // max_bytes_per_pic_denom
    b.ue(0); // max_bits_per_mb_denom
    b.ue(16); // log2_max_mv_length_horizontal
    b.ue(16); // log2_max_mv_length_vertical
    b.ue(0); // max_num_reorder_frames
    b.ue(max_refs); // max_dec_frame_buffering
    b.trailing();
    b.nal(&[0x67], &mut out); // nal_ref_idc 3, type 7

    let mut b = Bits::default();
    let chroma_qp = f4::PPS_DATA_CHROMA_QP_INDEX_OFFSET.signed(setup);
    let second_chroma_qp = f4::PPS_DATA_SECOND_CHROMA_QP_INDEX_OFFSET.signed(setup);
    let transform_8x8 = get(f4::PPS_DATA_TRANSFORM_8X8_MODE_FLAG);
    b.ue(get(f4::PPS_DATA_PIC_PARAM_SET_ID));
    b.ue(0); // seq_parameter_set_id
    b.u(1, get(f4::PPS_DATA_ENTROPY_CODING_MODE_FLAG));
    b.u(1, get(f4::PPS_DATA_PIC_ORDER_PRESENT_FLAG));
    b.ue(0); // num_slice_groups_minus1
    b.ue(get(f4::PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1));
    b.ue(get(f4::PPS_DATA_NUM_REF_IDX_L1_ACTIVE_MINUS1));
    b.u(1, get(f4::PPS_DATA_WEIGHTED_PRED_FLAG));
    b.u(2, get(f4::PPS_DATA_WEIGHTED_BIPRED_IDC));
    b.se(f4::PPS_DATA_PIC_INIT_QP_MINUS26.signed(setup));
    b.se(0); // pic_init_qs_minus26
    b.se(chroma_qp);
    b.u(1, get(f4::PPS_DATA_DEBLOCKING_FILTER_CONTROL_PRESENT_FLAG));
    b.u(1, get(f4::PPS_DATA_CONSTRAINED_INTRA_PRED_FLAG));
    b.u(1, 0); // redundant_pic_cnt_present_flag
    if transform_8x8 != 0 || second_chroma_qp != chroma_qp {
        b.u(1, transform_8x8);
        b.u(1, 0); // pic_scaling_matrix_present_flag
        b.se(second_chroma_qp);
    }
    b.trailing();
    b.nal(&[0x68], &mut out); // nal_ref_idc 3, type 8
    out
}

/// HEVC level 4.1 (x30), which 1920x1088 at 60 fps and 20 Mb/s fit; NVIDIA's
/// own choice was not captured.
pub const HEVC_LEVEL_4_1: u64 = 123;
/// Level 5.1, for a picture past 4.1's luma samples.
pub const HEVC_LEVEL_5_1: u64 = 153;
const HEVC_LEVEL_4_1_SAMPLES: u64 = 2_228_224;

/// `profile_tier_level(1, 0)`: the general profile only, no sub-layers.
fn profile_tier_level(b: &mut Bits, profile: u64, level: u64) {
    b.u(2, 0); // general_profile_space
    b.u(1, 0); // general_tier_flag: Main
    b.u(5, profile);
    // general_profile_compatibility_flag[j], j = 0 first: Main is also
    // Main 10 compatible.
    let compatible = (0x8000_0000u64 >> profile) | if profile == 1 { 0x8000_0000 >> 2 } else { 0 };
    b.u(32, compatible);
    b.u(1, 1); // progressive_source_flag
    b.u(1, 0); // interlaced_source_flag
    b.u(1, 0); // non_packed_constraint_flag
    b.u(1, 1); // frame_only_constraint_flag
    b.u(32, 0); // 43 reserved zero bits...
    b.u(11, 0);
    b.u(1, 0); // ...and general_inbld_flag
    b.u(8, level);
}

fn timing(b: &mut Bits, fps8: u64) {
    b.u(32, 256); // num_units_in_tick: fps = time_scale / 256
    b.u(32, fps8); // time_scale
}

/// The HEVC VPS, SPS and PPS for the stream `setup` (an IDR's) begins,
/// keeping up to `max_refs` earlier pictures (the 4 that fill the 5
/// reconstructed surfaces with the current one): Main, the coded size with a
/// conformance window to `width` x `height`, the setup's CTB, CB and TB
/// sizes, one short-term RPS (the slices code their own, but its count
/// shapes their headers). None for a setup this writer cannot express.
pub fn hevc_parameter_sets(
    setup: &[u8],
    width: u32,
    height: u32,
    max_refs: u64,
) -> Option<Vec<u8>> {
    let get = |field: super::setup::Field| field.get(setup);
    let min_cu = get(f5::SPS_DATA_MIN_CU_SIZE);
    let max_cu = get(f5::SPS_DATA_MAX_CU_SIZE);
    let min_tu = get(f5::SPS_DATA_MIN_TU_SIZE);
    let max_tu = get(f5::SPS_DATA_MAX_TU_SIZE);
    let (min_cb, max_cb) = (8u64 << min_cu, 8u64 << max_cu);
    let coded_w = (get(f5::INPUT_CFG_FRAME_WIDTH_MINUS1) + min_cb) / min_cb * min_cb;
    let coded_h = (get(f5::INPUT_CFG_FRAME_HEIGHT_MINUS1) + min_cb) / min_cb * min_cb;
    let bit_depth = get(f5::SPS_DATA_BIT_DEPTH_MINUS_8);
    let (width, height) = (u64::from(width), u64::from(height));
    if get(f5::SPS_DATA_CHROMA_FORMAT_IDC) != 1
        || get(f5::SPS_DATA_SEPARATE_COLOUR_PLANE_FLAG) != 0
        || get(f5::SPS_DATA_PCM_ENABLED_FLAG) != 0
        || get(f5::SPS_DATA_LONG_TERM_REF_PICS_PRESENT_FLAG) != 0
        || max_cb < min_cb
        || max_tu < min_tu
        || coded_w < width
        || coded_h < height
        || (bit_depth != 0 && bit_depth != 2)
    {
        return None;
    }
    let profile = if bit_depth != 0 { 2 } else { 1 };
    let level = if coded_w * coded_h <= HEVC_LEVEL_4_1_SAMPLES {
        HEVC_LEVEL_4_1
    } else {
        HEVC_LEVEL_5_1
    };
    let fps8 = get(f5::RATE_CONTROL_FRAMERATE);
    let mut out = Vec::new();
    let nal = |b: &Bits, kind: u8, out: &mut Vec<u8>| b.nal(&[kind << 1, 1], out);

    let mut b = Bits::default();
    b.u(4, 0); // vps_video_parameter_set_id
    b.u(1, 1); // vps_base_layer_internal_flag
    b.u(1, 1); // vps_base_layer_available_flag
    b.u(6, 0); // vps_max_layers_minus1
    b.u(3, 0); // vps_max_sub_layers_minus1
    b.u(1, 1); // vps_temporal_id_nesting_flag
    b.u(16, 0xffff); // vps_reserved_0xffff_16bits
    profile_tier_level(&mut b, profile, level);
    b.u(1, 1); // vps_sub_layer_ordering_info_present_flag
    b.ue(max_refs); // vps_max_dec_pic_buffering_minus1
    b.ue(0); // vps_max_num_reorder_pics
    b.ue(0); // vps_max_latency_increase_plus1
    b.u(6, 0); // vps_max_layer_id
    b.ue(0); // vps_num_layer_sets_minus1
    b.flag(fps8 != 0); // vps_timing_info_present_flag
    if fps8 != 0 {
        timing(&mut b, fps8);
        b.u(1, 0); // vps_poc_proportional_to_timing_flag
        b.ue(0); // vps_num_hrd_parameters
    }
    b.u(1, 0); // vps_extension_flag
    b.trailing();
    nal(&b, 32, &mut out);

    let mut b = Bits::default();
    b.u(4, 0); // sps_video_parameter_set_id
    b.u(3, 0); // sps_max_sub_layers_minus1
    b.u(1, 1); // sps_temporal_id_nesting_flag
    profile_tier_level(&mut b, profile, level);
    b.ue(0); // sps_seq_parameter_set_id
    b.ue(get(f5::SPS_DATA_CHROMA_FORMAT_IDC));
    b.ue(coded_w);
    b.ue(coded_h);
    if coded_w != width || coded_h != height {
        b.u(1, 1); // conformance_window_flag: right and bottom, units of 2
        b.ue(0);
        b.ue((coded_w - width) / 2);
        b.ue(0);
        b.ue((coded_h - height) / 2);
    } else {
        b.u(1, 0);
    }
    b.ue(bit_depth); // bit_depth_luma_minus8
    b.ue(bit_depth); // bit_depth_chroma_minus8
    b.ue(get(f5::SPS_DATA_LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4));
    b.u(1, 1); // sps_sub_layer_ordering_info_present_flag
    b.ue(max_refs); // sps_max_dec_pic_buffering_minus1
    b.ue(0); // sps_max_num_reorder_pics
    b.ue(0); // sps_max_latency_increase_plus1
    b.ue(min_cu); // log2_min_luma_coding_block_size_minus3
    b.ue(max_cu - min_cu);
    b.ue(min_tu); // log2_min_luma_transform_block_size_minus2
    b.ue(max_tu - min_tu);
    b.ue(get(f5::SPS_DATA_MAX_TRANSFORM_DEPTH_INTER));
    b.ue(get(f5::SPS_DATA_MAX_TRANSFORM_DEPTH_INTRA));
    b.u(1, 0); // scaling_list_enabled_flag
    b.u(1, get(f5::SPS_DATA_AMP_ENABLED_FLAG));
    b.u(1, get(f5::SPS_DATA_SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG));
    b.u(1, 0); // pcm_enabled_flag (checked above)
    let rps = get(f5::SPS_DATA_NUM_SHORT_TERM_REF_PIC_SETS);
    b.ue(rps);
    for index in 0..rps {
        // st_ref_pic_set(index): the previous picture, used.
        if index != 0 {
            b.u(1, 0); // inter_ref_pic_set_prediction_flag
        }
        b.ue(1); // num_negative_pics
        b.ue(0); // num_positive_pics
        b.ue(0); // delta_poc_s0_minus1[0]
        b.u(1, 1); // used_by_curr_pic_s0_flag[0]
    }
    b.u(1, 0); // long_term_ref_pics_present_flag (checked above)
    b.u(1, get(f5::SPS_DATA_SPS_TEMPORAL_MVP_ENABLED_FLAG));
    b.u(1, get(f5::SPS_DATA_STRONG_INTRA_SMOOTHING_ENABLED_FLAG));
    b.u(1, 1); // vui_parameters_present_flag
    for _ in 0..8 {
        // aspect ratio, overscan, video signal type, chroma location,
        // neutral chroma, field_seq, frame field info, default display
        // window: none.
        b.u(1, 0);
    }
    b.flag(fps8 != 0); // vui_timing_info_present_flag
    if fps8 != 0 {
        timing(&mut b, fps8);
        b.u(1, 0); // vui_poc_proportional_to_timing_flag
        b.u(1, 0); // vui_hrd_parameters_present_flag
    }
    b.u(1, 0); // bitstream_restriction_flag
    b.u(1, 0); // sps_extension_present_flag
    b.trailing();
    nal(&b, 33, &mut out);

    let mut b = Bits::default();
    let cu_qp_delta = get(f5::PPS_DATA_CU_QP_DELTA_ENABLED_FLAG);
    let override_enabled = get(f5::PPS_DATA_DEBLOCKING_FILTER_OVERRIDE_ENABLED_FLAG);
    b.ue(get(f5::PPS_DATA_PIC_PARAM_SET_ID));
    b.ue(0); // pps_seq_parameter_set_id
    b.u(1, get(f5::PPS_DATA_DEPENDENT_SLICE_SEGMENTS_ENABLED_FLAG));
    b.u(1, get(f5::PPS_DATA_OUTPUT_FLAG_PRESENT_FLAG));
    b.u(3, get(f5::PPS_DATA_NUM_EXTRA_SLICE_HEADER_BITS));
    b.u(1, get(f5::PPS_DATA_SIGN_DATA_HIDING_ENABLED_FLAG));
    b.u(1, get(f5::PPS_DATA_CABAC_INIT_PRESENT_FLAG));
    b.ue(get(f5::PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1));
    b.ue(get(f5::PPS_DATA_NUM_REF_IDX_L1_ACTIVE_MINUS1));
    b.se(f5::PPS_DATA_PIC_INIT_QP_MINUS26.signed(setup));
    b.u(1, get(f5::PPS_DATA_CONSTRAINED_INTRA_PRED_FLAG));
    b.u(1, get(f5::PPS_DATA_TRANSFORM_SKIP_ENABLED_FLAG));
    b.u(1, cu_qp_delta);
    if cu_qp_delta != 0 {
        b.ue(get(f5::PPS_DATA_DIFF_CU_QP_DELTA_DEPTH));
    }
    b.se(f5::PPS_DATA_PPS_CB_QP_OFFSET.signed(setup));
    b.se(f5::PPS_DATA_PPS_CR_QP_OFFSET.signed(setup));
    b.u(1, get(f5::PPS_DATA_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT));
    b.u(1, get(f5::PPS_DATA_WEIGHTED_PRED_FLAG));
    b.u(1, get(f5::PPS_DATA_WEIGHTED_BIPRED_FLAG));
    b.u(1, get(f5::PPS_DATA_TRANSQUANT_BYPASS_ENABLED_FLAG));
    b.u(1, 0); // tiles_enabled_flag
    b.u(1, 0); // entropy_coding_sync_enabled_flag
    b.u(
        1,
        get(f5::PPS_DATA_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG),
    );
    // deblocking_filter_control_present_flag, only to allow the slices'
    // overrides; deblocking stays on with zero offsets.
    b.u(1, override_enabled);
    if override_enabled != 0 {
        b.u(1, 1); // deblocking_filter_override_enabled_flag
        b.u(1, 0); // pps_deblocking_filter_disabled_flag
        b.se(0); // pps_beta_offset_div2
        b.se(0); // pps_tc_offset_div2
    }
    b.u(1, 0); // pps_scaling_list_data_present_flag
    b.u(1, get(f5::PPS_DATA_LISTS_MODIFICATION_PRESENT_FLAG));
    b.ue(get(f5::PPS_DATA_LOG2_PARALLEL_MERGE_LEVEL_MINUS2));
    b.u(1, 0); // slice_segment_header_extension_present_flag
    b.u(1, 0); // pps_extension_present_flag
    b.trailing();
    nal(&b, 34, &mut out);
    Some(out)
}

/// Where Annex B start codes (`00 00 01`, possibly after a fourth zero)
/// begin in `data`: each item is the offset of the NAL header after one.
pub fn nal_starts(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
}

/// The NAL unit types in `data`, in order: H.264's 5-bit type or HEVC's
/// 6-bit one.
pub fn nal_types(data: &[u8], hevc: bool) -> Vec<u8> {
    nal_starts(data)
        .into_iter()
        .filter(|&start| start < data.len())
        .map(|start| {
            if hevc {
                (data[start] >> 1) & 0x3f
            } else {
                data[start] & 0x1f
            }
        })
        .collect()
}

/// Whether a start code (3 or 4 bytes) begins at `at`.
pub fn start_code_at(data: &[u8], at: usize) -> bool {
    let rest = data.get(at..).unwrap_or(&[]);
    rest.starts_with(&[0, 0, 1]) || rest.starts_with(&[0, 0, 0, 1])
}
