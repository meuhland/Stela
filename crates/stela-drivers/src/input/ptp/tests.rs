use super::*;
use crate::usb::hid::{Translated, Translator};
use alloc::vec;

/// A precision touchpad of two finger collections, 1000 x 600 units over
/// 100 x 60 mm, with a contact count, a scan time and one button, its
/// contact maximum in feature report 2, Input Mode in feature report 3 and
/// the surface and button switches in feature report 4.
fn finger() -> Vec<u8> {
    vec![
        0x09, 0x22, 0xa1, 0x02, // Finger
        0x15, 0x00, 0x25, 0x01, 0x09, 0x47, 0x09, 0x42, 0x95, 0x02, 0x75, 0x01, 0x81,
        0x02, // Confidence, Tip Switch
        0x95, 0x06, 0x81, 0x03, // padding
        0x25, 0x05, 0x09, 0x51, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, // Contact ID
        0x05, 0x01, 0x15, 0x00, 0x26, 0xe8, 0x03, 0x35, 0x00, 0x46, 0xe8, 0x03, 0x65, 0x11, 0x55,
        0x0e, 0x75, 0x10, 0x95, 0x01, 0x09, 0x30, 0x81, 0x02, // X
        0x26, 0x58, 0x02, 0x46, 0x58, 0x02, 0x09, 0x31, 0x81, 0x02, // Y
        0x65, 0x00, 0x55, 0x00, 0x35, 0x00, 0x45, 0x00, 0x05, 0x0d, 0xc0,
    ]
}

fn descriptor() -> Vec<u8> {
    let mut d = vec![0x05, 0x0d, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01];
    d.extend(finger());
    d.extend(finger());
    d.extend([
        0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x09, 0x56, 0x81,
        0x02, // Scan Time
        0x25, 0x7f, 0x75, 0x08, 0x09, 0x54, 0x81, 0x02, // Contact Count
        0x05, 0x09, 0x09, 0x01, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, // Button 1
        0x95, 0x07, 0x81, 0x03, // padding
        0x05, 0x0d, 0x85, 0x02, 0x09, 0x55, 0x25, 0x05, 0x75, 0x08, 0x95, 0x01, 0xb1,
        0x02, // Contact Count Maximum
        0xc0, // end Touch Pad
        0x05, 0x0d, 0x09, 0x0e, 0xa1, 0x01, 0x85, 0x03, // Device Configuration
        0x09, 0x22, 0xa1, 0x02, 0x09, 0x52, 0x15, 0x00, 0x25, 0x0a, 0x75, 0x08, 0x95, 0x01, 0xb1,
        0x02, 0xc0, // Input Mode
        0x09, 0x22, 0xa1, 0x00, 0x85, 0x04, 0x09, 0x57, 0x09, 0x58, 0x75, 0x01, 0x95, 0x02, 0x25,
        0x01, 0xb1, 0x02, 0x95, 0x06, 0xb1, 0x03, 0xc0, // switches
        0xc0,
    ]);
    d
}

/// One finger slot: (confidence, tip, id, x, y).
type Slot = (bool, bool, u8, u16, u16);

fn report(slots: [Slot; 2], scan: u16, count: u8, button: bool) -> Vec<u8> {
    let mut r = vec![0x01];
    for (confidence, tip, id, x, y) in slots {
        r.push(u8::from(confidence) | u8::from(tip) << 1);
        r.push(id);
        r.extend(x.to_le_bytes());
        r.extend(y.to_le_bytes());
    }
    r.extend(scan.to_le_bytes());
    r.push(count);
    r.push(u8::from(button));
    r
}

const EMPTY: Slot = (false, false, 0, 0, 0);

struct Pad {
    translator: Translator,
    ptp: Ptp,
}

impl Pad {
    fn new() -> Self {
        let translator = Translator::describe(&descriptor()).unwrap();
        let profile = Profile::select(translator.descriptor().unwrap()).unwrap();
        Self {
            translator,
            ptp: Ptp::new(profile),
        }
    }

    fn feed(&mut self, bytes: &[u8], now_us: u64) -> Vec<Input> {
        let mut events = Vec::new();
        let mut inputs = Vec::new();
        let Translated::Data(frame) = self.translator.translate(bytes, &mut events, &mut inputs)
        else {
            panic!("report rejected");
        };
        assert!(inputs.is_empty(), "the shared translator reads no pointer");
        self.ptp.report(
            self.translator.descriptor().unwrap(),
            &frame,
            &events,
            now_us,
            &mut inputs,
        );
        inputs
    }
}

#[test]
fn a_precision_touchpad_is_selected_with_its_size_and_mode_reports() {
    let translator = Translator::describe(&descriptor()).unwrap();
    let profile = Profile::select(translator.descriptor().unwrap()).unwrap();
    assert_eq!(
        profile.geometry(),
        Geometry::size((1000, 600), (100, 60)),
        "10 units per mm on both axes"
    );
    assert_eq!(
        profile.mode_reports(),
        vec![(3, vec![3]), (4, vec![0b11])],
        "Input Mode 3, then the surface and button switches on"
    );
}

#[test]
fn a_descriptor_without_an_input_mode_is_no_precision_touchpad() {
    let mut d = descriptor();
    // Input Mode becomes a vendor usage.
    let at = d.windows(2).position(|w| w == [0x09, 0x52]).unwrap();
    d[at + 1] = 0x99;
    let translator = Translator::describe(&d).unwrap();
    assert_eq!(Profile::select(translator.descriptor().unwrap()), None);
}

#[test]
fn one_finger_moves_two_fingers_scroll_and_a_lift_ends_the_touch() {
    let mut pad = Pad::new();
    // A finger lands, then moves 25 mm right: 250 units, 393.7 counts at
    // 400 counts per inch.
    let first = report([(true, true, 1, 100, 300), EMPTY], 10, 1, false);
    assert!(pad.feed(&first, 0).is_empty());
    let moved = report([(true, true, 1, 350, 300), EMPTY], 20, 1, false);
    let inputs = pad.feed(&moved, 300_000);
    assert_eq!(inputs, vec![Input::Motion { dx: 393, dy: 0 }]);
    // A second finger lands: no jump; both move 10 mm up: 4 detents away.
    let two = report(
        [(true, true, 1, 350, 300), (true, true, 2, 450, 300)],
        30,
        2,
        false,
    );
    assert!(pad.feed(&two, 310_000).is_empty());
    let up = report(
        [(true, true, 1, 350, 200), (true, true, 2, 450, 200)],
        40,
        2,
        false,
    );
    assert_eq!(pad.feed(&up, 320_000), vec![Input::MouseWheel(4)]);
    // Both lift, reported once with their tips up.
    let lift = report(
        [(true, false, 1, 350, 200), (true, false, 2, 450, 200)],
        50,
        2,
        false,
    );
    assert!(pad.feed(&lift, 330_000).is_empty());
}

#[test]
fn a_short_still_touch_taps_and_a_palm_is_not_a_finger() {
    let mut pad = Pad::new();
    let down = report([(true, true, 1, 500, 300), EMPTY], 1, 1, false);
    assert!(pad.feed(&down, 0).is_empty());
    let up = report([(true, false, 1, 500, 300), EMPTY], 2, 1, false);
    assert_eq!(
        pad.feed(&up, 50_000),
        vec![Input::MouseButtons(1), Input::MouseButtons(0)]
    );
    // A contact the pad marks without confidence moves nothing.
    let palm = report([(false, true, 3, 100, 100), EMPTY], 3, 1, false);
    assert!(pad.feed(&palm, 100_000).is_empty());
    let palm = report([(false, true, 3, 900, 500), EMPTY], 4, 1, false);
    assert!(pad.feed(&palm, 110_000).is_empty());
}

#[test]
fn hybrid_frames_wait_for_their_continuation_and_the_button_is_read() {
    let mut pad = Pad::new();
    let first = report(
        [(true, true, 1, 100, 100), (true, true, 2, 200, 100)],
        7,
        2,
        false,
    );
    assert!(pad.feed(&first, 0).is_empty());
    // Three fingers over two reports: the frame completes on the second.
    let start = report(
        [(true, true, 1, 100, 100), (true, true, 2, 200, 100)],
        8,
        3,
        true,
    );
    assert!(pad.feed(&start, 10_000).is_empty(), "frame incomplete");
    let rest = report([(true, true, 3, 300, 100), EMPTY], 8, 0, true);
    assert_eq!(pad.feed(&rest, 10_000), vec![Input::MouseButtons(1)]);
    // A report of the button alone, with no finger on the pad.
    let released = report([EMPTY, EMPTY], 9, 0, false);
    assert_eq!(pad.feed(&released, 20_000), vec![Input::MouseButtons(0)]);
}
