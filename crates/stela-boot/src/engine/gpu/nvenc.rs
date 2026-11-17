//! NVENC before macOS, on render node files of its own
//! (`stela_drivers::gpu::nvenc`):
//!
//! - `--probe-nvenc`: right after the GPUs start, each bound GA10x GPU's
//!   video encoder is asked for one semaphore release. The answer is a few
//!   `NVENC PROBE` lines.
//! - `--nvenc-selftest`: each such GPU encodes a moving pattern at
//!   1920x1080, H.264 then HEVC, at QP 25 then 20 Mb/s CBR, every frame
//!   checked and logged as `NVENC SELFTEST` lines. The fixed-QP streams are
//!   also logged as base64 (`NVENC SELFTEST DATA`), since the engine cannot
//!   create a file on the stick: its FAT32 is read-only to Stela's drivers,
//!   which write only the clusters the stela tool preallocated for the log.
//!
//! Nothing either finds stops the boot. A push that never finished keeps
//! its channel and file open, since freeing either makes nouveau idle the
//! channel, a busy wait of up to 15 s; a GPU that timed out once is not
//! asked again.
use super::client::{self, RENDER_MINOR};
use crate::output;
use alloc::{string::String, vec::Vec};
use core::{
    ffi::{c_int, c_void},
    hint,
    ptr::NonNull,
};
use stela_drivers::gpu::nvenc::{self, Node, Rate, selftest};

/// How long the probe's encoder gets to release its semaphore.
const BOUND_NS: u64 = 500_000_000;
/// The pause between two reads of a semaphore, each followed by a fence
/// query that lets Linux run.
const PAUSE_NS: u64 = 50_000;
/// The self-test's fixed-QP streams are logged up to this many bytes, in
/// whole frames: about 100 KiB each at QP 25.
const DATA_CAP: usize = 256 * 1024;

/// A render node file of the probe's or the self-test's own.
struct RenderFile(c_int);

// SAFETY: Linux's DRM ioctls honor their UAPI bounds, and `client::map`
// keeps a mapping until `client::unmap` removes it.
unsafe impl Node for RenderFile {
    fn control(&mut self, request: u32, argument: *mut c_void) -> i64 {
        client::control(self.0, request, argument)
    }

    fn map(&mut self, offset: u64, bytes: usize) -> Option<NonNull<u8>> {
        client::map(self.0, offset, bytes)
    }

    fn unmap(&mut self, address: NonNull<u8>, _: usize) {
        client::unmap(address);
    }

    fn now_ns(&mut self) -> u64 {
        client::now_ns()
    }

    fn pause(&mut self) {
        let until = client::now_ns().saturating_add(PAUSE_NS);
        while client::now_ns() < until {
            hint::spin_loop();
        }
    }
}

fn line(args: core::fmt::Arguments<'_>) {
    output::arguments(format_args!("{args}\n"));
}

/// The GPUs nouveau bound, by index, with their chipset. Every one is
/// NVIDIA's, since this build gives GPUs to nouveau only, and bound GPU
/// `index` has render node `RENDER_MINOR + index`.
fn nvidia_gpus(prefix: &str) -> Vec<(u32, u16)> {
    let Some(installed) = super::installed() else {
        line(format_args!("{prefix} skipped: no GPU answers"));
        return Vec::new();
    };
    let mut found = Vec::new();
    for (index, bound) in installed.start.bound.iter().enumerate() {
        let index = index as u32;
        match client::chipset(index) {
            Some(chipset) => found.push((index, chipset)),
            None => line(format_args!(
                "{prefix} GPU {index} ({}): nouveau did not say its chipset",
                bound.name
            )),
        }
    }
    if found.is_empty() {
        line(format_args!("{prefix} skipped: no NVIDIA GPU is bound"));
    }
    found
}

/// A render node file of GPU `index`.
fn open(prefix: &str, index: u32) -> Option<c_int> {
    let fd = client::open_node(RENDER_MINOR + index);
    if fd.is_none() {
        line(format_args!(
            "{prefix} GPU {index}: its render node did not open"
        ));
    }
    fd
}

/// Probes the encoder of every NVIDIA GPU nouveau bound, in order.
pub(in crate::engine) fn probe() {
    for (index, chipset) in nvidia_gpus("NVENC PROBE") {
        let Some(fd) = open("NVENC PROBE", index) else {
            continue;
        };
        line(format_args!(
            "NVENC PROBE GPU {index}: render node file {fd}"
        ));
        let outcome = nvenc::probe(&mut RenderFile(fd), chipset, BOUND_NS, &mut |text| {
            line(text);
        });
        if outcome.keeps_channel() {
            line(format_args!(
                "NVENC PROBE GPU {index}: its file stays open with the unfinished channel"
            ));
        } else {
            client::close_descriptor(fd);
        }
    }
    // A wedge after this still leaves the lines on the USB.
    output::flush_log();
}

/// Runs the self-test on every NVIDIA GPU nouveau bound whose encoder this
/// port drives.
pub(in crate::engine) fn selftest() {
    let started = client::now_ns();
    for (index, chipset) in nvidia_gpus("NVENC SELFTEST") {
        if nvenc::encoder_class(chipset).is_none() {
            line(format_args!(
                "NVENC SELFTEST GPU {index}: chipset {chipset:#x} has no encoder this port drives \
                 (GA102-GA107 only)"
            ));
            continue;
        }
        line(format_args!(
            "NVENC SELFTEST GPU {index}: chipset {chipset:#x}, {} runs of {} frames at 1920x1080",
            selftest::RUNS.len(),
            selftest::FRAMES
        ));
        let mut clock = client::now_ns;
        let (took, grey) = selftest::time_conversion(&mut clock);
        line(format_args!(
            "NVENC SELFTEST convert: BGRA to block-linear NV12, 1920x1080 on the host CPU into \
             host memory, {} us; grey {}",
            took / 1000,
            if grey {
                "stays grey"
            } else {
                "CHANGED: the conversion is wrong"
            }
        ));
        let mut summary: Vec<String> = Vec::new();
        let mut wedged = false;
        for (codec, rate) in selftest::RUNS {
            let name = selftest::run_name(codec, rate);
            if wedged {
                line(format_args!(
                    "NVENC SELFTEST {name} skipped: an earlier run left the GPU's encoder busy"
                ));
                summary.push(alloc::format!("{name} skipped"));
                continue;
            }
            let Some(fd) = open("NVENC SELFTEST", index) else {
                summary.push(alloc::format!("{name} no-node"));
                continue;
            };
            let (report, closed) =
                selftest::run(RenderFile(fd), chipset, codec, rate, &mut |text| line(text));
            if closed.keeps_file {
                wedged = true;
            } else {
                client::close_descriptor(fd);
            }
            wedged |= report.timed_out;
            if matches!(rate, Rate::ConstQp(_)) {
                data(&report);
            }
            summary.push(alloc::format!(
                "{name} {}",
                if report.passed { "PASS" } else { "FAIL" }
            ));
            output::flush_log();
        }
        line(format_args!(
            "NVENC SELFTEST GPU {index} summary: {} in {} ms",
            summary.join(", "),
            client::now_ns().saturating_sub(started) / 1_000_000
        ));
    }
    output::flush_log();
}

/// Logs the whole frames of `report`'s stream that fit [`DATA_CAP`] as
/// base64, one `NVENC SELFTEST DATA` line each, for ffmpeg afterwards:
/// `grep -a 'NVENC SELFTEST DATA h264-qp25 ' boot.log | grep -v ' begin:' |
/// awk '{print $NF}' | base64 -d > h264-qp25.h264`.
fn data(report: &selftest::Report) {
    let name = &report.name;
    let kept = selftest::capped(report, DATA_CAP);
    if kept.is_empty() {
        line(format_args!(
            "NVENC SELFTEST DATA {name} begin: none ({} bytes, no whole frame within {DATA_CAP})",
            report.stream.len()
        ));
        return;
    }
    let lines = selftest::base64_lines(kept);
    line(format_args!(
        "NVENC SELFTEST DATA {name} begin: {} of {} bytes, {} lines, sha256 {}",
        kept.len(),
        report.stream.len(),
        lines.len(),
        selftest::sha256_hex(kept)
    ));
    let count = lines.len();
    for (number, text) in lines.iter().enumerate() {
        line(format_args!(
            "NVENC SELFTEST DATA {name} {:05}/{count:05} {text}",
            number + 1
        ));
    }
}
