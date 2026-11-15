//! `--probe-nvenc`: right after the GPUs start, before macOS, each bound
//! GPU's video encoder is asked for one semaphore release
//! (`stela_drivers::gpu::nvenc`), on a render node of the probe's own. The
//! answer is a few `NVENC PROBE` lines on the serial log; nothing it finds
//! stops the boot.
use super::client::{self, RENDER_MINOR};
use crate::output;
use core::{
    ffi::{c_int, c_void},
    hint,
    ptr::NonNull,
};
use stela_drivers::gpu::nvenc::{self, Node};

/// How long the encoder gets to release its semaphore.
const BOUND_NS: u64 = 500_000_000;
/// The pause between two reads of it, each followed by a fence query that
/// lets Linux run.
const PAUSE_NS: u64 = 50_000;
/// The most GPUs the probe looks at.
const GPUS: u32 = 8;

/// A render node the probe opened.
struct Probe(c_int);

// SAFETY: Linux's DRM ioctls honor their UAPI bounds, and `client::map`
// keeps a mapping until `client::unmap` removes it.
unsafe impl Node for Probe {
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

/// Probes the encoder of every GPU nouveau bound, in order.
pub(in crate::engine) fn probe() {
    for index in 0..GPUS {
        let Some(chipset) = client::chipset(index) else {
            if index == 0 {
                output::arguments(format_args!("NVENC PROBE skipped: no GPU answers\n"));
            }
            break;
        };
        let Some(fd) = client::open_node(RENDER_MINOR + index) else {
            output::arguments(format_args!(
                "NVENC PROBE GPU {index}: its render node did not open\n"
            ));
            continue;
        };
        output::arguments(format_args!(
            "NVENC PROBE GPU {index}: render node {}\n",
            RENDER_MINOR + index
        ));
        let outcome = nvenc::probe(&mut Probe(fd), chipset, BOUND_NS, &mut |line| {
            output::arguments(format_args!("{line}\n"));
        });
        if outcome.keeps_channel() {
            // Closing the file frees the channel, which nouveau idles first:
            // a busy wait for a push that never finished.
            output::arguments(format_args!(
                "NVENC PROBE GPU {index}: its file stays open with the unfinished channel\n"
            ));
        } else {
            client::close_descriptor(fd);
        }
    }
    // A wedge after this still leaves the lines on the USB.
    output::flush_log();
}
