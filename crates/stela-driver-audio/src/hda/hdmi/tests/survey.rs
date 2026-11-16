// SPDX-License-Identifier: GPL-2.0-only
//! The owner's survey of an HDMI codec's pins before any route.
use super::*;

#[test]
fn survey_reads_a_legacy_pins_sense_and_eld() {
    let mut f = Fixture::new(false);
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let entries = c.survey_hdmi(&graph).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!((entry.pin, entry.device, entry.devices), (4, 0, 1));
    assert!(entry.present() && entry.eld_valid());
    let read = entry.eld.clone().unwrap();
    assert_eq!((read.reads, read.first_failure), (1, None));
    assert_eq!(read.raw, f.hardware.lock().unwrap().eld.to_vec());
    let eld = read.eld.unwrap();
    assert_eq!(eld.connection, Connection::Hdmi);
    assert_eq!(eld.speakers, 0x4f);
    assert_eq!(eld.port_id, 0x12345678);
    assert_eq!(eld.descriptors.len(), 1);
    assert_eq!(eld.descriptors[0].channels, 8);
    // A legacy codec has no device entries to select.
    assert!(
        !f.hardware
            .lock()
            .unwrap()
            .commands
            .iter()
            .any(|c| (c >> 8) & 0xfff == 0x735)
    );
    // An unplugged sink: no presence, no ELD read.
    f.hardware.lock().unwrap().sense = 0;
    let entries = c.survey_hdmi(&graph).unwrap();
    assert!(!entries[0].present() && !entries[0].eld_valid());
    assert_eq!(entries[0].eld, None);
    // Present without a valid ELD: the GPU has not written one.
    f.hardware.lock().unwrap().sense = 0x8000_0000;
    let entries = c.survey_hdmi(&graph).unwrap();
    assert!(entries[0].present() && !entries[0].eld_valid());
    assert_eq!(entries[0].eld, None);
    c.shutdown().unwrap();
}

#[test]
fn survey_lists_every_mst_device_entry_and_reads_the_eld_where_the_gpu_wrote_it() {
    let mut f = Fixture::new(false);
    {
        let mut h = f.hardware.lock().unwrap();
        h.vendor = 0x10de00a8;
        h.device_list = 3;
        h.device = 2;
        h.other_entries_empty = true;
        h.pin_caps |= 2;
    }
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let entries = c.survey_hdmi(&graph).unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.pin, e.device, e.devices))
            .collect::<Vec<_>>(),
        [(4, 0, 4), (4, 1, 4), (4, 2, 4), (4, 3, 4)]
    );
    let with_eld: Vec<u8> = entries
        .iter()
        .filter(|e| e.eld.is_some())
        .map(|e| e.device)
        .collect();
    assert_eq!(with_eld, [2]);
    c.shutdown().unwrap();
    let h = f.hardware.lock().unwrap();
    // Each entry was selected in turn, then the pin left on entry 0.
    let selects: Vec<u32> = h
        .commands
        .iter()
        .filter(|c| (*c >> 8) & 0xfff == 0x735)
        .map(|c| c & 0xff)
        .collect();
    assert_eq!(selects, [0, 1, 2, 3, 0]);
    assert_eq!(h.selected, 0);
    // The pin requires a trigger before each sense.
    let triggers = h
        .commands
        .iter()
        .filter(|c| **c == (4 << 20) | (0x709 << 8))
        .count();
    assert_eq!(triggers, 4);
}

/// The ELD reads (get ELD size) the hardware has seen.
fn eld_reads(f: &Fixture) -> usize {
    f.hardware
        .lock()
        .unwrap()
        .commands
        .iter()
        .filter(|c| **c == (4 << 20) | (0xf2e << 8) | 8)
        .count()
}

#[test]
fn survey_names_a_failed_eld_check_and_repolls_like_linux() {
    // Byte 12 never valid: six reads 300 ms apart, the bytes before it kept.
    let mut f = Fixture::new(false);
    f.hardware.lock().unwrap().bad_eld = Some(12);
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let start = f.clock.0.load(Ordering::SeqCst);
    let entries = c.survey_hdmi(&graph).unwrap();
    assert!(f.clock.0.load(Ordering::SeqCst) - start >= 5 * ELD_REPOLL_US);
    assert_eq!(eld_reads(&f), usize::from(ELD_READS));
    let fault = Error::EldRead(EldReadFault::ByteNotValid {
        index: 12,
        response: 0,
    });
    let read = entries[0].eld.clone().unwrap();
    let before = f.hardware.lock().unwrap().eld[..12].to_vec();
    assert_eq!(read.raw, before);
    assert_eq!(read.eld, Err(fault));
    assert_eq!(read.reads, ELD_READS);
    assert_eq!(read.first_failure, Some((fault, before)));
    assert!(!read.malformed());
    // Still the playback target: basic audio, never a wider format.
    assert_eq!(
        choose_hdmi_target(entries.iter().map(|e| (0, e)), BASIC_AUDIO)
            .map(|(codec, e, audio)| (codec, e.pin, e.device, audio)),
        Some((0, 4, 0, SinkAudio::Basic(fault)))
    );
    let wide = PcmFormat {
        bits: 24,
        ..BASIC_AUDIO
    };
    assert_eq!(entries[0].takes(wide), None);
    c.shutdown().unwrap();

    // Valid on the third read: the first failure and the ELD.
    let mut f = Fixture::new(false);
    {
        let mut h = f.hardware.lock().unwrap();
        h.bad_eld = Some(12);
        h.bad_eld_reads = 2;
    }
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let entries = c.survey_hdmi(&graph).unwrap();
    let read = entries[0].eld.clone().unwrap();
    assert_eq!(read.reads, 3);
    assert_eq!(read.raw, f.hardware.lock().unwrap().eld.to_vec());
    assert!(read.eld.is_ok());
    assert_eq!(
        read.first_failure.map(|(e, raw)| (e, raw.len())),
        Some((fault, 12))
    );
    assert_eq!(entries[0].takes(BASIC_AUDIO), Some(SinkAudio::Eld));
    c.shutdown().unwrap();

    // A size out of range: nothing read; malformed bytes: all read.
    for malformed in [false, true] {
        let mut f = Fixture::new(false);
        let expected = if malformed {
            f.hardware.lock().unwrap().eld[4] = 31;
            Error::EldParse(EldParseFault::MonitorNameLength { length: 31 })
        } else {
            f.hardware.lock().unwrap().eld_size = 19;
            Error::EldRead(EldReadFault::Size { response: 19 })
        };
        let mut c = f.controller();
        let graph = c.enumerate_codec(0).unwrap();
        let entries = c.survey_hdmi(&graph).unwrap();
        let read = entries[0].eld.clone().unwrap();
        assert_eq!((read.eld.clone(), read.reads), (Err(expected), ELD_READS));
        assert_eq!(read.malformed(), malformed);
        assert_eq!(read.raw.len(), if malformed { 32 } else { 0 });
        assert_eq!(
            entries[0].takes(BASIC_AUDIO),
            Some(SinkAudio::Basic(expected))
        );
        c.shutdown().unwrap();
    }
}

/// What the GA106's codec (10de009f) answered on pin 0x04 device 0 for a
/// Samsung Odyssey G40B in boot audio6: BLL (byte 2) 8, so a 36-byte ELD,
/// in a buffer the codec says is larger, whose byte 37 has no ELD-valid
/// bit. The fixture answers these 37 bytes valid and every later one not.
const ODYSSEY_G40B: [u8; 37] = [
    0x10, 0x00, 0x08, 0x00, 0x6c, 0x12, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x4c, 0x2d, 0x7a, 0x72, 0x4f, 0x64, 0x79, 0x73, 0x73, 0x65, 0x79, 0x20, 0x47, 0x34, 0x30, 0x42,
    0x09, 0x07, 0x07, 0x00, 0x00,
];

/// The highest ELD byte index read (get ELD data), if any.
fn highest_eld_byte(f: &Fixture) -> Option<u32> {
    f.hardware
        .lock()
        .unwrap()
        .commands
        .iter()
        .filter(|c| (*c >> 8) & 0xfff == 0xf2f)
        .map(|c| c & 0xff)
        .max()
}

#[test]
fn the_eld_is_read_to_its_own_length_not_the_codecs_buffer() {
    let mut f = Fixture::new(false);
    {
        let mut h = f.hardware.lock().unwrap();
        h.vendor = 0x10de009f;
        h.device_list = 3;
        h.device = 0;
        h.other_entries_empty = true;
        h.eld = ODYSSEY_G40B.to_vec();
        h.eld_size = 64;
    }
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let entries = c.survey_hdmi(&graph).unwrap();
    assert_eq!(entries.len(), 4);
    let entry = &entries[0];
    assert_eq!((entry.pin, entry.device, entry.devices), (4, 0, 4));
    let read = entry.eld.clone().unwrap();
    // One read, valid: the header, then 4 + 4 * 8 = 36 bytes, no repoll.
    assert_eq!((read.reads, read.first_failure.clone()), (1, None));
    assert_eq!(read.buffer, Some(64));
    assert_eq!(read.raw, &ODYSSEY_G40B[..36]);
    assert_eq!(highest_eld_byte(&f), Some(35));
    let eld = read.eld.unwrap();
    assert_eq!(eld.version, 2);
    assert_eq!(eld.connection, Connection::Hdmi);
    assert_eq!(eld.monitor_name, b"Odyssey G40B");
    assert_eq!((eld.manufacturer, eld.product), (0x2d4c, 0x727a));
    // One LPCM SAD: 2 channels, 32/44.1/48 kHz, 16/20/24-bit.
    assert_eq!(
        eld.descriptors,
        [AudioDescriptor {
            coding_type: 1,
            channels: 2,
            rates: 0b111,
            sample_bits: 0b111,
        }]
    );
    // Speakers FL/FR only.
    assert_eq!(eld.speakers, 1);
    assert_eq!(
        ChannelMap::choose(2, eld.speakers, None)
            .unwrap()
            .speakers(),
        [Speaker::Fl, Speaker::Fr]
    );
    for rate in [32000, 44100, 48000] {
        for bits in [16, 20, 24] {
            assert!(eld.supports(PcmFormat {
                rate,
                bits,
                channels: 2
            }));
        }
    }
    assert!(!eld.supports(PcmFormat {
        rate: 96000,
        bits: 24,
        channels: 2
    }));
    assert!(!eld.supports(PcmFormat {
        rate: 48000,
        bits: 16,
        channels: 6
    }));
    assert_eq!(entry.takes(BASIC_AUDIO), Some(SinkAudio::Eld));
    // The route takes the parsed ELD: no basic-audio fallback.
    let port = f.attach(&mut c);
    let (s, _) = f.stream(&mut c, 2, 16);
    let map = c.prepare_hdmi(port, s, 2, None).unwrap();
    assert_eq!(map.speakers(), [Speaker::Fl, Speaker::Fr]);
    let sink = c.hdmi_sink(port).unwrap().unwrap();
    assert_eq!(sink.eld_error, None);
    assert_eq!(sink.eld.unwrap().monitor_name, b"Odyssey G40B");
    assert_eq!(highest_eld_byte(&f), Some(35));
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}

#[test]
fn an_eld_longer_than_the_codecs_buffer_is_named() {
    let mut f = Fixture::new(false);
    // BLL 8: 36 bytes, in a 32-byte buffer.
    f.hardware.lock().unwrap().eld[2] = 8;
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let entries = c.survey_hdmi(&graph).unwrap();
    let read = entries[0].eld.clone().unwrap();
    let fault = Error::EldParse(EldParseFault::BaselineBeyondBuffer {
        baseline_dwords: 8,
        buffer: 32,
    });
    assert_eq!((read.eld.clone(), read.reads), (Err(fault), ELD_READS));
    assert_eq!(read.buffer, Some(32));
    // Only the header was read.
    assert_eq!(read.raw, &[0x10u8, 0, 8, 0]);
    assert_eq!(highest_eld_byte(&f), Some(3));
    assert_eq!(entries[0].takes(BASIC_AUDIO), Some(SinkAudio::Basic(fault)));
    c.shutdown().unwrap();
}

#[test]
fn the_target_prefers_an_eld_that_takes_the_format() {
    let mut f = Fixture::new(false);
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    let good = c.survey_hdmi(&graph).unwrap().remove(0);
    f.hardware.lock().unwrap().bad_eld = Some(3);
    let bad = c.survey_hdmi(&graph).unwrap().remove(0);
    f.hardware.lock().unwrap().sense = 0x8000_0000;
    let no_eld = c.survey_hdmi(&graph).unwrap().remove(0);
    c.shutdown().unwrap();
    assert_eq!(no_eld.takes(BASIC_AUDIO), None);
    let target = |entries: &[(u8, &HdmiEntry)]| {
        choose_hdmi_target(entries.iter().copied(), BASIC_AUDIO)
            .map(|(codec, _, audio)| (codec, audio == SinkAudio::Eld))
    };
    assert_eq!(
        target(&[(1, &no_eld), (2, &bad), (3, &good)]),
        Some((3, true))
    );
    assert_eq!(target(&[(1, &no_eld), (2, &bad)]), Some((2, false)));
    assert_eq!(target(&[(1, &no_eld)]), None);
}

#[test]
fn survey_refuses_another_codec_and_a_running_stream() {
    let mut f = Fixture::new(false);
    f.hardware.lock().unwrap().vendor = 0x10de0017;
    let mut c = f.controller();
    let graph = c.enumerate_codec(0).unwrap();
    assert_eq!(c.survey_hdmi(&graph), Err(Error::UnsupportedTopology));
    c.shutdown().unwrap();

    let mut f = Fixture::new(false);
    let mut c = f.controller();
    let port = f.attach(&mut c);
    let (stream, period) = f.stream(&mut c, 2, 16);
    c.prepare_hdmi(port, stream, 2, None).unwrap();
    for n in 0..4 {
        c.write_pcm(stream, n * period, &vec![0; period]).unwrap();
    }
    c.start(stream).unwrap();
    let graph = c.enumerate_codec(0).unwrap();
    assert_eq!(c.survey_hdmi(&graph), Err(Error::StreamRunning));
    c.close_hdmi(port).unwrap();
    c.detach_hdmi_port(port).unwrap();
    c.shutdown().unwrap();
}
