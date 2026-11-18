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
//! - `--nvdec-selftest`: after the probe, each such GPU decodes two embedded
//!   clips (NVENC's boot-1 streams, H.264 then HEVC, 1920x1080, 30 frames
//!   each) on a session of its own per clip, and checks every decoded
//!   frame's MD5 against the software decode's. Each picture is read back
//!   by the copy engine into system memory (a copy-engine channel on the
//!   session's file), or over BAR1 when that fails. Each frame is an
//!   `NVDEC SELFTEST <codec> frame N: ... md5 ... MATCH|MISMATCH ... gpu_us
//!   ..., read_us N (ce_us C + cpu_us P)` line, or `read_us N (bar1)`,
//!   ending with the GPU's P-state, and each GPU ends with
//!   `NVDEC SELFTEST GPU 0 summary: h264 PASS (30/30, read ce), hevc PASS
//!   (30/30, read ce) in N ms`. A copy that never completed keeps its file
//!   open, and the GPU's next clips read over BAR1.
//!
//! Nothing either finds stops the boot or changes NVENC: its sessions are
//! served as before. Waits are bounded, and a push that never finished
//! keeps its channel and file open, since freeing either makes nouveau idle
//! the channel, a busy wait of up to 15 s; a GPU whose decoder timed out
//! is not asked again.
use super::client;
use super::nvenc::{RenderFile, line, nvidia_gpus, open};
use crate::output;
use alloc::{format, string::String, vec::Vec};
use stela_drivers::gpu::nvdec::{self, selftest};

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

/// Decodes the self-test's clips on every NVIDIA GPU nouveau bound whose
/// decoder this port drives.
pub(in crate::engine) fn selftest() {
    let started = client::now_ns();
    let clips = selftest::clips();
    for (index, chipset, name) in nvidia_gpus("NVDEC SELFTEST") {
        if nvdec::decoder_class(chipset).is_none() {
            line(format_args!(
                "NVDEC SELFTEST GPU {index} ({name}): chipset {chipset:#x} has no decoder this \
                 port drives (GA102-GA107 only)"
            ));
            continue;
        }
        line(format_args!(
            "NVDEC SELFTEST GPU {index} ({name}): chipset {chipset:#x}, {} clips of 30 frames at \
             1920x1080, each frame's MD5 against the software decode",
            clips.len()
        ));
        let mut summary: Vec<String> = Vec::new();
        let mut wedged = false;
        // A copy that never completed leaves a copy-engine channel busy in
        // its file: the GPU's next clips read over BAR1.
        let mut readback = selftest::Readback::CopyEngine;
        for clip in &clips {
            let codec = clip.codec.name();
            if wedged {
                line(format_args!(
                    "NVDEC SELFTEST {codec} skipped: an earlier clip left the GPU's decoder busy"
                ));
                summary.push(format!("{codec} SKIPPED"));
                continue;
            }
            let Some(fd) = open("NVDEC SELFTEST", index) else {
                summary.push(format!("{codec} FAIL (no render node)"));
                continue;
            };
            // Each line carries the GPU's P-state and PCIe link as nouveau
            // last sampled them: the engine's time follows both.
            let (report, closed) =
                selftest::run_with(RenderFile(fd), chipset, clip, readback, &mut |text| {
                    line(format_args!("{text} | gpu {}", super::perf_now()));
                });
            if closed.keeps_file {
                // Only the copy engine's channel is busy when the decoder
                // never timed out: the decoder is asked again, the copy
                // engine not.
                if report.ce_hung && !report.timed_out {
                    readback = selftest::Readback::Bar1;
                } else {
                    wedged = true;
                }
            } else {
                client::close_descriptor(fd);
            }
            wedged |= report.timed_out;
            summary.push(format!(
                "{codec} {} ({}/{}, read {})",
                if report.passed { "PASS" } else { "FAIL" },
                report.matched,
                report.expected,
                match (report.ce_frames > 0, report.bar1_frames > 0) {
                    (true, false) => "ce",
                    (false, true) => "bar1",
                    (true, true) => "ce+bar1",
                    (false, false) => "none",
                }
            ));
            // A wedge after this still leaves the lines on the USB.
            output::flush_log();
        }
        line(format_args!(
            "NVDEC SELFTEST GPU {index} summary: {} in {} ms",
            summary.join(", "),
            client::now_ns().saturating_sub(started) / 1_000_000
        ));
        output::flush_log();
    }
    output::flush_log();
}
