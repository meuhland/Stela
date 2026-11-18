//! NVENC before macOS, on render node files of its own
//! (`stela_drivers::gpu::nvenc`):
//!
//! - `--probe-nvenc`: right after the GPUs start, each bound GA10x GPU's
//!   video encoder is asked for one semaphore release. The answer is a few
//!   `NVENC PROBE` lines.
//! - `--nvenc-selftest`: each such GPU encodes a moving pattern at
//!   1920x1080, H.264 then HEVC, at QP 25 then 20 Mb/s CBR, every frame
//!   checked and logged as `NVENC SELFTEST` lines, each ending with the
//!   GPU's P-state and PCIe link (`| gpu pstate=P0 pcie=gen4x16`), which
//!   the engine's time follows. The fixed-QP streams are
//!   also logged as base64 (`NVENC SELFTEST DATA`), since the engine cannot
//!   create a file on the stick: its FAT32 is read-only to Stela's drivers,
//!   which write only the clusters the stela tool preallocated for the log.
//!
//! - **The guest's encoder sessions** ([`attach`]): each NVIDIA GPU whose
//!   encoder this port drives serves the sessions StelaVT opens through its
//!   Metal service's gate (`metal::service::encoders`), unless `--no-nvenc`
//!   says not to, or the probe or the self-test found its encoder failing.
//!   Each session is a render node file of its own, as the self-test's.
//!
//! Nothing either finds stops the boot. A push that never finished keeps
//! its channel and file open, since freeing either makes nouveau idle the
//! channel, a busy wait of up to 15 s; a GPU that timed out once is not
//! asked again.
use super::client::{self, RENDER_MINOR};
use super::stack::{self, Stack};
use crate::{native_mmio::Window, output};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    ffi::{c_int, c_void},
    fmt, hint,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use stela_drivers::gpu::Vendor;
use stela_drivers::gpu::metal::service::encoders::{EncoderPlatform, Encoders, GateEncoders};
use stela_drivers::gpu::nvenc::{self, Node, Rate, selftest};

/// How long the probe's encoder gets to release its semaphore.
const BOUND_NS: u64 = 500_000_000;
/// The pause between two reads of a semaphore, each followed by a fence
/// query that lets Linux run.
const PAUSE_NS: u64 = 50_000;
/// The self-test's fixed-QP streams are logged up to this many bytes, in
/// whole frames: about 100 KiB each at QP 25.
const DATA_CAP: usize = 256 * 1024;

/// Whether the Metal services serve the guest's encoder sessions: off with
/// `--no-nvenc`.
static SERVING: AtomicBool = AtomicBool::new(true);
/// The bound GPUs, by index, whose encoder the probe or the self-test found
/// failing: they serve no sessions.
static FAILED: AtomicU64 = AtomicU64::new(0);

/// The pause between two looks at a guest frame's semaphore, within one
/// gate call's budget.
const GATE_PAUSE_NS: u64 = 10_000;
/// The longest an encoder call runs the host's Linux in place to push its
/// job ([`EncoderPlatform::kick`]); it stops sooner once nothing runs.
const KICK_NS: u64 = 200_000;
/// The stack each GPU's encoder calls run on, one at a time under the
/// encoders' lock: session code is shallow.
const ENCODER_STACK_BYTES: usize = 1 << 20;

/// `--no-nvenc`: the Metal services serve no encoder sessions, and StelaVT
/// encodes H.264 with x264 as before.
pub(in crate::engine) fn set_serving(serving: bool) {
    SERVING.store(serving, Ordering::Relaxed);
}

/// Notes that GPU `index`'s encoder failed before macOS.
fn failed(index: u32) {
    FAILED.fetch_or(1 << index.min(63), Ordering::Relaxed);
}

/// A render node file of the probe's, the self-test's or a session's own.
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

/// The bound NVIDIA GPUs, by index, with their nouveau chipset; every
/// other is named and left alone under `prefix`.
fn nvidia_gpus(prefix: &str) -> Vec<(u32, u16)> {
    let Some(installed) = super::installed() else {
        line(format_args!("{prefix} skipped: no GPU answers"));
        return Vec::new();
    };
    let mut found = Vec::new();
    for (index, bound) in installed.start.bound.iter().enumerate() {
        let index = index as u32;
        if bound.stack.vendor != Vendor::Nvidia {
            line(format_args!(
                "{prefix} GPU {index} ({}): not NVIDIA's, left alone",
                bound.name
            ));
            continue;
        }
        match client::query(index, bound.stack.kernel.chip) {
            Some(chipset) => found.push((index, chipset as u16)),
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
    let fd =
        client::minor(index as usize).and_then(|minor| client::open_node(RENDER_MINOR + minor));
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
        if matches!(
            outcome,
            nvenc::Outcome::Timeout { .. } | nvenc::Outcome::Failed { .. }
        ) {
            failed(index);
        }
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
            // Each line carries the GPU's P-state and PCIe link as nouveau
            // last sampled them: the engine's time and the stores into the
            // GPU's memory follow both.
            let (report, closed) =
                selftest::run(RenderFile(fd), chipset, codec, rate, &mut |text| {
                    line(format_args!("{text} | gpu {}", super::perf_now()));
                });
            if closed.keeps_file {
                wedged = true;
            } else {
                client::close_descriptor(fd);
            }
            wedged |= report.timed_out;
            if matches!(rate, Rate::ConstQp(_)) {
                data(&report);
            }
            if !report.passed {
                failed(index);
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

/// The encoders of one bound GPU, on render node files of its own.
struct GateGpu {
    index: usize,
    chipset: u16,
    minor: u32,
    stack: u64,
}

impl EncoderPlatform for GateGpu {
    type Node = RenderFile;

    fn index(&self) -> usize {
        self.index
    }

    fn chipset(&self) -> u16 {
        self.chipset
    }

    fn open_node(&self) -> Result<RenderFile, &'static str> {
        client::open_node(RENDER_MINOR + self.minor)
            .map(RenderFile)
            .ok_or("the GPU's render node did not open")
    }

    fn close_node(&self, node: RenderFile, keeps_file: bool) {
        // A file whose channel is still busy stays open: closing it would
        // make nouveau idle the channel.
        if !keeps_file {
            client::close_descriptor(node.0);
        }
    }

    fn now_ns(&self) -> u64 {
        client::now_ns()
    }

    fn pause(&self) {
        let until = client::now_ns().saturating_add(GATE_PAUSE_NS);
        while client::now_ns() < until {
            hint::spin_loop();
        }
    }

    fn kick(&self) -> bool {
        super::run_ready_here(KICK_NS)
    }

    fn relax(&self) {
        super::gate::relax();
    }

    fn gpu_state(&self) -> Option<String> {
        Some(super::perf_now())
    }

    fn run(&self, body: &mut dyn FnMut()) {
        // SAFETY: the stack lives for the boot, and only the encoders'
        // lock's holder runs on it.
        unsafe { stack::on_stack(self.stack, body) };
    }

    fn log(&self, line: fmt::Arguments<'_>) {
        output::arguments(format_args!("{line}\n"));
    }
}

/// The encoders bound GPU `index` serves the guest's sessions with, or why
/// it serves none, which is logged: it must be NVIDIA's, with an encoder
/// this port drives (GA102 to GA107), and neither `--no-nvenc` nor a failed
/// probe or self-test may say no.
pub(super) fn attach(
    window: &'static Window,
    index: usize,
    name: &str,
    vendor: Vendor,
    chipset: u16,
) -> Option<Arc<dyn GateEncoders>> {
    let refuse = |why: fmt::Arguments<'_>| {
        line(format_args!("NVENC SESSION GPU {index} ({name}): {why}"));
        None
    };
    if vendor != Vendor::Nvidia {
        return None;
    }
    if !SERVING.load(Ordering::Relaxed) {
        return refuse(format_args!("no sessions: --no-nvenc"));
    }
    if nvenc::encoder_class(chipset).is_none() {
        return refuse(format_args!(
            "no sessions: chipset {chipset:#x} has no encoder this port drives (GA102-GA107 only)"
        ));
    }
    if FAILED.load(Ordering::Relaxed) & (1 << index.min(63)) != 0 {
        return refuse(format_args!(
            "no sessions: its encoder failed the probe or the self-test"
        ));
    }
    let Some(minor) = client::minor(index) else {
        return refuse(format_args!("no sessions: its render node is unknown"));
    };
    let Some(stack) = Stack::map(window, ENCODER_STACK_BYTES) else {
        return refuse(format_args!("no sessions: no memory for their stack"));
    };
    let top = stack.top();
    // It serves for the boot.
    core::mem::forget(stack);
    line(format_args!(
        "NVENC SESSION GPU {index} ({name}): serves H.264 and HEVC sessions on chipset          {chipset:#x}, at most {} at once",
        stela_drivers::gpu::metal::service::encoders::MAX_SESSIONS
    ));
    Some(Arc::new(Encoders::new(GateGpu {
        index,
        chipset,
        minor,
        stack: top,
    })))
}
