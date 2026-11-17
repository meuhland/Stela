use super::super::mock::{Engine, Gpu};
use super::*;
use alloc::format;

fn lines_of(engine: Engine, codec: Codec, rate: Rate) -> (Report, Closed<Gpu>, Vec<String>) {
    let mut lines = Vec::new();
    let (report, closed) = run(Gpu::new(engine), 0x176, codec, rate, &mut |line| {
        lines.push(format!("{line}"))
    });
    (report, closed, lines)
}

fn has(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line.contains(text))
}

#[test]
fn every_run_passes_on_a_working_engine() {
    for (codec, rate) in RUNS {
        let (report, closed, lines) = lines_of(Engine::Encode, codec, rate);
        let name = run_name(codec, rate);
        assert!(report.passed, "{name}: {lines:#?}");
        assert_eq!(report.frames, FRAMES);
        assert_eq!(report.frame_ends.len(), FRAMES as usize);
        assert!(!closed.keeps_file && closed.node.channel_freed);
        assert_eq!(lines.len(), 2 + FRAMES as usize, "{lines:#?}");
        assert!(has(
            &lines,
            &format!("NVENC SELFTEST {name} open: 1920x1080 coded 1920x1088")
        ));
        assert!(has(&lines, "at NVIDIA's captured addresses"));
        assert!(has(&lines, &format!("NVENC SELFTEST {name} frame 0: IDR")));
        assert!(has(
            &lines,
            &format!("NVENC SELFTEST {name} frame 29: P 100 bytes, NAL 1,")
        ));
        assert!(has(&lines, "error_status 2 ucode_error_status 0x0"));
        assert!(has(&lines, "recon_y 99.00 dB"));
        let pass = format!(
            "NVENC SELFTEST {name} PASS: 30/30 frames, {} bytes",
            report.stream.len()
        );
        assert!(has(&lines, &pass), "{lines:#?}");
        assert!(has(&lines, "error_status 2 accepted on 30 frames"));
        assert!(has(&lines, &hex(&report.sha256)));
        let parameter_sets = match codec {
            Codec::H264 => 36,
            Codec::Hevc => 95,
        };
        assert_eq!(report.stream.len(), parameter_sets + 2000 + 29 * 100);
    }
    assert_eq!(
        RUNS.map(|(codec, rate)| run_name(codec, rate)),
        ["h264-qp25", "h264-cbr", "hevc-qp25", "hevc-cbr"]
    );
}

#[test]
fn a_hang_fails_the_run_and_keeps_the_file() {
    let (report, closed, lines) = lines_of(Engine::HangsAfter(4), Codec::Hevc, Rate::ConstQp(QP));
    assert!(!report.passed && report.timed_out);
    assert_eq!(report.frames, 4);
    assert!(closed.keeps_file);
    assert!(has(
        &lines,
        "NVENC SELFTEST hevc-qp25 FAIL at frame 4: TIMEOUT: the semaphore held 0x00000004; the \
         channel fence is pending"
    ));
    assert!(has(&lines, "the channel is still busy"));
}

#[test]
fn wrong_output_fails_the_run() {
    let (report, closed, lines) = lines_of(Engine::WrongType, Codec::H264, Rate::ConstQp(QP));
    assert!(!report.passed && !report.timed_out);
    assert_eq!(report.frames, 1);
    assert!(!closed.keeps_file);
    assert!(has(&lines, "FAIL at frame 1: engine output refused"));
    let mut gpu = Gpu::new(Engine::Encode);
    gpu.refuse.push((crate::nvenc::CHANNEL_ALLOC, -38));
    let mut lines = Vec::new();
    let (report, closed) = run(gpu, 0x176, Codec::H264, Rate::ConstQp(QP), &mut |line| {
        lines.push(format!("{line}"))
    });
    assert!(!report.passed && !closed.keeps_file);
    assert!(has(
        &lines,
        "NVENC SELFTEST h264-qp25 FAIL at open: channel: ENOSYS (-38): this nouveau has no NVENC \
         case in abi16"
    ));
}

#[test]
fn base64_and_caps() {
    assert_eq!(base64_lines(b"Man"), ["TWFu"]);
    assert_eq!(base64_lines(b"Ma"), ["TWE="]);
    assert_eq!(base64_lines(b"M"), ["TQ=="]);
    let lines = base64_lines(&[0xffu8; 200]);
    assert_eq!(lines.len(), 4);
    assert!(lines[..3].iter().all(|line| line.len() == 76));
    assert!(lines[0].chars().all(|c| c == '/'));
    let report = Report {
        name: String::new(),
        passed: true,
        frames: 3,
        stream: vec![1; 300],
        frame_ends: vec![100, 200, 300],
        sha256: [0; 32],
        timed_out: false,
    };
    assert_eq!(capped(&report, 250).len(), 200);
    assert_eq!(capped(&report, 50).len(), 0);
    assert_eq!(capped(&report, 300).len(), 300);
}

#[test]
fn the_conversion_is_timed_and_grey_stays_grey() {
    let mut clock = 0;
    let (took, grey) = time_conversion(&mut || {
        clock += 5;
        clock
    });
    assert_eq!(took, 5);
    assert!(grey);
}
