// SPDX-License-Identifier: GPL-2.0-only
//! transport behavior tests.
use super::*;

#[test]
fn cyclic_playback_refills_without_restart() {
    for dp in [false, true] {
        for channels in [2, 4, 6, 8] {
            let mut f = Fixture::new(dp);
            let mut c = f.controller();
            let port = f.attach(&mut c);
            let (stream, period) = f.stream(&mut c, channels, 24);
            let map = c.prepare_hdmi(port, stream, 2, None).unwrap();
            for n in 0..4 {
                c.write_pcm(stream, n * period, &vec![n as u8; period])
                    .unwrap();
            }
            c.start(stream).unwrap();
            let format = c.stream_format(stream).unwrap();
            for n in 0..24 {
                f.advance(period, format);
                let next = vec![(n + 4) as u8; period];
                let done = c.refill_hdmi_period(stream, &next).unwrap().unwrap();
                assert_eq!(done.sequence, n as u64);
                assert_eq!(done.ring_offset, n % 4 * period);
            }
            {
                let h = f.hardware.lock().unwrap();
                assert_eq!(h.starts, 1);
                assert_eq!(h.stops, 0);
                for (n, chunk) in h.playback.chunks(period).enumerate() {
                    assert!(chunk.iter().all(|b| *b == n as u8));
                }
                assert_eq!(h.packet[0], 0x84);
                assert_eq!(&h.packet[1..3], if dp { &[0x1b, 0x44] } else { &[1, 10] });
                assert_eq!(h.packet[7], map.allocation());
                assert_eq!(h.digital & 0x21, 1);
                assert_eq!(h.count, channels - 1);
                // A legacy codec has no device entries to select.
                assert!(!h.commands.iter().any(|c| (c >> 8) & 0xfff == 0x735));
            }
            c.close_hdmi(port).unwrap();
            assert_eq!(c.start(stream), Err(Error::SinkUnavailable));
            c.detach_hdmi_port(port).unwrap();
            c.shutdown().unwrap();
            assert_eq!(Arc::strong_count(&f.owner), 1);
        }
    }
}

#[test]
fn invalid_port_rejected() {
    for fault in 0..6 {
        let mut f = Fixture::new(false);
        let mut c = f.controller();
        match fault {
            0 => f.hardware.lock().unwrap().vendor = 0x10de0017,
            1 => f.hardware.lock().unwrap().vendor = 0x10de00a2,
            2 => f.hardware.lock().unwrap().pin_caps = 0x10,
            3 => f.hardware.lock().unwrap().bad_eld = Some(12),
            4 => f.hardware.lock().unwrap().eld[4] = 31,
            _ => f.owner.state.lock().unwrap().observe_fault = true,
        }
        let graph = c.enumerate_codec(0).unwrap();
        // SAFETY: The controller and display owner share this fixture's locked Hardware.
        // Invalid pin/device selectors are rejected before keeping a binding.
        let attached = unsafe { c.attach_hdmi_port(&graph, 4, 0, f.owner.clone()) };
        if fault == 3 || fault == 4 {
            let port = attached.unwrap();
            // Beyond basic audio, an ELD that does not read or parse
            // refuses the route, naming the check.
            let (s, _) = f.stream(&mut c, 6, 24);
            let expected = if fault == 3 {
                Error::EldRead(EldReadFault::ByteNotValid {
                    index: 12,
                    response: 0,
                })
            } else {
                Error::EldParse(EldParseFault::MonitorNameLength { length: 31 })
            };
            assert_eq!(c.prepare_hdmi(port, s, 2, None), Err(expected));
            c.detach_hdmi_port(port).unwrap();
        } else {
            assert!(attached.is_err());
        }
        assert_eq!(f.owner.state.lock().unwrap().enable, 0);
        assert_eq!(f.hardware.lock().unwrap().pin, 0);
    }
}

#[test]
fn a_sink_whose_eld_does_not_read_or_parse_plays_basic_audio() {
    for (fault, wide) in (0..5).flat_map(|fault| [(fault, true), (fault, false)]) {
        let mut f = Fixture::new(false);
        let expected = {
            let mut h = f.hardware.lock().unwrap();
            match fault {
                0 => {
                    h.bad_eld = Some(12);
                    Error::EldRead(EldReadFault::ByteNotValid {
                        index: 12,
                        response: 0,
                    })
                }
                1 => {
                    h.eld_size = 300;
                    Error::EldRead(EldReadFault::Size { response: 300 })
                }
                2 => {
                    h.eld[0] = 0;
                    Error::EldRead(EldReadFault::FirstByteZero {
                        response: 0x8000_0000,
                    })
                }
                3 => {
                    h.eld[4] = 31;
                    Error::EldParse(EldParseFault::MonitorNameLength { length: 31 })
                }
                _ => {
                    // A DP-only pin: the packet is DisplayPort's.
                    h.pin_caps = 0x01000014;
                    h.eld[0] = 0x08;
                    Error::EldParse(EldParseFault::Version { byte0: 0x08 })
                }
            }
        };
        let mut c = f.controller();
        let port = f.attach(&mut c);
        if wide {
            // 24-bit stereo is not basic audio: refused, naming the check.
            let (s, _) = f.stream(&mut c, 2, 24);
            assert_eq!(c.prepare_hdmi(port, s, 2, None), Err(expected));
            assert_eq!(f.owner.state.lock().unwrap().enable, 0);
            assert_eq!(f.hardware.lock().unwrap().pin, 0);
            c.detach_hdmi_port(port).unwrap();
            continue;
        }
        let (s, period) = f.stream(&mut c, 2, 16);
        let map = c.prepare_hdmi(port, s, 2, None).unwrap();
        assert_eq!(map.allocation(), 0);
        assert_eq!(map.speakers(), [Speaker::Fl, Speaker::Fr]);
        let sink = c.hdmi_sink(port).unwrap().unwrap();
        assert_eq!((sink.eld, sink.eld_error), (None, Some(expected)));
        for n in 0..4 {
            c.write_pcm(s, n * period, &vec![n as u8; period]).unwrap();
        }
        c.start(s).unwrap();
        let format = c.stream_format(s).unwrap();
        f.advance(period, format);
        assert!(c.refill_hdmi_period(s, &vec![4; period]).unwrap().is_some());
        {
            let h = f.hardware.lock().unwrap();
            assert_eq!(h.starts, 1);
            assert_eq!(h.count, 1);
            assert_eq!(h.packet[0], 0x84);
            let dp = fault == 4;
            assert_eq!(&h.packet[1..3], if dp { &[0x1b, 0x44] } else { &[1, 10] });
            assert_eq!(h.packet[7], 0);
        }
        c.close_hdmi(port).unwrap();
        c.detach_hdmi_port(port).unwrap();
        c.shutdown().unwrap();
    }
    // Present without ELD_VALID: still no sink to play to.
    let mut f = Fixture::new(false);
    f.hardware.lock().unwrap().sense = 0x8000_0000;
    let mut c = f.controller();
    let port = f.attach(&mut c);
    let (s, _) = f.stream(&mut c, 2, 16);
    assert_eq!(
        c.prepare_hdmi(port, s, 2, None),
        Err(Error::SinkUnavailable)
    );
    c.detach_hdmi_port(port).unwrap();
}

#[test]
fn mst_selects_each_device_entry() {
    let mut f = Fixture::new(true);
    {
        let mut h = f.hardware.lock().unwrap();
        h.vendor = 0x10de00a8;
        h.device_list = 3;
        h.device = 2;
        h.pin_caps |= 2;
    }
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    // Four entries: index 4 does not exist, and the owner must agree.
    f.owner.state.lock().unwrap().device = 4;
    assert_eq!(
        // SAFETY: The controller and display owner share this fixture's locked Hardware.
        // Invalid pin/device selectors are rejected before keeping a binding.
        unsafe { c.attach_hdmi_port(&graph, 4, 4, f.owner.clone()) },
        Err(Error::InvalidHdmiPort)
    );
    assert_eq!(
        // SAFETY: The controller and display owner share this fixture's locked Hardware.
        // Invalid pin/device selectors are rejected before keeping a binding.
        unsafe { c.attach_hdmi_port(&graph, 4, 2, f.owner.clone()) },
        Err(Error::InvalidHdmiPort)
    );
    f.owner.state.lock().unwrap().device = 2;
    // SAFETY: The controller and display owner share this fixture's locked Hardware.
    // Invalid pin/device selectors are rejected before keeping a binding.
    let port = unsafe { c.attach_hdmi_port(&graph, 4, 2, f.owner.clone()) }.unwrap();
    let (stream, period) = f.stream(&mut c, 2, 16);
    c.prepare_hdmi(port, stream, 2, None).unwrap();
    for n in 0..4 {
        c.write_pcm(stream, n * period, &vec![n as u8; period])
            .unwrap();
    }
    c.start(stream).unwrap();
    let format = c.stream_format(stream).unwrap();
    f.advance(period, format);
    c.refill_hdmi_period(stream, &vec![4; period])
        .unwrap()
        .unwrap();
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
    let h = f.hardware.lock().unwrap();
    let select = (4 << 20) | (0x735 << 8) | 2;
    assert!(h.commands.contains(&select));
    // Sense is triggered, then read for the device entry.
    assert!(h.commands.contains(&((4 << 20) | (0x709 << 8))));
    assert!(h.commands.contains(&((4 << 20) | (0xf09 << 8) | 2)));
    assert_eq!(h.packet[..3], [0x84, 0x1b, 0x44]);
    assert_eq!((h.pin, h.transmit), (0, 0));
}

#[test]
fn hotplug_reprepares_same_port() {
    let mut f = Fixture::new(false);
    let mut c = f.controller();
    let port = f.attach(&mut c);
    let (s, period) = f.stream(&mut c, 2, 16);
    c.prepare_hdmi(port, s, 2, None).unwrap();
    c.start(s).unwrap();
    {
        let mut o = f.owner.state.lock().unwrap();
        o.generation += 1;
        o.connected = false;
    }
    f.hardware.lock().unwrap().sense = 0;
    assert_eq!(c.status(s).unwrap_err(), Error::SinkChanged);
    assert_eq!(f.hardware.lock().unwrap().bar[0x80] & 2, 0);
    assert_eq!(f.hardware.lock().unwrap().pin, 0);
    assert_eq!(c.start(s), Err(Error::SinkUnavailable));
    assert_eq!(Arc::strong_count(&f.owner), 2);
    {
        let mut o = f.owner.state.lock().unwrap();
        o.generation += 1;
        o.connected = true;
    }
    f.hardware.lock().unwrap().sense = 0xc0000000;
    c.prepare_hdmi(port, s, 2, None).unwrap();
    c.write_pcm(s, 0, &vec![0x7a; period * 4]).unwrap();
    c.start(s).unwrap();
    assert_eq!(f.hardware.lock().unwrap().get(0x84, 4), 0);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
}

#[test]
fn coalesced_ioc_preserves_frontier() {
    let mut f = Fixture::new(false);
    let mut c = f.controller();
    let port = f.attach(&mut c);
    let (s, p) = f.stream(&mut c, 2, 16);
    c.prepare_hdmi(port, s, 2, None).unwrap();
    c.start(s).unwrap();
    let fmt = c.stream_format(s).unwrap();
    f.advance(p / 2, fmt);
    assert!(c.refill_hdmi_period(s, &vec![8; p]).unwrap().is_none());
    f.advance(p + p / 2, fmt);
    assert!(c.status(s).unwrap().period_completed);
    for _ in 0..2 {
        assert!(c.refill_hdmi_period(s, &vec![9; p]).unwrap().is_some());
    }
    assert!(c.refill_hdmi_period(s, &vec![10; p]).unwrap().is_none());
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
}

#[test]
fn late_refill_stops_stream() {
    for fault in 0..3 {
        let mut f = Fixture::new(false);
        let mut c = f.controller();
        let port = f.attach(&mut c);
        let (s, p) = f.stream(&mut c, 2, 16);
        c.prepare_hdmi(port, s, 2, None).unwrap();
        c.start(s).unwrap();
        let fmt = c.stream_format(s).unwrap();
        match fault {
            0 => {
                f.clock.0.fetch_add(100000, Ordering::SeqCst);
            }
            1 => f.advance(p * 4 - 64, fmt),
            _ => {
                f.advance(p, fmt);
                let mut h = f.hardware.lock().unwrap();
                h.late_position = Some((h.position_reads + 2, (p * 4 - 64) as u32, 32_000));
            }
        }
        assert_eq!(
            c.refill_hdmi_period(s, &vec![0x39; p]),
            Err(Error::PlaybackUnderrun)
        );
        assert_eq!(f.hardware.lock().unwrap().bar[0x80] & 2, 0);
        assert_eq!(
            c.hdmi_first_error(port).unwrap(),
            Some(Error::PlaybackUnderrun)
        );
        // Only the stream stopped: the route restarts it.
        assert!(c.hdmi_prepared(port).unwrap());
        assert_eq!(f.hardware.lock().unwrap().pin, 0x40);
        assert_eq!(f.owner.state.lock().unwrap().disable, 0);
        assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Stream));
        assert_eq!(f.hardware.lock().unwrap().bar[0x80] & 2, 2);
        c.close_hdmi(port).unwrap();
        c.detach_hdmi_port(port).unwrap();
    }
}

/// Codec verbs that walk the graph or read the ELD: parameters,
/// connection lists, configuration defaults, ELD size and data.
fn walks(command: u32) -> bool {
    matches!(
        (command >> 8) & 0xfff,
        0xf00 | 0xf02 | 0xf1c | 0xf2e | 0xf2f
    )
}

/// Codec verbs that set state: every short set verb (0x7xx: mux, pin
/// control, digital, channel count, stream tag, DIP, slots, device
/// select), the format (long 2) and amplifier (long 3) sets.
fn sets(command: u32) -> bool {
    matches!((command >> 16) & 15, 2 | 3 | 7)
}

/// A prepared, started route with one period refilled.
fn playing(f: &mut Fixture) -> (Controller<Regs, Clock>, HdmiPortId, StreamId, usize) {
    let mut c = f.controller();
    let port = f.attach(&mut c);
    let (s, p) = f.stream(&mut c, 2, 16);
    c.prepare_hdmi(port, s, 2, None).unwrap();
    c.start(s).unwrap();
    let fmt = c.stream_format(s).unwrap();
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![1; p]).unwrap().is_some());
    (c, port, s, p)
}

/// The stream stopped while the route stayed as prepared, and the display
/// owner's audio was not disabled.
fn only_the_stream_stopped(f: &Fixture, c: &Controller<Regs, Clock>, port: HdmiPortId) {
    let h = f.hardware.lock().unwrap();
    assert_eq!(h.bar[0x80] & 2, 0);
    assert_eq!((h.pin, h.transmit), (0x40, 0xc0));
    assert_ne!(h.binding, 0);
    assert_eq!(f.owner.state.lock().unwrap().disable, 0);
    assert!(c.hdmi_prepared(port).unwrap());
}

#[test]
fn an_underrun_restarts_only_the_stream() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // A service gap longer than the ring.
    f.clock.0.fetch_add(100_000, Ordering::SeqCst);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::PlaybackUnderrun)
    );
    only_the_stream_stopped(&f, &c, port);
    assert!(!c.hdmi_paused(port).unwrap());
    let (binding, format, before) = {
        let h = f.hardware.lock().unwrap();
        (h.binding, h.format, h.commands.len())
    };
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Stream));
    {
        let h = f.hardware.lock().unwrap();
        let restart = &h.commands[before..];
        // The route's readback only: no graph walk, no ELD read, no set.
        assert!(!restart.is_empty());
        assert!(restart.iter().all(|c| !walks(*c)), "{restart:x?}");
        assert!(restart.iter().all(|c| !sets(*c)), "{restart:x?}");
        // The same converter, stream tag and format; the descriptor reset.
        assert_eq!((h.binding, h.format), (binding, format));
        assert_eq!(h.starts, 2);
        assert_eq!(h.get(0x84, 4), 0);
    }
    assert_eq!(f.owner.state.lock().unwrap().enable, 1);
    // It plays again from a silent ring.
    f.hardware.lock().unwrap().playback.clear();
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    assert!(f.hardware.lock().unwrap().playback.iter().all(|b| *b == 0));
    assert_eq!(c.restart_hdmi(port), Err(Error::StreamRunning));
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_transient_bad_answer_does_not_end_playback() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // A wrong pin-control readback, wrong again when read again.
    f.hardware.lock().unwrap().glitch = Some((0xf07, 2));
    c.schedule_hdmi_readback(port).unwrap();
    f.advance(p, fmt);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::RouteRejected)
    );
    only_the_stream_stopped(&f, &c, port);
    let before = f.hardware.lock().unwrap().commands.len();
    // The route's verbs set again, with no graph walk, then the stream.
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Reprogrammed));
    {
        let h = f.hardware.lock().unwrap();
        let restart = &h.commands[before..];
        assert!(restart.iter().all(|c| !walks(*c)), "{restart:x?}");
        assert!(restart.iter().any(|c| sets(*c)), "{restart:x?}");
    }
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    // Faults spread out by played periods never add up.
    for _ in 0..2 * HDMI_TRANSIENT_FAULTS {
        f.hardware.lock().unwrap().glitch = Some((0xf07, 2));
        c.schedule_hdmi_readback(port).unwrap();
        f.advance(p, fmt);
        assert_eq!(
            c.refill_hdmi_period(s, &vec![4; p]),
            Err(Error::RouteRejected)
        );
        assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Reprogrammed));
        f.advance(p, fmt);
        assert!(c.refill_hdmi_period(s, &vec![5; p]).unwrap().is_some());
    }
    assert_eq!(
        f.hardware.lock().unwrap().starts,
        2 + 2 * HDMI_TRANSIENT_FAULTS as usize
    );
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn the_route_state_reads_back_and_a_rejection_names_its_answer() {
    for dp in [false, true] {
        let mut f = Fixture::new(dp);
        let (mut c, port, s, p) = playing(&mut f);
        let fmt = c.stream_format(s).unwrap();
        assert_eq!(c.take_rejection(), None);
        let state = c.hdmi_route_state(port).unwrap();
        assert_eq!((state.converter, state.pin), (2, 4));
        assert_eq!(state.pin_control, 0x40);
        assert_eq!(state.dip_transmit, 0xc0);
        assert_eq!(state.connection, u32::from(state.set_connection));
        assert_eq!(state.digital & 0xff, u32::from(state.set_digital));
        assert_eq!(state.set_digital & 0x21, 1);
        assert_eq!((state.channels, state.set_channels), (1, 1));
        assert_eq!(state.format, u32::from(state.set_format));
        assert_eq!(state.stream, u32::from(state.set_stream));
        let header: [u8; 3] = if dp {
            [0x84, 0x1b, 0x44]
        } else {
            [0x84, 1, 10]
        };
        assert_eq!(state.dip[..3], header);
        assert_eq!(state.dip_sum(), 0);
        assert!(!state.single_command);
        assert!(alloc::format!("{state}").contains("transmit 0xc0 bytes 84 "));
        // Reading it set nothing the route checks: playback goes on.
        f.advance(p, fmt);
        assert!(c.refill_hdmi_period(s, &vec![2; p]).unwrap().is_some());
        // A pin-control readback wrong twice in a row is named, once.
        f.hardware.lock().unwrap().glitch = Some((0xf07, 2));
        c.schedule_hdmi_readback(port).unwrap();
        f.advance(p, fmt);
        assert_eq!(
            c.refill_hdmi_period(s, &vec![3; p]),
            Err(Error::RouteRejected)
        );
        let rejection = c.take_rejection().unwrap();
        assert_eq!(
            (
                rejection.node,
                rejection.verb,
                rejection.payload,
                rejection.read
            ),
            (4, 0xf07, 0, 0)
        );
        assert_eq!(rejection.expected, Expected::Value(0x40));
        assert!(!rejection.single_command);
        assert!(alloc::format!("{rejection}").contains(
            "node 0x04 verb 0xf07 payload 0x0000: answered 0x00000000, expected 0x00000040"
        ));
        assert_eq!(c.take_rejection(), None);
        assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Reprogrammed));
        c.close_hdmi(port).unwrap();
        assert_eq!(c.hdmi_route_state(port), Err(Error::SinkUnavailable));
        c.detach_hdmi_port(port).unwrap();
        c.shutdown().unwrap();
    }
}

#[test]
fn a_codec_that_keeps_answering_wrong_needs_a_reset_after_the_bound() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    f.hardware.lock().unwrap().reject = Some(0xf07);
    c.schedule_hdmi_readback(port).unwrap();
    f.advance(p, fmt);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::RouteRejected)
    );
    // Not counted yet: each restart sets the route's verbs again, and
    // its readback, still wrong right after, counts.
    assert_eq!(route_faults(&c, port), 0);
    for fault in 1..=HDMI_TRANSIENT_FAULTS {
        let before = f.hardware.lock().unwrap().commands.len();
        assert_eq!(c.restart_hdmi(port), Err(Error::RouteRejected));
        only_the_stream_stopped(&f, &c, port);
        assert_eq!(route_faults(&c, port), fault);
        let h = f.hardware.lock().unwrap();
        assert!(h.commands[before..].contains(&((4 << 20) | (0x707 << 8) | 0x40)));
        assert!(h.commands[before..].iter().all(|c| !walks(*c)));
    }
    assert_eq!(c.restart_hdmi(port), Err(Error::NeedsReset));
    assert_eq!(c.restart_hdmi(port), Err(Error::NeedsReset));
    assert_eq!(
        c.hdmi_first_error(port).unwrap(),
        Some(Error::RouteRejected)
    );
    // Given up: the route closed, the display owner's audio disabled.
    assert!(!c.hdmi_prepared(port).unwrap());
    assert_eq!(f.hardware.lock().unwrap().pin, 0);
    assert_eq!(f.owner.state.lock().unwrap().disable, 1);
    f.hardware.lock().unwrap().reject = None;
    c.reinitialize().unwrap();
    assert_eq!(Arc::strong_count(&f.owner), 1);
}

#[test]
fn a_sleeping_monitor_pauses_and_resumes() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // The display side's empty ELD: presence and ELD valid drop.
    f.hardware.lock().unwrap().sense = 0;
    f.advance(p, fmt);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::SinkUnavailable)
    );
    only_the_stream_stopped(&f, &c, port);
    assert!(c.hdmi_paused(port).unwrap());
    // Asleep for longer than any fault bound: paused, never a reset.
    for _ in 0..2 * HDMI_TRANSIENT_FAULTS {
        assert_eq!(c.restart_hdmi(port), Err(Error::SinkUnavailable));
    }
    only_the_stream_stopped(&f, &c, port);
    // Awake with the same ELD: the stream resumes on the route as
    // prepared, its verbs set again, with no graph walk.
    f.hardware.lock().unwrap().sense = 0xc000_0000;
    let before = f.hardware.lock().unwrap().commands.len();
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Resumed));
    {
        let h = f.hardware.lock().unwrap();
        let resume = &h.commands[before..];
        assert!(resume.iter().any(|c| (c >> 8) & 0xfff == 0xf2f));
        assert!(
            resume
                .iter()
                .all(|c| !matches!((c >> 8) & 0xfff, 0xf00 | 0xf02 | 0xf1c))
        );
        assert!(resume.iter().any(|c| sets(*c)), "{resume:x?}");
        assert_eq!(h.starts, 2);
    }
    assert!(!c.hdmi_paused(port).unwrap());
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    // Asleep again, back with another sink's ELD: prepared again.
    f.hardware.lock().unwrap().sense = 0;
    f.advance(p, fmt);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![4; p]),
        Err(Error::SinkUnavailable)
    );
    {
        let mut h = f.hardware.lock().unwrap();
        h.sense = 0xc000_0000;
        h.eld[7] = 0x01;
    }
    assert_eq!(c.restart_hdmi(port), Err(Error::SinkChanged));
    assert!(!c.hdmi_prepared(port).unwrap());
    assert_eq!(f.hardware.lock().unwrap().pin, 0);
    assert_eq!(f.owner.state.lock().unwrap().disable, 1);
    c.prepare_hdmi(port, s, 2, None).unwrap();
    c.write_pcm(s, 0, &vec![0; p * 4]).unwrap();
    c.start(s).unwrap();
    assert_eq!(c.hdmi_sink(port).unwrap().unwrap().eld.unwrap().speakers, 1);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn enable_failure_requires_reset() {
    for fault in 0..3 {
        let mut f = Fixture::new(false);
        let mut c = f.controller();
        let port = f.attach(&mut c);
        let (s, _) = f.stream(&mut c, 6, 24);
        match fault {
            0 => f.owner.state.lock().unwrap().enable_late = true,
            1 => f.hardware.lock().unwrap().reject = Some(0x72d),
            _ => f.hardware.lock().unwrap().drop_reply = Some(0x706),
        }
        let error = c.prepare_hdmi(port, s, 2, None).unwrap_err();
        assert_eq!(c.hdmi_first_error(port).unwrap(), Some(error));
        assert_eq!(Arc::strong_count(&f.owner), 2);
        assert_eq!(c.start(s), Err(Error::NeedsReset));
        f.owner.state.lock().unwrap().enable_late = false;
        {
            let mut h = f.hardware.lock().unwrap();
            h.reject = None;
            h.drop_reply = None;
        }
        c.reinitialize().unwrap();
        assert_eq!(Arc::strong_count(&f.owner), 1);
        assert_eq!(c.hdmi_first_error(port), Err(Error::InvalidHdmiPort));
    }
}

/// The prepared route's transient faults in a row.
fn route_faults(c: &Controller<Regs, Clock>, port: HdmiPortId) -> u8 {
    c.hdmi_ports[port.index]
        .as_ref()
        .unwrap()
        .route
        .as_ref()
        .unwrap()
        .faults
}

/// The codec commands since `before` whose verb is `verb`.
fn count(f: &Fixture, before: usize, verb: u32) -> usize {
    f.hardware.lock().unwrap().commands[before..]
        .iter()
        .filter(|c| (*c >> 8) & 0xfff == verb)
        .count()
}

#[test]
fn a_fifo_error_is_cleared_and_the_stream_plays_on() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    let before = f.hardware.lock().unwrap().commands.len();
    for n in 1..=2 * u64::from(HDMI_TRANSIENT_FAULTS) {
        // audio8a's SDnSTS 0x28 and 0x2c: FIFOE and FIFORDY (and BCIS), no
        // descriptor error.
        f.advance(p, fmt);
        f.hardware.lock().unwrap().bar[0x83] |= 0x28;
        // Not a fault: the period is refilled on the running stream.
        assert!(c.refill_hdmi_period(s, &vec![2; p]).unwrap().is_some());
        {
            let h = f.hardware.lock().unwrap();
            // FIFOE was written 1 to clear; the stream runs, never restarted.
            assert_eq!(h.bar[0x83] & 0x18, 0);
            assert_eq!(h.bar[0x80] & 2, 2);
            assert_eq!((h.starts, h.stops), (1, 0));
        }
        let stats = c.stats();
        assert_eq!(stats.fifo_errors, n);
        // FIFOE and FIFORDY, BCIS as the period completed, no DESE.
        assert_eq!(stats.fifo_status & 0x38, 0x28);
    }
    // No stop, no restart, nothing set, the route never closed.
    assert!(
        f.hardware.lock().unwrap().commands[before..]
            .iter()
            .all(|c| !walks(*c) && !sets(*c))
    );
    assert_eq!(c.hdmi_first_error(port).unwrap(), None);
    assert_eq!(f.owner.state.lock().unwrap().disable, 0);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_stale_status_is_cleared_at_start_and_a_fifo_error_on_the_first_read_is_not_a_fault() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // An underrun stops the stream; its descriptor then holds every stale
    // bit, a descriptor error among them.
    f.clock.0.fetch_add(100_000, Ordering::SeqCst);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::PlaybackUnderrun)
    );
    {
        let mut h = f.hardware.lock().unwrap();
        h.bar[0x83] |= 0x1c;
        // And the new FIFO underruns as it first fills.
        h.fifo_on_start = true;
    }
    // The model refuses to arm over stale status: cleared before RUN.
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Stream));
    {
        let h = f.hardware.lock().unwrap();
        assert_eq!(h.starts, 2);
        // The first status read (inside the start) took the new FIFOE.
        assert_eq!(h.bar[0x83] & 0x18, 0);
    }
    assert_eq!(c.stats().fifo_errors, 1);
    f.hardware.lock().unwrap().fifo_on_start = false;
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    assert_eq!(f.hardware.lock().unwrap().stops, 1);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_stalled_position_stops_the_stream_only() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // A FIFO error, then a DMA that no longer moves: status reads 10 ms
    // apart (each well inside the ring) until the ring's duration passed.
    f.hardware.lock().unwrap().bar[0x83] |= 0x28;
    let mut stalled = None;
    for _ in 0..20 {
        f.clock.0.fetch_add(10_000, Ordering::SeqCst);
        if let Err(error) = c.status(s) {
            stalled = Some(error);
            break;
        }
    }
    assert_eq!(stalled, Some(Error::PlaybackStalled));
    assert_eq!(c.stats().fifo_errors, 1);
    only_the_stream_stopped(&f, &c, port);
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Stream));
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

/// One service turn: the readback scheduled, one period played and
/// refilled.
fn turn(f: &Fixture, c: &mut Controller<Regs, Clock>, port: HdmiPortId, s: StreamId, p: usize) {
    let fmt = c.stream_format(s).unwrap();
    c.schedule_hdmi_readback(port).unwrap();
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![1; p]).unwrap().is_some());
}

#[test]
fn the_audio8a_misread_is_reproduced_and_fixed() {
    for legacy in [true, false] {
        let mut f = Fixture::new(false);
        let (mut c, port, s, p) = playing(&mut f);
        // The transport as at baf62aad: no landing check.
        c.legacy_rirb = legacy;
        // More than a ring lap of responses, so that every slot holds an
        // answer from the lap before (written while the legacy reader ran).
        let start = f.hardware.lock().unwrap().responses;
        while f.hardware.lock().unwrap().responses < start + 300 {
            turn(&f, &mut c, port, s, p);
        }
        // The write pointer covers pin control's answer before its entry
        // lands, as audio8a's controller did under display traffic.
        f.hardware.lock().unwrap().rirb_lag = Some((0xf07, 4));
        for _ in 0..64 {
            turn(&f, &mut c, port, s, p);
            if !f.hardware.lock().unwrap().stale.is_empty() {
                break;
            }
        }
        let (verb, stale, answer) = f.hardware.lock().unwrap().stale[0];
        assert_eq!((verb, answer), (0xf07, 0x40));
        // The slot still held another verb's answer from one lap earlier:
        // the pin sense, the infoframe transmit, the stream tag, a format.
        assert_ne!(stale, 0x40);
        assert_eq!(c.take_rejection(), None);
        let stats = c.stats();
        let misread = c.take_misread();
        if legacy {
            // audio8a: pin control "answered" another verb's value. The
            // landing check was off; the readback's second read saw 0x40
            // and kept the route, but the first answer was the stale one.
            let misread = misread.unwrap();
            assert_eq!((misread.first.verb, misread.first.read), (0xf07, stale));
            assert_eq!(misread.again, 0x40);
            assert_eq!(stats.misreads, 1);
            assert_eq!(stats.rirb_late, 0);
        } else {
            // Fixed: the reader waited for the entry to land; no misread.
            assert_eq!(misread, None);
            assert_eq!(stats.misreads, 0);
            assert_eq!(stats.rirb_late, 1);
            assert_eq!(stats.rirb_lost, 0);
        }
        assert_eq!(f.hardware.lock().unwrap().starts, 1);
        c.close_hdmi(port).unwrap();
        c.detach_hdmi_port(port).unwrap();
        c.shutdown().unwrap();
    }
}

#[test]
fn unsolicited_and_other_codecs_responses_are_never_an_answer() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // Ahead of the turn's first answer (codec 0's pin sense), each time:
    // an unsolicited event (bit 4) carrying the infoframe transmit's
    // value, a response from codec 1 (nothing outstanding) carrying pin
    // control's, and one from address 15 (no codec) carrying a channel
    // count. Taken as answers, each would read as a wrong value or a
    // sink away.
    for _ in 0..4 {
        c.schedule_hdmi_readback(port).unwrap();
        f.advance(p, fmt);
        let before = c.stats();
        f.hardware.lock().unwrap().inject = vec![(0xc0, 0x10), (0x40, 1), (0x1, 15)];
        assert!(c.refill_hdmi_period(s, &vec![2; p]).unwrap().is_some());
        let after = c.stats();
        assert_eq!(after.unsolicited - before.unsolicited, 1);
        assert_eq!(after.spurious - before.spurious, 2);
    }
    let stats = c.stats();
    assert_eq!((stats.misreads, stats.sense_misreads), (0, 0));
    assert_eq!(c.take_rejection(), None);
    assert_eq!(f.hardware.lock().unwrap().starts, 1);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_response_landing_after_its_verb_timed_out_is_skipped() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let start = f.hardware.lock().unwrap().responses;
    while f.hardware.lock().unwrap().responses < start + 300 {
        turn(&f, &mut c, port, s, p);
    }
    // Pin control's entry lands far past the verb timeout (1 ms here).
    f.hardware.lock().unwrap().rirb_lag = Some((0xf07, 100_000));
    c.schedule_hdmi_readback(port).unwrap();
    let error = c.status(s).unwrap_err();
    assert_eq!(error, Error::Timeout(Phase::Verb));
    let stats = c.stats();
    assert_eq!((stats.rirb_late, stats.rirb_lost), (1, 1));
    // The reader moved past the entry, and the timeout took the usual
    // path: the immediate command interface from here on.
    assert_eq!(c.rirb_unlanded, None);
    assert!(c.single_command);
}

#[test]
fn a_readback_misread_once_neither_stops_nor_rejects() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // The model's glitch answers zero, which a mux of 0 would accept.
    for (n, verb) in [0xf07u16, 0xf06, 0xf2d, 0xf32, 0xf0d]
        .into_iter()
        .enumerate()
    {
        f.hardware.lock().unwrap().glitch = Some((verb, 1));
        c.schedule_hdmi_readback(port).unwrap();
        f.advance(p, fmt);
        assert!(c.refill_hdmi_period(s, &vec![2; p]).unwrap().is_some());
        let misread = c.take_misread().unwrap();
        assert_eq!((misread.first.verb, misread.first.read), (verb, 0));
        assert!(misread.first.expected.accepts(misread.again));
        assert!(alloc::format!("{misread}").contains("; read again: 0x"));
        assert_eq!(c.stats().misreads, n as u64 + 1);
    }
    assert_eq!(c.take_rejection(), None);
    let h = f.hardware.lock().unwrap();
    assert_eq!((h.starts, h.stops), (1, 0));
    drop(h);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_single_sense_without_presence_does_not_pause() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // audio8a: a pin sense answered 0x10 (another verb's answer) once.
    for n in 1..=4 {
        f.hardware.lock().unwrap().sense_glitch = 1;
        f.advance(p, fmt);
        assert!(c.refill_hdmi_period(s, &vec![2; p]).unwrap().is_some());
        assert!(!c.hdmi_paused(port).unwrap());
        assert_eq!(c.stats().sense_misreads, n);
        let misread = c.take_misread().unwrap();
        assert_eq!((misread.first.verb, misread.first.read), (0xf09, 0x10));
        assert_eq!(misread.again, 0xc000_0000);
    }
    // Two in a row: the monitor is away; paused at once, as before.
    f.hardware.lock().unwrap().sense_glitch = 2;
    f.advance(p, fmt);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![3; p]),
        Err(Error::SinkUnavailable)
    );
    assert!(c.hdmi_paused(port).unwrap());
    assert_eq!(c.stats().sense_misreads, 4);
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Resumed));
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_descriptor_error_or_an_unacknowledged_stop_needs_a_reset() {
    for unacknowledged in [false, true] {
        let mut f = Fixture::new(false);
        let (mut c, port, s, p) = playing(&mut f);
        let fmt = c.stream_format(s).unwrap();
        f.advance(p, fmt);
        {
            let mut h = f.hardware.lock().unwrap();
            // DESE: a BDL or buffer fetch failed, with FIFOE; or with a
            // stop the DMA never acknowledges.
            h.bar[0x83] |= if unacknowledged { 0x10 } else { 0x18 };
            h.stop_stuck = unacknowledged;
        }
        let error = c.refill_hdmi_period(s, &vec![2; p]).unwrap_err();
        assert!(matches!(error, Error::StreamDma { .. }), "{error:?}");
        assert_eq!(c.restart_hdmi(port), Err(Error::NeedsReset));
        assert_eq!(c.start(s), Err(Error::NeedsReset));
        f.hardware.lock().unwrap().stop_stuck = false;
        c.reinitialize().unwrap();
        assert_eq!(Arc::strong_count(&f.owner), 1);
    }
}

#[test]
fn a_controller_reset_plays_again_on_the_same_dma_buffers() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    let buffers = |c: &Controller<Regs, Clock>, s: StreamId| {
        let stream = c.streams[usize::from(s.descriptor_index())]
            .as_ref()
            .unwrap();
        (stream.buffer.bus_address(), stream._bdl.bus_address())
    };
    let before = buffers(&c, s);
    f.advance(p, fmt);
    f.hardware.lock().unwrap().bar[0x83] |= 0x10;
    assert!(matches!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::StreamDma { .. })
    ));
    assert_eq!(c.restart_hdmi(port), Err(Error::NeedsReset));
    // The owner's recovery: the controller's own reset (CRST), the codecs
    // walked again, the port attached again, a stream allocated, the route
    // prepared. No function-level reset is involved.
    c.reinitialize().unwrap();
    assert_eq!(Arc::strong_count(&f.owner), 1);
    assert_eq!(c.start(s), Err(Error::InvalidStream));
    let discovery = c.discover_codecs().unwrap();
    let graph = discovery
        .codecs
        .iter()
        .find(|graph| graph.address == 0)
        .unwrap()
        .clone();
    // SAFETY: owner and controller share this fixture's locked hardware
    // and the display grant identifies the enumerated codec/pin.
    let port = unsafe { c.attach_hdmi_port(&graph, 4, 0, f.owner.clone()) }.unwrap();
    let (s, _) = f.stream(&mut c, 2, 16);
    // The retired stream's ring and BDL, not new memory.
    assert_eq!(buffers(&c, s), before);
    c.prepare_hdmi(port, s, 2, None).unwrap();
    c.write_pcm(s, 0, &vec![0; p * 4]).unwrap();
    c.start(s).unwrap();
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![4; p]).unwrap().is_some());
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_resume_programs_the_route_again_without_a_graph_walk() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    // The display side's sleep: presence and ELD valid drop, and the pin
    // control and infoframe transmit it shares with the codec are cleared.
    {
        let mut h = f.hardware.lock().unwrap();
        h.sense = 0;
        h.pin = 0;
        h.transmit = 0;
    }
    f.advance(p, fmt);
    assert_eq!(
        c.refill_hdmi_period(s, &vec![2; p]),
        Err(Error::SinkUnavailable)
    );
    assert!(c.hdmi_paused(port).unwrap());
    f.hardware.lock().unwrap().sense = 0xc000_0000;
    let before = f.hardware.lock().unwrap().commands.len();
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Resumed));
    {
        let h = f.hardware.lock().unwrap();
        let resume = &h.commands[before..];
        // No graph walk, connection list or configuration default.
        assert!(
            resume
                .iter()
                .all(|c| !matches!((c >> 8) & 0xfff, 0xf00 | 0xf02 | 0xf1c)),
            "{resume:x?}"
        );
        // Pin control and the infoframe's transmit set again, before the
        // start's readback passed.
        assert!(resume.contains(&((4 << 20) | (0x707 << 8) | 0x40)));
        assert!(resume.contains(&((4 << 20) | (0x732 << 8) | 0xc0)));
        assert_eq!((h.pin, h.transmit), (0x40, 0xc0));
        assert_eq!(h.starts, 2);
    }
    // Same route, same owner: neither closed nor prepared again.
    let owner = f.owner.state.lock().unwrap();
    assert_eq!((owner.enable, owner.disable), (1, 0));
    drop(owner);
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn a_rejected_readback_programs_the_route_before_it_counts() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, _) = playing(&mut f);
    // More rejections in a row than the bound, with no period refilled
    // between them: each is fixed by setting the route's verbs again, so
    // none counts and the route is never given up.
    for _ in 0..2 * HDMI_TRANSIENT_FAULTS {
        // The display side cleared the infoframe's transmit.
        f.hardware.lock().unwrap().transmit = 0;
        c.schedule_hdmi_readback(port).unwrap();
        assert_eq!(c.status(s), Err(Error::RouteRejected));
        let rejection = c.take_rejection().unwrap();
        assert_eq!((rejection.verb, rejection.read), (0xf32, 0));
        only_the_stream_stopped_but_transmit(&f, &c, port);
        assert_eq!(route_faults(&c, port), 0);
        let before = f.hardware.lock().unwrap().commands.len();
        assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Reprogrammed));
        let h = f.hardware.lock().unwrap();
        let restart = &h.commands[before..];
        assert!(restart.iter().all(|c| !walks(*c)), "{restart:x?}");
        assert!(restart.contains(&((4 << 20) | (0x732 << 8) | 0xc0)));
        assert_eq!(h.transmit, 0xc0);
        drop(h);
        assert_eq!(route_faults(&c, port), 0);
    }
    // A stream-only restart (an underrun) sets nothing again.
    f.clock.0.fetch_add(100_000, Ordering::SeqCst);
    assert_eq!(c.status(s), Err(Error::PlaybackUnderrun));
    let before = f.hardware.lock().unwrap().commands.len();
    assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Stream));
    assert!(
        f.hardware.lock().unwrap().commands[before..]
            .iter()
            .all(|c| !sets(*c))
    );
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

/// As [`only_the_stream_stopped`], for a route whose infoframe transmit
/// the display side cleared.
fn only_the_stream_stopped_but_transmit(
    f: &Fixture,
    c: &Controller<Regs, Clock>,
    port: HdmiPortId,
) {
    let h = f.hardware.lock().unwrap();
    assert_eq!(h.bar[0x80] & 2, 0);
    assert_eq!(h.pin, 0x40);
    assert_ne!(h.binding, 0);
    assert_eq!(f.owner.state.lock().unwrap().disable, 0);
    assert!(c.hdmi_prepared(port).unwrap());
}

#[test]
fn the_route_is_read_back_once_a_turn_and_its_pin_sense_at_each_status() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    for turn in 0..4u8 {
        let before = f.hardware.lock().unwrap().commands.len();
        // The turn's start: one whole readback scheduled.
        c.schedule_hdmi_readback(port).unwrap();
        f.advance(2 * p, fmt);
        let mut refilled = 0;
        while c.refill_hdmi_period(s, &vec![turn; p]).unwrap().is_some() {
            refilled += 1;
        }
        assert_eq!(refilled, 2);
        // Two status reads a refilled period and one for the period not
        // yet due: each senses the pin, one reads the whole route back.
        let statuses = 2 * refilled + 1;
        assert_eq!(count(&f, before, 0xf09), statuses);
        for verb in [0xf01, 0xf07, 0xf06, 0xf2d, 0xf32, 0xf0d] {
            assert_eq!(count(&f, before, verb), 1, "{verb:#x}");
        }
        // The format, a long verb (0xa).
        let formats = f.hardware.lock().unwrap().commands[before..]
            .iter()
            .filter(|c| (*c >> 16) & 15 == 0xa)
            .count();
        assert_eq!(formats, 1);
        // Eight verbs of readback a turn, plus one pin sense a status
        // read: no longer eight at every status read.
        assert_eq!(
            f.hardware.lock().unwrap().commands.len() - before,
            statuses + 7
        );
    }
    // No turn scheduled a readback: the pin sense alone.
    let before = f.hardware.lock().unwrap().commands.len();
    f.advance(p, fmt);
    assert!(c.refill_hdmi_period(s, &vec![9; p]).unwrap().is_some());
    assert_eq!(count(&f, before, 0xf09), 2);
    assert_eq!(f.hardware.lock().unwrap().commands.len() - before, 2);
    // A start always reads the whole route back.
    c.stop(s).unwrap();
    let before = f.hardware.lock().unwrap().commands.len();
    c.start(s).unwrap();
    assert_eq!(count(&f, before, 0xf32), 1);
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}
