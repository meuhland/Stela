//! The NVENC "alive" probe (`--probe-nvenc`): does GSP-RM run the video
//! encoder for nouveau on this GPU?
//!
//! On a render node of its own the probe opens a channel on the encoder's
//! runlist (nouveau's abi16 `NOUVEAU_FIFO_ENGINE_NVENC`, Stela's patch
//! 0202), creates the encoder class on it, and pushes the least the class
//! takes: `SET_OBJECT`, `SET_APPLICATION_ID` (H.264) and a `SEMAPHORE_A..D`
//! release of [`PAYLOAD`] to a small buffer of video memory. It then reads
//! that buffer until the payload lands or [`Node::now_ns`] passes the bound.
//! No picture is set up and nothing is encoded: the release alone proves the
//! channel, the class and the encoder's falcon under GSP.
//!
//! The probe never stops: every refusal is a line and an [`Outcome`]. Its
//! waits are bounded by the caller's clock; the ioctls themselves are bounded
//! by nouveau (GSP RPCs by `R535_GSP_RPC_MS`). A channel whose pushes never
//! finished is left open ([`Outcome::keeps_channel`]): freeing it would make
//! nouveau idle it, a busy wait of up to 15 s on a fence that may never come.
//!
//! The legacy abi16 uAPI keeps it small: a fresh file has no VM_BIND state,
//! so `CHANNEL_ALLOC` turns it off, buffers get their addresses from
//! `GEM_NEW`, and `GEM_PUSHBUF` submits with a fence of nouveau's own.
use core::ffi::c_void;
use core::fmt;
use core::ptr::NonNull;

use crate::nouveau_chip::{Architecture, Chip};

/// `NVC7B7_VIDEO_ENCODER`, GA10x's NVENC (7.3).
pub const ENCODER_CLASS: u32 = 0xc7b7;
/// The subchannel video classes take on their own channel, as Mesa's
/// `SUBC_NVC5B0` and `SUBC_NVC7B7`.
pub const SUBCHANNEL: u32 = 4;
/// What the release writes: "NVEN" in ASCII.
pub const PAYLOAD: u32 = 0x4e56_454e;
/// The probe's object handle for the encoder on its channel.
pub const OBJECT_HANDLE: u32 = 0xc7b7_0001;
/// The bytes of each buffer the probe allocates.
pub const BUFFER_BYTES: usize = 0x1000;

/// `clc7b7.h` methods.
pub mod method {
    /// The host's `NV_UDMA_OBJECT`: binds the class to the subchannel.
    pub const SET_OBJECT: u32 = 0x0000;
    pub const NOP: u32 = 0x0100;
    pub const SET_APPLICATION_ID: u32 = 0x0200;
    pub const SEMAPHORE_A: u32 = 0x0240;
    pub const SEMAPHORE_B: u32 = 0x0244;
    pub const SEMAPHORE_C: u32 = 0x0248;
    pub const EXECUTE: u32 = 0x0300;
    pub const SEMAPHORE_D: u32 = 0x0304;
}

/// `NVC7B7_SET_APPLICATION_ID_ID_NVENC_H264`.
pub const APPLICATION_H264: u32 = 1;

/// The `NVC7B7_SEMAPHORE_D` the probe sends: `STRUCTURE_SIZE_ONE` (1:0, the
/// payload alone, no timestamp), `AWAKEN_ENABLE_FALSE` (8:8, no interrupt),
/// `OPERATION_RELEASE` (17:16), `FLUSH_DISABLE_FALSE` (21:21, flush before
/// the write) and `PAYLOAD_SIZE_32BIT` (23:23): every field zero.
pub const SEMAPHORE_D_RELEASE_32: u32 = 0;

/// A Fermi-and-later incrementing method header (`NVC0_FIFO_PKHDR_SQ`):
/// `count` data words follow, to `method`, `method + 4`, and so on.
pub const fn header(subchannel: u32, method: u32, count: u32) -> u32 {
    1 << 29 | (count & 0x1fff) << 16 | (subchannel & 7) << 13 | ((method >> 2) & 0xfff)
}

/// The words of a push.
pub const PUSH_WORDS: usize = 10;

/// The probe's push: bind the encoder, name H.264, and release `payload` to
/// the GPU address `semaphore`.
pub const fn push(semaphore: u64, payload: u32) -> [u32; PUSH_WORDS] {
    [
        header(SUBCHANNEL, method::SET_OBJECT, 1),
        ENCODER_CLASS,
        header(SUBCHANNEL, method::SET_APPLICATION_ID, 1),
        APPLICATION_H264,
        header(SUBCHANNEL, method::SEMAPHORE_A, 3),
        // SEMAPHORE_A_UPPER is bits 7:0, a 40-bit address.
        (semaphore >> 32) as u32 & 0xff,
        semaphore as u32,
        payload,
        header(SUBCHANNEL, method::SEMAPHORE_D, 1),
        SEMAPHORE_D_RELEASE_32,
    ]
}

/// The encoder class of `chipset`, when the probe knows it: the GA10x parts
/// with an encoder. GA100 has none; Turing's NVC4B7 and Ada's NVC9B7 are
/// not probed.
pub fn encoder_class(chipset: u16) -> Option<u32> {
    let chip = Chip::find(chipset)?;
    (chip.architecture == Architecture::Ampere && chip.display).then_some(ENCODER_CLASS)
}

// --- nouveau's abi16 uAPI (include/uapi/drm/nouveau_drm.h) ---

/// `DRM_COMMAND_BASE`.
const COMMAND_BASE: u32 = 0x40;

/// `_IOWR('d', number, T)`.
const fn read_write<T>(number: u32) -> u32 {
    3 << 30 | (size_of::<T>() as u32) << 16 | 0x64 << 8 | number
}

/// `_IOW('d', number, T)`.
const fn write<T>(number: u32) -> u32 {
    1 << 30 | (size_of::<T>() as u32) << 16 | 0x64 << 8 | number
}

/// `NOUVEAU_FIFO_ENGINE_NVENC`, Stela's abi16 engine (Linux patch 0202).
pub const FIFO_ENGINE_NVENC: u32 = 0x400;
pub const DOMAIN_VRAM: u32 = 1 << 1;
pub const DOMAIN_GART: u32 = 1 << 2;
pub const DOMAIN_MAPPABLE: u32 = 1 << 3;
/// `NOUVEAU_GEM_CPU_PREP_NOWAIT`.
const CPU_PREP_NOWAIT: u32 = 1;

/// `struct drm_nouveau_channel_alloc`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelAlloc {
    pub fb_ctxdma_handle: u32,
    pub tt_ctxdma_handle: u32,
    pub channel: i32,
    pub pushbuf_domains: u32,
    pub notifier_handle: u32,
    pub subchan: [[u32; 2]; 8],
    pub nr_subchan: u32,
}

/// `struct drm_nouveau_channel_free`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelFree {
    pub channel: i32,
}

/// `struct drm_nouveau_grobj_alloc` (`nouveau_abi16.h`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GrobjAlloc {
    pub channel: i32,
    pub handle: u32,
    pub class: i32,
}

/// `struct drm_nouveau_gem_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GemInfo {
    pub handle: u32,
    pub domain: u32,
    pub size: u64,
    /// The buffer's GPU address in the file's VM.
    pub offset: u64,
    /// Its `mmap` offset.
    pub map_handle: u64,
    pub tile_mode: u32,
    pub tile_flags: u32,
}

/// `struct drm_nouveau_gem_new`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GemNew {
    pub info: GemInfo,
    pub channel_hint: u32,
    pub align: u32,
}

/// `struct drm_nouveau_gem_pushbuf_bo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PushbufBo {
    pub user_priv: u64,
    pub handle: u32,
    pub read_domains: u32,
    pub write_domains: u32,
    pub valid_domains: u32,
    pub presumed_valid: u32,
    pub presumed_domain: u32,
    pub presumed_offset: u64,
}

/// `struct drm_nouveau_gem_pushbuf_push`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PushbufPush {
    pub bo_index: u32,
    pub pad: u32,
    pub offset: u64,
    pub length: u64,
}

/// `struct drm_nouveau_gem_pushbuf`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Pushbuf {
    pub channel: u32,
    pub nr_buffers: u32,
    pub buffers: u64,
    pub nr_relocs: u32,
    pub nr_push: u32,
    pub relocs: u64,
    pub push: u64,
    pub suffix0: u32,
    pub suffix1: u32,
    pub vram_available: u64,
    pub gart_available: u64,
}

/// `struct drm_nouveau_gem_cpu_prep`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuPrep {
    pub handle: u32,
    pub flags: u32,
}

/// `struct drm_gem_close`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GemClose {
    pub handle: u32,
    pub pad: u32,
}

pub const CHANNEL_ALLOC: u32 = read_write::<ChannelAlloc>(COMMAND_BASE + 0x02);
pub const CHANNEL_FREE: u32 = write::<ChannelFree>(COMMAND_BASE + 0x03);
pub const GROBJ_ALLOC: u32 = write::<GrobjAlloc>(COMMAND_BASE + 0x04);
pub const GEM_NEW: u32 = read_write::<GemNew>(COMMAND_BASE + 0x40);
pub const GEM_PUSHBUF: u32 = read_write::<Pushbuf>(COMMAND_BASE + 0x41);
pub const GEM_CPU_PREP: u32 = write::<CpuPrep>(COMMAND_BASE + 0x42);
/// `DRM_IOCTL_GEM_CLOSE`, DRM's own.
pub const GEM_CLOSE: u32 = write::<GemClose>(0x09);

const EBUSY: i64 = -16;

/// A bound GPU's render node, as the probe uses it.
///
/// # Safety
/// [`Node::control`] must honor nouveau's uAPI bounds for each request it is
/// given. A mapping [`Node::map`] gives must stay readable and writable for
/// its bytes until [`Node::unmap`].
pub unsafe trait Node {
    /// The ioctl `request` on `argument`: zero or a negative errno.
    fn control(&mut self, request: u32, argument: *mut c_void) -> i64;
    /// Maps `bytes` of the node at `offset`, a buffer's `map_handle`.
    fn map(&mut self, offset: u64, bytes: usize) -> Option<NonNull<u8>>;
    /// Removes the mapping of `bytes` [`Node::map`] gave at `address`.
    fn unmap(&mut self, address: NonNull<u8>, bytes: usize);
    /// A monotonic clock, in nanoseconds.
    fn now_ns(&mut self) -> u64;
    /// Waits a little between two reads of the semaphore.
    fn pause(&mut self);
}

/// The step a probe failed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Channel,
    Class,
    Buffer,
    Map,
    Submit,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Class => "class",
            Self::Buffer => "buffer",
            Self::Map => "map",
            Self::Submit => "submit",
        }
    }
}

/// How a probe ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The chipset has no encoder the probe knows.
    Skipped,
    /// The encoder released the semaphore this many microseconds after the
    /// submit returned.
    Alive { micros: u64 },
    /// The semaphore never held the payload within the bound; `value` is what
    /// it held last. `fence_done` says whether nouveau's own fence after the
    /// push signalled, which a host semaphore after the engine's methods
    /// releases.
    Timeout { value: u32, fence_done: bool },
    /// `step` was refused with this errno (zero for a refused mapping).
    Failed { step: Step, errno: i64 },
}

impl Outcome {
    /// Whether the probe left its channel open, so its file must stay open
    /// too: a push that never finished makes freeing either one wait for it.
    pub fn keeps_channel(&self) -> bool {
        matches!(
            self,
            Self::Timeout {
                fence_done: false,
                ..
            }
        )
    }

    /// A word for the summary line.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Alive { .. } => "alive",
            Self::Timeout { .. } => "timeout",
            Self::Failed { .. } => "failed",
        }
    }
}

/// An errno's name, for the lines.
pub fn errno_name(errno: i64) -> &'static str {
    match errno {
        0 => "refused",
        -1 => "EPERM",
        -2 => "ENOENT",
        -5 => "EIO",
        -12 => "ENOMEM",
        -14 => "EFAULT",
        -16 => "EBUSY",
        -19 => "ENODEV",
        -22 => "EINVAL",
        -28 => "ENOSPC",
        -38 => "ENOSYS",
        -95 => "EOPNOTSUPP",
        -110 => "ETIMEDOUT",
        _ => "errno",
    }
}

/// A buffer of video memory, mapped.
struct Buffer {
    handle: u32,
    address: u64,
    pointer: NonNull<u8>,
}

/// The lines go to `log` without a newline.
type Log<'a> = &'a mut dyn FnMut(fmt::Arguments<'_>);

/// Probes the encoder of `chipset` through `node`, waiting at most
/// `timeout_ns` for its semaphore, and says what it found to `log`.
pub fn probe<N: Node>(node: &mut N, chipset: u16, timeout_ns: u64, log: Log<'_>) -> Outcome {
    let Some(class) = encoder_class(chipset) else {
        log(format_args!(
            "NVENC PROBE skipped: chipset {chipset:#x} has no encoder class the probe knows"
        ));
        return Outcome::Skipped;
    };
    log(format_args!(
        "NVENC PROBE start: chipset {chipset:#x}, class {class:#06x}, bound {} ms",
        timeout_ns / 1_000_000
    ));
    let mut alloc = ChannelAlloc {
        fb_ctxdma_handle: !0,
        tt_ctxdma_handle: FIFO_ENGINE_NVENC,
        ..ChannelAlloc::default()
    };
    let result = node.control(CHANNEL_ALLOC, (&raw mut alloc).cast());
    if result != 0 {
        let why = match result {
            -38 => "this nouveau has no NVENC case in abi16 (Linux patch 0202)",
            -22 => "nouveau found no NVENC runlist on this GPU",
            _ => "nouveau or GSP-RM refused the channel",
        };
        log(format_args!(
            "NVENC PROBE channel refused: {} ({result}): {why}",
            errno_name(result)
        ));
        let outcome = Outcome::Failed {
            step: Step::Channel,
            errno: result,
        };
        done(log, &outcome);
        return outcome;
    }
    log(format_args!(
        "NVENC PROBE channel ok: engine NVENC, channel {}, push domains {:#x}",
        alloc.channel, alloc.pushbuf_domains
    ));
    let outcome = on_channel(node, alloc.channel, class, timeout_ns, log);
    if !outcome.keeps_channel() {
        let mut free = ChannelFree {
            channel: alloc.channel,
        };
        let result = node.control(CHANNEL_FREE, (&raw mut free).cast());
        if result != 0 {
            log(format_args!(
                "NVENC PROBE channel free refused: {} ({result})",
                errno_name(result)
            ));
        }
    }
    done(log, &outcome);
    outcome
}

/// The probe's last line.
fn done(log: Log<'_>, outcome: &Outcome) {
    match outcome {
        Outcome::Failed { step, errno } => log(format_args!(
            "NVENC PROBE done: failed at {}: {} ({errno})",
            step.name(),
            errno_name(*errno)
        )),
        outcome => log(format_args!("NVENC PROBE done: {}", outcome.name())),
    }
}

/// The class, buffers, push and wait on `channel`.
fn on_channel<N: Node>(
    node: &mut N,
    channel: i32,
    class: u32,
    timeout_ns: u64,
    log: Log<'_>,
) -> Outcome {
    let mut object = GrobjAlloc {
        channel,
        handle: OBJECT_HANDLE,
        class: class as i32,
    };
    let result = node.control(GROBJ_ALLOC, (&raw mut object).cast());
    if result != 0 {
        log(format_args!(
            "NVENC PROBE class refused: {class:#06x}: {} ({result}): GSP-RM or nouveau has no \
             encoder object on this channel",
            errno_name(result)
        ));
        return Outcome::Failed {
            step: Step::Class,
            errno: result,
        };
    }
    log(format_args!(
        "NVENC PROBE class ok: {class:#06x} instantiated as {OBJECT_HANDLE:#x}"
    ));
    let push_buffer = match buffer(node, channel, log, "push") {
        Ok(buffer) => buffer,
        Err(outcome) => return outcome,
    };
    let semaphore = match buffer(node, channel, log, "semaphore") {
        Ok(buffer) => buffer,
        Err(outcome) => {
            release(node, push_buffer, true);
            return outcome;
        }
    };
    log(format_args!(
        "NVENC PROBE buffers ok: push at {:#x}, semaphore at {:#x}",
        push_buffer.address, semaphore.address
    ));
    let words = push(semaphore.address, PAYLOAD);
    // SAFETY: both mappings are BUFFER_BYTES, past the push's words, and
    // writable until released; video memory is read and written whole words.
    unsafe {
        semaphore.pointer.cast::<u32>().write_volatile(0);
        for (index, word) in words.iter().enumerate() {
            push_buffer
                .pointer
                .cast::<u32>()
                .add(index)
                .write_volatile(*word);
        }
    }
    // The mappings may be write-combining: the words land before the submit.
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    let mut buffers = [
        PushbufBo {
            handle: push_buffer.handle,
            read_domains: DOMAIN_VRAM,
            valid_domains: DOMAIN_VRAM | DOMAIN_GART,
            ..PushbufBo::default()
        },
        PushbufBo {
            handle: semaphore.handle,
            write_domains: DOMAIN_VRAM,
            valid_domains: DOMAIN_VRAM | DOMAIN_GART,
            ..PushbufBo::default()
        },
    ];
    let mut pushes = [PushbufPush {
        bo_index: 0,
        pad: 0,
        offset: 0,
        length: (PUSH_WORDS * 4) as u64,
    }];
    let mut submit = Pushbuf {
        channel: channel as u32,
        nr_buffers: buffers.len() as u32,
        buffers: buffers.as_mut_ptr() as u64,
        nr_push: pushes.len() as u32,
        push: pushes.as_mut_ptr() as u64,
        ..Pushbuf::default()
    };
    let result = node.control(GEM_PUSHBUF, (&raw mut submit).cast());
    if result != 0 {
        log(format_args!(
            "NVENC PROBE submit refused: {} ({result})",
            errno_name(result)
        ));
        release(node, push_buffer, true);
        release(node, semaphore, true);
        return Outcome::Failed {
            step: Step::Submit,
            errno: result,
        };
    }
    log(format_args!(
        "NVENC PROBE submitted: {PUSH_WORDS} words: SET_OBJECT {ENCODER_CLASS:#06x}, \
         SET_APPLICATION_ID H264, SEMAPHORE_A..D release {PAYLOAD:#010x}"
    ));
    let outcome = wait(node, &semaphore, timeout_ns, log);
    // A push that never finished keeps its buffers: nouveau's fence still
    // holds them, and freeing them is the channel's wait over again.
    let free = !outcome.keeps_channel();
    release(node, push_buffer, free);
    release(node, semaphore, free);
    outcome
}

/// Reads the semaphore until it holds [`PAYLOAD`] or `timeout_ns` passes,
/// asking nouveau between reads whether its fence after the push signalled.
/// Each ask lets the host's Linux run, which takes the GPU's interrupts.
fn wait<N: Node>(node: &mut N, semaphore: &Buffer, timeout_ns: u64, log: Log<'_>) -> Outcome {
    let started = node.now_ns();
    let deadline = started.saturating_add(timeout_ns);
    let mut fence_done = false;
    loop {
        // SAFETY: the mapping holds a word and stays until released.
        let value = unsafe { semaphore.pointer.cast::<u32>().read_volatile() };
        let now = node.now_ns();
        if value == PAYLOAD {
            let micros = now.saturating_sub(started) / 1000;
            log(format_args!(
                "NVENC PROBE ALIVE: semaphore {value:#010x} after {micros} us; NVENC runs under \
                 GSP-RM on this GPU"
            ));
            return Outcome::Alive { micros };
        }
        if now >= deadline {
            let micros = now.saturating_sub(started) / 1000;
            log(format_args!(
                "NVENC PROBE TIMEOUT: semaphore {value:#010x} after {micros} us, channel fence {}",
                if fence_done {
                    "done: the engine took the methods but wrote nothing"
                } else {
                    "pending: the channel is left open"
                }
            ));
            return Outcome::Timeout { value, fence_done };
        }
        if !fence_done {
            let mut prep = CpuPrep {
                handle: semaphore.handle,
                flags: CPU_PREP_NOWAIT,
            };
            match node.control(GEM_CPU_PREP, (&raw mut prep).cast()) {
                0 => {
                    fence_done = true;
                    log(format_args!(
                        "NVENC PROBE fence done after {} us",
                        node.now_ns().saturating_sub(started) / 1000
                    ));
                }
                EBUSY => {}
                result => {
                    // Not a reason to stop reading: the semaphore decides.
                    fence_done = true;
                    log(format_args!(
                        "NVENC PROBE fence query refused: {} ({result})",
                        errno_name(result)
                    ));
                }
            }
        }
        node.pause();
    }
}

/// A mapped buffer of video memory for `channel`.
fn buffer<N: Node>(
    node: &mut N,
    channel: i32,
    log: Log<'_>,
    name: &str,
) -> Result<Buffer, Outcome> {
    let mut new = GemNew {
        info: GemInfo {
            domain: DOMAIN_VRAM | DOMAIN_MAPPABLE,
            size: BUFFER_BYTES as u64,
            ..GemInfo::default()
        },
        channel_hint: channel as u32,
        align: BUFFER_BYTES as u32,
    };
    let result = node.control(GEM_NEW, (&raw mut new).cast());
    if result != 0 {
        log(format_args!(
            "NVENC PROBE {name} buffer refused: {} ({result})",
            errno_name(result)
        ));
        return Err(Outcome::Failed {
            step: Step::Buffer,
            errno: result,
        });
    }
    let Some(pointer) = node.map(new.info.map_handle, BUFFER_BYTES) else {
        log(format_args!(
            "NVENC PROBE {name} buffer map refused at {:#x}",
            new.info.map_handle
        ));
        close(node, new.info.handle);
        return Err(Outcome::Failed {
            step: Step::Map,
            errno: 0,
        });
    };
    Ok(Buffer {
        handle: new.info.handle,
        address: new.info.offset,
        pointer,
    })
}

/// Unmaps `buffer`, and gives it back when `free`.
fn release<N: Node>(node: &mut N, buffer: Buffer, free: bool) {
    node.unmap(buffer.pointer, BUFFER_BYTES);
    if free {
        close(node, buffer.handle);
    }
}

fn close<N: Node>(node: &mut N, handle: u32) {
    let mut close = GemClose { handle, pad: 0 };
    node.control(GEM_CLOSE, (&raw mut close).cast());
}

#[cfg(test)]
mod tests;
