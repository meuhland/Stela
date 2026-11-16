//! The guest-audio ring, its sink and its clock, on a host counter the
//! tests move.
use super::*;

const HZ: u64 = 24_000_000;
const MS: u64 = HZ / 1000;
const S16_48K: Format = Format {
    channels: 2,
    sample_bytes: 2,
    rate: 48_000,
};

/// The frames a host counter at `tick` (from 0) gives at 48 kHz moved by
/// `ppm`: the HDA link's clock against the host's.
fn hda_frames(tick: u64, ppm: i64) -> u64 {
    (u128::from(tick) * 48_000 * (1_000_000 + ppm) as u128 / (u128::from(HZ) * 1_000_000)) as u64
}

/// The output publishes, its backlog at the target (no trim).
fn publish(ring: &GuestRing, frames: u64, tick: u64) {
    ring.publish(frames, tick, BACKLOG_TARGET_FRAMES);
}

/// A deterministic jitter in `0..span`.
struct Jitter(u64);

impl Jitter {
    fn next(&mut self, span: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % span
    }
}

#[test]
fn ring_frames_bring_s16_and_s32_to_msb_aligned_32_bits() {
    let s16: Vec<u8> = [0x1234i16, -2, i16::MAX, i16::MIN]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .chain([0xaa, 0xbb]) // a part of a frame, left unread
        .collect();
    assert_eq!(
        ring_frames(&s16, 2).collect::<Vec<_>>(),
        [[0x1234_0000, -2 << 16], [0x7fff_0000, i32::MIN],]
    );
    let s32: Vec<u8> = [0x1234_5678i32, -3, i32::MAX, i32::MIN]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    assert_eq!(
        ring_frames(&s32, 4).collect::<Vec<_>>(),
        [[0x1234_5678, -3], [i32::MAX, i32::MIN]]
    );
    // The USB speaker's ring stores 16-bit samples the same way, so the
    // HDMI route's 16-bit period gets the guest's samples back exactly.
    for sample in [0x1234i16, -2, i16::MAX, i16::MIN] {
        let [left, _] = ring_frames(&[sample.to_le_bytes(), [0, 0]].concat(), 2)
            .next()
            .unwrap();
        assert_eq!((left >> 16) as i16, sample);
    }
}

#[test]
fn the_ring_hands_over_what_was_pushed_in_order_and_drops_past_its_room() {
    let ring = GuestRing::new();
    assert_eq!(ring.queued(), 0);
    let frames = |from: i32, count: i32| (from..from + count).map(|n| [n, -n]);
    assert_eq!(ring.push(frames(0, 100)), 100);
    let mut out = [[0; 2]; 64];
    assert_eq!(ring.take(&mut out[..40]), 40);
    assert_eq!(out[0], [0, 0]);
    assert_eq!(out[39], [39, -39]);
    assert_eq!(ring.queued(), 60);
    // Fill it past its room: the frames that do not fit are dropped.
    let room = RING_FRAMES as i32 - 60;
    assert_eq!(ring.push(frames(100, room + 7)), room as u64);
    assert_eq!(ring.queued(), RING_FRAMES);
    assert_eq!(
        ring.stats(),
        RingStats {
            pushed: 100 + room as u64,
            taken: 40,
            dropped: 7,
        }
    );
    // In order across the wrap.
    let mut next = 40;
    let mut all = alloc::vec![[0; 2]; RING_FRAMES];
    while ring.queued() > 0 {
        let count = ring.take(&mut all[..1000]);
        for frame in &all[..count] {
            assert_eq!(*frame, [next, -next]);
            next += 1;
        }
    }
    assert_eq!(next, 100 + room);
    assert_eq!(ring.take(&mut out), 0);
}

#[test]
fn the_output_publication_reads_back_whole() {
    let ring = GuestRing::new();
    assert_eq!(ring.published(), None);
    ring.publish(4800, 123 * MS, 2000);
    assert_eq!(
        ring.published(),
        Some(Published {
            frames: 4800,
            tick: 123 * MS,
            backlog: 2000,
            live: true,
            generation: 1,
        })
    );
    ring.publish(5280, 133 * MS, 1500);
    ring.publish_stopped();
    assert_eq!(
        ring.published(),
        Some(Published {
            frames: 5280,
            tick: 133 * MS,
            backlog: 1500,
            live: false,
            generation: 3,
        })
    );
}

/// A backlog the output keeps over its target slows the clock a little
/// until it comes back; one at the target changes nothing.
#[test]
fn a_backlog_away_from_its_target_trims_the_clock() {
    let ring = GuestRing::new();
    let mut clock = FollowClock::new(HZ);
    clock.start(48_000, 0, 0);
    ring.publish(0, 0, BACKLOG_TARGET_FRAMES);
    assert!(clock.advance(0, ring.published()).is_some());
    // An output exactly at 48 kHz, published every 5 ms, 40 ms over its
    // target (left at the cap after a pause).
    let over = BACKLOG_TARGET_FRAMES + 1920;
    for turn in 1..=2000u64 {
        let now = 5 * MS * turn;
        ring.publish(240 * turn, now, over);
        assert_eq!(clock.advance(now, ring.published()), None);
    }
    // 2000 publications 1920 frames over moved the correspondence back by
    // 2000 * 1920 / 2048 frames, which the clock follows: it ran slower.
    let now = 10_000 * MS;
    let moved = (2000 * 1920 / LEVEL_STEP_FRAMES) as u64;
    assert!(
        clock.position(now) < 480_000 - moved / 2,
        "{} after 10 s",
        clock.position(now)
    );
    assert!(clock.ppm() < 0 && clock.ppm() >= -MAX_SLEW_PPM);
    // At the target: no trim.
    let mut clock = FollowClock::new(HZ);
    clock.start(48_000, 0, 0);
    ring.publish(0, 0, BACKLOG_TARGET_FRAMES);
    clock.advance(0, ring.published());
    for turn in 1..=2000u64 {
        let now = 5 * MS * turn;
        ring.publish(240 * turn, now, BACKLOG_TARGET_FRAMES);
        clock.advance(now, ring.published());
    }
    assert_eq!(clock.position(10_000 * MS), 480_000);
    assert_eq!(clock.ppm(), 0);
}

/// An output at 48 kHz of a link `ppm` off the host's counter, written in
/// periods of 480 frames as a service turn finds them completed: what it
/// publishes at `tick`, from its start at `start`.
fn periods_written(tick: u64, start: u64, ppm: i64) -> u64 {
    hda_frames(tick - start, ppm) / 480 * 480
}

#[test]
fn the_clock_follows_the_hdmi_frames_and_falls_back_to_the_device_clock() {
    let ring = GuestRing::new();
    let mut clock = FollowClock::new(HZ);
    let mut now = 10 * MS;
    clock.start(48_000, now, 0);
    // Nothing published: the host's counter, 48 frames a millisecond.
    assert_eq!(clock.advance(now, ring.published()), None);
    assert_eq!(clock.source(), ClockSource::DeviceTicks);
    assert_eq!(clock.position(now + 100 * MS), 4800);
    assert_eq!(clock.when(4800), Some(now + 100 * MS));

    // The output starts 20 ms later, its link 500 ppm fast; a service turn
    // every 2 to 8 ms publishes the periods written.
    let start = now + 20 * MS;
    let ppm = 500;
    let mut jitter = Jitter(7);
    let mut last = 0;
    let mut first = None;
    let mut gaps = Vec::new();
    let mut slews = Vec::new();
    now = start;
    while now < start + 60_000 * MS {
        now += 2 * MS + jitter.next(6 * MS);
        publish(&ring, periods_written(now, start, ppm), now);
        // The device's step comes a little after the turn.
        let step = now + jitter.next(MS);
        let change = clock.advance(step, ring.published());
        let position = clock.position(step);
        assert!(position >= last, "the clock never goes back");
        last = position;
        if let Some(change) = change {
            assert_eq!(first, None, "one handoff, then it follows: {change:?}");
            assert!(matches!(change, ClockChange::Following { .. }));
            first = Some((periods_written(now, start, ppm), clock.position(now)));
        }
        assert_eq!(clock.source(), ClockSource::HdmiFrames);
        let (frames, at) = first.unwrap();
        if now > start + 10_000 * MS {
            // Where the output says the clock should be, against where it is.
            let wanted = at + periods_written(now, start, ppm) - frames;
            gaps.push(wanted as i64 - clock.position(now) as i64);
            slews.push(clock.ppm());
        }
    }
    // It tracks the output within a period and a turn, and runs at its rate.
    let worst = gaps.iter().map(|gap| gap.abs()).max().unwrap();
    assert!(
        worst < 960,
        "the clock stays within 20 ms of the output: {worst}"
    );
    let mean = slews.iter().sum::<i64>() / slews.len() as i64;
    assert!(
        (ppm - 150..=ppm + 150).contains(&mean),
        "the clock runs at the link's rate: {mean} ppm"
    );
    assert_eq!(clock.resyncs(), 0);

    // The route pauses: back to the device clock at once, from where the
    // clock is.
    now += 10 * MS;
    ring.publish_stopped();
    let before = clock.position(now);
    assert!(matches!(
        clock.advance(now, ring.published()),
        Some(ClockChange::Fallback { stale: false, .. })
    ));
    assert_eq!(clock.position(now), before);
    assert_eq!((clock.source(), clock.ppm()), (ClockSource::DeviceTicks, 0));
    assert_eq!(clock.position(now + 1000 * MS), before + 48_000);
    now += 1000 * MS;
    assert_eq!(clock.advance(now, ring.published()), None);

    // The route plays again, its ring primed: the clock follows from where
    // it is, not from the jump.
    let restart = now;
    let primed = periods_written(now, start, ppm) + 22 * 480;
    publish(&ring, primed, now);
    let at = clock.position(now);
    assert_eq!(
        clock.advance(now, ring.published()),
        Some(ClockChange::Following {
            position: at,
            frames: primed
        })
    );
    // A service that stops looking for over 100 ms: back to the device
    // clock, marked stale.
    now += 30 * MS;
    publish(
        &ring,
        primed + hda_frames(now - restart, ppm) / 480 * 480,
        now,
    );
    assert_eq!(clock.advance(now, ring.published()), None);
    let quiet = now + STALE_MILLIS * MS + MS;
    let before = clock.position(quiet);
    assert!(matches!(
        clock.advance(quiet, ring.published()),
        Some(ClockChange::Fallback { stale: true, .. })
    ));
    assert_eq!(clock.position(quiet), before);
    assert_eq!(clock.handoffs(), 4);

    // Stopped, it holds; a start begins on the device clock again.
    clock.stop(quiet + MS);
    let held = clock.position(quiet + MS);
    assert_eq!(clock.position(quiet + 500 * MS), held);
    assert_eq!(clock.when(held + 1), None);
    assert_eq!(clock.advance(quiet + 2 * MS, ring.published()), None);
}

#[test]
fn an_output_that_jumps_is_taken_again_from_where_the_clock_is() {
    let ring = GuestRing::new();
    let mut clock = FollowClock::new(HZ);
    clock.start(48_000, 0, 1000);
    publish(&ring, 0, 0);
    assert!(clock.advance(0, ring.published()).is_some());
    for step in 1..=10 {
        publish(&ring, 480 * step, 10 * MS * step);
        assert_eq!(clock.advance(10 * MS * step, ring.published()), None);
    }
    let now = 110 * MS;
    let at = clock.position(now);
    publish(&ring, 480 * 11 + 10_000, now);
    let change = clock.advance(now, ring.published());
    let Some(ClockChange::Resync { position, gap }) = change else {
        panic!("{change:?}");
    };
    assert_eq!(position, at);
    assert!((9_500..=10_500).contains(&gap), "{gap}");
    assert_eq!((clock.resyncs(), clock.ppm()), (1, 0));
    // From there it follows the output's count again.
    publish(&ring, 480 * 12 + 10_000, now + 10 * MS);
    assert_eq!(clock.advance(now + 10 * MS, ring.published()), None);
    assert!(clock.ppm().abs() <= 500, "{}", clock.ppm());
}

#[test]
fn the_slew_is_bounded_and_its_ticks_reach_the_position() {
    let ring = GuestRing::new();
    let mut clock = FollowClock::new(HZ);
    clock.start(48_000, 0, 0);
    publish(&ring, 0, 0);
    clock.advance(0, ring.published());
    // The output 4000 frames ahead: the most slew, not a jump.
    publish(&ring, 480 + 4000, 10 * MS);
    assert_eq!(clock.advance(10 * MS, ring.published()), None);
    assert_eq!(clock.ppm(), MAX_SLEW_PPM);
    let at = clock.position(10 * MS);
    let due = clock.when(at + 48_240).unwrap();
    assert!(clock.position(due) >= at + 48_240);
    assert!(clock.position(due - 1) < at + 48_240);
    // 1.005 s of frames in a second of ticks.
    assert_eq!(clock.position(10 * MS + 1000 * MS), at + 48_240);
    // And 4000 frames behind: the least.
    let mut clock = FollowClock::new(HZ);
    clock.start(48_000, 0, 0);
    publish(&ring, 5000, 0);
    clock.advance(0, ring.published());
    publish(&ring, 5000 + 480 - 4000, 10 * MS);
    assert_eq!(clock.advance(10 * MS, ring.published()), None);
    assert_eq!(clock.ppm(), -MAX_SLEW_PPM);
    let at = clock.position(10 * MS);
    assert_eq!(clock.position(10 * MS + 1000 * MS), at + 47_760);
}

#[test]
fn the_ring_sink_pushes_the_guests_frames_and_its_silence() {
    let ring = GuestRing::new();
    let mut sink = GuestAudioRing::new(ring.clone(), HZ, None);
    assert_eq!(sink.rates(), 1 << RATE_48000);
    assert_eq!(sink.clock_source(), Some("device-ticks"));
    assert!(sink.describe().contains("hdmi-frames"));
    assert_eq!(ring.bits(), 0);
    sink.start(S16_48K, 0, 0);
    assert_eq!(ring.bits(), 16);
    // 100 frames of the guest's, then 200 of silence.
    let pcm: Vec<u8> = (0..100i16)
        .flat_map(|n| [(n * 100).to_le_bytes(), (-n).to_le_bytes()].concat())
        .collect();
    sink.play(&pcm);
    assert!(!sink.take_due(), "less than 5 ms waits");
    sink.silence(200);
    assert!(sink.take_due(), "5 ms wait for a service turn");
    assert!(!sink.take_due());
    assert_eq!(ring.queued(), 300);
    let mut out = [[1; 2]; 300];
    assert_eq!(ring.take(&mut out), 300);
    assert_eq!(out[1], [100 << 16, -1 << 16]);
    assert_eq!(out[99], [9900 << 16, -99 << 16]);
    assert!(out[100..].iter().all(|frame| *frame == [0, 0]));
    assert_eq!(
        sink.played(),
        Played {
            frames: 100,
            silent: 200
        }
    );
    // A long gap's silence fills the ring at most.
    sink.silence(10 * RING_FRAMES as u64);
    assert_eq!(ring.queued(), RING_FRAMES);
    assert_eq!(
        ring.stats().dropped,
        0,
        "silence past the room is not a drop"
    );
    // The clock: the host's counter until the output publishes.
    assert_eq!(sink.clock(10 * MS), 480);
    publish(&ring, 0, 10 * MS);
    sink.advance(10 * MS);
    assert_eq!(sink.clock_source(), Some("hdmi-frames"));
    let notes = sink.take_notes();
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].starts_with("clock: hdmi-frames from frame 480: follows the HDMI periods"),
        "{notes:?}"
    );
    assert!(
        sink.counters().contains(&("clock_handoffs", 1)),
        "{:?}",
        sink.counters()
    );
    sink.stop(20 * MS);
    assert_eq!(ring.bits(), 0);
    assert_eq!(
        sink.clock_source(),
        Some("hdmi-frames"),
        "the source it ran on, for the stop's line"
    );
    // S32 keeps its 32 bits.
    sink.start(
        Format {
            sample_bytes: 4,
            ..S16_48K
        },
        30 * MS,
        sink.clock(20 * MS),
    );
    assert_eq!(ring.bits(), 32);
    let mut out = alloc::vec![[0; 2]; RING_FRAMES];
    ring.take(&mut out);
    sink.play(&[0x1234_5678i32.to_le_bytes(), (-5i32).to_le_bytes()].concat());
    assert_eq!(ring.take(&mut out[..1]), 1);
    assert_eq!(out[0], [0x1234_5678, -5]);
}

#[test]
fn the_ring_sink_captures_as_the_counting_sink_does() {
    let ring = GuestRing::new();
    let mut sink = GuestAudioRing::new(ring.clone(), HZ, Some(1));
    assert!(
        sink.describe()
            .ends_with("keeping the first 1 s of sound for its fingerprint")
    );
    sink.start(S16_48K, 0, 0);
    let pcm = 1000i16.to_le_bytes().repeat(2 * 48_000);
    sink.play(&pcm[..pcm.len() / 2]);
    sink.play(&pcm[pcm.len() / 2..]);
    let notes = sink.take_notes();
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].starts_with("capture: 48000 frames (1.000 s, 192000 bytes, 2ch S16 48000 Hz)"),
        "{notes:?}"
    );
}
