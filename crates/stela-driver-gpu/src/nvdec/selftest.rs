//! `--nvdec-selftest`: before macOS starts, each codec's clip (NVENC's own
//! boot-1 streams, 1920x1080, an IDR and 29 P frames) is parsed on the host
//! and decoded on NVDEC picture by picture; each decoded picture is read
//! back as 1920x1080 NV12 and hashed, and the MD5 must equal the clip's
//! golden checksum (FFmpeg's software decode, which NVIDIA's own NVDEC
//! decode matched frame for frame in nvdec-cap-1118). Each frame and each
//! clip is an `NVDEC SELFTEST` line; nothing stops the boot.
//!
//! **Readback** ([`Readback`]): the copy engine blits the picture's
//! displayed rectangle block-linear to pitch-linear NV12 in system memory
//! ([`copy`]), which the CPU then copies out cached; each frame's line says
//! `read_us N (ce_us C + cpu_us P)`. BAR1 is the fallback, the CPU copying
//! the raw planes out of video memory and de-tiling them
//! (`read_us N (bar1)`): for every frame when the copy engine could not be
//! opened, and from a frame on once a copy fails or times out. A copy that
//! never completes leaves its channel busy: the session then keeps its
//! channel, buffers and file ([`Report::ce_hung`]), as for a hung decoder.
//!
//! **A frame whose MD5 differs** is told apart once per clip: a frame the
//! copy engine read is read again over BAR1 (a BAR1 match means the blit,
//! not the decode, is wrong, and BAR1 reads the rest of the clip), then
//! under the other block-linear block heights and pitch-linear
//! ([`surface::Tiling::ALL`]), so a wrong guess of the surface layout shows
//! apart from a wrong decode.
use super::copy::{self, Copier};
use super::session::{Decoded, Error, Session};
use super::surface::{self, Tiling};
use super::{Codec, H264_CLIP, HEVC_CLIP, Picture, golden, md5};
use crate::nvenc::selftest::hex;
use crate::nvenc::session::Closed;
use crate::nvenc::{Log, Node, errno_name};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Each frame's bound.
pub const FRAME_BOUND_NS: u64 = 2_000_000_000;
/// Each copy's bound: a 1080p frame's blit takes about a millisecond.
pub const COPY_BOUND_NS: u64 = 500_000_000;

/// How decoded pictures come back to the CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Readback {
    /// The copy engine to system memory; BAR1 when it fails.
    #[default]
    CopyEngine,
    /// BAR1 alone: an earlier clip left a copy-engine channel busy.
    Bar1,
}

/// A clip the self-test decodes.
#[derive(Clone, Debug)]
pub struct Clip {
    pub codec: Codec,
    pub stream: &'static [u8],
    /// One MD5 per frame, in decoding order (the clips have no B-frames, so
    /// it is display order too).
    pub golden: Vec<[u8; 16]>,
}

/// The two clips, H.264 first.
pub fn clips() -> [Clip; 2] {
    [
        Clip {
            codec: Codec::H264,
            stream: H264_CLIP,
            golden: golden(Codec::H264),
        },
        Clip {
            codec: Codec::Hevc,
            stream: HEVC_CLIP,
            golden: golden(Codec::Hevc),
        },
    ]
}

/// How a clip's run ended.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub name: &'static str,
    pub passed: bool,
    /// Frames decoded, and those whose MD5 matched.
    pub frames: u32,
    pub matched: u32,
    pub expected: u32,
    /// Whether a frame timed out, which ends the GPU's other runs too.
    pub timed_out: bool,
    pub gpu_ns: u64,
    /// Every frame's readback: the copy engine's and the CPU's time, or
    /// BAR1's.
    pub read_ns: u64,
    /// Frames the copy engine read back, their time on the engine (from the
    /// methods to the semaphore) and the CPU's copy out of system memory.
    pub ce_frames: u32,
    pub ce_ns: u64,
    pub ce_most: u64,
    pub cpu_ns: u64,
    /// Frames read over BAR1.
    pub bar1_frames: u32,
    /// Whether a copy never completed with its job pending: the copy
    /// engine's channel, the session and its file stay open, and the GPU's
    /// next clips read over BAR1.
    pub ce_hung: bool,
}

/// What a session's error says in a line.
pub fn describe(error: &Error) -> String {
    match error {
        Error::NoDecoder => String::from("no decoder this module drives on this chipset"),
        Error::Config(why) => format!("refused: {why}"),
        Error::Refused { step, errno } => {
            let why = if *step == "channel" {
                format!("{}", super::PROBE.channel_refusal(*errno))
            } else {
                String::from("refused")
            };
            format!("{step}: {} ({errno}): {why}", errno_name(*errno))
        }
        Error::TooLarge { name, bytes } => {
            format!("nouveau made {name} {bytes:#x} bytes, past its addresses")
        }
        Error::Map { name } => format!("the CPU mapping of {name} was refused"),
        Error::Timeout { value, job_done } => format!(
            "TIMEOUT: the semaphore held {value:#010x}; the channel fence is {}",
            if *job_done {
                "done: the engine took the methods but wrote nothing"
            } else {
                "pending: the channel and its file are left open"
            }
        ),
        Error::Dead => String::from("the session is dead"),
    }
}

fn us(ns: u64) -> u64 {
    ns / 1000
}

/// A picture read back and hashed.
#[derive(Clone, Copy, Debug)]
struct Read {
    digest: [u8; 16],
    /// The copy engine's time, when it read the picture.
    ce_ns: Option<u64>,
    /// The CPU's: its copy out of system memory, or BAR1's read.
    cpu_ns: u64,
    blank: bool,
}

impl Read {
    fn total(&self) -> u64 {
        self.ce_ns.unwrap_or(0) + self.cpu_ns
    }
}

/// The NV12 MD5 of `picture`'s decoded surface under `tiling`, read out of
/// BAR1; `None` when the planes could not be read.
fn bar1<N: Node>(
    session: &mut Session<N>,
    picture: &Picture,
    decoded: &Decoded,
    tiling: Tiling,
) -> Option<Read> {
    let geometry = *session.geometry();
    let pitch = geometry.pitch;
    let luma_bytes = tiling.plane_bytes(pitch, geometry.coded_height);
    let chroma_bytes = tiling.plane_bytes(pitch, geometry.coded_height / 2);
    let started = session.now_ns();
    let (luma, chroma) = session.planes(decoded.slot, luma_bytes, chroma_bytes)?;
    let read = session.now_ns().saturating_sub(started);
    let blank = luma.iter().all(|&b| b == 0);
    let nv12 = surface::picture(&luma, &chroma, tiling, pitch, &picture.crop);
    Some(Read {
        digest: md5::md5(&nv12),
        ce_ns: None,
        cpu_ns: read,
        blank,
    })
}

/// The NV12 MD5 of `picture`'s decoded surface, blitted by `copier` to
/// system memory and copied out by the CPU.
fn blit<N: Node>(
    session: &mut Session<N>,
    copier: &mut Copier,
    picture: &Picture,
    decoded: &Decoded,
) -> Result<Read, copy::Error> {
    let geometry = *session.geometry();
    let (luma, chroma) = session.layout().surface(decoded.slot);
    let blits = copy::nv12(
        luma,
        chroma,
        &geometry,
        &picture.crop,
        copy::STAGING_ADDRESS,
    );
    let copied = copier.copy(session.node(), &blits)?;
    let bytes = copy::nv12_bytes(&picture.crop) as usize;
    let started = session.now_ns();
    let nv12 = copier
        .output(bytes)
        .ok_or(copy::Error::Config(
            "the picture is larger than the staging buffer",
        ))?
        .to_vec();
    let cpu_ns = session.now_ns().saturating_sub(started);
    let luma_bytes = (picture.crop.width * picture.crop.height) as usize;
    Ok(Read {
        digest: md5::md5(&nv12),
        ce_ns: Some(copied.ce_ns),
        cpu_ns,
        blank: nv12[..luma_bytes].iter().all(|&b| b == 0),
    })
}

/// Decodes `clip` on `node`, a fresh render node file of `chipset`'s GPU,
/// reading each picture back by the copy engine, and gives the node back
/// with whether its file must stay open.
pub fn run<N: Node>(node: N, chipset: u16, clip: &Clip, log: Log<'_>) -> (Report, Closed<N>) {
    run_with(node, chipset, clip, Readback::CopyEngine, log)
}

/// [`run`], reading pictures back as `readback` says.
pub fn run_with<N: Node>(
    node: N,
    chipset: u16,
    clip: &Clip,
    readback: Readback,
    log: Log<'_>,
) -> (Report, Closed<N>) {
    let name = clip.codec.name();
    let mut report = Report {
        name,
        expected: clip.golden.len() as u32,
        ..Report::default()
    };
    let refuse = |node: N| Closed {
        node,
        keeps_file: false,
    };
    let pictures = match Picture::all(clip.codec, clip.stream) {
        Ok(pictures) => pictures,
        Err(error) => {
            log(format_args!(
                "NVDEC SELFTEST {name} FAIL at parse: {} ({})",
                error.describe(),
                error.kind()
            ));
            return (report, refuse(node));
        }
    };
    if pictures.len() != clip.golden.len() {
        log(format_args!(
            "NVDEC SELFTEST {name} FAIL at parse: {} pictures for {} checksums",
            pictures.len(),
            clip.golden.len()
        ));
        return (report, refuse(node));
    }
    let Some(first) = pictures.first() else {
        log(format_args!(
            "NVDEC SELFTEST {name} FAIL at parse: no picture"
        ));
        return (report, refuse(node));
    };
    let geometry = match first.geometry() {
        Ok(geometry) => geometry,
        Err(why) => {
            log(format_args!("NVDEC SELFTEST {name} FAIL at parse: {why}"));
            return (report, refuse(node));
        }
    };
    let slots = pictures.iter().map(|p| p.slots).max().unwrap_or(1);
    let mut session = match Session::open(
        node,
        chipset,
        clip.codec,
        geometry,
        usize::from(slots),
        FRAME_BOUND_NS,
    ) {
        Ok(session) => session,
        Err((error, node)) => {
            log(format_args!(
                "NVDEC SELFTEST {name} FAIL at open: {}",
                describe(&error)
            ));
            return (report, refuse(node));
        }
    };
    log(format_args!(
        "NVDEC SELFTEST {name} open: {} pictures, coded {}x{} shown {}x{}, pitch {}, {} surface \
         slots, {} buffers at NVIDIA's captured addresses, {} stream bytes",
        pictures.len(),
        geometry.coded_width,
        geometry.coded_height,
        first.crop.width,
        first.crop.height,
        geometry.pitch,
        slots,
        session.layout().plans.len(),
        clip.stream.len()
    ));
    let staging = pictures
        .iter()
        .map(|p| copy::nv12_bytes(&p.crop))
        .max()
        .unwrap_or(0);
    let mut copier = match readback {
        Readback::Bar1 => {
            log(format_args!(
                "NVDEC SELFTEST {name} readback: BAR1, since an earlier clip left a copy-engine \
                 channel busy"
            ));
            None
        }
        Readback::CopyEngine => match Copier::open(session.node(), staging, COPY_BOUND_NS) {
            Ok(copier) => {
                log(format_args!(
                    "NVDEC SELFTEST {name} readback: copy engine, channel {} class {:#06x}, \
                     GOB_2 to pitch-linear NV12 {}x{} in {} bytes of system memory at {:#x}, \
                     bound {} ms",
                    copier.channel(),
                    copy::COPY_CLASS,
                    first.crop.width,
                    first.crop.height,
                    staging,
                    copy::STAGING_ADDRESS,
                    COPY_BOUND_NS / 1_000_000
                ));
                Some(copier)
            }
            Err(error) => {
                log(format_args!(
                    "NVDEC SELFTEST {name} readback: the copy engine is unavailable ({error}); \
                     reading over BAR1"
                ));
                None
            }
        },
    };
    let mut blitting = copier.is_some();
    let mut failure: Option<String> = None;
    let mut probed = false;
    let mut cross_checked = false;
    let mut gpu_most = 0u64;
    for (n, picture) in pictures.iter().enumerate() {
        let decoded = match session.decode(picture) {
            Ok(decoded) => decoded,
            Err(error) => {
                report.timed_out = matches!(error, Error::Timeout { .. });
                log(format_args!(
                    "NVDEC SELFTEST {name} frame {n}: {} slot {}: {}",
                    picture.kind,
                    picture.slot,
                    describe(&error)
                ));
                failure = Some(format!("frame {n}: {}", describe(&error)));
                break;
            }
        };
        report.frames += 1;
        report.gpu_ns += decoded.timing.gpu_ns;
        gpu_most = gpu_most.max(decoded.timing.gpu_ns);
        let status = decoded.status;
        let golden = clip.golden[n];
        let mut read = None;
        if let (true, Some(engine)) = (blitting, copier.as_mut()) {
            match blit(&mut session, engine, picture, &decoded) {
                Ok(blitted) => read = Some(blitted),
                Err(error) => {
                    blitting = false;
                    if error.pending() {
                        report.ce_hung = true;
                        session.hold();
                    }
                    log(format_args!(
                        "NVDEC SELFTEST {name} frame {n}: copy engine {error}; BAR1 reads this \
                         frame and the rest"
                    ));
                }
            }
        }
        // A blit that differs is told from a decode that differs by BAR1.
        if let Some(blitted) = read
            && blitted.digest != golden
            && !cross_checked
        {
            cross_checked = true;
            if let Some(direct) = bar1(&mut session, picture, &decoded, Tiling::NVDEC) {
                let verdict = if direct.digest == golden {
                    blitting = false;
                    read = Some(direct);
                    "MATCHES: the copy engine's blit is wrong, so BAR1 reads this frame and the rest"
                } else if direct.digest == blitted.digest {
                    "agrees with the copy engine: the decode itself differs"
                } else {
                    "differs from both: the blit and the decode are both suspect"
                };
                log(format_args!(
                    "NVDEC SELFTEST {name} frame {n} readback check: copy engine md5 {} in {} us, \
                     BAR1 md5 {} in {} us {verdict}",
                    hex(&blitted.digest),
                    us(blitted.total()),
                    hex(&direct.digest),
                    us(direct.total())
                ));
            }
        }
        let read = match read {
            Some(read) => Some(read),
            None => bar1(&mut session, picture, &decoded, Tiling::NVDEC),
        };
        let Some(read) = read else {
            failure = Some(format!("frame {n}: its surface could not be read"));
            break;
        };
        report.read_ns += read.total();
        let split = match read.ce_ns {
            Some(ce_ns) => {
                report.ce_frames += 1;
                report.ce_ns += ce_ns;
                report.ce_most = report.ce_most.max(ce_ns);
                report.cpu_ns += read.cpu_ns;
                format!("ce_us {} + cpu_us {}", us(ce_ns), us(read.cpu_ns))
            }
            None => {
                report.bar1_frames += 1;
                String::from("bar1")
            }
        };
        let matched = read.digest == golden;
        report.matched += u32::from(matched);
        log(format_args!(
            "NVDEC SELFTEST {name} frame {n}: {} slot {} poc {}, {} bytes, md5 {} {}, status \
             mbs {} in_error {} error_status {:#x} slice_error {:#x} cycles {}, write_us {}, \
             gpu_us {}, read_us {} ({split}){}",
            picture.kind,
            picture.slot,
            picture.poc,
            picture.bitstream().len(),
            hex(&read.digest),
            if matched { "MATCH" } else { "MISMATCH" },
            status.mbs_correctly_decoded,
            status.mbs_in_error,
            status.error_status,
            status.slice_header_error_code,
            status.cycle_count,
            us(decoded.timing.write_ns),
            us(decoded.timing.gpu_ns),
            us(read.total()),
            if read.blank {
                ", luma all zero: the engine wrote nothing"
            } else {
                ""
            }
        ));
        if !matched && !probed {
            probed = true;
            let mut found = Vec::new();
            let mut tried = String::new();
            for tiling in Tiling::ALL.iter().skip(1) {
                if let Some(other) = bar1(&mut session, picture, &decoded, *tiling) {
                    tried.push_str(&format!(" {} {}", tiling.name(), hex(&other.digest)));
                    if other.digest == golden {
                        found.push(tiling.name());
                    }
                }
            }
            log(format_args!(
                "NVDEC SELFTEST {name} frame {n} layout probe: want {}, tried{}; {}",
                hex(&golden),
                tried,
                if found.is_empty() {
                    String::from("no layout matches: the decode itself differs")
                } else {
                    format!(
                        "MATCHES as {}: the surface layout is not GOB_2",
                        found.join(",")
                    )
                }
            ));
        }
    }
    let frames = u64::from(report.frames.max(1));
    report.passed = failure.is_none() && report.matched == report.expected;
    let verdict = if report.passed { "PASS" } else { "FAIL" };
    let ce_frames = u64::from(report.ce_frames.max(1));
    log(format_args!(
        "NVDEC SELFTEST {name} {verdict}: {}/{} frames match the software decode, {} decoded, \
         gpu_us avg {} max {}, read_us avg {} (copy engine {} frames: ce_us avg {} max {}, cpu_us \
         avg {}; BAR1 {} frames){}",
        report.matched,
        report.expected,
        report.frames,
        us(report.gpu_ns) / frames,
        us(gpu_most),
        us(report.read_ns) / frames,
        report.ce_frames,
        us(report.ce_ns) / ce_frames,
        us(report.ce_most),
        us(report.cpu_ns) / ce_frames,
        report.bar1_frames,
        failure
            .as_deref()
            .map(|why| format!("; stopped at {why}"))
            .unwrap_or_default()
    ));
    if let Some(copier) = copier.take() {
        if session.dead() == Some(true) {
            // The decoder's channel keeps the file open anyway.
            copier.leave(session.node());
        } else if copier.close(session.node()) {
            log(format_args!(
                "NVDEC SELFTEST {name}: the copy engine's channel is still busy, so it, the \
                 session and their file stay open"
            ));
        }
    }
    let closed = session.close();
    if closed.keeps_file && !report.ce_hung {
        log(format_args!(
            "NVDEC SELFTEST {name}: the channel is still busy, so it and its file stay open"
        ));
    }
    (report, closed)
}

#[cfg(test)]
mod tests;
