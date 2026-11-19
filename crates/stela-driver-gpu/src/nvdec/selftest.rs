//! `--nvdec-selftest`: before macOS starts, each codec's clip (NVENC's own
//! boot-1 streams, 1920x1080, an IDR and 29 P frames) is fed to NVDEC as a
//! guest's VideoToolbox session feeds the gate: its parameter sets as an
//! `avcC`/`hvcC` record ([`Stream::open`]), then one length-prefixed sample
//! per access unit ([`Stream::decode`]), each picture decoded on NVDEC and
//! each picture it shows ([`Picture::outputs`], then [`Stream::flush`]'s)
//! read back as 1920x1080 NV12 after its decode and before the next, as the
//! stream's caller contract says. Each shown picture is hashed, and the MD5
//! must equal the clip's golden checksum (FFmpeg's software decode, which
//! NVIDIA's own NVDEC decode matched frame for frame in nvdec-cap-1118).
//! Each frame and each clip is an `NVDEC SELFTEST` line; nothing stops the
//! boot.
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
use super::dpb::Output;
use super::layout::Layout;
use super::session::{Error, Session};
use super::stream::{self, Stream};
use super::surface::{self, Crop, Tiling};
use super::{Codec, H264_CLIP, HEVC_CLIP, golden, md5};
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
    /// The surfaces NVIDIA's driver's address map had room for
    /// (`ulNumDecodeSurfaces`, FFmpeg's): the session's buffers sit at
    /// NVIDIA's addresses for as many.
    pub pool: usize,
}

/// The two clips, H.264 first.
pub fn clips() -> [Clip; 2] {
    [
        Clip {
            codec: Codec::H264,
            stream: H264_CLIP,
            golden: golden(Codec::H264),
            pool: 16,
        },
        Clip {
            codec: Codec::Hevc,
            stream: HEVC_CLIP,
            golden: golden(Codec::Hevc),
            pool: 19,
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

/// The NV12 MD5 of the picture `crop` of surface `surface`, under
/// `tiling`, read out of BAR1; `None` when the planes could not be read.
fn bar1<N: Node>(
    session: &mut Session<N>,
    surface: u8,
    crop: &Crop,
    tiling: Tiling,
) -> Option<Read> {
    let geometry = *session.geometry();
    let pitch = geometry.pitch;
    let luma_bytes = tiling.plane_bytes(pitch, geometry.coded_height);
    let chroma_bytes = tiling.plane_bytes(pitch, geometry.coded_height / 2);
    let started = session.now_ns();
    let (luma, chroma) = session.planes(surface, luma_bytes, chroma_bytes)?;
    let read = session.now_ns().saturating_sub(started);
    let blank = luma.iter().all(|&b| b == 0);
    let nv12 = surface::picture(&luma, &chroma, tiling, pitch, crop);
    Some(Read {
        digest: md5::md5(&nv12),
        ce_ns: None,
        cpu_ns: read,
        blank,
    })
}

/// The NV12 MD5 of the picture `crop` of surface `surface`, blitted by
/// `copier` to system memory and copied out by the CPU.
fn blit<N: Node>(
    session: &mut Session<N>,
    copier: &mut Copier,
    surface: u8,
    crop: &Crop,
) -> Result<Read, copy::Error> {
    let geometry = *session.geometry();
    let (luma, chroma) = session.layout().surface(surface);
    let blits = copy::nv12(luma, chroma, &geometry, crop, copier.staging_address());
    let copied = copier.copy(session.node(), &blits)?;
    let bytes = copy::nv12_bytes(crop) as usize;
    let started = session.now_ns();
    let nv12 = copier
        .output(bytes)
        .ok_or(copy::Error::Config(
            "the picture is larger than the staging buffer",
        ))?
        .to_vec();
    let cpu_ns = session.now_ns().saturating_sub(started);
    let luma_bytes = (crop.width * crop.height) as usize;
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

/// What the self-test knows of a decoded picture when it is shown.
#[derive(Clone, Copy, Debug)]
struct Shown {
    kind: char,
    slot: u8,
    bytes: usize,
    status: super::session::Status,
    write_ns: u64,
    gpu_ns: u64,
}

/// The run's readback and its checks, across the pictures shown.
struct Reader {
    name: &'static str,
    copier: Option<Copier>,
    blitting: bool,
    probed: bool,
    cross_checked: bool,
    /// Pictures shown so far: the index of the next golden checksum.
    shown: usize,
}

impl Reader {
    /// Reads, hashes, checks and logs the shown picture `output` (with
    /// `crop`); an error ends the run.
    #[allow(clippy::too_many_arguments)]
    fn show<N: Node>(
        &mut self,
        session: &mut Session<N>,
        clip: &Clip,
        output: &Output,
        crop: &Crop,
        what: &Shown,
        report: &mut Report,
        log: Log<'_>,
    ) -> Result<(), String> {
        let name = self.name;
        let n = self.shown;
        self.shown += 1;
        let Some(&golden) = clip.golden.get(n) else {
            return Err(format!(
                "frame {n}: shown past the clip's {} checksums",
                clip.golden.len()
            ));
        };
        let surface = output.surface;
        let mut read = None;
        if let (true, Some(engine)) = (self.blitting, self.copier.as_mut()) {
            match blit(session, engine, surface, crop) {
                Ok(blitted) => read = Some(blitted),
                Err(error) => {
                    self.blitting = false;
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
            && !self.cross_checked
        {
            self.cross_checked = true;
            if let Some(direct) = bar1(session, surface, crop, Tiling::NVDEC) {
                let verdict = if direct.digest == golden {
                    self.blitting = false;
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
            None => bar1(session, surface, crop, Tiling::NVDEC),
        };
        let Some(read) = read else {
            return Err(format!("frame {n}: its surface could not be read"));
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
        let status = what.status;
        log(format_args!(
            "NVDEC SELFTEST {name} frame {n}: {} slot {} poc {}, {} bytes, md5 {} {}, status \
             mbs {} in_error {} error_status {:#x} slice_error {:#x} cycles {}, write_us {}, \
             gpu_us {}, read_us {} ({split}){}",
            what.kind,
            what.slot,
            output.poc,
            what.bytes,
            hex(&read.digest),
            if matched { "MATCH" } else { "MISMATCH" },
            status.mbs_correctly_decoded,
            status.mbs_in_error,
            status.error_status,
            status.slice_header_error_code,
            status.cycle_count,
            us(what.write_ns),
            us(what.gpu_ns),
            us(read.total()),
            if read.blank {
                ", luma all zero: the engine wrote nothing"
            } else {
                ""
            }
        ));
        if !matched && !self.probed {
            self.probed = true;
            let mut found = Vec::new();
            let mut tried = String::new();
            for tiling in Tiling::ALL.iter().skip(1) {
                if let Some(other) = bar1(session, surface, crop, *tiling) {
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
        Ok(())
    }
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
    // The clip as a VideoToolbox client hands it over: a record, then one
    // sample per access unit.
    let opened = stream::samples(clip.codec, clip.stream).and_then(|(record, samples)| {
        Stream::open(clip.codec, &record).map(|stream| (stream, samples))
    });
    let (mut stream, samples) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            log(format_args!(
                "NVDEC SELFTEST {name} FAIL at parse: {} ({})",
                error.describe(),
                error.kind()
            ));
            return (report, refuse(node));
        }
    };
    let Some(sequence) = stream.sequence() else {
        log(format_args!(
            "NVDEC SELFTEST {name} FAIL at parse: no sequence"
        ));
        return (report, refuse(node));
    };
    let geometry = match crate::nvenc::layout::Geometry::new(
        sequence.coded.0.next_multiple_of(16),
        sequence.coded.1.next_multiple_of(16),
    ) {
        Ok(geometry) => geometry,
        Err(why) => {
            log(format_args!("NVDEC SELFTEST {name} FAIL at parse: {why}"));
            return (report, refuse(node));
        }
    };
    // Every surface the stream's slots may be bound to, on NVIDIA's address
    // map for the clip's pool.
    let slots = usize::from(sequence.surfaces.max(1));
    let layout = match Layout::with(
        clip.codec,
        &geometry,
        sequence.bit_depth,
        slots,
        clip.pool.max(slots),
    ) {
        Ok(layout) => layout,
        Err(why) => {
            log(format_args!("NVDEC SELFTEST {name} FAIL at open: {why}"));
            return (report, refuse(node));
        }
    };
    let Some(ring) = copy::ring_after(layout.end()) else {
        log(format_args!(
            "NVDEC SELFTEST {name} FAIL at open: no room for the copy engine's buffers"
        ));
        return (report, refuse(node));
    };
    let mut session = match Session::open_layout(node, chipset, geometry, layout, FRAME_BOUND_NS) {
        Ok(session) => session,
        Err((error, node)) => {
            log(format_args!(
                "NVDEC SELFTEST {name} FAIL at open: {}",
                describe(&error)
            ));
            return (report, refuse(node));
        }
    };
    let mut crop = Crop {
        left: 0,
        top: 0,
        width: sequence.display.0,
        height: sequence.display.1,
    };
    log(format_args!(
        "NVDEC SELFTEST {name} open: {} samples, coded {}x{} shown {}x{}, pitch {}, {} surface \
         slots, {} buffers at NVIDIA's captured addresses, {} stream bytes",
        samples.len(),
        geometry.coded_width,
        geometry.coded_height,
        crop.width,
        crop.height,
        geometry.pitch,
        slots,
        session.layout().plans.len(),
        clip.stream.len()
    ));
    let staging = copy::nv12_bytes(&crop);
    let copier = match readback {
        Readback::Bar1 => {
            log(format_args!(
                "NVDEC SELFTEST {name} readback: BAR1, since an earlier clip left a copy-engine \
                 channel busy"
            ));
            None
        }
        Readback::CopyEngine => match Copier::open_at(session.node(), ring, staging, COPY_BOUND_NS)
        {
            Ok(copier) => {
                log(format_args!(
                    "NVDEC SELFTEST {name} readback: copy engine, channel {} class {:#06x}, \
                     GOB_2 to pitch-linear NV12 {}x{} in {} bytes of system memory at {:#x}, \
                     bound {} ms",
                    copier.channel(),
                    copy::COPY_CLASS,
                    crop.width,
                    crop.height,
                    staging,
                    copier.staging_address(),
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
    let mut reader = Reader {
        name,
        blitting: copier.is_some(),
        copier,
        probed: false,
        cross_checked: false,
        shown: 0,
    };
    let mut failure: Option<String> = None;
    let mut gpu_most = 0u64;
    // Each decode's line facts, by decode index, until it is shown.
    let mut decoded: Vec<Option<Shown>> = Vec::new();
    for sample in &samples {
        let picture = match stream.decode(sample) {
            Ok(Some(picture)) => picture,
            Ok(None) => continue,
            Err(error) => {
                let n = report.frames;
                failure = Some(format!(
                    "frame {n}: parse: {} ({})",
                    error.describe(),
                    error.kind()
                ));
                break;
            }
        };
        crop = picture.crop;
        let n = picture.decoded;
        let done = match session.decode(&picture) {
            Ok(done) => done,
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
        report.gpu_ns += done.timing.gpu_ns;
        gpu_most = gpu_most.max(done.timing.gpu_ns);
        let index = n as usize;
        if decoded.len() <= index {
            decoded.resize(index + 1, None);
        }
        decoded[index] = Some(Shown {
            kind: picture.kind,
            slot: picture.slot,
            bytes: picture.bitstream().len(),
            status: done.status,
            write_ns: done.timing.write_ns,
            gpu_ns: done.timing.gpu_ns,
        });
        // The caller's contract: every output copied out now, before the
        // next picture's submit.
        if let Err(why) = show_all(
            &mut reader,
            &mut session,
            clip,
            &picture.outputs,
            &crop,
            &decoded,
            &mut report,
            log,
        ) {
            failure = Some(why);
            break;
        }
    }
    if failure.is_none() {
        let rest = stream.flush();
        if let Err(why) = show_all(
            &mut reader,
            &mut session,
            clip,
            &rest,
            &crop,
            &decoded,
            &mut report,
            log,
        ) {
            failure = Some(why);
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
    if let Some(copier) = reader.copier.take() {
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

/// Shows each of `outputs` in order through `reader`.
#[allow(clippy::too_many_arguments)]
fn show_all<N: Node>(
    reader: &mut Reader,
    session: &mut Session<N>,
    clip: &Clip,
    outputs: &[Output],
    crop: &Crop,
    decoded: &[Option<Shown>],
    report: &mut Report,
    log: Log<'_>,
) -> Result<(), String> {
    for output in outputs {
        let Some(Some(what)) = decoded.get(output.decoded as usize) else {
            return Err(format!(
                "frame {}: shows decode {}, never decoded",
                reader.shown, output.decoded
            ));
        };
        reader.show(session, clip, output, crop, what, report, log)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
