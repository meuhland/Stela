use super::*;
use crate::{
    dwc3::xhci::tests::host,
    usb::{
        Speed,
        control::{
            GET_DESCRIPTOR, SET_ADDRESS, SET_CONFIGURATION,
            tests::{setup, transfer},
        },
        hub::{Hub, Ports},
    },
};
use stela_drivers::{audio::Direction, usb::audio::Sync as StreamSync};

fn configured() -> Audio {
    configured_as(Mode::Adaptive)
}

fn configured_as(mode: Mode) -> Audio {
    let mut a = Audio::with_mode(mode);
    transfer(&mut a, setup(0, SET_ADDRESS, 4, 0, 0), &[]).unwrap();
    transfer(&mut a, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
    a
}

/// A class request to entity `entity` of the control interface.
fn entity(request_type: u8, request: u8, selector: u8, entity: u8, length: u16) -> [u8; 8] {
    setup(
        request_type,
        request,
        u16::from(selector) << 8,
        u16::from(entity) << 8,
        length,
    )
}

fn stream(a: &mut Audio, alternate: u16) {
    transfer(a, setup(0x01, SET_INTERFACE, alternate, 1, 0), &[]).unwrap();
}

#[test]
fn descriptors_describe_one_stereo_uac2_output_without_capture() {
    let mut a = configured();
    let config = transfer(&mut a, setup(0x80, GET_DESCRIPTOR, 0x200, 0, 512), &[]).unwrap();
    assert_eq!(
        usize::from(u16::from_le_bytes([config[2], config[3]])),
        config.len()
    );
    assert_eq!(config[4], 2, "control and streaming interfaces");
    // Every descriptor's length walks the configuration exactly.
    let mut at = 0;
    let mut terminals = Vec::new();
    while at < config.len() {
        let d = &config[at..at + usize::from(config[at])];
        if d[1] == INTERFACE && d[2] == 1 {
            // The streaming interface: its subtypes mean other things.
            break;
        }
        if d[1] == CS_INTERFACE && matches!(d[2], INPUT_TERMINAL | OUTPUT_TERMINAL) && d.len() >= 6
        {
            terminals.push((d[2], u16::from_le_bytes([d[4], d[5]])));
        }
        at += d.len();
    }
    let mut rest = at;
    while rest < config.len() {
        rest += usize::from(config[rest]);
    }
    assert_eq!(rest, config.len());
    // USB streaming in, speaker out: no microphone or line input.
    assert_eq!(
        terminals,
        [(INPUT_TERMINAL, 0x0101), (OUTPUT_TERMINAL, 0x0301)]
    );
    let other = transfer(&mut a, setup(0x80, GET_DESCRIPTOR, 0x700, 0, 512), &[]).unwrap();
    assert_eq!(other.len(), config.len());
}

/// A configuration's descriptors, each whole, its lengths walking it
/// exactly.
fn split(config: &[u8]) -> Vec<&[u8]> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < config.len() {
        let length = usize::from(config[at]);
        assert!(length >= 2 && at + length <= config.len(), "at {at}");
        out.push(&config[at..at + length]);
        at += length;
    }
    out
}

/// Each streaming alternate setting of a configuration: (alternate,
/// bNumEndpoints, its descriptors after the interface's).
fn settings(config: &[u8]) -> Vec<(u8, u8, Vec<Vec<u8>>)> {
    let mut out: Vec<(u8, u8, Vec<Vec<u8>>)> = Vec::new();
    let mut streaming = false;
    for d in split(config) {
        if d[1] == INTERFACE {
            streaming = d[2] == 1;
            if streaming {
                out.push((d[3], d[4], Vec::new()));
            }
        } else if streaming && let Some(last) = out.last_mut() {
            last.2.push(d.to_vec());
        }
    }
    out
}

/// The asynchronous speaker's descriptors parse back as UAC2 defines them:
/// each streaming setting holds its asynchronous data endpoint, the
/// class-specific endpoint after it, then the explicit feedback endpoint
/// (isochronous IN 0x81, no synchronization, feedback usage): 4 bytes
/// (16.16) every 1 ms at high speed, 3 bytes (10.14) every frame at full
/// speed. The data endpoint holds 54 frames a packet, the most feedback can
/// ask. The clock stays internal and fixed; the adaptive descriptors are
/// unchanged.
#[test]
fn asynchronous_descriptors_pair_each_setting_with_a_feedback_endpoint() {
    let mut a = configured_as(Mode::Asynchronous);
    let config = transfer(&mut a, setup(0x80, GET_DESCRIPTOR, 0x200, 0, 1024), &[]).unwrap();
    assert_eq!(
        usize::from(u16::from_le_bytes([config[2], config[3]])),
        config.len()
    );
    // The clock source: internal fixed (1), frequency and validity read
    // only (bmControls 0b0101), as in the adaptive speaker.
    let clock = split(&config)
        .into_iter()
        .find(|d| d[1] == CS_INTERFACE && d[2] == CLOCK_SOURCE)
        .unwrap()
        .to_vec();
    assert_eq!(
        clock,
        [8, CS_INTERFACE, CLOCK_SOURCE, CLOCK, 0x01, 0x05, 0, 0]
    );
    let high = settings(&config);
    assert_eq!(high.len(), 3, "alternates 0, 1 and 2");
    assert_eq!((high[0].0, high[0].1, high[0].2.len()), (0, 0, 0));
    for (n, subslot) in [(1usize, 2u16), (2, 3)] {
        let (alternate, endpoints, ref descriptors) = high[n];
        assert_eq!((alternate, endpoints), (n as u8, 2));
        let endpoints: Vec<&Vec<u8>> = descriptors.iter().filter(|d| d[1] == ENDPOINT).collect();
        let data_max = (54 * 2 * subslot).to_le_bytes();
        assert_eq!(
            *endpoints[0],
            [7, ENDPOINT, STREAM, 0x05, data_max[0], data_max[1], 4]
        );
        assert_eq!(*endpoints[1], [7, ENDPOINT, FEEDBACK, 0x11, 4, 0, 4]);
        // The class-specific data endpoint follows the data endpoint.
        let at = descriptors.iter().position(|d| d[1] == ENDPOINT).unwrap();
        assert_eq!(descriptors[at + 1][..3], [8, CS_ENDPOINT, EP_GENERAL]);
        assert_eq!(descriptors[at + 2][2], FEEDBACK);
    }
    let other = transfer(&mut a, setup(0x80, GET_DESCRIPTOR, 0x700, 0, 1024), &[]).unwrap();
    for (alternate, endpoints, descriptors) in settings(&other).into_iter().skip(1) {
        let feedback = descriptors
            .iter()
            .find(|d| d[1] == ENDPOINT && d[2] == FEEDBACK);
        assert_eq!(endpoints, 2, "alternate {alternate}");
        assert_eq!(**feedback.unwrap(), [7, ENDPOINT, FEEDBACK, 0x11, 3, 0, 1]);
    }
    // Adaptive: one data endpoint a setting, 49 frames, attributes 0x09.
    let mut a = configured();
    let config = transfer(&mut a, setup(0x80, GET_DESCRIPTOR, 0x200, 0, 1024), &[]).unwrap();
    for (alternate, endpoints, descriptors) in settings(&config).into_iter().skip(1) {
        assert_eq!(endpoints, 1);
        let data: Vec<_> = descriptors.iter().filter(|d| d[1] == ENDPOINT).collect();
        let max = (49 * 2 * (u16::from(alternate) + 1)).to_le_bytes();
        assert_eq!(*data[0], [7, ENDPOINT, STREAM, 0x09, max[0], max[1], 4]);
        assert_eq!(data.len(), 1);
    }
}

/// The feedback endpoint answers the owner's rate: 16.16 frames per
/// microframe in a 4-byte packet, 10.14 frames per frame in a 3-byte one,
/// held to an eighth of nominal either way. It exists only on a streaming
/// setting of the asynchronous speaker.
#[test]
fn feedback_is_sent_in_16_16_at_high_speed_and_10_14_at_full_speed() {
    assert_eq!(FEEDBACK_NOMINAL, 6 << 16);
    assert_eq!((FEEDBACK_MIN, FEEDBACK_MAX), (0x5_4000, 0x6_c000));
    let mut a = configured_as(Mode::Asynchronous);
    let mut packet = [0u8; 4];
    // Alternate 0: no feedback endpoint.
    assert_eq!(a.packet(FEEDBACK, Token::In, &mut packet), Ok(Reply::Stall));
    stream(&mut a, 1);
    assert_eq!(
        a.packet(FEEDBACK, Token::In, &mut packet),
        Ok(Reply::Ack(4))
    );
    assert_eq!(packet, (6u32 << 16).to_le_bytes());
    // +2%: 6.12 frames a microframe, 48.96 kHz.
    let faster = FEEDBACK_NOMINAL + FEEDBACK_NOMINAL / 50;
    a.set_feedback(faster);
    assert_eq!(a.status().feedback, 0x6_1eb8);
    a.packet(FEEDBACK, Token::In, &mut packet).unwrap();
    assert_eq!(u32::from_le_bytes(packet), 0x6_1eb8);
    let mut full = [0u8; 3];
    assert_eq!(a.packet(FEEDBACK, Token::In, &mut full), Ok(Reply::Ack(3)));
    // 48.96 frames a 1 ms frame in 10.14.
    assert_eq!(full, [0x70, 0x3d, 0x0c]);
    assert_eq!(
        u32::from_le_bytes([full[0], full[1], full[2], 0]) as f64 / f64::from(1 << 14),
        f64::from(0x6_1eb8u32) * 8.0 / 65536.0
    );
    a.set_feedback(0);
    assert_eq!(a.status().feedback, FEEDBACK_MIN);
    a.set_feedback(u32::MAX);
    assert_eq!(a.status().feedback, FEEDBACK_MAX);
    assert_eq!(a.status().counters.feedback_reads, 3);
    assert_eq!(a.status().mode, Mode::Asynchronous);
    // The endpoint's standard requests succeed, as the data endpoint's.
    assert_eq!(
        transfer(&mut a, setup(0x82, GET_STATUS, 0, 0x81, 2), &[]).unwrap(),
        [0, 0]
    );
    transfer(
        &mut a,
        setup(0x02, CLEAR_FEATURE, ENDPOINT_HALT, 0x81, 0),
        &[],
    )
    .unwrap();
    // The adaptive speaker has no feedback endpoint.
    let mut a = configured();
    stream(&mut a, 1);
    assert_eq!(a.packet(FEEDBACK, Token::In, &mut packet), Ok(Reply::Stall));
    assert!(transfer(&mut a, setup(0x82, GET_STATUS, 0, 0x81, 2), &[]).is_err());
    assert_eq!(a.status().mode, Mode::Adaptive);
}

#[test]
fn clock_and_feature_unit_requests_answer_as_uac2_defines() {
    let mut a = configured();
    assert_eq!(
        transfer(&mut a, entity(0xa1, CUR, SAM_FREQ, CLOCK, 4), &[]).unwrap(),
        48_000u32.to_le_bytes()
    );
    // A host reads the subrange count first, then the whole block.
    assert_eq!(
        transfer(&mut a, entity(0xa1, RANGE, SAM_FREQ, CLOCK, 2), &[]).unwrap(),
        [1, 0]
    );
    let block = transfer(&mut a, entity(0xa1, RANGE, SAM_FREQ, CLOCK, 14), &[]).unwrap();
    assert_eq!(&block[2..6], 48_000u32.to_le_bytes());
    assert_eq!(&block[6..10], 48_000u32.to_le_bytes());
    assert_eq!(
        transfer(&mut a, entity(0xa1, CUR, CLOCK_VALID, CLOCK, 1), &[]).unwrap(),
        [1]
    );
    transfer(
        &mut a,
        entity(0x21, CUR, SAM_FREQ, CLOCK, 4),
        &48_000u32.to_le_bytes(),
    )
    .unwrap();
    assert!(
        transfer(
            &mut a,
            entity(0x21, CUR, SAM_FREQ, CLOCK, 4),
            &44_100u32.to_le_bytes()
        )
        .is_err()
    );
    // Volume: range, clamping and rounding toward the minimum.
    assert_eq!(
        transfer(&mut a, entity(0xa1, RANGE, VOLUME, FEATURE, 8), &[]).unwrap(),
        [1, 0, 0x00, 0xc4, 0, 0, 0x80, 0]
    );
    for (asked, kept) in [
        (-6 * 256, -6 * 256),
        (-77, -128),
        (100, 0),
        (-30_000, -60 * 256),
    ] {
        transfer(
            &mut a,
            entity(0x21, CUR, VOLUME, FEATURE, 2),
            &i16::to_le_bytes(asked),
        )
        .unwrap();
        assert_eq!(
            transfer(&mut a, entity(0xa1, CUR, VOLUME, FEATURE, 2), &[]).unwrap(),
            i16::to_le_bytes(kept)
        );
    }
    transfer(&mut a, entity(0x21, CUR, MUTE, FEATURE, 1), &[1]).unwrap();
    assert_eq!(
        transfer(&mut a, entity(0xa1, CUR, MUTE, FEATURE, 1), &[]).unwrap(),
        [1]
    );
    // Per-channel controls, unknown entities and selectors STALL.
    assert!(transfer(&mut a, setup(0xa1, CUR, 0x0201, 0x0300, 2), &[]).is_err());
    assert!(transfer(&mut a, entity(0xa1, CUR, VOLUME, 9, 2), &[]).is_err());
    assert!(transfer(&mut a, entity(0xa1, CUR, 7, FEATURE, 2), &[]).is_err());
}

#[test]
fn streamed_samples_reach_the_ring_msb_aligned_with_gain_applied() {
    let mut a = configured();
    // No setting selected: the endpoint does not exist.
    assert_eq!(a.packet(STREAM, Token::Out, &mut [0; 4]), Ok(Reply::Stall));
    stream(&mut a, 1);
    assert!(a.streaming());
    let mut packet = [0x34, 0x12, 0xcc, 0xed, 0xff, 0x7f, 0x00, 0x80];
    assert_eq!(a.packet(STREAM, Token::Out, &mut packet), Ok(Reply::Ack(8)));
    let mut out = [[0; 2]; 4];
    assert_eq!(a.take(&mut out), 2);
    assert_eq!(out[0], [0x1234_0000, 0xedcc_0000u32 as i32]);
    assert_eq!(out[1], [0x7fff_0000, i32::MIN]);
    stream(&mut a, 2);
    let mut packet = [0x56, 0x34, 0x12, 0x00, 0x00, 0x80, 0xff];
    a.packet(STREAM, Token::Out, &mut packet).unwrap();
    assert_eq!(a.take(&mut out), 1);
    assert_eq!(out[0], [0x1234_5600, i32::MIN]);
    assert_eq!(a.status().counters.split, 1);
    // -6 dB halves the amplitude to within the step table's rounding.
    transfer(
        &mut a,
        entity(0x21, CUR, VOLUME, FEATURE, 2),
        &i16::to_le_bytes(-6 * 256),
    )
    .unwrap();
    let mut packet = [0, 0, 0x40, 0, 0, 0x40];
    a.packet(STREAM, Token::Out, &mut packet).unwrap();
    a.take(&mut out);
    let half = f64::from(out[0][0]) / f64::from(0x4000_0000);
    assert!((half - 0.501).abs() < 0.001, "{half}");
    transfer(&mut a, entity(0x21, CUR, MUTE, FEATURE, 1), &[1]).unwrap();
    a.packet(STREAM, Token::Out, &mut packet).unwrap();
    a.take(&mut out);
    assert_eq!(out[0], [0, 0]);
}

#[test]
fn a_full_ring_drops_its_oldest_frames_and_asks_for_service() {
    let mut a = configured();
    stream(&mut a, 1);
    let mut first = true;
    // 300 ms of 1 ms packets, frame n carrying n.
    for packet in 0..300u32 {
        let mut bytes = Vec::new();
        for n in packet * 48..packet * 48 + 48 {
            bytes.extend_from_slice(&(n as u16).to_le_bytes());
            bytes.extend_from_slice(&(n as u16).to_le_bytes());
        }
        a.packet(STREAM, Token::Out, &mut bytes).unwrap();
        if first {
            // One 1 ms packet is under the 5 ms that asks for service.
            assert!(!a.take_due());
            first = false;
        }
    }
    assert!(a.take_due());
    assert!(!a.take_due());
    assert_eq!(a.queued(), RING_FRAMES);
    let status = a.status();
    assert_eq!(status.counters.received, 300 * 48);
    assert_eq!(status.counters.overflowed, (300 * 48 - RING_FRAMES) as u64);
    let mut out = [[0; 2]; 1];
    a.take(&mut out);
    let oldest = (300 * 48 - RING_FRAMES) as u16;
    assert_eq!(out[0][0], i32::from(oldest as i16) << 16);
    // Leaving the setting keeps what is queued; reconfiguring drops it.
    stream(&mut a, 0);
    assert!(!a.streaming());
    assert_eq!(a.queued(), RING_FRAMES - 1);
    transfer(&mut a, setup(0, SET_CONFIGURATION, 1, 0, 0), &[]).unwrap();
    assert_eq!(a.queued(), 0);
}

struct One(Audio);
impl Ports for One {
    fn count(&self) -> usize {
        1
    }
    fn speed(&self, _: usize) -> Speed {
        Speed::High
    }
    fn function(&mut self, _: usize) -> &mut dyn Function {
        &mut self.0
    }
    fn function_ref(&self, _: usize) -> &dyn Function {
        &self.0
    }
}

/// The host's own UAC2 class driver binds the function behind the native
/// hub on the modeled xHCI and plays PCM through its isochronous endpoint:
/// the descriptors, the class requests and the isochronous OUT path.
#[test]
fn a_uac2_host_driver_plays_through_the_isochronous_endpoint() {
    let hub = Hub::new(One(Audio::new()), "HUB").unwrap();
    let mut bench = host::bench(hub);
    let mut found = host::run(&mut bench);
    assert!(found.failures.is_empty(), "{:?}", found.failures);
    assert_eq!(found.audio.len(), 1, "{:?}", found.unused);
    let mut audio = found.audio.remove(0);
    let formats: Vec<_> = audio
        .binding()
        .formats
        .iter()
        .map(|f| (f.direction, f.channels, f.subslot, f.bits, f.sync.clone()))
        .collect();
    assert_eq!(
        formats,
        [
            (Direction::Playback, 2, 2, 16, StreamSync::Adaptive),
            (Direction::Playback, 2, 3, 24, StreamSync::Adaptive),
        ]
    );
    for (index, subslot) in [(1usize, 3usize), (0, 2)] {
        // 100 ms of a ramp, each sample's subslot bytes distinct.
        let frames = 4800usize;
        let mut pcm = Vec::new();
        for n in 0..frames * 2 {
            let value = (n as u32).wrapping_mul(2_654_435_761);
            pcm.extend_from_slice(&value.to_le_bytes()[..subslot]);
        }
        audio.start_playback(index, RATE, &pcm, true).unwrap();
        let mut drained = false;
        for _ in 0..20_000 {
            if audio.poll().unwrap().playback_drained {
                drained = true;
                break;
            }
        }
        assert!(drained, "{:?}", audio.progress());
        let model = &mut bench.model.borrow_mut().function.ports.0;
        // A finite stream ends by selecting the zero-bandwidth setting.
        assert!(!model.streaming());
        let mut out = vec![[0i32; 2]; frames + 1];
        assert_eq!(model.take(&mut out), frames);
        for (n, frame) in out[..frames].iter().enumerate() {
            for (c, sample) in frame.iter().enumerate() {
                let value = ((n * 2 + c) as u32).wrapping_mul(2_654_435_761);
                let mut word = [0u8; 4];
                word[4 - subslot..].copy_from_slice(&value.to_le_bytes()[..subslot]);
                assert_eq!(*sample, i32::from_le_bytes(word), "frame {n}");
            }
        }
        assert_eq!(model.status().counters.overflowed, 0);
    }
    // The host's mute and volume reach the feature unit.
    audio.set_mute(FEATURE, 0, true).unwrap();
    audio.set_volume(FEATURE, 0, -12 * 256).unwrap();
    let status = bench.model.borrow().function.ports.0.status();
    assert!(status.muted);
    assert_eq!(status.volume, -12 * 256);
    // Adaptive: 48 frames a 1 ms packet.
    let counters = status.counters;
    assert_eq!(counters.received, 2 * 4800);
    assert_eq!(counters.packets, 2 * 100);
    assert_eq!(counters.feedback_reads, 0);
}

/// Stela's host UAC2 driver (a port of Linux snd-usb-audio's) binds the
/// asynchronous speaker through the modeled xHCI: it parses each setting's
/// explicit feedback endpoint, polls it as isochronous IN TDs and sizes its
/// packets from the 16.16 value, so a speaker that asks for 2% more gets
/// 48.96 frames a 1 ms packet instead of 48.
#[test]
fn an_asynchronous_uac2_host_driver_follows_the_explicit_feedback() {
    let hub = Hub::new(One(Audio::asynchronous()), "HUB").unwrap();
    let mut bench = host::bench(hub);
    let mut found = host::run(&mut bench);
    assert!(found.failures.is_empty(), "{:?}", found.failures);
    assert_eq!(found.audio.len(), 1, "{:?}", found.unused);
    let mut audio = found.audio.remove(0);
    for (format, subslot) in audio.binding().formats.iter().zip([2, 3]) {
        assert_eq!(
            (format.direction, format.channels, format.subslot),
            (Direction::Playback, 2, subslot)
        );
        assert!(
            matches!(
                format.sync,
                StreamSync::Explicit(endpoint)
                    if endpoint.address == FEEDBACK
                        && endpoint.attributes == 0x11
                        && endpoint.max_packet == 4
                        && endpoint.interval == 4
            ),
            "{:?}",
            format.sync
        );
        assert_eq!(format.max_bytes, 54 * 2 * usize::from(subslot));
    }
    let faster = FEEDBACK_NOMINAL + FEEDBACK_NOMINAL / 50;
    bench
        .model
        .borrow_mut()
        .function
        .ports
        .0
        .set_feedback(faster);
    // 500 ms of 16-bit stereo.
    let frames = 24_000usize;
    let pcm: Vec<u8> = (0..frames * 4).map(|n| n as u8).collect();
    audio.start_playback(0, RATE, &pcm, true).unwrap();
    let mut drained = false;
    for _ in 0..200_000 {
        if audio.poll().unwrap().playback_drained {
            drained = true;
            break;
        }
    }
    assert!(drained, "{:?}", audio.progress());
    assert!(audio.progress().feedback_updates > 0);
    let model = &mut bench.model.borrow_mut().function.ports.0;
    let counters = model.status().counters;
    assert_eq!(
        counters.received,
        frames as u64,
        "{:?} {counters:?}",
        audio.progress()
    );
    assert!(counters.feedback_reads > 0);
    // The first packets go out before the first feedback lands; the rest
    // carry 48.96 frames on average.
    let per_packet = counters.received as f64 / counters.packets as f64;
    assert!((48.85..=48.97).contains(&per_packet), "{per_packet}");
    // The ring keeps the newest 200 ms, in order.
    let mut out = vec![[0i32; 2]; RING_FRAMES];
    assert_eq!(model.take(&mut out), RING_FRAMES);
    let last = (frames - 1) * 4;
    assert_eq!(
        out[RING_FRAMES - 1][0],
        i32::from_le_bytes([0, 0, last as u8, (last + 1) as u8])
    );
}
