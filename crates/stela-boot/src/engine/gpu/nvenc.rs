//! `--probe-nvenc`: right after the GPUs start, before macOS, each bound
//! GA10x GPU's video encoder is asked for one semaphore release
//! (`stela_drivers::gpu::nvenc`), on a render node file of the probe's own.
//! The answer is a few `NVENC PROBE` lines on the serial log; nothing it
//! finds stops the boot. A push that never finished keeps its channel and
//! file open, since freeing either makes nouveau idle the channel, a busy
//! wait of up to 15 s.
use super::client::{self, RENDER_MINOR};
use crate::output;
use alloc::vec::Vec;
use core::{
    ffi::{c_int, c_void},
    hint,
    ptr::NonNull,
};
use stela_drivers::gpu::Vendor;
use stela_drivers::gpu::nvenc::{self, Node};

/// How long the probe's encoder gets to release its semaphore.
const BOUND_NS: u64 = 500_000_000;
/// The pause between two reads of a semaphore, each followed by a fence
/// query that lets Linux run.
const PAUSE_NS: u64 = 50_000;

/// A render node file of the probe's own.
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
