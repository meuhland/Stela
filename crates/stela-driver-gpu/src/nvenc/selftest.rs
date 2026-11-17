//! `--nvenc-selftest`: before macOS starts, each codec encodes nvenc-test's
//! moving pattern at 1920x1080, an IDR and 29 P frames, first at a fixed QP
//! and then at CBR, and every frame is checked: its start code and NAL
//! types (parameter sets and an IDR slice, then non-IDR slices), a size in
//! a sane range, and the engine's reconstruction against the input. Each
//! frame and each run is an `NVENC SELFTEST` line; nothing stops the boot.
use super::Log;
use super::bits::nal_types;
use super::layout::Geometry;
use super::session::{Closed, Error, Frame, Options, Session};
use super::surface::{self, Order, Rgb};
use super::{Codec, Config, Node, Rate, errno_name};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};
use stela_crypto::Sha256;
use stela_crypto::digest::Digest;

/// Frames per run: an IDR and 29 P frames.
pub const FRAMES: u32 = 30;
/// The fixed QP of each codec's first run.
pub const QP: u8 = 25;
/// Each frame's bound, as nvenc-test's default.
pub const FRAME_BOUND_NS: u64 = 2_000_000_000;
/// The least luma PSNR of a reconstruction against its input, in
/// hundredths of a dB. nvenc-test saw 51.8 dB at QP 25 and 58 dB at CBR.
pub const RECON_FLOOR_CENTI_DB: u32 = 3000;
/// The least bytes of an IDR's slices and of a P frame's.
const IDR_LEAST: usize = 1024;
const P_LEAST: usize = 8;

/// The four runs, in order.
pub const RUNS: [(Codec, Rate); 4] = [
    (Codec::H264, Rate::ConstQp(QP)),
    (
        Codec::H264,
        Rate::Cbr {
            bitrate: Rate::CAPTURED_BITRATE,
        },
    ),
    (Codec::Hevc, Rate::ConstQp(QP)),
    (
        Codec::Hevc,
        Rate::Cbr {
            bitrate: Rate::CAPTURED_BITRATE,
        },
    ),
];

/// A run's name in the lines: `h264-qp25`, `hevc-cbr`.
pub fn run_name(codec: Codec, rate: Rate) -> String {
    match rate {
        Rate::ConstQp(qp) => format!("{}-qp{qp}", codec.name()),
        Rate::Cbr { .. } => format!("{}-cbr", codec.name()),
    }
}

/// How a run ended.
#[derive(Clone, Debug)]
pub struct Report {
    pub name: String,
    pub passed: bool,
    /// Frames encoded and checked.
    pub frames: u32,
    /// The stream: every checked frame, Annex B.
    pub stream: Vec<u8>,
    /// Where each frame ends in [`Report::stream`].
    pub frame_ends: Vec<usize>,
    pub sha256: [u8; 32],
    /// Whether a frame timed out, which ends the GPU's other runs too.
    pub timed_out: bool,
}

/// A digest in hex.
pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The SHA-256 of `data`, in hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Microseconds of nanoseconds.
fn us(ns: u64) -> u64 {
    ns / 1000
}

fn dot_db(centi: u32) -> String {
    format!("{}.{:02}", centi / 100, centi % 100)
}

fn joined(types: &[u8]) -> String {
    let mut text = String::new();
    for (index, kind) in types.iter().enumerate() {
        if index != 0 {
            text.push(',');
        }
        text.push_str(&format!("{kind}"));
    }
    text
}

/// What a session's error says in a line.
pub fn describe(error: &Error) -> String {
    match error {
        Error::NoEncoder => String::from("no encoder this module drives on this chipset"),
        Error::Config(why) => format!("configuration refused: {why}"),
        Error::Refused { step, errno } => {
            let why = if *step == "channel" {
                super::channel_refusal(*errno)
            } else {
                "refused"
            };
            format!("{step}: {} ({errno}): {why}", errno_name(*errno))
        }
        Error::TooLarge { name, bytes } => {
            format!("nouveau made {name} {bytes:#x} bytes, past its addresses")
        }
        Error::Map { name } => format!("the CPU mapping of {name} was refused"),
        Error::Input(why) => format!("input refused: {why}"),
        Error::Timeout { value, job_done } => format!(
            "TIMEOUT: the semaphore held {value:#010x}; the channel fence is {}",
            if *job_done {
                "done: the engine took the methods but wrote nothing"
            } else {
                "pending: the channel and its file are left open"
            }
        ),
        Error::Engine { why, status } => format!(
            "engine output refused: {why} (picture {}, {} bits, type {}, error_status {} \
             ucode_error_status {:#x})",
            status.picture_index,
            status.total_bits,
            status.picture_type,
            status.error_status,
            status.ucode_error_status
        ),
        Error::Dead => String::from("the session is dead"),
    }
}

/// The NAL types a frame must hold: the parameter sets and an IDR slice
/// first, a non-IDR slice after.
fn types_ok(codec: Codec, idr: bool, types: &[u8]) -> bool {
    match (codec, idr) {
        (Codec::H264, true) => types == [7, 8, 5],
        (Codec::H264, false) => types == [1],
        (Codec::Hevc, true) => matches!(types, [32, 33, 34, 19 | 20]),
        (Codec::Hevc, false) => matches!(types, [0 | 1]),
    }
}

/// Runs one of [`RUNS`] on `node`, a fresh render node file of `chipset`'s
/// GPU, saying what happens to `log`, and gives the node back with whether
/// its file must stay open.
pub fn run<N: Node>(
    node: N,
    chipset: u16,
    codec: Codec,
    rate: Rate,
    log: Log<'_>,
) -> (Report, Closed<N>) {
    let name = run_name(codec, rate);
    let mut report = Report {
        name: name.clone(),
        passed: false,
        frames: 0,
        stream: Vec::new(),
        frame_ends: Vec::new(),
        sha256: [0; 32],
        timed_out: false,
    };
    let config = Config {
        map_recon: true,
        ..Config::hd(codec, rate)
    };
    let mut session = match Session::open(node, chipset, config, FRAME_BOUND_NS) {
        Ok(session) => session,
        Err((error, node)) => {
            log(format_args!(
                "NVENC SELFTEST {name} FAIL at open: {}",
                describe(&error)
            ));
            return (
                report,
                Closed {
                    node,
                    keeps_file: false,
                },
            );
        }
    };
    let geometry = *session.geometry();
    let layout = session.layout();
    log(format_args!(
        "NVENC SELFTEST {name} open: {}x{} coded {}x{} pitch {}, {} buffers at {} addresses, \
         control {:#010x}, {} bytes of parameter sets",
        geometry.width,
        geometry.height,
        geometry.coded_width,
        geometry.coded_height,
        geometry.pitch,
        layout.plans.len(),
        if layout.captured {
            "NVIDIA's captured"
        } else {
            "scaled"
        },
        codec.control(rate),
        session.headers().len()
    ));
    let mut failure = None;
    let (mut gpu_total, mut gpu_most, mut all_total, mut accepted) = (0u64, 0u64, 0u64, 0u32);
    let mut idr_bytes = 0;
    for n in 0..FRAMES {
        let input = surface::pattern(&geometry, n);
        let picture = surface::pattern_picture(&geometry, &input);
        let encoded = match session.encode(Frame::Nv12(picture), Options::default()) {
            Ok(encoded) => encoded,
            Err(error) => {
                report.timed_out = matches!(error, Error::Timeout { .. });
                failure = Some(format!("frame {n}: {}", describe(&error)));
                break;
            }
        };
        let types = nal_types(&encoded.bytes, codec == Codec::Hevc);
        let slices = encoded.bytes.len()
            - if encoded.idr {
                session.headers().len()
            } else {
                0
            };
        let recon = session
            .recon(n)
            .map(|recon| surface::psnr_luma_centi_db(&input, &recon, &geometry));
        let status = encoded.status;
        log(format_args!(
            "NVENC SELFTEST {name} frame {n}: {} {} bytes, NAL {}, pic_type {}, avgQP {}, \
             intra/inter {}/{}, error_status {} ucode_error_status {:#x}, store_us {}, gpu_us {}, \
             total_us {}, recon_y {}",
            if encoded.idr { "IDR" } else { "P" },
            encoded.bytes.len(),
            joined(&types),
            status.picture_type,
            status.average_qp,
            status.intra,
            status.inter,
            status.error_status,
            status.ucode_error_status,
            us(encoded.timing.store_ns),
            us(encoded.timing.wait_ns),
            us(encoded.timing.total_ns),
            recon.map_or_else(|| String::from("unread"), |db| format!("{} dB", dot_db(db)))
        ));
        report.stream.extend_from_slice(&encoded.bytes);
        report.frame_ends.push(report.stream.len());
        report.frames += 1;
        gpu_total += encoded.timing.wait_ns;
        gpu_most = gpu_most.max(encoded.timing.wait_ns);
        all_total += encoded.timing.total_ns;
        accepted += u32::from(status.accepted_error());
        if encoded.idr {
            idr_bytes = encoded.bytes.len();
        }
        let why = if encoded.idr != (n == 0) {
            Some("the picture type is not the one asked for")
        } else if !encoded.bytes.starts_with(&[0, 0, 0, 1]) {
            Some("the frame does not start with a 4-byte start code")
        } else if !types_ok(codec, encoded.idr, &types) {
            Some("its NAL types are not the parameter sets and an IDR, or one P slice")
        } else if slices < if encoded.idr { IDR_LEAST } else { P_LEAST } {
            Some("its slices are implausibly small")
        } else if recon.is_some_and(|db| db < RECON_FLOOR_CENTI_DB) {
            Some("its reconstruction is far from the input")
        } else {
            None
        };
        if let Some(why) = why {
            failure = Some(format!("frame {n}: {why}"));
            break;
        }
    }
    report.sha256 = Sha256::digest(&report.stream).into();
    let frames = report.frames.max(1);
    match &failure {
        None => {
            report.passed = true;
            let p_frames = u64::from(report.frames.saturating_sub(1)).max(1);
            log(format_args!(
                "NVENC SELFTEST {name} PASS: {}/{FRAMES} frames, {} bytes (IDR {idr_bytes}, P avg \
                 {}), gpu_us avg {} max {}, total_us avg {}, error_status 2 accepted on {accepted} \
                 frames, sha256 {}",
                report.frames,
                report.stream.len(),
                (report.stream.len() - idr_bytes) as u64 / p_frames,
                us(gpu_total) / u64::from(frames),
                us(gpu_most),
                us(all_total) / u64::from(frames),
                hex(&report.sha256)
            ));
        }
        Some(why) => log(format_args!(
            "NVENC SELFTEST {name} FAIL at {why}; {} frames good, {} bytes, sha256 {}",
            report.frames,
            report.stream.len(),
            hex(&report.sha256)
        )),
    }
    let closed = session.close();
    if closed.keeps_file {
        log(format_args!(
            "NVENC SELFTEST {name}: the channel is still busy, so it and its file stay open"
        ));
    }
    (report, closed)
}

/// Times the CPU's BGRA to block-linear NV12 conversion of one 1920x1080
/// frame into host memory, in nanoseconds of `now`, and checks a grey
/// picture comes out grey.
pub fn time_conversion(now: &mut dyn FnMut() -> u64) -> (u64, bool) {
    let geometry = Geometry::new(1920, 1080).expect("1080p is a geometry");
    let pitch = 4 * geometry.width as usize;
    let pixels = vec![0x80u8; pitch * geometry.height as usize];
    let mut out = vec![0u8; geometry.surface_bytes() as usize];
    let picture = Rgb {
        pixels: &pixels,
        pitch,
        order: Order::Bgra,
    };
    let started = now();
    // SAFETY: `out` holds the surface and the picture fits it.
    unsafe { surface::store_bgra(out.as_mut_ptr(), &geometry, &picture) };
    let took = now().saturating_sub(started);
    let luma = 16 + (220 * 0x80 + 128) / 256;
    let luma_bytes = geometry.luma_bytes() as usize;
    let grey = out[..luma_bytes].iter().all(|&y| u32::from(y) == luma)
        && out[luma_bytes..].iter().all(|&c| c == 128);
    (took, grey)
}

/// Base64 (RFC 4648, padded) of `data`, in lines of at most 76 characters.
pub fn base64_lines(data: &[u8]) -> Vec<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    data.chunks(57)
        .map(|line| {
            let mut text = String::with_capacity(76);
            for chunk in line.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let word = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
                for index in 0..4 {
                    if index <= chunk.len() {
                        text.push(char::from(
                            ALPHABET[(word >> (18 - 6 * index) & 63) as usize],
                        ));
                    } else {
                        text.push('=');
                    }
                }
            }
            text
        })
        .collect()
}

/// The whole frames of `report`'s stream that fit in `cap` bytes.
pub fn capped(report: &Report, cap: usize) -> &[u8] {
    let end = report
        .frame_ends
        .iter()
        .copied()
        .take_while(|&end| end <= cap)
        .last()
        .unwrap_or(0);
    &report.stream[..end]
}

#[cfg(test)]
mod tests;
