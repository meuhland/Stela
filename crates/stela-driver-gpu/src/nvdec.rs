//! NVDEC, GA10x's video decoder (`NVC7B0_VIDEO_DECODER`), driven through
//! nouveau from Stela's host.
//!
//! For now this is the "alive" probe alone ([`probe`], `--probe-nvdec`):
//! does GSP-RM run the decoder for nouveau on this GPU? It is NVENC's probe
//! ([`nvenc::probe_engine`]) on the decoder: on a render node of its own it
//! opens a channel on the decoder's runlist (nouveau's abi16
//! `NOUVEAU_FIFO_ENGINE_NVDEC`, Stela's Linux patch 0208 "allow allocating
//! NVDEC channels via abi16"), creates the NVC7B0 class on it, and pushes
//! `SET_OBJECT`, `SET_APPLICATION_ID` (H.264) and a `SEMAPHORE_A..D`
//! release of [`PAYLOAD`] to a small buffer of video memory, read until the
//! payload lands or the caller's bound passes. NVC7B0's semaphore methods
//! sit where NVC7B7's do (`clc7b0.h`), so the push is NVENC's with the
//! decoder's class and application.
//!
//! Like NVENC's, it never stops the boot: every refusal is a line and a
//! value, the wait is bounded by the caller's clock, and a channel whose
//! push never finished is left open with its file.
use crate::nouveau_chip::{Architecture, Chip};
use crate::nvenc::{self, Log, Node, Outcome, PUSH_WORDS, Target};

/// `NVC7B0_VIDEO_DECODER`, GA10x's NVDEC (`clc7b0.h`). GA100's NVC6B0,
/// Turing's NVC4B0 and Ada's NVC9B0 are not driven.
pub const DECODER_CLASS: u32 = 0xc7b0;
/// What the release writes: "NVDE" in ASCII.
pub const PAYLOAD: u32 = 0x4e56_4445;
/// The probe's object handle for the decoder on its channel.
pub const OBJECT_HANDLE: u32 = 0xc7b0_0001;

/// `NOUVEAU_FIFO_ENGINE_NVDEC`, Stela's abi16 engine (Linux patch 0208).
/// Provisional: the NVDEC engine queued upstream for 7.3 is not in Stela's
/// tree, so this is Stela's own value, the next free bit after
/// [`nvenc::FIFO_ENGINE_NVENC`], to be aligned with upstream's once seen.
pub const FIFO_ENGINE_NVDEC: u32 = 0x800;

/// The `clc7b0.h` methods the probe sends.
pub mod method {
    /// The host's `NV_UDMA_OBJECT`: binds the class to the subchannel.
    pub const SET_OBJECT: u32 = 0x0000;
    pub const NOP: u32 = 0x0100;
    pub const SET_APPLICATION_ID: u32 = 0x0200;
    pub const SET_WATCHDOG_TIMER: u32 = 0x0204;
    pub const SEMAPHORE_A: u32 = 0x0240;
    pub const SEMAPHORE_B: u32 = 0x0244;
    pub const SEMAPHORE_C: u32 = 0x0248;
    pub const EXECUTE: u32 = 0x0300;
    pub const SEMAPHORE_D: u32 = 0x0304;
}

// The probe's push is NVENC's: the decoder's methods must sit where the
// encoder's do.
const _: () = assert!(
    method::SET_OBJECT == nvenc::method::SET_OBJECT
        && method::SET_APPLICATION_ID == nvenc::method::SET_APPLICATION_ID
        && method::SEMAPHORE_A == nvenc::method::SEMAPHORE_A
        && method::SEMAPHORE_B == nvenc::method::SEMAPHORE_B
        && method::SEMAPHORE_C == nvenc::method::SEMAPHORE_C
        && method::SEMAPHORE_D == nvenc::method::SEMAPHORE_D
);

/// `NVC7B0_SET_APPLICATION_ID_ID_H264`.
pub const APPLICATION_H264: u32 = 3;
/// `NVC7B0_SET_APPLICATION_ID_ID_HEVC`.
pub const APPLICATION_HEVC: u32 = 7;

/// The decoder class of `chipset`, when this module knows it: the GA10x
/// parts with a display, GA102 to GA107, as NVENC's.
pub fn decoder_class(chipset: u16) -> Option<u32> {
    let chip = Chip::find(chipset)?;
    (chip.architecture == Architecture::Ampere && chip.display).then_some(DECODER_CLASS)
}

/// The decoder's probe (`--probe-nvdec`).
pub const PROBE: Target = Target {
    name: "NVDEC",
    engine: "decoder",
    class: decoder_class,
    parts: "GA102-GA107 only",
    fifo_engine: FIFO_ENGINE_NVDEC,
    patch: "0208",
    application: APPLICATION_H264,
    application_name: "H264",
    object_handle: OBJECT_HANDLE,
    payload: PAYLOAD,
};

/// The probe's push: bind the decoder, name H.264, and release `payload` to
/// the GPU address `semaphore`.
pub const fn push(semaphore: u64, payload: u32) -> [u32; PUSH_WORDS] {
    nvenc::probe_push(DECODER_CLASS, APPLICATION_H264, semaphore, payload)
}

/// Probes the decoder of `chipset` through `node`, waiting at most
/// `timeout_ns` for its semaphore, and says what it found to `log` as
/// `NVDEC PROBE` lines.
pub fn probe<N: Node>(node: &mut N, chipset: u16, timeout_ns: u64, log: Log<'_>) -> Outcome {
    nvenc::probe_engine(node, &PROBE, chipset, timeout_ns, log)
}

#[cfg(test)]
mod tests;
