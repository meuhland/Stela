//! NVDEC picture setups (`nvdec_h264_pic_s`, `nvdec_hevc_pic_s`), filled
//! from a parsed picture the way NVIDIA's driver fills them
//! (`nvdec-conventions.md`): every field from the parameter sets, the slice
//! header and the decoded-picture buffer, plus what the surfaces and the
//! engine's own buffers fix. No captured setup is patched: the tests check
//! that each of the captured clips' setups comes out byte for byte.
//!
//! A setup lives at the start of a 4 KiB page of system memory, followed on
//! that page by the frame's other small inputs and its status:
//!
//! - H.264: the setup, the slice offsets at +0x300 (a `u32` byte offset of
//!   each slice's start code in the bitstream), the status at +0x700;
//! - HEVC: the setup, the scaling lists at +0x200 (1 KiB, zero when the
//!   sequence has none: [`hevc::ScalingList::buffer`]), the tile sizes at
//!   +0x600 (a `u16` pair of CTB columns and rows per tile), the status at
//!   +0xd00.
//!
//! [`h264_page`] and [`hevc_page`] build the whole page but the status.
use super::fields::{h264 as f4, hevc as f5};
use super::parse::{h264, hevc};
use crate::nvenc::layout::Geometry;
use crate::nvenc::setup::Field;
use alloc::vec;
use alloc::vec::Vec;

/// The bytes of a setup page.
pub const PAGE_BYTES: usize = 0x1000;
/// H.264's slice offsets and status on the setup page.
pub const H264_SLICE_OFFSETS: usize = 0x300;
pub const H264_STATUS: usize = 0x700;
/// HEVC's scaling list, tile sizes and status on the setup page.
pub const HEVC_SCALING_LIST: usize = 0x200;
pub const HEVC_TILE_SIZES: usize = 0x600;
pub const HEVC_STATUS: usize = 0xd00;

/// The setups' `tileFormat`: KBL, the block-linear layout every captured
/// surface had, with `gob_height` 0 (GOB_2: blocks of two 8-row GOBs).
pub const TILE_FORMAT_KBL: u64 = 1;
pub const GOB_HEIGHT_2: u64 = 0;

/// Why a picture's setup cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// `gptimer_timeout_value`: 10,000 cycles per 16x16 block of the coded
/// picture, at least 81,000,000 (what NVIDIA's driver gave 720p and 1080p
/// alike; 1920x1088 got 81,600,000 and 3840x2160 324,000,000).
pub fn gptimer(blocks: u32) -> u32 {
    blocks.saturating_mul(10_000).max(81_000_000)
}

fn put(setup: &mut [u8], field: Field, value: u64) {
    field.put(setup, value);
}

fn put_signed(setup: &mut [u8], field: Field, value: i32) {
    field.put(setup, i64::from(value) as u64);
}

/// The H.264 surfaces' bottom-field offsets, in units of 256 bytes: half a
/// plane whose rows round up to 32 (NVIDIA's driver gave 1088 rows 4080 and
/// 720 rows, coded 720 of 736, 1840).
fn bottom_offset(pitch: u32, rows: u32) -> u64 {
    u64::from(pitch) * u64::from(rows.next_multiple_of(32)) / 2 / 256
}

/// `mbhist_buffer_size`: 104 bytes per macroblock column, in whole 256s.
pub fn h264_mbhist_bytes(width_mbs: u32) -> u32 {
    (width_mbs * 104).next_multiple_of(256)
}

/// `HistBufferSize`, in units of 256 bytes: 768 bytes per macroblock column.
pub fn h264_history_units(width_mbs: u32) -> u32 {
    width_mbs * 3
}

/// The setup of `picture`, whose surfaces have `geometry`.
pub fn h264(picture: &h264::Picture, geometry: &Geometry) -> Result<Vec<u8>, Refused> {
    let sps = &picture.sps;
    let pps = &picture.pps;
    if geometry.coded_width != sps.coded_width() || geometry.coded_height != sps.coded_height() {
        return Err(Refused("the surfaces are not the sequence's size"));
    }
    let mut s = vec![0u8; f4::SIZE];
    // Two end-of-stream NALs after the data (`00 00 01 0b`), explicitly.
    for at in [2, 10] {
        put(&mut s, f4::EOS[at], 1);
        put(&mut s, f4::EOS[at + 1], 0x0b);
    }
    put(&mut s, f4::EXPLICIT_EOSPRESENT_FLAG, 1);
    put(&mut s, f4::STREAM_LEN, picture.bitstream.len() as u64);
    put(&mut s, f4::SLICE_COUNT, picture.slice_offsets.len() as u64);
    put(
        &mut s,
        f4::MBHIST_BUFFER_SIZE,
        h264_mbhist_bytes(sps.width_mbs).into(),
    );
    put(
        &mut s,
        f4::GPTIMER_TIMEOUT_VALUE,
        gptimer(sps.width_mbs * sps.height_mbs()).into(),
    );
    if sps.pic_order_cnt_type == 0 {
        put(
            &mut s,
            f4::LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4,
            u64::from(sps.log2_max_poc_lsb - 4),
        );
    }
    put(
        &mut s,
        f4::DELTA_PIC_ORDER_ALWAYS_ZERO_FLAG,
        sps.delta_pic_order_always_zero.into(),
    );
    put(&mut s, f4::FRAME_MBS_ONLY_FLAG, sps.frame_mbs_only.into());
    put(&mut s, f4::PIC_WIDTH_IN_MBS, sps.width_mbs.into());
    put(&mut s, f4::FRAME_HEIGHT_IN_MBS, sps.height_mbs().into());
    put(&mut s, f4::TILE_FORMAT, TILE_FORMAT_KBL);
    put(&mut s, f4::GOB_HEIGHT, GOB_HEIGHT_2);
    put(
        &mut s,
        f4::ENTROPY_CODING_MODE_FLAG,
        pps.entropy_coding_mode.into(),
    );
    put(
        &mut s,
        f4::PIC_ORDER_PRESENT_FLAG,
        pps.bottom_field_pic_order_in_frame_present.into(),
    );
    put(
        &mut s,
        f4::NUM_REF_IDX_L0_ACTIVE_MINUS1,
        pps.num_ref_idx_l0_default_minus1.into(),
    );
    put(
        &mut s,
        f4::NUM_REF_IDX_L1_ACTIVE_MINUS1,
        pps.num_ref_idx_l1_default_minus1.into(),
    );
    put(
        &mut s,
        f4::DEBLOCKING_FILTER_CONTROL_PRESENT_FLAG,
        pps.deblocking_filter_control_present.into(),
    );
    put(
        &mut s,
        f4::REDUNDANT_PIC_CNT_PRESENT_FLAG,
        pps.redundant_pic_cnt_present.into(),
    );
    put(
        &mut s,
        f4::TRANSFORM_8X8_MODE_FLAG,
        pps.transform_8x8_mode.into(),
    );
    let pitch = geometry.pitch;
    put(&mut s, f4::PITCH_LUMA, pitch.into());
    put(&mut s, f4::PITCH_CHROMA, pitch.into());
    put(
        &mut s,
        f4::LUMA_BOT_OFFSET,
        bottom_offset(pitch, geometry.coded_height),
    );
    put(
        &mut s,
        f4::CHROMA_BOT_OFFSET,
        bottom_offset(pitch, geometry.coded_height / 2),
    );
    put(
        &mut s,
        f4::HIST_BUFFER_SIZE,
        h264_history_units(sps.width_mbs).into(),
    );
    put(
        &mut s,
        f4::DIRECT_8X8_INFERENCE_FLAG,
        sps.direct_8x8_inference.into(),
    );
    put(&mut s, f4::WEIGHTED_PRED_FLAG, pps.weighted_pred.into());
    put(
        &mut s,
        f4::CONSTRAINED_INTRA_PRED_FLAG,
        pps.constrained_intra_pred.into(),
    );
    put(
        &mut s,
        f4::REF_PIC_FLAG,
        u64::from(picture.nal_ref_idc != 0),
    );
    put(
        &mut s,
        f4::LOG2_MAX_FRAME_NUM_MINUS4,
        u64::from(sps.log2_max_frame_num - 4),
    );
    put(&mut s, f4::CHROMA_FORMAT_IDC, sps.chroma_format_idc.into());
    put(
        &mut s,
        f4::PIC_ORDER_CNT_TYPE,
        sps.pic_order_cnt_type.into(),
    );
    put_signed(&mut s, f4::PIC_INIT_QP_MINUS26, pps.pic_init_qp_minus26);
    put_signed(
        &mut s,
        f4::CHROMA_QP_INDEX_OFFSET,
        pps.chroma_qp_index_offset,
    );
    put_signed(
        &mut s,
        f4::SECOND_CHROMA_QP_INDEX_OFFSET,
        pps.second_chroma_qp_index_offset,
    );
    put(
        &mut s,
        f4::WEIGHTED_BIPRED_IDC,
        pps.weighted_bipred_idc.into(),
    );
    put(&mut s, f4::CURR_PIC_IDX, picture.slot.into());
    put(&mut s, f4::CURR_COL_IDX, picture.slot.into());
    put(&mut s, f4::FRAME_NUM, picture.frame_num.into());
    for field in 0..2 {
        put_signed(&mut s, f4::CURR_FIELD_ORDER_CNT[field], picture.poc[field]);
    }
    for (place, entry) in picture.dpb.iter().enumerate() {
        match entry {
            Some(reference) => {
                put(&mut s, f4::DPB_INDEX[place], reference.slot.into());
                put(&mut s, f4::DPB_COL_IDX[place], reference.slot.into());
                put(&mut s, f4::DPB_STATE[place], 3);
                let (long, marking, frame_idx) = match reference.long_term {
                    Some(index) => (1, 2, index),
                    None => (0, 1, reference.frame_num),
                };
                put(&mut s, f4::DPB_IS_LONG_TERM[place], long);
                put(&mut s, f4::DPB_TOP_FIELD_MARKING[place], marking);
                put(&mut s, f4::DPB_BOTTOM_FIELD_MARKING[place], marking);
                for field in 0..2 {
                    put_signed(
                        &mut s,
                        f4::DPB_FIELD_ORDER_CNT[place * 2 + field],
                        reference.poc[field],
                    );
                }
                put(&mut s, f4::DPB_FRAME_IDX[place], frame_idx.into());
            }
            None => {
                // NVIDIA's driver names the previous picture's surface here.
                put(&mut s, f4::DPB_INDEX[place], picture.previous_slot.into());
                put(&mut s, f4::DPB_COL_IDX[place], picture.previous_slot.into());
            }
        }
    }
    // The scaling matrices in force, in raster order: the six 4x4 lists,
    // then the intra and inter Y 8x8 lists (flat 16 without matrices).
    let scaling = &pps.scaling;
    let fours = scaling.list4x4.iter().flatten();
    let eights = scaling.list8x8.iter().flatten();
    for (field, value) in f4::WEIGHT_SCALE.iter().zip(fours) {
        put(&mut s, *field, u64::from(*value));
    }
    for (field, value) in f4::WEIGHT_SCALE8X8.iter().zip(eights) {
        put(&mut s, *field, u64::from(*value));
    }
    put(
        &mut s,
        f4::QPPRIME_Y_ZERO_TRANSFORM_BYPASS_FLAG,
        sps.qpprime_y_zero_transform_bypass.into(),
    );
    put(
        &mut s,
        f4::DISPLAY_PARA_HISTOGRAM_END_X,
        u64::from(geometry.coded_width - 1),
    );
    put(
        &mut s,
        f4::DISPLAY_PARA_HISTOGRAM_END_Y,
        u64::from(geometry.coded_height - 1),
    );
    Ok(s)
}

/// Where the HEVC engine's filter buffer keeps each of its areas, in units
/// of 256 bytes from its start, and the buffer's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcFilter {
    pub sao: u32,
    pub bsd_ctrl: u32,
    pub flt_above: u32,
    pub sao_above: u32,
    pub slice_edge: u32,
    /// `SET_INTRA_TOP_BUF_OFFSET`, in bytes into the filter buffer.
    pub intra_top: u64,
    pub filter_bytes: u64,
}

/// The room left for the intra-prediction top row at the filter buffer's
/// end: NVIDIA's buffers left 1,216,000 bytes (1080p) and 408,576 bytes
/// (2160p Main 10) there.
const HEVC_INTRA_TOP_BYTES: u64 = 0x4_0000;

/// The filter layout of a `width` x `height` picture of `bit_depth`, as
/// NVIDIA's driver lays it out. Fitted on the two captured sizes, 1920x1080
/// (and 1088) at 8 bits and 3840x2160 at 10, which it gives exactly: with
/// `rows` the height rounded up to 64 and `row` the width rounded up to 64
/// in bytes (2 per sample above 8 bits),
///
/// - deblocking at 0, 608 bytes per row;
/// - SAO (`HevcSaoBufferOffset`), 4864 bytes per row;
/// - BSD control (`HevcBsdCtrlOffset`), 152 bytes per row;
/// - filter above (`HevcFltAboveOffset`), `row + 16` bytes per row;
/// - SAO above and slice edge (`HevcSaoAboveOffset`,
///   `HevcSliceEdgeOffset`, the same place), `row / 2` bytes per row;
/// - the intra top row ([`HEVC_INTRA_TOP_BYTES`]), the buffer then
///   rounded up to 2 MiB.
pub fn hevc_filter(width: u32, height: u32, bit_depth: u8) -> HevcFilter {
    let rows = u64::from(height.next_multiple_of(64));
    let row = u64::from(width.next_multiple_of(64)) * if bit_depth > 8 { 2 } else { 1 };
    let units = |bytes: u64| (bytes / 256) as u32;
    let sao = units(608 * rows);
    let bsd_ctrl = sao + units(4864 * rows);
    let flt_above = bsd_ctrl + units(152 * rows);
    let sao_above = flt_above + units((row + 16) * rows);
    let intra_top = u64::from(sao_above + units(row / 2 * rows)) * 256;
    HevcFilter {
        sao,
        bsd_ctrl,
        flt_above,
        sao_above,
        slice_edge: sao_above,
        intra_top,
        filter_bytes: (intra_top + HEVC_INTRA_TOP_BYTES).next_multiple_of(0x20_0000),
    }
}

/// `colMvBuffersize`: 16 bytes per 16x16 block of the CTB-aligned picture,
/// in units of 256.
pub fn hevc_col_mv_units(sps: &hevc::Sps) -> u32 {
    let ctb = sps.ctb_size();
    let width = sps.width.next_multiple_of(ctb);
    let height = sps.height.next_multiple_of(ctb);
    (width / 16) * (height / 16) * 16 / 256
}

/// The setup of `picture`, whose surfaces have `geometry`.
pub fn hevc(picture: &hevc::Picture, geometry: &Geometry) -> Result<Vec<u8>, Refused> {
    let sps = &picture.sps;
    let pps = &picture.pps;
    let filter = hevc_filter(sps.width, sps.height, sps.bit_depth_luma);
    if geometry.coded_width < sps.width || geometry.coded_height < sps.height {
        return Err(Refused("the surfaces are smaller than the sequence"));
    }
    let mut s = vec![0u8; f5::SIZE];
    put(&mut s, f5::STREAM_LEN, picture.bitstream.len() as u64);
    put(
        &mut s,
        f5::GPTIMER_TIMEOUT_VALUE,
        gptimer(sps.width * sps.height / 256).into(),
    );
    put(&mut s, f5::TILEFORMAT, TILE_FORMAT_KBL);
    put(&mut s, f5::GOB_HEIGHT, GOB_HEIGHT_2);
    put(&mut s, f5::SW_START_CODE_E, 1);
    for field in f5::FRAMESTRIDE {
        put(&mut s, field, geometry.pitch.into());
    }
    put(&mut s, f5::COL_MV_BUFFERSIZE, hevc_col_mv_units(sps).into());
    put(&mut s, f5::HEVC_SAO_BUFFER_OFFSET, filter.sao.into());
    put(&mut s, f5::HEVC_BSD_CTRL_OFFSET, filter.bsd_ctrl.into());
    put(&mut s, f5::PIC_WIDTH_IN_LUMA_SAMPLES, sps.width.into());
    put(&mut s, f5::PIC_HEIGHT_IN_LUMA_SAMPLES, sps.height.into());
    put(&mut s, f5::CHROMA_FORMAT_IDC, sps.chroma_format_idc.into());
    put(&mut s, f5::BIT_DEPTH_LUMA, sps.bit_depth_luma.into());
    put(&mut s, f5::BIT_DEPTH_CHROMA, sps.bit_depth_chroma.into());
    put(
        &mut s,
        f5::LOG2_MIN_LUMA_CODING_BLOCK_SIZE,
        sps.log2_min_cb.into(),
    );
    put(
        &mut s,
        f5::LOG2_MAX_LUMA_CODING_BLOCK_SIZE,
        sps.log2_max_cb.into(),
    );
    put(
        &mut s,
        f5::LOG2_MIN_TRANSFORM_BLOCK_SIZE,
        sps.log2_min_tb.into(),
    );
    put(
        &mut s,
        f5::LOG2_MAX_TRANSFORM_BLOCK_SIZE,
        sps.log2_max_tb.into(),
    );
    put(
        &mut s,
        f5::MAX_TRANSFORM_HIERARCHY_DEPTH_INTER,
        sps.max_transform_hierarchy_depth_inter.into(),
    );
    put(
        &mut s,
        f5::MAX_TRANSFORM_HIERARCHY_DEPTH_INTRA,
        sps.max_transform_hierarchy_depth_intra.into(),
    );
    put(
        &mut s,
        f5::SCALING_LIST_ENABLE,
        sps.scaling_list_enabled.into(),
    );
    put(&mut s, f5::AMP_ENABLE_FLAG, sps.amp.into());
    put(
        &mut s,
        f5::SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG,
        sps.sample_adaptive_offset.into(),
    );
    put(&mut s, f5::PCM_ENABLED_FLAG, sps.pcm.into());
    if sps.pcm {
        put(
            &mut s,
            f5::PCM_SAMPLE_BIT_DEPTH_LUMA,
            sps.pcm_bit_depth_luma.into(),
        );
        put(
            &mut s,
            f5::PCM_SAMPLE_BIT_DEPTH_CHROMA,
            sps.pcm_bit_depth_chroma.into(),
        );
        put(
            &mut s,
            f5::LOG2_MIN_PCM_LUMA_CODING_BLOCK_SIZE,
            sps.log2_min_pcm_cb.into(),
        );
        put(
            &mut s,
            f5::LOG2_MAX_PCM_LUMA_CODING_BLOCK_SIZE,
            sps.log2_max_pcm_cb.into(),
        );
        put(
            &mut s,
            f5::PCM_LOOP_FILTER_DISABLED_FLAG,
            sps.pcm_loop_filter_disabled.into(),
        );
    }
    put(
        &mut s,
        f5::SPS_TEMPORAL_MVP_ENABLED_FLAG,
        sps.temporal_mvp.into(),
    );
    put(
        &mut s,
        f5::STRONG_INTRA_SMOOTHING_ENABLED_FLAG,
        sps.strong_intra_smoothing.into(),
    );
    put(
        &mut s,
        f5::DEPENDENT_SLICE_SEGMENTS_ENABLED_FLAG,
        pps.dependent_slice_segments_enabled.into(),
    );
    put(
        &mut s,
        f5::OUTPUT_FLAG_PRESENT_FLAG,
        pps.output_flag_present.into(),
    );
    put(
        &mut s,
        f5::NUM_EXTRA_SLICE_HEADER_BITS,
        pps.num_extra_slice_header_bits.into(),
    );
    put(
        &mut s,
        f5::SIGN_DATA_HIDING_ENABLED_FLAG,
        pps.sign_data_hiding.into(),
    );
    put(
        &mut s,
        f5::CABAC_INIT_PRESENT_FLAG,
        pps.cabac_init_present.into(),
    );
    put(
        &mut s,
        f5::NUM_REF_IDX_L0_DEFAULT_ACTIVE,
        u64::from(pps.num_ref_idx_l0_default_minus1) + 1,
    );
    put(
        &mut s,
        f5::NUM_REF_IDX_L1_DEFAULT_ACTIVE,
        u64::from(pps.num_ref_idx_l1_default_minus1) + 1,
    );
    // In the bit depth's QP range: 26 + init_qp_minus26 + QpBdOffsetY
    // (hevc-main10-4k gave 38 where 8-bit clips give 26).
    let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
    put(
        &mut s,
        f5::INIT_QP,
        (26 + pps.init_qp_minus26 + qp_bd_offset) as u64,
    );
    put(
        &mut s,
        f5::CONSTRAINED_INTRA_PRED_FLAG,
        pps.constrained_intra_pred.into(),
    );
    put(
        &mut s,
        f5::TRANSFORM_SKIP_ENABLED_FLAG,
        pps.transform_skip_enabled.into(),
    );
    put(
        &mut s,
        f5::CU_QP_DELTA_ENABLED_FLAG,
        pps.cu_qp_delta_enabled.into(),
    );
    put(
        &mut s,
        f5::DIFF_CU_QP_DELTA_DEPTH,
        pps.diff_cu_qp_delta_depth.into(),
    );
    put_signed(&mut s, f5::PPS_CB_QP_OFFSET, pps.cb_qp_offset);
    put_signed(&mut s, f5::PPS_CR_QP_OFFSET, pps.cr_qp_offset);
    put_signed(&mut s, f5::PPS_BETA_OFFSET, 2 * pps.beta_offset_div2);
    put_signed(&mut s, f5::PPS_TC_OFFSET, 2 * pps.tc_offset_div2);
    put(
        &mut s,
        f5::PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT_FLAG,
        pps.slice_chroma_qp_offsets_present.into(),
    );
    put(&mut s, f5::WEIGHTED_PRED_FLAG, pps.weighted_pred.into());
    put(&mut s, f5::WEIGHTED_BIPRED_FLAG, pps.weighted_bipred.into());
    put(
        &mut s,
        f5::TRANSQUANT_BYPASS_ENABLED_FLAG,
        pps.transquant_bypass_enabled.into(),
    );
    put(&mut s, f5::TILES_ENABLED_FLAG, pps.tiles_enabled.into());
    put(
        &mut s,
        f5::ENTROPY_CODING_SYNC_ENABLED_FLAG,
        pps.entropy_coding_sync_enabled.into(),
    );
    put(
        &mut s,
        f5::LOOP_FILTER_ACROSS_TILES_ENABLED_FLAG,
        pps.loop_filter_across_tiles_enabled.into(),
    );
    put(
        &mut s,
        f5::LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG,
        pps.loop_filter_across_slices_enabled.into(),
    );
    put(
        &mut s,
        f5::DEBLOCKING_FILTER_CONTROL_PRESENT_FLAG,
        pps.deblocking_filter_control_present.into(),
    );
    put(
        &mut s,
        f5::DEBLOCKING_FILTER_OVERRIDE_ENABLED_FLAG,
        pps.deblocking_filter_override_enabled.into(),
    );
    put(
        &mut s,
        f5::PPS_DEBLOCKING_FILTER_DISABLED_FLAG,
        pps.pps_deblocking_filter_disabled.into(),
    );
    put(
        &mut s,
        f5::LISTS_MODIFICATION_PRESENT_FLAG,
        pps.lists_modification_present.into(),
    );
    put(
        &mut s,
        f5::LOG2_PARALLEL_MERGE_LEVEL,
        pps.log2_parallel_merge_level.into(),
    );
    put(
        &mut s,
        f5::SLICE_SEGMENT_HEADER_EXTENSION_PRESENT_FLAG,
        pps.slice_segment_header_extension_present.into(),
    );
    // The references, by slot.
    put(
        &mut s,
        f5::NUM_REF_FRAMES,
        picture.num_pic_total_curr() as u64,
    );
    let mut long_terms = 0u64;
    for kept in &picture.dpb {
        let difference = picture.poc - kept.poc;
        if !(-128..=127).contains(&difference) {
            return Err(Refused("a reference more than 128 POCs away"));
        }
        put_signed(
            &mut s,
            f5::REF_DIFF_PIC_ORDER_CNTS[usize::from(kept.slot)],
            difference,
        );
        if kept.long_term {
            long_terms |= 1 << (15 - kept.slot);
        }
    }
    put(&mut s, f5::LONGTERMFLAG, long_terms);
    let list0: Vec<u8> = picture
        .st_curr_before
        .iter()
        .chain(&picture.st_curr_after)
        .chain(&picture.lt_curr)
        .copied()
        .collect();
    let list1: Vec<u8> = picture
        .st_curr_after
        .iter()
        .chain(&picture.st_curr_before)
        .chain(&picture.lt_curr)
        .copied()
        .collect();
    if !list0.is_empty() {
        for i in 0..16 {
            put(
                &mut s,
                f5::INITREFLISTIDXL0[i],
                list0[i % list0.len()].into(),
            );
            put(
                &mut s,
                f5::INITREFLISTIDXL1[i],
                list1[i % list1.len()].into(),
            );
        }
    }
    put(&mut s, f5::IDR_PICTURE_FLAG, picture.idr.into());
    put(&mut s, f5::RAP_PICTURE_FLAG, picture.irap.into());
    put(&mut s, f5::CURR_PIC_IDX, picture.slot.into());
    put(
        &mut s,
        f5::SW_HDR_SKIP_LENGTH,
        picture.slice.skip_bits.into(),
    );
    put(
        &mut s,
        f5::SEPARATE_COLOUR_PLANE_FLAG,
        sps.separate_colour_plane.into(),
    );
    put(
        &mut s,
        f5::LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4,
        u64::from(sps.log2_max_poc_lsb - 4),
    );
    put(
        &mut s,
        f5::NUM_SHORT_TERM_REF_PIC_SETS,
        sps.st_rps.len() as u64,
    );
    put(
        &mut s,
        f5::NUM_LONG_TERM_REF_PICS_SPS,
        sps.lt_ref_pics.len() as u64,
    );
    put(
        &mut s,
        f5::NUM_DELTA_POCS_OF_RPS_IDX,
        picture.slice.num_delta_pocs_of_ref_rps.into(),
    );
    put(
        &mut s,
        f5::LONG_TERM_REF_PICS_PRESENT_FLAG,
        sps.long_term_ref_pics_present.into(),
    );
    put(
        &mut s,
        f5::NUM_BITS_SHORT_TERM_REF_PICS_IN_SLICE,
        picture.slice.st_rps_bits.into(),
    );
    put(
        &mut s,
        f5::V1_HEVC_MAIN10_444_EXT_LOG2_MAX_TRANSFORM_SKIP_SIZE,
        2,
    );
    put(
        &mut s,
        f5::V1_HEVC_MAIN10_444_EXT_HEVC_FLT_ABOVE_OFFSET,
        filter.flt_above.into(),
    );
    put(
        &mut s,
        f5::V1_HEVC_MAIN10_444_EXT_HEVC_SAO_ABOVE_OFFSET,
        filter.sao_above.into(),
    );
    put(&mut s, f5::V3_SLICE_EC_MV_TYPE, 1);
    put(
        &mut s,
        f5::V3_SLICE_EC_SLICE_TYPE,
        u64::from(picture.kind == 'I'),
    );
    put(
        &mut s,
        f5::V3_HEVC_SLICE_EDGE_OFFSET,
        filter.slice_edge.into(),
    );
    Ok(s)
}

/// HEVC's tile sizes word for a picture of one tile: its CTB columns, then
/// rows, as `u16`s.
pub fn hevc_tile_sizes(sps: &hevc::Sps) -> [u8; 4] {
    let columns = sps.width_ctbs() as u16;
    let rows = sps.height_ctbs() as u16;
    let mut word = [0u8; 4];
    word[..2].copy_from_slice(&columns.to_le_bytes());
    word[2..].copy_from_slice(&rows.to_le_bytes());
    word
}

/// The setup page of an H.264 `picture`: its setup, and the byte offset of
/// each slice's start code at +0x300. The status area is the engine's.
pub fn h264_page(picture: &h264::Picture, geometry: &Geometry) -> Result<Vec<u8>, Refused> {
    let mut page = vec![0u8; PAGE_BYTES];
    let setup = h264(picture, geometry)?;
    page[..setup.len()].copy_from_slice(&setup);
    if picture.slice_offsets.len() * 4 > H264_STATUS - H264_SLICE_OFFSETS {
        return Err(Refused("more slices than the slice offsets hold"));
    }
    for (k, offset) in picture.slice_offsets.iter().enumerate() {
        let at = H264_SLICE_OFFSETS + 4 * k;
        page[at..at + 4].copy_from_slice(&offset.to_le_bytes());
    }
    Ok(page)
}

/// The setup page of an HEVC `picture`: its setup, the scaling lists in
/// force at +0x200 (zero when disabled) and the tile sizes at +0x600.
pub fn hevc_page(picture: &hevc::Picture, geometry: &Geometry) -> Result<Vec<u8>, Refused> {
    let mut page = vec![0u8; PAGE_BYTES];
    let setup = hevc(picture, geometry)?;
    page[..setup.len()].copy_from_slice(&setup);
    if let Some(lists) = &picture.scaling {
        page[HEVC_SCALING_LIST..HEVC_SCALING_LIST + 1024].copy_from_slice(&lists.buffer());
    }
    page[HEVC_TILE_SIZES..HEVC_TILE_SIZES + 4].copy_from_slice(&hevc_tile_sizes(&picture.sps));
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_formula_gives_both_captured_layouts() {
        // hevc-qp25 (1088 rows), hevc-main-bf and -slices (1080): 8 bits.
        for height in [1080, 1088] {
            assert_eq!(
                hevc_filter(1920, height, 8),
                HevcFilter {
                    sao: 2584,
                    bsd_ctrl: 23256,
                    flt_above: 23902,
                    sao_above: 32130,
                    slice_edge: 32130,
                    intra_top: 0x8d_7200,
                    filter_bytes: 0xa0_0000,
                }
            );
        }
        // hevc-main10-4k.
        assert_eq!(
            hevc_filter(3840, 2160, 10),
            HevcFilter {
                sao: 5168,
                bsd_ctrl: 46512,
                flt_above: 47804,
                sao_above: 113220,
                slice_edge: 113220,
                intra_top: 0x239_c400,
                filter_bytes: 0x240_0000,
            }
        );
    }
}
