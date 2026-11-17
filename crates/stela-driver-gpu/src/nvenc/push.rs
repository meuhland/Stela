//! A frame's NVC7B7 method stream, as NVIDIA's driver sends it.
//!
//! Every method gets its own one-word incrementing header on subchannel 4
//! (`0x2001_8xxx`), as in the captured pushbuffers, so a stream compares
//! with nvenc-test's and NVIDIA's word for word. Addresses are GPU
//! addresses shifted right by 8. Buffer rotation, read off the captures:
//!
//! - setup: a ring of 16, n % 16;
//! - status, RC data, input, bitstream: n % 4;
//! - reconstructed picture (`OUT_REF_PIC`): n % 5. H.264 references are the
//!   up to three newest, newest first, each as `SET_IN_REF_PIC<m % 4>`; HEVC
//!   has one, the previous frame's, always in `SET_IN_REF_PIC0`;
//! - H.264 co-located MVs: out to area n % 5, in (from the first P frame)
//!   from the previous frame's area. HEVC sends none;
//! - ME predictors, from the first P frame: in from one buffer, out to the
//!   other, alternating by n's parity.
//!
//! NVIDIA brackets each frame with semaphores of its own bookkeeping and an
//! upload's; the encoder ends each frame with one `SEMAPHORE_A..D` release
//! of a sequence number to its fence page instead.
use super::layout::{Layout, RECONS};
use super::method as m;
use super::{Codec, ENCODER_CLASS, SEMAPHORE_D_RELEASE_32, SUBCHANNEL, header};
use alloc::vec::Vec;

/// NVIDIA's `SET_WATCHDOG_TIMER`, sent once first.
pub const WATCHDOG: u32 = 0x5fff_fffe;

/// What a frame's submission names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The session's picture index.
    pub index: u32,
    pub idr: bool,
    /// L0 references, newest first: the pictures index - 1, index - 2, ...
    pub refs: u32,
    /// `SET_CONTROL_PARAMS`.
    pub control: u32,
    /// What the release writes to the fence.
    pub payload: u32,
    /// The input picture's GPU address.
    pub input: u64,
    /// Whether the session's first submission binds the class first.
    pub preamble: bool,
}

/// The most words a frame's submission takes.
pub const MOST_WORDS: usize = 96;

fn emit(words: &mut Vec<u32>, method: u32, data: u32) {
    words.push(header(SUBCHANNEL, method, 1));
    words.push(data);
}

fn a8(address: u64) -> u32 {
    (address >> 8) as u32
}

/// The words of `frame`'s submission on `layout`'s buffers.
pub fn words(layout: &Layout, frame: &Frame) -> Vec<u32> {
    let mut words = Vec::with_capacity(MOST_WORDS);
    let n = frame.index;
    let slot = (n % 4) as usize;
    let recons = RECONS as u32;
    let address = |index: usize| layout.address(index);
    if frame.preamble {
        emit(&mut words, m::SET_OBJECT, ENCODER_CLASS);
        emit(&mut words, m::SET_WATCHDOG_TIMER, WATCHDOG);
    }
    emit(
        &mut words,
        m::SET_APPLICATION_ID,
        layout.codec.application(),
    );
    emit(&mut words, m::SET_CONTROL_PARAMS, frame.control);
    emit(&mut words, m::SET_PICTURE_INDEX, n);
    emit(
        &mut words,
        m::SET_IN_DRV_PIC_SETUP,
        a8(address(layout.setup[(n % 16) as usize])),
    );
    emit(&mut words, m::SET_IOHISTORY, a8(address(layout.history)));
    if let Some(coloc) = layout.coloc {
        let base = address(coloc);
        if !frame.idr {
            let previous = u64::from((n - 1) % recons);
            emit(
                &mut words,
                m::SET_IN_COLOC_DATA,
                a8(base + layout.coloc_stride * previous),
            );
        }
        let current = u64::from(n % recons);
        emit(
            &mut words,
            m::SET_OUT_COLOC_DATA,
            a8(base + layout.coloc_stride * current),
        );
    }
    if !frame.idr {
        let odd = n & 1 != 0;
        let (input, output) = if odd { (0, 1) } else { (1, 0) };
        emit(
            &mut words,
            m::SET_IN_MEPRED_DATA,
            a8(address(layout.mepred[input])),
        );
        emit(
            &mut words,
            m::SET_OUT_MEPRED_DATA,
            a8(address(layout.mepred[output])),
        );
    }
    emit(
        &mut words,
        m::SET_IO_RC_PROCESS,
        a8(address(layout.rc_process)),
    );
    // Luma and chroma carry the same base; the setup's surface
    // configuration places the chroma.
    let recon = a8(address(layout.recon[(n % recons) as usize]));
    emit(&mut words, m::SET_OUT_REF_PIC_LUMA, recon);
    emit(&mut words, m::SET_OUT_REF_PIC_CHROMA, recon);
    emit(
        &mut words,
        m::SET_OUT_ENC_STATUS,
        a8(address(layout.status[slot])),
    );
    emit(
        &mut words,
        m::SET_OUT_BITSTREAM,
        a8(address(layout.bitstream[slot])),
    );
    emit(&mut words, m::SET_IN_CUR_PIC, a8(frame.input));
    emit(&mut words, m::SET_IN_CUR_PIC_CHROMA_U, a8(frame.input));
    emit(&mut words, m::SET_IN_CUR_PIC_CHROMA_V, a8(frame.input));
    emit(
        &mut words,
        m::SET_IN_RCDATA,
        a8(address(layout.rc_data[slot])),
    );
    for r in 0..frame.refs {
        let picture = n - 1 - r;
        let dpb = match layout.codec {
            Codec::H264 => picture % 4,
            Codec::Hevc => r,
        };
        let reference = a8(address(layout.recon[(picture % recons) as usize]));
        emit(&mut words, m::SET_IN_REF_PIC0_LUMA + 4 * dpb, reference);
        emit(&mut words, m::SET_IN_REF_PIC0_CHROMA + 4 * dpb, reference);
    }
    emit(&mut words, m::EXECUTE, 0);
    let fence = address(layout.fence);
    emit(&mut words, m::SEMAPHORE_A, (fence >> 32) as u32 & 0xff);
    emit(&mut words, m::SEMAPHORE_B, fence as u32);
    emit(&mut words, m::SEMAPHORE_C, frame.payload);
    emit(&mut words, m::SEMAPHORE_D, SEMAPHORE_D_RELEASE_32);
    words
}
