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
    let (raw, eld) = entry.eld.clone().unwrap().unwrap();
    assert_eq!(raw, f.hardware.lock().unwrap().eld.to_vec());
    let eld = eld.unwrap();
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
