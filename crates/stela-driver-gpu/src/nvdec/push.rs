//! A frame's NVC7B0 method stream, as NVIDIA's driver sends it
//! (nvdec-cap-1118's `push.txt`), with one `SEMAPHORE_A..D` release of a
//! sequence number to the session's fence page in place of NVIDIA's own
//! semaphores and trap.
//!
//! Every method gets its own one-word incrementing header on subchannel 4,
//! and addresses are GPU addresses shifted right by 8. In order:
//!
//! - `SET_APPLICATION_ID` and `SET_CONTROL_PARAMS` (the codec, the GP and
//!   macroblock timers, error concealment from the previous picture's
//!   slot, a production run);
//! - the setup page, the bitstream, and for H.264 the slice offsets;
//! - the surfaces, luma then chroma per entry: H.264 sends
//!   [`super::layout::h264_entries`] (5 at 1080p, 9 at 720p), HEVC entries
//!   0 to 15; an entry whose slot the stream has not reached yet names the
//!   current picture (H.264) or slot 0 (HEVC), as NVIDIA's driver does;
//! - the co-located buffer, the status, the picture index (the session's
//!   decode count);
//! - H.264: the macroblock history and the history; HEVC: the scaling list,
//!   the tile sizes, the filter buffer, the intra top row, and the current
//!   picture again as entry 16;
//! - `EXECUTE`, then the release.
use super::layout::Layout;
use super::method as m;
use super::setup::{
    H264_SLICE_OFFSETS, H264_STATUS, HEVC_SCALING_LIST, HEVC_STATUS, HEVC_TILE_SIZES,
};
use super::{Codec, DECODER_CLASS, Picture};
use crate::nvenc::{SEMAPHORE_D_RELEASE_32, SUBCHANNEL, header};
use alloc::vec::Vec;

/// NVIDIA's `SET_WATCHDOG_TIMER`, sent once after `SET_OBJECT`.
pub const WATCHDOG: u32 = 0x5fff_fffe;
/// The most words a frame's submission takes.
pub const MOST_WORDS: usize = 128;

/// `SET_CONTROL_PARAMS` as NVIDIA's driver sent it: the codec, GPTIMER_ON,
/// ERR_CONCEAL_ON with ERROR_FRM_IDX the slot of the picture decoded
/// before (0 for the first), what a damaged picture is concealed from,
/// MBTIMER_ON, bit 16 and TESTRUN_ENV PROD_RUN (0x52050 with the codec and
/// slot).
pub fn control(codec: Codec, slot: u8) -> u32 {
    let codec_type = match codec {
        Codec::H264 => 3,
        Codec::Hevc => 7,
    };
    0x5_2050 | codec_type | (u32::from(slot) & 0x3f) << 7
}

fn emit(words: &mut Vec<u32>, method: u32, data: u32) {
    words.push(header(SUBCHANNEL, method, 1));
    words.push(data);
}

fn a8(address: u64) -> u32 {
    (address >> 8) as u32
}

/// The words of decode `index` of `picture` on `layout`'s buffers, releasing
/// `payload` to the fence; `preamble` binds the class first.
pub fn words(
    layout: &Layout,
    picture: &Picture,
    index: u32,
    payload: u32,
    preamble: bool,
) -> Vec<u32> {
    let mut words = Vec::with_capacity(MOST_WORDS);
    let ring = index as usize % layout.setup.len();
    let page = layout.address(layout.setup[ring]);
    let current = layout.surface(picture.surface);
    if preamble {
        emit(&mut words, m::SET_OBJECT, DECODER_CLASS);
        emit(&mut words, m::SET_WATCHDOG_TIMER, WATCHDOG);
    }
    emit(
        &mut words,
        m::SET_APPLICATION_ID,
        layout.codec.application(),
    );
    emit(
        &mut words,
        m::SET_CONTROL_PARAMS,
        control(layout.codec, picture.previous_slot),
    );
    emit(&mut words, m::SET_DRV_PIC_SETUP_OFFSET, a8(page));
    emit(
        &mut words,
        m::SET_IN_BUF_BASE_OFFSET,
        a8(layout.address(layout.bitstream[ring])),
    );
    let (entries, fallback) = match layout.codec {
        Codec::H264 => {
            emit(
                &mut words,
                m::SET_SLICE_OFFSETS_BUF_OFFSET,
                a8(page + H264_SLICE_OFFSETS as u64),
            );
            let reached = usize::from(picture.high_water) + 1;
            (layout.entries.max(reached).min(17), current)
        }
        Codec::Hevc => (16, layout.surface(0)),
    };
    for entry in 0..entries {
        let (luma, chroma) =
            if entry <= usize::from(picture.high_water) && entry < picture.bindings.len() {
                layout.surface(picture.bindings[entry])
            } else {
                fallback
            };
        emit(
            &mut words,
            m::SET_PICTURE_LUMA_OFFSET0 + 4 * entry as u32,
            a8(luma),
        );
        emit(
            &mut words,
            m::SET_PICTURE_CHROMA_OFFSET0 + 4 * entry as u32,
            a8(chroma),
        );
    }
    emit(
        &mut words,
        m::SET_COLOC_DATA_OFFSET,
        a8(layout.address(layout.coloc)),
    );
    let status = match layout.codec {
        Codec::H264 => H264_STATUS,
        Codec::Hevc => HEVC_STATUS,
    };
    emit(
        &mut words,
        m::SET_NVDEC_STATUS_OFFSET,
        a8(page + status as u64),
    );
    emit(&mut words, m::SET_PICTURE_INDEX, index);
    let scratch = layout.address(layout.scratch);
    match layout.codec {
        Codec::H264 => {
            emit(&mut words, m::H264_SET_MBHIST_BUF_OFFSET, a8(scratch));
            emit(
                &mut words,
                m::SET_HISTORY_OFFSET,
                a8(scratch + layout.mbhist_bytes),
            );
        }
        Codec::Hevc => {
            emit(
                &mut words,
                m::HEVC_SET_SCALING_LIST_OFFSET,
                a8(page + HEVC_SCALING_LIST as u64),
            );
            emit(
                &mut words,
                m::HEVC_SET_TILE_SIZES_OFFSET,
                a8(page + HEVC_TILE_SIZES as u64),
            );
            emit(&mut words, m::HEVC_SET_FILTER_BUFFER_OFFSET, a8(scratch));
            emit(
                &mut words,
                m::SET_INTRA_TOP_BUF_OFFSET,
                a8(scratch + layout.intra_top),
            );
            emit(
                &mut words,
                m::SET_PICTURE_LUMA_OFFSET0 + 4 * 16,
                a8(current.0),
            );
            emit(
                &mut words,
                m::SET_PICTURE_CHROMA_OFFSET0 + 4 * 16,
                a8(current.1),
            );
        }
    }
    emit(&mut words, m::EXECUTE, 0);
    let fence = layout.address(layout.fence);
    emit(&mut words, m::SEMAPHORE_A, (fence >> 32) as u32 & 0xff);
    emit(&mut words, m::SEMAPHORE_B, fence as u32);
    emit(&mut words, m::SEMAPHORE_C, payload);
    emit(&mut words, m::SEMAPHORE_D, SEMAPHORE_D_RELEASE_32);
    words
}

/// The (method, data) pairs of a stream of one-word incrementing headers.
pub fn decode(words: &[u32]) -> Vec<(u32, u32)> {
    words
        .chunks(2)
        .map(|pair| ((pair[0] & 0xfff) << 2, pair.get(1).copied().unwrap_or(0)))
        .collect()
}
