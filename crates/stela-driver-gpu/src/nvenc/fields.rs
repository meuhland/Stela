//! Where `nvenc_drv.h`'s bitfields lie in a picture setup and an encoder
//! status, as the compiler laid the header out: generated from nvenc-lab's
//! `fieldtab` (`nvenc-test/src/fields_gen.c`, NVIDIA open-gpu-doc, MIT) by
//! the port's `genfields.py`. Each is a byte offset, a byte length, the
//! shift of its lowest bit and its width in bits, little-endian.
//!
//! `h264`/`hevc` are `nvenc_h264_drv_pic_setup_s` and
//! `nvenc_h265_drv_pic_setup_s` with their motion-search, mode-decision,
//! quantisation and slice controls at the captured offsets (`slice.` at
//! +0x600 and +0x5500); `h264_status`/`hevc_status` are
//! `nvenc_pic_stat_s` and `nvenc_h265_pic_stat_s`.
// Generated: keep every field the encoder or its tests may name.
#![allow(dead_code)]

use super::setup::Field;

pub mod h264 {
    use super::Field;
    pub const MAGIC: Field = Field::new(0, 4, 0, 32);
    pub const REFPIC_CFG_FRAME_WIDTH_MINUS1: Field = Field::new(4, 2, 0, 16);
    pub const REFPIC_CFG_FRAME_HEIGHT_MINUS1: Field = Field::new(6, 2, 0, 16);
    pub const REFPIC_CFG_SFC_PITCH: Field = Field::new(8, 2, 0, 16);
    pub const REFPIC_CFG_SFC_PITCH_CHROMA: Field = Field::new(10, 2, 0, 16);
    pub const REFPIC_CFG_LUMA_TOP_FRM_OFFSET: Field = Field::new(16, 4, 0, 32);
    pub const REFPIC_CFG_LUMA_BOT_OFFSET: Field = Field::new(20, 4, 0, 32);
    pub const REFPIC_CFG_CHROMA_TOP_FRM_OFFSET: Field = Field::new(24, 4, 0, 32);
    pub const REFPIC_CFG_CHROMA_BOT_OFFSET: Field = Field::new(28, 4, 0, 32);
    pub const REFPIC_CFG_BLOCK_HEIGHT: Field = Field::new(32, 1, 0, 7);
    pub const REFPIC_CFG_TILED_16X16: Field = Field::new(32, 1, 7, 1);
    pub const REFPIC_CFG_MEMORY_MODE: Field = Field::new(33, 1, 0, 2);
    pub const INPUT_CFG_FRAME_WIDTH_MINUS1: Field = Field::new(36, 2, 0, 16);
    pub const INPUT_CFG_FRAME_HEIGHT_MINUS1: Field = Field::new(38, 2, 0, 16);
    pub const INPUT_CFG_SFC_PITCH: Field = Field::new(40, 2, 0, 16);
    pub const INPUT_CFG_SFC_PITCH_CHROMA: Field = Field::new(42, 2, 0, 16);
    pub const INPUT_CFG_LUMA_TOP_FRM_OFFSET: Field = Field::new(48, 4, 0, 32);
    pub const INPUT_CFG_LUMA_BOT_OFFSET: Field = Field::new(52, 4, 0, 32);
    pub const INPUT_CFG_CHROMA_TOP_FRM_OFFSET: Field = Field::new(56, 4, 0, 32);
    pub const INPUT_CFG_CHROMA_BOT_OFFSET: Field = Field::new(60, 4, 0, 32);
    pub const INPUT_CFG_BLOCK_HEIGHT: Field = Field::new(64, 1, 0, 7);
    pub const INPUT_CFG_TILED_16X16: Field = Field::new(64, 1, 7, 1);
    pub const INPUT_CFG_MEMORY_MODE: Field = Field::new(65, 1, 0, 2);
    pub const OUTPUTPIC_CFG_FRAME_WIDTH_MINUS1: Field = Field::new(68, 2, 0, 16);
    pub const OUTPUTPIC_CFG_FRAME_HEIGHT_MINUS1: Field = Field::new(70, 2, 0, 16);
    pub const OUTPUTPIC_CFG_SFC_PITCH: Field = Field::new(72, 2, 0, 16);
    pub const OUTPUTPIC_CFG_SFC_PITCH_CHROMA: Field = Field::new(74, 2, 0, 16);
    pub const OUTPUTPIC_CFG_LUMA_TOP_FRM_OFFSET: Field = Field::new(80, 4, 0, 32);
    pub const OUTPUTPIC_CFG_LUMA_BOT_OFFSET: Field = Field::new(84, 4, 0, 32);
    pub const OUTPUTPIC_CFG_CHROMA_TOP_FRM_OFFSET: Field = Field::new(88, 4, 0, 32);
    pub const OUTPUTPIC_CFG_CHROMA_BOT_OFFSET: Field = Field::new(92, 4, 0, 32);
    pub const OUTPUTPIC_CFG_BLOCK_HEIGHT: Field = Field::new(96, 1, 0, 7);
    pub const OUTPUTPIC_CFG_TILED_16X16: Field = Field::new(96, 1, 7, 1);
    pub const OUTPUTPIC_CFG_MEMORY_MODE: Field = Field::new(97, 1, 0, 2);
    pub const SPS_DATA_PROFILE_IDC: Field = Field::new(100, 1, 0, 8);
    pub const SPS_DATA_LEVEL_IDC: Field = Field::new(101, 1, 0, 8);
    pub const SPS_DATA_CHROMA_FORMAT_IDC: Field = Field::new(102, 1, 0, 2);
    pub const SPS_DATA_PIC_ORDER_CNT_TYPE: Field = Field::new(102, 1, 2, 2);
    pub const SPS_DATA_LOG2_MAX_FRAME_NUM_MINUS4: Field = Field::new(102, 1, 4, 4);
    pub const SPS_DATA_LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4: Field = Field::new(103, 1, 0, 4);
    pub const SPS_DATA_FRAME_MBS_ONLY: Field = Field::new(103, 1, 4, 1);
    pub const SPS_DATA_SEPARATE_COLOUR_PLANE_FLAG: Field = Field::new(103, 1, 6, 1);
    pub const SPS_DATA_LOSSLESS_QPPRIME_FLAG: Field = Field::new(103, 1, 7, 1);
    pub const PPS_DATA_PIC_PARAM_SET_ID: Field = Field::new(104, 1, 0, 8);
    pub const PPS_DATA_ENTROPY_CODING_MODE_FLAG: Field = Field::new(105, 1, 0, 1);
    pub const PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1: Field = Field::new(105, 1, 1, 5);
    pub const PPS_DATA_NUM_REF_IDX_L1_ACTIVE_MINUS1: Field = Field::new(105, 2, 6, 5);
    pub const PPS_DATA_WEIGHTED_BIPRED_IDC: Field = Field::new(106, 1, 3, 2);
    pub const PPS_DATA_PIC_INIT_QP_MINUS26: Field = Field::new(106, 2, 5, 6);
    pub const PPS_DATA_CHROMA_QP_INDEX_OFFSET: Field = Field::new(107, 1, 3, 5);
    pub const PPS_DATA_SECOND_CHROMA_QP_INDEX_OFFSET: Field = Field::new(108, 1, 0, 5);
    pub const PPS_DATA_CONSTRAINED_INTRA_PRED_FLAG: Field = Field::new(108, 1, 5, 1);
    pub const PPS_DATA_DEBLOCKING_FILTER_CONTROL_PRESENT_FLAG: Field = Field::new(108, 1, 6, 1);
    pub const PPS_DATA_TRANSFORM_8X8_MODE_FLAG: Field = Field::new(108, 1, 7, 1);
    pub const PPS_DATA_PIC_ORDER_PRESENT_FLAG: Field = Field::new(109, 1, 0, 1);
    pub const PPS_DATA_WEIGHTED_PRED_FLAG: Field = Field::new(109, 1, 1, 1);
    pub const RATE_CONTROL_QP: [Field; 3] = [
        Field::new(113, 1, 0, 8),
        Field::new(114, 1, 0, 8),
        Field::new(115, 1, 0, 8),
    ];
    pub const RATE_CONTROL_FRAMERATE: Field = Field::new(136, 4, 0, 32);
    pub const RATE_CONTROL_BUFFERSIZE: Field = Field::new(140, 4, 0, 32);
    pub const RATE_CONTROL_NAL_CPB_SIZE: Field = Field::new(144, 4, 0, 32);
    pub const RATE_CONTROL_NAL_BITRATE: Field = Field::new(148, 4, 0, 32);
    pub const RATE_CONTROL_VCL_CPB_SIZE: Field = Field::new(152, 4, 0, 32);
    pub const RATE_CONTROL_VCL_BITRATE: Field = Field::new(156, 4, 0, 32);
    pub const RATE_CONTROL_GOP_LENGTH: Field = Field::new(160, 4, 0, 32);
    pub const RATE_CONTROL_HRD_TYPE: Field = Field::new(112, 1, 0, 8);
    pub const RATE_CONTROL_NP: Field = Field::new(164, 4, 0, 32);
    pub const RATE_CONTROL_BMIN: Field = Field::new(168, 4, 0, 32);
    pub const RATE_CONTROL_RAVG: Field = Field::new(172, 4, 0, 32);
    pub const RATE_CONTROL_R: Field = Field::new(176, 4, 0, 32);
    pub const RATE_CONTROL_AB_BETA: Field = Field::new(181, 1, 0, 8);
    pub const RATE_CONTROL_PREV_ACT: Field = Field::new(182, 2, 0, 16);
    pub const RATE_CONTROL_SINGLE_FRAME_VBV: Field = Field::new(185, 1, 0, 8);
    pub const RATE_CONTROL_TWO_PASS_RC: Field = Field::new(186, 1, 0, 8);
    pub const RATE_CONTROL_I_SIZE_RATIO_X: Field = Field::new(189, 1, 0, 8);
    pub const RATE_CONTROL_I_SIZE_RATIO_Y: Field = Field::new(190, 1, 0, 8);
    pub const PIC_CONTROL_MAX_SLICE_SIZE: Field = Field::new(368, 4, 0, 32);
    pub const PIC_CONTROL_SLICE_MODE: Field = Field::new(424, 1, 5, 1);
    pub const PIC_CONTROL_INTRA_REFRESH_COUNT: Field = Field::new(431, 1, 0, 8);
    pub const MD_INTRA_REFRESH_CNT: Field = Field::new(1024, 2, 0, 16);
    pub const PIC_CONTROL_L0: [Field; 8] = [
        Field::new(200, 1, 0, 8),
        Field::new(201, 1, 0, 8),
        Field::new(202, 1, 0, 8),
        Field::new(203, 1, 0, 8),
        Field::new(204, 1, 0, 8),
        Field::new(205, 1, 0, 8),
        Field::new(206, 1, 0, 8),
        Field::new(207, 1, 0, 8),
    ];
    pub const PIC_CONTROL_TEMP_DIST_L0: [Field; 8] = [
        Field::new(216, 1, 0, 8),
        Field::new(217, 1, 0, 8),
        Field::new(218, 1, 0, 8),
        Field::new(219, 1, 0, 8),
        Field::new(220, 1, 0, 8),
        Field::new(221, 1, 0, 8),
        Field::new(222, 1, 0, 8),
        Field::new(223, 1, 0, 8),
    ];
    pub const PIC_CONTROL_DIST_SCALE_FACTOR: [[Field; 8]; 8] = [
        [
            Field::new(232, 2, 0, 16),
            Field::new(234, 2, 0, 16),
            Field::new(236, 2, 0, 16),
            Field::new(238, 2, 0, 16),
            Field::new(240, 2, 0, 16),
            Field::new(242, 2, 0, 16),
            Field::new(244, 2, 0, 16),
            Field::new(246, 2, 0, 16),
        ],
        [
            Field::new(248, 2, 0, 16),
            Field::new(250, 2, 0, 16),
            Field::new(252, 2, 0, 16),
            Field::new(254, 2, 0, 16),
            Field::new(256, 2, 0, 16),
            Field::new(258, 2, 0, 16),
            Field::new(260, 2, 0, 16),
            Field::new(262, 2, 0, 16),
        ],
        [
            Field::new(264, 2, 0, 16),
            Field::new(266, 2, 0, 16),
            Field::new(268, 2, 0, 16),
            Field::new(270, 2, 0, 16),
            Field::new(272, 2, 0, 16),
            Field::new(274, 2, 0, 16),
            Field::new(276, 2, 0, 16),
            Field::new(278, 2, 0, 16),
        ],
        [
            Field::new(280, 2, 0, 16),
            Field::new(282, 2, 0, 16),
            Field::new(284, 2, 0, 16),
            Field::new(286, 2, 0, 16),
            Field::new(288, 2, 0, 16),
            Field::new(290, 2, 0, 16),
            Field::new(292, 2, 0, 16),
            Field::new(294, 2, 0, 16),
        ],
        [
            Field::new(296, 2, 0, 16),
            Field::new(298, 2, 0, 16),
            Field::new(300, 2, 0, 16),
            Field::new(302, 2, 0, 16),
            Field::new(304, 2, 0, 16),
            Field::new(306, 2, 0, 16),
            Field::new(308, 2, 0, 16),
            Field::new(310, 2, 0, 16),
        ],
        [
            Field::new(312, 2, 0, 16),
            Field::new(314, 2, 0, 16),
            Field::new(316, 2, 0, 16),
            Field::new(318, 2, 0, 16),
            Field::new(320, 2, 0, 16),
            Field::new(322, 2, 0, 16),
            Field::new(324, 2, 0, 16),
            Field::new(326, 2, 0, 16),
        ],
        [
            Field::new(328, 2, 0, 16),
            Field::new(330, 2, 0, 16),
            Field::new(332, 2, 0, 16),
            Field::new(334, 2, 0, 16),
            Field::new(336, 2, 0, 16),
            Field::new(338, 2, 0, 16),
            Field::new(340, 2, 0, 16),
            Field::new(342, 2, 0, 16),
        ],
        [
            Field::new(344, 2, 0, 16),
            Field::new(346, 2, 0, 16),
            Field::new(348, 2, 0, 16),
            Field::new(350, 2, 0, 16),
            Field::new(352, 2, 0, 16),
            Field::new(354, 2, 0, 16),
            Field::new(356, 2, 0, 16),
            Field::new(358, 2, 0, 16),
        ],
    ];
    pub const PIC_CONTROL_DIFF_PIC_ORDER_CNT_ZERO: [Field; 2] =
        [Field::new(360, 4, 0, 32), Field::new(364, 4, 0, 32)];
    pub const PIC_CONTROL_FRAME_NUM: Field = Field::new(380, 2, 0, 16);
    pub const PIC_CONTROL_PIC_ORDER_CNT_LSB: Field = Field::new(382, 2, 0, 16);
    pub const PIC_CONTROL_IDR_PIC_ID: Field = Field::new(384, 2, 0, 16);
    pub const PIC_CONTROL_SLICE_CONTROL_OFFSET: Field = Field::new(396, 4, 0, 32);
    pub const PIC_CONTROL_ME_CONTROL_OFFSET: Field = Field::new(400, 4, 0, 32);
    pub const PIC_CONTROL_MD_CONTROL_OFFSET: Field = Field::new(404, 4, 0, 32);
    pub const PIC_CONTROL_Q_CONTROL_OFFSET: Field = Field::new(408, 4, 0, 32);
    pub const PIC_CONTROL_HIST_BUF_SIZE: Field = Field::new(412, 4, 0, 32);
    pub const PIC_CONTROL_BITSTREAM_BUF_SIZE: Field = Field::new(416, 4, 0, 32);
    pub const PIC_CONTROL_BITSTREAM_START_POS: Field = Field::new(420, 4, 0, 32);
    pub const PIC_CONTROL_PIC_TYPE: Field = Field::new(424, 1, 2, 2);
    pub const SLICE_QP_AVR: Field = Field::new(1538, 2, 3, 8);
    pub const SLICE_NUM_REF_IDX_ACTIVE_OVERRIDE_FLAG: Field = Field::new(1548, 1, 0, 1);
    pub const SLICE_NUM_REF_IDX_L0_ACTIVE_MINUS1: Field = Field::new(1548, 1, 1, 5);
    pub const SLICE_NUM_MB: Field = Field::new(1536, 3, 0, 19);
}

pub mod hevc {
    use super::Field;
    pub const MAGIC: Field = Field::new(0, 4, 0, 32);
    pub const REFPIC_CFG_FRAME_WIDTH_MINUS1: Field = Field::new(4, 2, 0, 16);
    pub const REFPIC_CFG_FRAME_HEIGHT_MINUS1: Field = Field::new(6, 2, 0, 16);
    pub const REFPIC_CFG_SFC_PITCH: Field = Field::new(8, 2, 0, 16);
    pub const REFPIC_CFG_SFC_PITCH_CHROMA: Field = Field::new(10, 2, 0, 16);
    pub const REFPIC_CFG_LUMA_TOP_FRM_OFFSET: Field = Field::new(16, 4, 0, 32);
    pub const REFPIC_CFG_LUMA_BOT_OFFSET: Field = Field::new(20, 4, 0, 32);
    pub const REFPIC_CFG_CHROMA_TOP_FRM_OFFSET: Field = Field::new(24, 4, 0, 32);
    pub const REFPIC_CFG_CHROMA_BOT_OFFSET: Field = Field::new(28, 4, 0, 32);
    pub const REFPIC_CFG_BLOCK_HEIGHT: Field = Field::new(32, 1, 0, 7);
    pub const REFPIC_CFG_TILED_16X16: Field = Field::new(32, 1, 7, 1);
    pub const REFPIC_CFG_MEMORY_MODE: Field = Field::new(33, 1, 0, 2);
    pub const INPUT_CFG_FRAME_WIDTH_MINUS1: Field = Field::new(36, 2, 0, 16);
    pub const INPUT_CFG_FRAME_HEIGHT_MINUS1: Field = Field::new(38, 2, 0, 16);
    pub const INPUT_CFG_SFC_PITCH: Field = Field::new(40, 2, 0, 16);
    pub const INPUT_CFG_SFC_PITCH_CHROMA: Field = Field::new(42, 2, 0, 16);
    pub const INPUT_CFG_LUMA_TOP_FRM_OFFSET: Field = Field::new(48, 4, 0, 32);
    pub const INPUT_CFG_LUMA_BOT_OFFSET: Field = Field::new(52, 4, 0, 32);
    pub const INPUT_CFG_CHROMA_TOP_FRM_OFFSET: Field = Field::new(56, 4, 0, 32);
    pub const INPUT_CFG_CHROMA_BOT_OFFSET: Field = Field::new(60, 4, 0, 32);
    pub const INPUT_CFG_BLOCK_HEIGHT: Field = Field::new(64, 1, 0, 7);
    pub const INPUT_CFG_TILED_16X16: Field = Field::new(64, 1, 7, 1);
    pub const INPUT_CFG_MEMORY_MODE: Field = Field::new(65, 1, 0, 2);
    pub const OUTPUTPIC_CFG_FRAME_WIDTH_MINUS1: Field = Field::new(68, 2, 0, 16);
    pub const OUTPUTPIC_CFG_FRAME_HEIGHT_MINUS1: Field = Field::new(70, 2, 0, 16);
    pub const OUTPUTPIC_CFG_SFC_PITCH: Field = Field::new(72, 2, 0, 16);
    pub const OUTPUTPIC_CFG_SFC_PITCH_CHROMA: Field = Field::new(74, 2, 0, 16);
    pub const OUTPUTPIC_CFG_LUMA_TOP_FRM_OFFSET: Field = Field::new(80, 4, 0, 32);
    pub const OUTPUTPIC_CFG_LUMA_BOT_OFFSET: Field = Field::new(84, 4, 0, 32);
    pub const OUTPUTPIC_CFG_CHROMA_TOP_FRM_OFFSET: Field = Field::new(88, 4, 0, 32);
    pub const OUTPUTPIC_CFG_CHROMA_BOT_OFFSET: Field = Field::new(92, 4, 0, 32);
    pub const OUTPUTPIC_CFG_BLOCK_HEIGHT: Field = Field::new(96, 1, 0, 7);
    pub const OUTPUTPIC_CFG_TILED_16X16: Field = Field::new(96, 1, 7, 1);
    pub const OUTPUTPIC_CFG_MEMORY_MODE: Field = Field::new(97, 1, 0, 2);
    pub const SPS_DATA_CHROMA_FORMAT_IDC: Field = Field::new(100, 1, 0, 2);
    pub const SPS_DATA_SEPARATE_COLOUR_PLANE_FLAG: Field = Field::new(100, 1, 2, 1);
    pub const SPS_DATA_BIT_DEPTH_MINUS_8: Field = Field::new(104, 2, 5, 4);
    pub const SPS_DATA_LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4: Field = Field::new(100, 1, 3, 4);
    pub const SPS_DATA_MIN_CU_SIZE: Field = Field::new(100, 2, 7, 2);
    pub const SPS_DATA_MAX_CU_SIZE: Field = Field::new(101, 1, 1, 2);
    pub const SPS_DATA_MIN_TU_SIZE: Field = Field::new(101, 1, 3, 2);
    pub const SPS_DATA_MAX_TU_SIZE: Field = Field::new(101, 1, 5, 2);
    pub const SPS_DATA_MAX_TRANSFORM_DEPTH_INTER: Field = Field::new(102, 1, 0, 2);
    pub const SPS_DATA_MAX_TRANSFORM_DEPTH_INTRA: Field = Field::new(102, 1, 2, 2);
    pub const SPS_DATA_AMP_ENABLED_FLAG: Field = Field::new(102, 1, 7, 1);
    pub const SPS_DATA_SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG: Field = Field::new(105, 1, 1, 1);
    pub const SPS_DATA_PCM_ENABLED_FLAG: Field = Field::new(102, 1, 4, 1);
    pub const SPS_DATA_NUM_SHORT_TERM_REF_PIC_SETS: Field = Field::new(103, 1, 1, 7);
    pub const SPS_DATA_LONG_TERM_REF_PICS_PRESENT_FLAG: Field = Field::new(102, 1, 6, 1);
    pub const SPS_DATA_SPS_TEMPORAL_MVP_ENABLED_FLAG: Field = Field::new(105, 1, 2, 1);
    pub const SPS_DATA_STRONG_INTRA_SMOOTHING_ENABLED_FLAG: Field = Field::new(103, 1, 0, 1);
    pub const PPS_DATA_PIC_PARAM_SET_ID: Field = Field::new(108, 1, 0, 8);
    pub const PPS_DATA_DEPENDENT_SLICE_SEGMENTS_ENABLED_FLAG: Field = Field::new(114, 1, 0, 1);
    pub const PPS_DATA_OUTPUT_FLAG_PRESENT_FLAG: Field = Field::new(111, 1, 1, 1);
    pub const PPS_DATA_NUM_EXTRA_SLICE_HEADER_BITS: Field = Field::new(114, 2, 7, 3);
    pub const PPS_DATA_SIGN_DATA_HIDING_ENABLED_FLAG: Field = Field::new(114, 1, 1, 1);
    pub const PPS_DATA_CABAC_INIT_PRESENT_FLAG: Field = Field::new(111, 1, 2, 1);
    pub const PPS_DATA_NUM_REF_IDX_L0_ACTIVE_MINUS1: Field = Field::new(109, 1, 0, 5);
    pub const PPS_DATA_NUM_REF_IDX_L1_ACTIVE_MINUS1: Field = Field::new(109, 2, 5, 5);
    pub const PPS_DATA_PIC_INIT_QP_MINUS26: Field = Field::new(112, 1, 0, 6);
    pub const PPS_DATA_CONSTRAINED_INTRA_PRED_FLAG: Field = Field::new(110, 1, 2, 1);
    pub const PPS_DATA_TRANSFORM_SKIP_ENABLED_FLAG: Field = Field::new(110, 1, 4, 1);
    pub const PPS_DATA_CU_QP_DELTA_ENABLED_FLAG: Field = Field::new(110, 1, 7, 1);
    pub const PPS_DATA_DIFF_CU_QP_DELTA_DEPTH: Field = Field::new(114, 1, 2, 2);
    pub const PPS_DATA_PPS_CB_QP_OFFSET: Field = Field::new(112, 2, 6, 5);
    pub const PPS_DATA_PPS_CR_QP_OFFSET: Field = Field::new(113, 1, 3, 5);
    pub const PPS_DATA_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT: Field = Field::new(111, 1, 3, 1);
    pub const PPS_DATA_WEIGHTED_PRED_FLAG: Field = Field::new(111, 1, 4, 1);
    pub const PPS_DATA_WEIGHTED_BIPRED_FLAG: Field = Field::new(111, 1, 5, 1);
    pub const PPS_DATA_TRANSQUANT_BYPASS_ENABLED_FLAG: Field = Field::new(110, 1, 3, 1);
    pub const PPS_DATA_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG: Field = Field::new(110, 1, 6, 1);
    pub const PPS_DATA_DEBLOCKING_FILTER_OVERRIDE_ENABLED_FLAG: Field = Field::new(110, 1, 5, 1);
    pub const PPS_DATA_LISTS_MODIFICATION_PRESENT_FLAG: Field = Field::new(111, 1, 0, 1);
    pub const PPS_DATA_LOG2_PARALLEL_MERGE_LEVEL_MINUS2: Field = Field::new(114, 1, 4, 3);
    pub const RATE_CONTROL_QP: [Field; 3] = [
        Field::new(117, 1, 0, 8),
        Field::new(118, 1, 0, 8),
        Field::new(119, 1, 0, 8),
    ];
    pub const RATE_CONTROL_FRAMERATE: Field = Field::new(140, 4, 0, 32);
    pub const RATE_CONTROL_BUFFERSIZE: Field = Field::new(144, 4, 0, 32);
    pub const RATE_CONTROL_NAL_CPB_SIZE: Field = Field::new(148, 4, 0, 32);
    pub const RATE_CONTROL_NAL_BITRATE: Field = Field::new(152, 4, 0, 32);
    pub const RATE_CONTROL_VCL_CPB_SIZE: Field = Field::new(156, 4, 0, 32);
    pub const RATE_CONTROL_VCL_BITRATE: Field = Field::new(160, 4, 0, 32);
    pub const RATE_CONTROL_GOP_LENGTH: Field = Field::new(164, 4, 0, 32);
    pub const RATE_CONTROL_HRD_TYPE: Field = Field::new(116, 1, 0, 8);
    pub const RATE_CONTROL_NP: Field = Field::new(168, 4, 0, 32);
    pub const RATE_CONTROL_BMIN: Field = Field::new(172, 4, 0, 32);
    pub const RATE_CONTROL_RAVG: Field = Field::new(176, 4, 0, 32);
    pub const RATE_CONTROL_R: Field = Field::new(180, 4, 0, 32);
    pub const RATE_CONTROL_AB_BETA: Field = Field::new(185, 1, 0, 8);
    pub const RATE_CONTROL_PREV_ACT: Field = Field::new(186, 2, 0, 16);
    pub const RATE_CONTROL_SINGLE_FRAME_VBV: Field = Field::new(189, 1, 0, 8);
    pub const RATE_CONTROL_TWO_PASS_RC: Field = Field::new(190, 1, 0, 8);
    pub const RATE_CONTROL_I_SIZE_RATIO_X: Field = Field::new(193, 1, 0, 8);
    pub const RATE_CONTROL_I_SIZE_RATIO_Y: Field = Field::new(194, 1, 0, 8);
    pub const PIC_CONTROL_MAX_SLICE_SIZE: Field = Field::new(380, 4, 0, 32);
    pub const PIC_CONTROL_SLICE_MODE: Field = Field::new(424, 1, 3, 1);
    pub const MD_INTRA_REFRESH_CNT: Field = Field::new(13312, 2, 0, 16);
    pub const PIC_CONTROL_L0: [Field; 8] = [
        Field::new(212, 1, 0, 8),
        Field::new(213, 1, 0, 8),
        Field::new(214, 1, 0, 8),
        Field::new(215, 1, 0, 8),
        Field::new(216, 1, 0, 8),
        Field::new(217, 1, 0, 8),
        Field::new(218, 1, 0, 8),
        Field::new(219, 1, 0, 8),
    ];
    pub const PIC_CONTROL_PIC_ORDER_CNT_LSB: Field = Field::new(390, 2, 0, 16);
    pub const PIC_CONTROL_SHORT_TERM_RPS_NUM_NEGATIVE_PICS: Field = Field::new(456, 2, 0, 16);
    pub const PIC_CONTROL_SLICE_CONTROL_OFFSET: Field = Field::new(396, 4, 0, 32);
    pub const PIC_CONTROL_ME_CONTROL_OFFSET: Field = Field::new(400, 4, 0, 32);
    pub const PIC_CONTROL_MD_CONTROL_OFFSET: Field = Field::new(404, 4, 0, 32);
    pub const PIC_CONTROL_Q_CONTROL_OFFSET: Field = Field::new(408, 4, 0, 32);
    pub const PIC_CONTROL_WP_CONTROL_OFFSET: Field = Field::new(688, 4, 0, 32);
    pub const PIC_CONTROL_HIST_BUF_SIZE: Field = Field::new(412, 4, 0, 32);
    pub const PIC_CONTROL_BITSTREAM_BUF_SIZE: Field = Field::new(416, 4, 0, 32);
    pub const PIC_CONTROL_BITSTREAM_START_POS: Field = Field::new(420, 4, 0, 32);
    pub const PIC_CONTROL_CEIL_LOG2_PIC_SIZE_IN_CTBS: Field = Field::new(425, 1, 0, 6);
    pub const PIC_CONTROL_PIC_TYPE: Field = Field::new(424, 1, 0, 2);
    pub const PIC_CONTROL_NAL_UNIT_TYPE: Field = Field::new(445, 1, 0, 6);
    pub const SLICE_QP_AVR: Field = Field::new(21762, 2, 3, 8);
    pub const SLICE_NUM_CTUS: Field = Field::new(21760, 3, 0, 19);
}

pub mod h264_status {
    use super::Field;
    pub const PICTURE_INDEX: Field = Field::new(0, 4, 0, 32);
    pub const ERROR_STATUS: Field = Field::new(4, 1, 0, 2);
    pub const UCODE_ERROR_STATUS: Field = Field::new(4, 4, 2, 30);
    pub const TOTAL_BIT_COUNT: Field = Field::new(8, 4, 0, 32);
    pub const TYPE1_BIT_COUNT: Field = Field::new(12, 4, 0, 32);
    pub const PIC_TYPE: Field = Field::new(16, 2, 0, 16);
    pub const NUM_SLICES: Field = Field::new(18, 2, 0, 16);
    pub const AVGQP: Field = Field::new(22, 2, 0, 16);
    pub const BITSTREAM_START_POS: Field = Field::new(32, 4, 0, 32);
    pub const LAST_VALID_BYTE_OFFSET: Field = Field::new(36, 4, 0, 32);
    pub const INTRA_MB_COUNT: Field = Field::new(40, 2, 0, 16);
    pub const INTER_MB_COUNT: Field = Field::new(42, 2, 0, 16);
}

pub mod hevc_status {
    use super::Field;
    pub const PICTURE_INDEX: Field = Field::new(0, 4, 0, 32);
    pub const ERROR_STATUS: Field = Field::new(4, 1, 0, 2);
    pub const UCODE_ERROR_STATUS: Field = Field::new(4, 4, 2, 30);
    pub const TOTAL_BIT_COUNT: Field = Field::new(8, 4, 0, 32);
    pub const TYPE1_BIT_COUNT: Field = Field::new(12, 4, 0, 32);
    pub const PIC_TYPE: Field = Field::new(16, 2, 0, 16);
    pub const NUM_SLICES: Field = Field::new(18, 2, 0, 16);
    pub const AVGQP: Field = Field::new(22, 2, 0, 16);
    pub const BITSTREAM_START_POS: Field = Field::new(32, 4, 0, 32);
    pub const LAST_VALID_BYTE_OFFSET: Field = Field::new(36, 4, 0, 32);
    pub const INTRA_CU32X32_COUNT: Field = Field::new(40, 4, 0, 32);
    pub const INTER_CU32X32_COUNT: Field = Field::new(44, 4, 0, 32);
    pub const INTRA_CU16X16_COUNT: Field = Field::new(48, 4, 0, 32);
    pub const INTER_CU16X16_COUNT: Field = Field::new(52, 4, 0, 32);
}
