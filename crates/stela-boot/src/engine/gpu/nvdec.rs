//! NVDEC before macOS, on render node files of its own
//! (`stela_drivers::gpu::nvdec`):
//!
//! - `--probe-nvdec`: right after the GPUs start, after NVENC's probe and
//!   self-test, each bound GA10x GPU's video decoder is asked for one
//!   semaphore release: an abi16 channel on the NVDEC runlist (Linux patch
//!   0208), the NVC7B0 class on it, and a push of `SET_OBJECT`,
//!   `SET_APPLICATION_ID` (H.264) and `SEMAPHORE_A..D`. Its steps are
//!   `NVDEC PROBE` lines, and each GPU ends with one line of what it found:
//!   `NVDEC PROBE GPU 0 (<name>): class 0xc7b0 channel opened, semaphore
//!   released in N us`, or the refusal, errno or timeout.
//!
//! Nothing it finds stops the boot or changes NVENC: its sessions are
//! served as before. The wait is bounded, and a push that never finished
//! keeps its channel and file open, since freeing either makes nouveau idle
//! the channel, a busy wait of up to 15 s.
use super::client;
use super::nvenc::{RenderFile, line, nvidia_gpus, open};
use crate::output;
use stela_drivers::gpu::nvdec;

/// How long the probe's decoder gets to release its semaphore.
const BOUND_NS: u64 = 500_000_000;

/// Probes the decoder of every NVIDIA GPU nouveau bound, in order.
pub(in crate::engine) fn probe() {
    for (index, chipset, name) in nvidia_gpus("NVDEC PROBE") {
        let Some(fd) = open("NVDEC PROBE", index) else {
            continue;
        };
        line(format_args!(
            "NVDEC PROBE GPU {index} ({name}): render node file {fd}"
        ));
        let outcome = nvdec::probe(&mut RenderFile(fd), chipset, BOUND_NS, &mut |text| {
            line(text);
        });
        line(format_args!(
            "NVDEC PROBE GPU {index} ({name}): {}",
            outcome.verdict(&nvdec::PROBE, chipset, BOUND_NS)
        ));
        if outcome.keeps_channel() {
            line(format_args!(
                "NVDEC PROBE GPU {index} ({name}): its file stays open with the unfinished channel"
            ));
        } else {
            client::close_descriptor(fd);
        }
        // A wedge after this still leaves the lines on the USB.
        output::flush_log();
    }
    output::flush_log();
}
