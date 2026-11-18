use super::*;
use crate::nvenc::tests::{BOUND_NS, Engine, Gpu, decode, has};
use crate::nvenc::{
    CHANNEL_ALLOC, CHANNEL_FREE, GEM_CLOSE, GEM_CPU_PREP, GEM_NEW, GEM_PUSHBUF, GROBJ_ALLOC, Step,
};
use alloc::{format, string::String, vec, vec::Vec};

fn run(gpu: &mut Gpu, chipset: u16) -> (Outcome, Vec<String>) {
    let mut lines = Vec::new();
    let outcome = probe(gpu, chipset, BOUND_NS, &mut |line| {
        lines.push(format!("{line}"))
    });
    (outcome, lines)
}

fn verdict(outcome: Outcome, chipset: u16) -> String {
    format!("{}", outcome.verdict(&PROBE, chipset, BOUND_NS))
}

#[test]
fn the_decoder_is_named_as_clc7b0_does() {
    // clc7b0.h.
    assert_eq!(DECODER_CLASS, 0xc7b0);
    assert_eq!((APPLICATION_H264, APPLICATION_HEVC), (3, 7));
    assert_eq!(
        (
            method::SET_APPLICATION_ID,
            method::SEMAPHORE_A,
            method::SEMAPHORE_B,
            method::SEMAPHORE_C,
            method::EXECUTE,
            method::SEMAPHORE_D
        ),
        (0x200, 0x240, 0x244, 0x248, 0x300, 0x304)
    );
    // Its own abi16 engine, beside NVENC's (Linux patch 0208).
    assert_eq!(FIFO_ENGINE_NVDEC, 0x800);
    assert_eq!(FIFO_ENGINE_NVDEC & nvenc::FIFO_ENGINE_NVENC, 0);
    assert_eq!(PROBE.fifo_engine, FIFO_ENGINE_NVDEC);
    assert_ne!(PAYLOAD, nvenc::PAYLOAD);
}

#[test]
fn only_ga10x_with_a_display_is_probed() {
    for chipset in [0x172, 0x173, 0x174, 0x176, 0x177] {
        assert_eq!(decoder_class(chipset), Some(0xc7b0), "{chipset:#x}");
    }
    // GA100's decoder is NVC6B0; Turing, Ada and unknown chipsets are not
    // probed.
    for chipset in [0x170, 0x166, 0x194, 0x1b2, 0x999] {
        assert_eq!(decoder_class(chipset), None, "{chipset:#x}");
    }
}

#[test]
fn the_push_binds_the_decoder_names_h264_and_releases() {
    let words = push(0x12_3456_7000, PAYLOAD);
    assert_eq!(
        words,
        [
            0x2001_8000,
            0xc7b0,
            0x2001_8080,
            3,
            0x2003_8090,
            0x12,
            0x3456_7000,
            0x4e56_4445,
            0x2001_80c1,
            0,
        ]
    );
    assert_eq!(
        decode(&words),
        vec![
            (4, method::SET_OBJECT, DECODER_CLASS),
            (4, method::SET_APPLICATION_ID, APPLICATION_H264),
            (4, method::SEMAPHORE_A, 0x12),
            (4, method::SEMAPHORE_B, 0x3456_7000),
            (4, method::SEMAPHORE_C, PAYLOAD),
            (4, method::SEMAPHORE_D, 0),
        ]
    );
}

#[test]
fn a_live_decoder_releases_the_semaphore_and_everything_is_given_back() {
    let mut gpu = Gpu::probing(Engine::Alive, PROBE);
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(outcome, Outcome::Alive { micros: 0 });
    assert!(!outcome.keeps_channel());
    assert_eq!(
        gpu.requests,
        [
            CHANNEL_ALLOC,
            GROBJ_ALLOC,
            GEM_NEW,
            GEM_NEW,
            GEM_PUSHBUF,
            GEM_CLOSE,
            GEM_CLOSE,
            CHANNEL_FREE
        ]
    );
    assert_eq!(gpu.closed, [1, 2]);
    assert_eq!((gpu.mapped, gpu.unmapped), (2, 2));
    assert!(gpu.channel_freed);
    for text in [
        "NVDEC PROBE start: chipset 0x176, class 0xc7b0, bound 500 ms",
        "NVDEC PROBE channel ok: engine NVDEC, channel 5",
        "NVDEC PROBE class ok: 0xc7b0 instantiated as 0xc7b00001",
        "NVDEC PROBE submitted: 10 words: SET_OBJECT 0xc7b0, SET_APPLICATION_ID H264, \
         SEMAPHORE_A..D release 0x4e564445",
        "NVDEC PROBE ALIVE: semaphore 0x4e564445 after 0 us; NVDEC runs under GSP-RM",
        "NVDEC PROBE done: alive",
    ] {
        assert!(has(&lines, text), "{text}\n{lines:#?}");
    }
    assert!(
        !lines.iter().any(|line| line.contains("NVENC")),
        "{lines:#?}"
    );
    assert_eq!(
        verdict(outcome, 0x176),
        "class 0xc7b0 channel opened, semaphore released in 0 us"
    );
}

#[test]
fn an_unpatched_nouveau_refuses_the_channel_with_enosys() {
    let mut gpu = Gpu::probing(Engine::Alive, PROBE);
    gpu.refuse.push((CHANNEL_ALLOC, -38));
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Failed {
            step: Step::Channel,
            errno: -38
        }
    );
    assert_eq!(gpu.requests, [CHANNEL_ALLOC]);
    assert!(has(
        &lines,
        "NVDEC PROBE channel refused: ENOSYS (-38): this nouveau has no NVDEC case in abi16 \
         (Stela's Linux patch 0208)"
    ));
    assert!(has(
        &lines,
        "NVDEC PROBE done: failed at channel: ENOSYS (-38)"
    ));
    assert_eq!(
        verdict(outcome, 0x176),
        "class 0xc7b0 channel allocation refused: ENOSYS (-38): this nouveau has no NVDEC case \
         in abi16 (Stela's Linux patch 0208)"
    );
    // A GPU without the runlist.
    assert_eq!(
        verdict(
            Outcome::Failed {
                step: Step::Channel,
                errno: -22
            },
            0x176
        ),
        "class 0xc7b0 channel allocation refused: EINVAL (-22): nouveau found no NVDEC runlist \
         on this GPU"
    );
}

#[test]
fn a_refused_class_frees_its_channel() {
    let mut gpu = Gpu::probing(Engine::Alive, PROBE);
    gpu.refuse.push((GROBJ_ALLOC, -22));
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Failed {
            step: Step::Class,
            errno: -22
        }
    );
    assert_eq!(gpu.requests, [CHANNEL_ALLOC, GROBJ_ALLOC, CHANNEL_FREE]);
    assert!(has(
        &lines,
        "NVDEC PROBE class refused: 0xc7b0: EINVAL (-22): GSP-RM or nouveau has no decoder object"
    ));
    assert_eq!(
        verdict(outcome, 0x176),
        "class 0xc7b0 channel opened, class allocation refused: EINVAL (-22): GSP-RM or nouveau \
         has no decoder object on this channel"
    );
}

#[test]
fn a_hung_decoder_times_out_within_the_bound_and_keeps_its_channel() {
    let mut gpu = Gpu::probing(Engine::Hung, PROBE);
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Timeout {
            value: 0,
            fence_done: false
        }
    );
    assert!(outcome.keeps_channel());
    // Bounded: about one query per pause, and the bound is 500 pauses.
    let queries = gpu.requests.iter().filter(|&&r| r == GEM_CPU_PREP).count();
    assert!((499..=501).contains(&queries), "{queries}");
    // Nothing that would make nouveau wait for the push.
    assert!(!gpu.channel_freed);
    assert!(gpu.closed.is_empty());
    assert!(has(
        &lines,
        "NVDEC PROBE TIMEOUT: semaphore 0x00000000 after 500000 us, channel fence pending"
    ));
    assert!(has(&lines, "NVDEC PROBE done: timeout"));
    assert_eq!(
        verdict(outcome, 0x176),
        "class 0xc7b0 channel opened, TIMEOUT: semaphore not released within 500 ms (it holds \
         0x00000000) and the channel's fence is pending: the channel and its file stay open"
    );
}

#[test]
fn a_silent_decoder_times_out_but_its_channel_is_freed() {
    let mut gpu = Gpu::probing(Engine::Silent, PROBE);
    let (outcome, _) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Timeout {
            value: 0,
            fence_done: true
        }
    );
    assert!(!outcome.keeps_channel());
    assert_eq!(gpu.queries, 3);
    assert!(gpu.channel_freed);
    assert_eq!(gpu.closed, [1, 2]);
    assert!(verdict(outcome, 0x176).contains(
        "TIMEOUT: semaphore not released within 500 ms (it holds 0x00000000) though the \
         channel's fence signalled"
    ));
}

#[test]
fn a_refused_submit_gives_its_buffers_back() {
    let mut gpu = Gpu::probing(Engine::Alive, PROBE);
    gpu.refuse.push((GEM_PUSHBUF, -19));
    let (outcome, _) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Failed {
            step: Step::Submit,
            errno: -19
        }
    );
    assert_eq!(gpu.closed, [1, 2]);
    assert!(gpu.channel_freed);
    assert_eq!(
        verdict(outcome, 0x176),
        "class 0xc7b0 channel opened, submit refused: ENODEV (-19)"
    );
}

#[test]
fn other_chipsets_are_skipped_untouched() {
    let mut gpu = Gpu::probing(Engine::Alive, PROBE);
    let (outcome, lines) = run(&mut gpu, 0x194);
    assert_eq!(outcome, Outcome::Skipped);
    assert!(gpu.requests.is_empty());
    assert!(has(
        &lines,
        "NVDEC PROBE skipped: chipset 0x194 has no decoder class"
    ));
    assert_eq!(
        verdict(outcome, 0x194),
        "skipped: chipset 0x194 has no decoder class the probe knows (GA102-GA107 only)"
    );
}
