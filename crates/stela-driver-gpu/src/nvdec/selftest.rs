//! `--nvdec-selftest`: before macOS starts, each codec's clip (NVENC's own
//! boot-1 streams, 1920x1080, an IDR and 29 P frames) is parsed on the host
//! and decoded on NVDEC picture by picture; each decoded picture is read
//! back, de-tiled, cropped to 1920x1080 NV12 and hashed, and the MD5 must
//! equal the clip's golden checksum (FFmpeg's software decode, which
//! NVIDIA's own NVDEC decode matched frame for frame in nvdec-cap-1118).
//! Each frame and each clip is an `NVDEC SELFTEST` line; nothing stops the
//! boot.
//!
//! A frame whose MD5 differs is read again under the other block-linear
//! block heights and pitch-linear ([`surface::Tiling::ALL`]), once per clip,
//! so a wrong guess of the surface layout shows apart from a wrong decode.
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
    pub read_ns: u64,
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

/// The NV12 MD5 of `picture`'s decoded surface under `tiling`, and how long
/// the read out of BAR1 took; `None` when the planes could not be read.
fn checksum<N: Node>(
    session: &mut Session<N>,
    picture: &Picture,
    decoded: &Decoded,
    tiling: Tiling,
) -> Option<([u8; 16], u64, bool)> {
    let geometry = *session.geometry();
    let pitch = geometry.pitch;
    let luma_bytes = tiling.plane_bytes(pitch, geometry.coded_height);
    let chroma_bytes = tiling.plane_bytes(pitch, geometry.coded_height / 2);
    let started = session.now_ns();
    let (luma, chroma) = session.planes(decoded.slot, luma_bytes, chroma_bytes)?;
    let read = session.now_ns().saturating_sub(started);
    let blank = luma.iter().all(|&b| b == 0);
    let nv12 = surface::picture(&luma, &chroma, tiling, pitch, &picture.crop);
    Some((md5::md5(&nv12), read, blank))
}

/// Decodes `clip` on `node`, a fresh render node file of `chipset`'s GPU,
/// saying what happens to `log`, and gives the node back with whether its
/// file must stay open.
pub fn run<N: Node>(node: N, chipset: u16, clip: &Clip, log: Log<'_>) -> (Report, Closed<N>) {
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
    let mut failure: Option<String> = None;
    let mut probed = false;
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
        let (digest, read_ns, blank) =
            match checksum(&mut session, picture, &decoded, Tiling::NVDEC) {
                Some(result) => result,
                None => {
                    failure = Some(format!("frame {n}: its surface could not be read"));
                    break;
                }
            };
        report.read_ns += read_ns;
        let matched = digest == clip.golden[n];
        report.matched += u32::from(matched);
        log(format_args!(
            "NVDEC SELFTEST {name} frame {n}: {} slot {} poc {}, {} bytes, md5 {} {}, status \
             mbs {} in_error {} error_status {:#x} slice_error {:#x} cycles {}, write_us {}, \
             gpu_us {}, read_us {}{}",
            picture.kind,
            picture.slot,
            picture.poc,
            picture.bitstream().len(),
            hex(&digest),
            if matched { "MATCH" } else { "MISMATCH" },
            status.mbs_correctly_decoded,
            status.mbs_in_error,
            status.error_status,
            status.slice_header_error_code,
            status.cycle_count,
            us(decoded.timing.write_ns),
            us(decoded.timing.gpu_ns),
            us(read_ns),
            if blank {
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
                if let Some((other, _, _)) = checksum(&mut session, picture, &decoded, *tiling) {
                    tried.push_str(&format!(" {} {}", tiling.name(), hex(&other)));
                    if other == clip.golden[n] {
                        found.push(tiling.name());
                    }
                }
            }
            log(format_args!(
                "NVDEC SELFTEST {name} frame {n} layout probe: want {}, tried{}; {}",
                hex(&clip.golden[n]),
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
    log(format_args!(
        "NVDEC SELFTEST {name} {verdict}: {}/{} frames match the software decode, {} decoded, \
         gpu_us avg {} max {}, read_us avg {}{}",
        report.matched,
        report.expected,
        report.frames,
        us(report.gpu_ns) / frames,
        us(gpu_most),
        us(report.read_ns) / frames,
        failure
            .as_deref()
            .map(|why| format!("; stopped at {why}"))
            .unwrap_or_default()
    ));
    let closed = session.close();
    if closed.keeps_file {
        log(format_args!(
            "NVDEC SELFTEST {name}: the channel is still busy, so it and its file stay open"
        ));
    }
    (report, closed)
}

#[cfg(test)]
mod tests;
