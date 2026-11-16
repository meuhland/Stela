//! The virtio sound device's guest-audio ring against the HDMI output's
//! backlog: a simulated guest that sends a period whenever the ring sink's
//! clock reaches it (as the device completes its transfers), and an HDMI
//! output on a link clock of its own, refilled by service turns that come
//! when they do.
use super::*;
use alloc::{collections::VecDeque, format, string::ToString, vec};
use stela_soc::virtio::snd::sink::{
    BACKLOG_TARGET_FRAMES, Format, GuestAudioRing, GuestRing, RING_FRAMES, Sink,
};

const HZ: u64 = 24_000_000;
const MS: u64 = HZ / 1000;
const PERIOD: usize = 480;

// The ring sink trims toward the same backlog the output keeps.
const _: () = assert!(BACKLOG_TARGET_FRAMES as usize == TARGET_FRAMES);
// The ring holds what one service turn takes at most.
const _: () = assert!(RING_FRAMES as u64 == MAX_STEP_FRAMES);

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

/// How the run goes.
struct Run {
    /// The HDMI link's clock against the host's, in parts per million.
    link_ppm: i64,
    /// The output publishes what it consumed (the hdmi-frames clock);
    /// without, the sink stays on the host's counter.
    publish: bool,
    seconds: u64,
    /// A pause of the route (monitor asleep): from, for, in ms.
    pause: Option<(u64, u64)>,
    /// Measured from this second on.
    from: u64,
}

/// What the measured part of a run did.
#[derive(Debug)]
struct Outcome {
    /// The backlog after each turn: least, mean and most, in frames.
    min: usize,
    mean: usize,
    max: usize,
    /// Silence the output played for want of frames, and frames dropped
    /// past the cap, while measured.
    silent: u64,
    dropped: u64,
    source: &'static str,
    handoffs: u64,
    resyncs: u64,
    /// The guest's frames sent in all, and those while the route paused.
    sent: u64,
    sent_paused: u64,
}

fn simulate(run: Run) -> Outcome {
    let ring = GuestRing::new();
    let mut sink = GuestAudioRing::new(ring.clone(), HZ, None);
    let format = Format {
        channels: 2,
        sample_bytes: 2,
        rate: 48_000,
    };
    sink.start(format, 0, 0);
    // The guest's next period starts at this frame; it always has one
    // queued (the kext keeps its whole buffer submitted).
    let mut next = 0u64;
    let pcm = 4000i16.to_le_bytes().repeat(2 * PERIOD);
    let mut sent = 0u64;
    let mut sent_paused = 0u64;
    // The output.
    let mut pending: VecDeque<[i32; 2]> = VecDeque::new();
    let mut taken = vec![[0i32; 2]; RING_FRAMES];
    let mut backlog = Backlog::default();
    let mut output_frames = 0u64;
    let mut written_periods = 0u64;
    let mut paused_ticks = 0u64;
    let mut was_running = false;
    let mut silent = 0u64;
    let mut dropped = 0u64;
    let mut level = Level::default();
    let mut jitter = Jitter(11);
    let mut turn_at = 3 * MS;
    let mut turns = 0u64;
    let mut long_gap = false;
    let end = run.seconds * 1000 * MS;
    let mut now = 0;
    while now < end {
        now += MS;
        let paused = run
            .pause
            .is_some_and(|(from, length)| (from * MS..(from + length) * MS).contains(&now));
        if paused {
            paused_ticks += MS;
        }
        // The device's step: the clock looks at the output, then the
        // guest's periods the clock reached go to the sink.
        sink.advance(now);
        let clock = sink.clock(now);
        while next <= clock {
            sink.play(&pcm);
            next += PERIOD as u64;
            sent += PERIOD as u64;
            if paused {
                sent_paused += PERIOD as u64;
            }
        }
        if now < turn_at {
            continue;
        }
        // A service turn, every 2 to 8 ms; a 40 ms gap every 1400 turns
        // (about 7 s), and one of 150 ms at 30 s (the clock goes back to
        // the host's counter for a while).
        turns += 1;
        turn_at = now
            + if now >= 30_000 * MS && !long_gap {
                long_gap = true;
                150 * MS
            } else if turns.is_multiple_of(1400) {
                40 * MS
            } else {
                2 * MS + jitter.next(6 * MS)
            };
        let measured = now >= run.from * 1000 * MS;
        // Under the lock: the ring's frames join the output's queue.
        let moved = ring.take(&mut taken);
        pending.extend(&taken[..moved]);
        // Outside it: every period the link's DMA completed is written.
        let running = !paused;
        if running {
            let link =
                (u128::from(now - paused_ticks) * 48_000 * (1_000_000 + run.link_ppm) as u128
                    / (u128::from(HZ) * 1_000_000)) as u64;
            while written_periods < link / PERIOD as u64 {
                let take = backlog.take(PERIOD, pending.len(), true);
                pending.drain(..take);
                if measured {
                    silent += (PERIOD - take) as u64;
                }
                written_periods += 1;
                output_frames += PERIOD as u64;
            }
        }
        let excess = pending.len().saturating_sub(CAP_FRAMES);
        pending.drain(..excess);
        if measured {
            dropped += excess as u64;
            level.observe(pending.len());
        }
        // The publication: a route that ran through this turn and the
        // last.
        if run.publish {
            if running && was_running {
                ring.publish(output_frames, now, (pending.len() + ring.queued()) as u64);
            } else {
                ring.publish_stopped();
            }
        }
        was_running = running;
    }
    let levels = level.take().unwrap();
    Outcome {
        min: levels.min,
        mean: levels.avg,
        max: levels.max,
        silent,
        dropped,
        source: sink.clock_source().unwrap(),
        handoffs: sink.follow_clock().handoffs(),
        resyncs: sink.follow_clock().resyncs(),
        sent,
        sent_paused,
    }
}

/// Following the HDMI periods written, the backlog holds at its target on a
/// link clock either side of the host's: nothing padded, nothing dropped,
/// through jittered turns, 40 ms gaps and a 150 ms one.
#[test]
fn the_backlog_holds_its_target_when_the_guest_follows_the_hdmi_frames() {
    for link_ppm in [0, 1000, -1000, 2000, -2000] {
        let outcome = simulate(Run {
            link_ppm,
            publish: true,
            seconds: 60,
            pause: None,
            from: 10,
        });
        let at = format!("link {link_ppm} ppm: {outcome:?}");
        assert_eq!((outcome.silent, outcome.dropped), (0, 0), "{at}");
        assert!(outcome.min > 0 && outcome.max <= CAP_FRAMES, "{at}");
        assert!(
            (TARGET_FRAMES - 240..=TARGET_FRAMES + 480).contains(&outcome.mean),
            "the backlog's mean stays within 5 to 10 ms of 40 ms: {at}"
        );
        assert_eq!(outcome.source, "hdmi-frames", "{at}");
        // The first handoff, then away and back at the 150 ms gap.
        assert_eq!((outcome.handoffs, outcome.resyncs), (3, 0), "{at}");
    }
}

/// On the host's counter alone (no publication, the first cut), a link
/// 0.2% off drifts the backlog: a fast link runs it dry (silence), a slow
/// one fills it to the cap (drops). What following the HDMI frames removes.
#[test]
fn on_the_host_counter_alone_a_link_off_its_rate_pads_or_drops() {
    let fast = simulate(Run {
        link_ppm: 2000,
        publish: false,
        seconds: 60,
        pause: None,
        from: 10,
    });
    assert!(fast.silent > 0, "{fast:?}");
    assert_eq!(fast.source, "device-ticks");
    let slow = simulate(Run {
        link_ppm: -2000,
        publish: false,
        seconds: 60,
        pause: None,
        from: 10,
    });
    assert!(slow.dropped > 0, "{slow:?}");
    let level = simulate(Run {
        link_ppm: 0,
        publish: false,
        seconds: 60,
        pause: None,
        from: 10,
    });
    assert_eq!((level.silent, level.dropped), (0, 0), "{level:?}");
}

/// A route that pauses (the monitor asleep) hands the clock to the host's
/// counter: the guest goes on sending at 48 kHz, never held, and the output
/// keeps the newest 80 ms. Once the route plays again the clock follows it
/// again, and the backlog the pause left at the cap comes back to its
/// target.
#[test]
fn a_paused_route_never_holds_the_guest_and_the_backlog_comes_back() {
    let outcome = simulate(Run {
        link_ppm: 500,
        publish: true,
        seconds: 90,
        pause: Some((20_000, 2000)),
        from: 60,
    });
    let at = format!("{outcome:?}");
    // Two seconds of the guest's frames while paused, within a period.
    assert!(
        (2 * 48_000 - 480..=2 * 48_000 + 480).contains(&outcome.sent_paused),
        "{at}"
    );
    assert!(outcome.sent >= 90 * 48_000 - 2 * 48_000, "{at}");
    assert_eq!((outcome.silent, outcome.dropped), (0, 0), "{at}");
    assert!(
        (TARGET_FRAMES - 240..=TARGET_FRAMES + 480).contains(&outcome.mean),
        "{at}"
    );
    assert_eq!(outcome.source, "hdmi-frames", "{at}");
}

#[test]
fn the_route_line_names_virtio_snd_the_ring_and_hdmi() {
    let line = virtio_route(Some("GA106 HDMI pin 0x04"));
    assert!(
        line.starts_with(
            "GUEST AUDIO route: virtio-snd -> guest-audio ring -> HDMI on GA106 HDMI pin 0x04: "
        ),
        "{line}"
    );
    assert!(line.contains("clock=hdmi-frames") && line.contains("clock=device-ticks"));
    assert!(line.ends_with("the backlog fills to 40 ms, 80 ms at most"));
    let line = virtio_route(None);
    assert!(line.contains("no HDMI playback was set up"), "{line}");
}

#[test]
fn the_virtio_report_marks_its_source_and_its_unsent_feedback() {
    let report = Report {
        bits: 16,
        received: 48_000,
        overflowed: 0,
        split: 0,
        queued: 1920,
        muted: false,
        volume: 0,
        host: Counters {
            consumed: 46_080,
            silent: 1920,
            trimmed: 0,
        },
        interval_silent: 0,
        interval_dropped: 0,
        backlog: None,
        asynchronous: false,
        feedback: FEEDBACK_NOMINAL,
        measured: None,
        packets: 0,
        feedback_reads: 0,
    };
    let usb = report.to_string();
    let virtio = VirtioReport(report).to_string();
    assert!(usb.contains("(unsent:adaptive)") && !usb.contains("source="));
    assert_eq!(
        virtio,
        format!(
            "{} source=virtio-snd",
            usb.replace("(unsent:adaptive)", "(unsent:virtio)")
        )
    );
    assert!(virtio.starts_with("GUEST AUDIO frames=48000 consumed=46080 silent=1920 "));
}
