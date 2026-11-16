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
    // One wrong pin-control readback at the turn's readback.
    f.hardware.lock().unwrap().glitch = Some((0xf07, 1));
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
        f.hardware.lock().unwrap().glitch = Some((0xf07, 1));
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
        // One wrong pin-control readback is named, once.
        f.hardware.lock().unwrap().glitch = Some((0xf07, 1));
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
fn a_fifo_error_restarts_only_the_stream() {
    let mut f = Fixture::new(false);
    let (mut c, port, s, p) = playing(&mut f);
    let fmt = c.stream_format(s).unwrap();
    for n in 0..2 * HDMI_TRANSIENT_FAULTS {
        // audio7's SDnSTS 0x28: FIFOE and FIFORDY, no descriptor error.
        f.advance(p, fmt);
        f.hardware.lock().unwrap().bar[0x83] |= 0x28;
        let error = c.refill_hdmi_period(s, &vec![2; p]).unwrap_err();
        assert!(
            matches!(error, Error::StreamDma { status } if status & 0x18 == 0x08),
            "{error:?}"
        );
        only_the_stream_stopped(&f, &c, port);
        // The stop acknowledged it: FIFOE, DESE and BCIS are clear.
        assert_eq!(f.hardware.lock().unwrap().bar[0x83] & 0x1c, 0);
        if n == 0 {
            assert_eq!(c.hdmi_first_error(port).unwrap(), Some(error));
        }
        let before = f.hardware.lock().unwrap().commands.len();
        // Never a reset, however often: the stream alone restarts.
        assert_eq!(c.restart_hdmi(port), Ok(HdmiRestart::Stream));
        {
            let h = f.hardware.lock().unwrap();
            let restart = &h.commands[before..];
            assert!(restart.iter().all(|c| !walks(*c) && !sets(*c)));
            assert_eq!(h.starts, 2 + n as usize);
            assert_eq!(h.get(0x84, 4), 0);
        }
        f.advance(p, fmt);
        assert!(c.refill_hdmi_period(s, &vec![3; p]).unwrap().is_some());
    }
    assert_eq!(f.owner.state.lock().unwrap().disable, 0);
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
            if unacknowledged {
                // A FIFO error whose stop the DMA never acknowledges.
                h.bar[0x83] |= 0x08;
                h.stop_stuck = true;
            } else {
                // DESE: a BDL or buffer fetch failed.
                h.bar[0x83] |= 0x10;
            }
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
