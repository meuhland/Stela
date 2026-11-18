use super::*;
use std::sync::Mutex;

/// The tests share the one tracker and clock: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// A clock the tests set, at a million ticks a second: one tick a microsecond.
static FAKE: AtomicU64 = AtomicU64::new(0);

fn fake() -> u64 {
    FAKE.load(Ordering::Relaxed)
}

fn at(micros: u64) {
    FAKE.store(micros, Ordering::Relaxed);
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_clock(fake, 1_000_000);
    reset();
    guard
}

/// One key press followed through every stage.
fn press(start: u64) {
    at(start);
    assert!(begin(Kind::Keyboard, start, 4000));
    assert!(delivering(Kind::Keyboard));
    at(start + 100);
    queued(Kind::Keyboard);
    at(start + 600);
    taken(Kind::Keyboard);
    assert_eq!(tag_event(), Some(Kind::Keyboard));
    // Only the event right after the take is the report's.
    assert_eq!(tag_event(), None);
    at(start + 700);
    on_ring(Kind::Keyboard, 1, 0x1040, 0x1020);
    // The guest wrote ERDP before it read the event, then after.
    at(start + 900);
    erdp(1, 0x1040);
    erdp(2, 0x1050);
    at(start + 2700);
    erdp(1, 0x1050);
    at(start + 9000);
    latched();
    at(start + 12700);
    shown();
}

#[test]
fn a_key_press_is_followed_from_its_read_to_the_screen() {
    let _serial = serial();
    press(1_000_000);
    let report = take(Kind::Keyboard);
    assert_eq!(report.samples(), 1);
    let line = report.to_string();
    for part in [
        "INPUT LATENCY keyboard n=1",
        " poll_gap_us=4000/4000/4000",
        " report_to_ring_us=700/700/700",
        " ring_to_ack_us=2000/2000/2000",
        " ack_to_present_us=10000/10000/10000",
        " total_ms=12.7/12.7/12.7",
        " deliver_us=100/100/100",
        " guest_poll_us=500/500/500",
        " ack_to_latch_us=6300/6300/6300",
        " latch_to_flip_us=3700/3700/3700",
        " dropped=0 overflow=0 stuck=read:0,queued:0,taken:0,ring:0,acked:0,latched:0",
    ] {
        assert!(line.contains(part), "{part} in {line}");
    }
    // The window starts again empty.
    assert!(take(Kind::Keyboard).is_empty());
}

#[test]
fn one_input_of_a_kind_is_followed_at_a_time() {
    let _serial = serial();
    at(10);
    assert!(begin(Kind::Pointer, 10, 0));
    assert!(!begin(Kind::Pointer, 20, 0));
    // The other kind is followed alongside.
    assert!(begin(Kind::Keyboard, 20, 0));
    assert!(delivering(Kind::Pointer) && delivering(Kind::Keyboard));
}

#[test]
fn an_input_that_stops_moving_is_given_up_where_it_stopped() {
    let _serial = serial();
    at(0);
    assert!(begin(Kind::Keyboard, 0, 0));
    queued(Kind::Keyboard);
    taken(Kind::Keyboard);
    assert_eq!(tag_event(), Some(Kind::Keyboard));
    on_ring(Kind::Keyboard, 1, 0x100, 0x100);
    erdp(1, 0x110);
    // Nothing new on screen: it stays acked.
    at(STALE_MILLIS * 1000 - 1);
    assert!(!begin(Kind::Keyboard, 0, 0));
    at(STALE_MILLIS * 1000 + 1);
    let report = take(Kind::Keyboard);
    assert_eq!(report.samples(), 0);
    assert!(
        report.to_string().contains("acked:1,latched:0"),
        "{}",
        report
    );
    assert!(begin(Kind::Keyboard, STALE_MILLIS * 1000 + 1, 0));
}

#[test]
fn an_input_without_a_report_is_counted_dropped() {
    let _serial = serial();
    at(0);
    assert!(begin(Kind::Keyboard, 0, 0));
    dropped(Kind::Keyboard);
    assert!(!delivering(Kind::Keyboard));
    assert!(take(Kind::Keyboard).to_string().contains("dropped=1"));
}

#[test]
fn a_frame_latched_before_the_ack_does_not_count() {
    let _serial = serial();
    at(0);
    assert!(begin(Kind::Pointer, 0, 0));
    queued(Kind::Pointer);
    taken(Kind::Pointer);
    assert_eq!(tag_event(), Some(Kind::Pointer));
    on_ring(Kind::Pointer, 1, 0x200, 0x1f0);
    latched();
    shown();
    assert_eq!(take(Kind::Pointer).samples(), 0);
    at(50);
    erdp(1, 0x210);
    at(60);
    shown();
    at(70);
    latched();
    at(80);
    shown();
    let report = take(Kind::Pointer);
    assert_eq!(report.samples(), 1);
    assert!(report.to_string().contains(" ack_to_present_us=30/30/30"));
}

#[test]
fn samples_past_a_full_window_are_counted() {
    let _serial = serial();
    for n in 0..WINDOW as u64 + 3 {
        press(n * 100_000);
    }
    let report = take(Kind::Keyboard);
    assert_eq!(report.samples(), WINDOW);
    assert!(report.to_string().contains("overflow=3"));
}

/// The native peripheral marks the followed input's report behind those
/// queued before it, and the guest's read of that report is its take.
#[test]
fn the_peripheral_times_the_guests_read_of_the_followed_report() {
    use crate::usb::{BusPeer, Reply, Token, hid::Peripheral};
    let _serial = serial();
    let mut device = Peripheral::new();
    let control = |device: &mut Peripheral, setup: [u8; 8]| {
        let mut setup = setup;
        assert_eq!(
            device.packet(0, Token::Setup, &mut setup).unwrap(),
            Reply::Ack(8)
        );
        assert_eq!(device.packet(0, Token::In, &mut []).unwrap(), Reply::Ack(0));
    };
    control(&mut device, [0x00, 5, 1, 0, 0, 0, 0, 0]); // SET_ADDRESS 1
    control(&mut device, [0x00, 9, 1, 0, 0, 0, 0, 0]); // SET_CONFIGURATION 1
    let key = |usage| Input::Key {
        usage,
        pressed: true,
    };
    device.input(key(0x04)).unwrap();
    at(100);
    assert!(begin(Kind::Keyboard, 100, 0));
    at(150);
    device.input(key(0x05)).unwrap();
    assert!(!delivering(Kind::Keyboard));
    let mut report = [0u8; 8];
    at(300);
    // The report queued before it goes first.
    assert_eq!(
        device.packet(0x81, Token::In, &mut report).unwrap(),
        Reply::Ack(8)
    );
    assert_eq!(tag_event(), None);
    at(400);
    assert_eq!(
        device.packet(0x81, Token::In, &mut report).unwrap(),
        Reply::Ack(8)
    );
    assert_eq!(report[2..4], [0x04, 0x05]);
    assert_eq!(tag_event(), Some(Kind::Keyboard));
    // A key already held makes no report.
    at(2_000_000);
    assert!(begin(Kind::Keyboard, 2_000_000, 0), "given up at taken");
    device.input(key(0x05)).unwrap();
    let report = take(Kind::Keyboard).to_string();
    assert!(report.contains("dropped=1"), "{report}");
    assert!(report.contains("taken:1"), "{report}");
}

#[test]
fn the_unread_stretch_runs_from_the_dequeue_pointer_to_the_event() {
    assert!(unread(0x100, 0x140, 0x100));
    assert!(unread(0x100, 0x140, 0x140));
    assert!(!unread(0x100, 0x140, 0x150));
    // Wrapped: from 0x1f0 to the ring's end, then from its start to 0x20.
    assert!(unread(0x1f0, 0x20, 0x1f0));
    assert!(unread(0x1f0, 0x20, 0x10));
    assert!(!unread(0x1f0, 0x20, 0x30));
}

#[test]
fn percentiles_are_taken_from_the_sorted_values() {
    assert_eq!(percentiles(&mut []), [0, 0, 0]);
    assert_eq!(percentiles(&mut [7]), [7, 7, 7]);
    let mut values: Vec<u64> = (1..=10).rev().collect();
    assert_eq!(percentiles(&mut values), [5, 9, 10]);
}

#[test]
fn key_presses_and_pointer_motion_are_followed() {
    let key = |usage, pressed| Input::Key { usage, pressed };
    assert_eq!(Kind::of(&key(0x04, true)), Some(Kind::Keyboard));
    assert_eq!(Kind::of(&key(0x04, false)), None);
    assert_eq!(Kind::of(&key(0xe1, true)), None, "a modifier");
    assert_eq!(Kind::of(&Input::Move { x: 1, y: 2 }), Some(Kind::Pointer));
    assert_eq!(
        Kind::of(&Input::Motion { dx: 1, dy: 0 }),
        Some(Kind::Pointer)
    );
    assert_eq!(Kind::of(&Input::Buttons(1)), None);
}
