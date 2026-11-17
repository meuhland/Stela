// SPDX-License-Identifier: GPL-2.0-only
// Linux mousedev touchpad emulation reference: drivers/input/mousedev.c,
// Copyright (c) 1999-2001 Vojtech Pavlik.
//! Touchpads as a mouse: ELAN and Cypress I²C touchpads, the RMI4 and ELAN
//! SMBus companions of PS/2 touchpads, and the PS/2 Synaptics and Elantech
//! protocols. One finger's travel becomes [`Input::Motion`], two fingers'
//! travel [`Input::MouseWheel`] and [`Input::MouseHWheel`], and the buttons
//! [`Input::MouseButtons`], tap to click included.
//!
//! The guest's mouse states no resolution, so macOS takes its counts as a
//! 400 dpi mouse's and accelerates them. A pad's coordinates are therefore
//! converted through its physical size, as libinput normalizes them: the same
//! finger travel in millimetres moves the pointer the same on every pad, as
//! far as the same hand travel of a mouse would. Sub-count remainders carry
//! into later frames.
use super::{
    Input, cyapa, elan,
    hid_i2c::Interrupt,
    rmi::{self, FunctionReport},
};
use crate::{
    Clock,
    bus::{i2c::Bus, smbus},
};
use alloc::{boxed::Box, string::String, vec::Vec};

/// The guest mouse's counts per inch: what macOS assumes of a mouse whose
/// descriptor states no resolution.
const COUNTS_PER_INCH: i64 = 400;
/// Micrometres per inch.
const UM_PER_INCH: i64 = 25_400;
/// Two-finger travel per wheel detent, in micrometres: about one 15 degree
/// notch at the rim of a 20 mm mouse wheel.
const SCROLL_UM: i64 = 2_500;
/// libinput's tap motion threshold: a touch that travels farther, in
/// micrometres, is no tap.
const TAP_MOTION_UM: i64 = 1_300;
/// libinput's assumed pad size in millimetres when a pad states none.
const ASSUMED_MM: (u32, u32) = (69, 50);
/// mousedev's pad width when a pad states no maximum.
const ASSUMED_UNITS: u32 = 256 * 2;
/// The largest travel between two frames, in pad units, that is scaled; a
/// larger one is a coordinate glitch, and this bound keeps the scaling's
/// products in range.
const MAX_DELTA: i64 = 1_000_000;
/// mousedev's default `tap_time`: a touch shorter than this is a click.
const TAP_US: u64 = 200_000;
/// Frames one poll reads at most.
const POLL_FRAMES: usize = 16;

/// One pad axis's scale: `units` of its coordinates span `mm` millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axis {
    units: u32,
    mm: u32,
}

impl Axis {
    /// An axis of `resolution` units per millimetre, if stated.
    pub fn resolution(resolution: u32) -> Option<Self> {
        (resolution != 0).then_some(Self {
            units: resolution,
            mm: 1,
        })
    }

    /// An axis whose `maximum` units span `mm` millimetres, if both are
    /// stated.
    pub fn extent(maximum: u16, mm: u16) -> Option<Self> {
        (maximum != 0 && mm != 0).then_some(Self {
            units: maximum.into(),
            mm: mm.into(),
        })
    }

    /// `delta` units times `numerator / denominator` per micrometre, with
    /// the remainder of earlier frames, which keeps this frame's.
    fn scale(self, delta: i32, numerator: i64, denominator: i64, remainder: &mut i64) -> i64 {
        let delta = i64::from(delta).clamp(-MAX_DELTA, MAX_DELTA);
        let total = delta * i64::from(self.mm) * 1000 * numerator + *remainder;
        let divisor = i64::from(self.units) * denominator;
        let whole = total / divisor;
        *remainder = total - whole * divisor;
        whole
    }

    /// `delta` units in micrometres, rounded towards zero.
    fn micrometres(self, delta: i32) -> i64 {
        i64::from(delta).clamp(-MAX_DELTA, MAX_DELTA) * i64::from(self.mm) * 1000
            / i64::from(self.units)
    }
}

/// A pad's physical scale on both axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub x: Axis,
    pub y: Axis,
}

impl Geometry {
    /// The scale of a pad whose coordinates run `0..=maximum`: the stated
    /// axes, else libinput's assumed 69 by 50 mm across the maximum.
    pub fn new(maximum: (u16, u16), x: Option<Axis>, y: Option<Axis>) -> Self {
        let assumed = |maximum: u16, mm: u32| Axis {
            units: if maximum == 0 {
                ASSUMED_UNITS
            } else {
                maximum.into()
            },
            mm,
        };
        Self {
            x: x.unwrap_or_else(|| assumed(maximum.0, ASSUMED_MM.0)),
            y: y.unwrap_or_else(|| assumed(maximum.1, ASSUMED_MM.1)),
        }
    }

    /// A pad stating its resolution in units per millimetre.
    pub fn resolution(maximum: (u16, u16), resolution: (u32, u32)) -> Self {
        Self::new(
            maximum,
            Axis::resolution(resolution.0),
            Axis::resolution(resolution.1),
        )
    }

    /// A pad stating its size in millimetres.
    pub fn size(maximum: (u16, u16), mm: (u16, u16)) -> Self {
        Self::new(
            maximum,
            Axis::extent(maximum.0, mm.0),
            Axis::extent(maximum.1, mm.1),
        )
    }
}

/// The fingers on the pad in one frame: how many, and the point that moves:
/// the only finger, or the middle of the first two. Coordinates are in pad
/// units, y top-down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Touch {
    pub x: i32,
    pub y: i32,
    pub fingers: u8,
}

impl Touch {
    /// The touch of `points`, the fingers' positions in contact order, if
    /// any finger is on the pad.
    pub fn of<T: Into<i32>>(points: impl IntoIterator<Item = (T, T)>) -> Option<Self> {
        let mut points = points.into_iter().map(|(x, y)| (x.into(), y.into()));
        let (x, y) = points.next()?;
        let Some((x2, y2)) = points.next() else {
            return Some(Self { x, y, fingers: 1 });
        };
        let fingers = 2 + points.count().min(usize::from(u8::MAX - 2)) as u8;
        Some(Self {
            x: ((i64::from(x) + i64::from(x2)) / 2) as i32,
            y: ((i64::from(y) + i64::from(y2)) / 2) as i32,
            fingers,
        })
    }
}

/// A touchpad's pointer state.
#[derive(Debug)]
pub struct Translator {
    geometry: Geometry,
    /// The last frame's touch, while fingers are on the pad.
    last: Option<Touch>,
    /// Motion and scroll remainders, numerators of their scales.
    motion: (i64, i64),
    scroll: (i64, i64),
    /// When the touch started, the most fingers it had and how far it
    /// travelled, in micrometres.
    touched_us: Option<u64>,
    most: u8,
    travel_um: i64,
    buttons: u8,
}

impl Translator {
    /// A translator for a pad of `geometry`.
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            last: None,
            motion: (0, 0),
            scroll: (0, 0),
            touched_us: None,
            most: 0,
            travel_um: 0,
            buttons: 0,
        }
    }

    /// One frame at `now_us`: the fingers on the pad, if any, and the
    /// buttons held (left, right, middle in bits 0..2).
    pub fn frame(
        &mut self,
        touch: Option<Touch>,
        buttons: u8,
        now_us: u64,
        inputs: &mut Vec<Input>,
    ) {
        match touch {
            Some(touch) => {
                self.touched_us.get_or_insert(now_us);
                self.most = self.most.max(touch.fingers);
                match self.last {
                    // A finger landing or lifting moves the point; travel
                    // counts only between frames of the same fingers.
                    Some(last) if last.fingers == touch.fingers => {
                        // Wrapping, as a pad's accumulated positions wrap.
                        let (dx, dy) = (touch.x.wrapping_sub(last.x), touch.y.wrapping_sub(last.y));
                        self.travel(dx, dy, touch.fingers, inputs);
                    }
                    _ => {
                        self.motion = (0, 0);
                        self.scroll = (0, 0);
                    }
                }
                self.last = Some(touch);
            }
            None => {
                // A short touch that stayed put is a tap: one finger the
                // left button, two the right and three the middle, as
                // libinput's default tap button map.
                if let Some(touched) = self.touched_us.take()
                    && now_us.saturating_sub(touched) < TAP_US
                    && self.travel_um <= TAP_MOTION_UM
                    && self.buttons == 0
                    && buttons == 0
                    && let Some(button) = [1, 2, 4].get(usize::from(self.most).wrapping_sub(1))
                {
                    inputs.push(Input::MouseButtons(*button));
                    inputs.push(Input::MouseButtons(0));
                }
                self.last = None;
                self.most = 0;
                self.travel_um = 0;
                self.motion = (0, 0);
                self.scroll = (0, 0);
            }
        }
        self.buttons(buttons, inputs);
    }

    /// The point moved by `dx`, `dy` pad units with `fingers` on the pad.
    fn travel(&mut self, dx: i32, dy: i32, fingers: u8, inputs: &mut Vec<Input>) {
        let Geometry { x, y } = self.geometry;
        self.travel_um = self
            .travel_um
            .saturating_add(x.micrometres(dx).abs() + y.micrometres(dy).abs());
        match fingers {
            1 => {
                let dx = x.scale(dx, COUNTS_PER_INCH, UM_PER_INCH, &mut self.motion.0);
                let dy = y.scale(dy, COUNTS_PER_INCH, UM_PER_INCH, &mut self.motion.1);
                if (dx, dy) != (0, 0) {
                    let counts = |v: i64| v.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
                    inputs.push(Input::Motion {
                        dx: counts(dx),
                        dy: counts(dy),
                    });
                }
            }
            2 => {
                // Fingers moving away from the user roll the wheel away, and
                // to the right pan right; macOS applies its own scrolling
                // direction to both.
                let pan = x.scale(dx, 1, SCROLL_UM, &mut self.scroll.0);
                let wheel = -y.scale(dy, 1, SCROLL_UM, &mut self.scroll.1);
                let detents = |v: i64| v.clamp(-127, 127) as i8;
                if wheel != 0 {
                    inputs.push(Input::MouseWheel(detents(wheel)));
                }
                if pan != 0 {
                    inputs.push(Input::MouseHWheel(detents(pan)));
                }
            }
            // Three or more fingers are gestures a mouse cannot report.
            _ => {}
        }
    }

    /// The buttons alone changed, as a button-only frame reports them.
    pub fn buttons(&mut self, buttons: u8, inputs: &mut Vec<Input>) {
        if buttons != self.buttons {
            self.buttons = buttons;
            inputs.push(Input::MouseButtons(buttons));
        }
    }

    /// Release the buttons and forget the touch, as a stopped pad's.
    pub fn release(&mut self, inputs: &mut Vec<Input>) {
        self.last = None;
        self.touched_us = None;
        self.most = 0;
        self.travel_um = 0;
        self.motion = (0, 0);
        self.scroll = (0, 0);
        self.buttons(0, inputs);
    }

    /// The buttons held.
    pub fn held(&self) -> u8 {
        self.buttons
    }
}

type BoxedBus = Box<dyn Bus>;
type BoxedInterrupt = Box<dyn Interrupt>;
type BoxedSmbus = Box<dyn smbus::Bus>;

/// An ELAN touchpad over SMBus, its interrupt a Host Notify.
pub type ElanSmbus = elan::smbus::Device<BoxedSmbus, BoxedInterrupt, Clock>;

/// An RMI4 touchpad over SMBus, its attention a Host Notify.
pub type RmiSmbus = rmi::Device<
    rmi::transport::Smbus<BoxedSmbus, Box<dyn rmi::transport::Companion>, Clock>,
    Box<dyn rmi::Attention>,
    Clock,
>;

/// RMI4 2D object types a finger reports: `RMI_2D_OBJECT_FINGER` and
/// `RMI_2D_OBJECT_GLOVE`.
const RMI_FINGERS: [u8; 2] = [1, 6];

/// The touchpad drivers a [`TouchpadInput`] reads.
#[allow(
    clippy::large_enum_variant,
    reason = "InputDevice boxes the whole touchpad input"
)]
pub enum Driver {
    Elan(elan::I2cDevice<BoxedBus, BoxedInterrupt, Clock>),
    /// Cypress Gen3.
    Cypress(cyapa::Device<BoxedBus, BoxedInterrupt, Clock>),
    /// Cypress Gen5 and Gen6.
    CypressPip(cyapa::pip::Device<BoxedBus, BoxedInterrupt, Clock>),
    /// ELAN over SMBus, the companion of an Elantech PS/2 touchpad.
    ElanSmbus(ElanSmbus),
    /// Synaptics RMI4 over SMBus, the InterTouch companion of a Synaptics
    /// PS/2 touchpad.
    Rmi(Box<RmiSmbus>),
}

/// Why reading a touchpad failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Elan(elan::Error),
    Cypress(cyapa::Error),
    CypressPip(cyapa::pip::Error),
    Rmi(rmi::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Elan(error) => error.fmt(f),
            Self::Cypress(error) => error.fmt(f),
            Self::CypressPip(error) => error.fmt(f),
            Self::Rmi(error) => error.fmt(f),
        }
    }
}

/// An initialized touchpad and its mouse emulation.
pub struct TouchpadInput {
    driver: Driver,
    translator: Translator,
    clock: Clock,
    pub name: String,
}

impl TouchpadInput {
    /// Emulate a mouse from `driver`, which is initialized: its queried
    /// maximum and physical size set the motion scale.
    pub fn new(driver: Driver, clock: Clock, name: String) -> Self {
        let geometry = match &driver {
            Driver::Elan(device) => device
                .capabilities()
                .map(|c| Geometry::resolution(c.maximum, c.resolution_per_mm)),
            Driver::Cypress(device) => device
                .capabilities()
                .map(|c| Geometry::size(c.maximum, c.physical_size)),
            Driver::CypressPip(device) => device
                .capabilities()
                .map(|c| Geometry::size(c.maximum, c.physical_size_mm)),
            Driver::ElanSmbus(device) => device
                .capabilities()
                .map(|c| Geometry::resolution(c.maximum, c.resolution_per_mm)),
            Driver::Rmi(device) => Some(rmi_geometry(device)),
        };
        Self {
            driver,
            translator: Translator::new(
                geometry.unwrap_or_else(|| Geometry::new((0, 0), None, None)),
            ),
            clock,
            name,
        }
    }

    /// Append the motion and buttons of the frames the pad has ready.
    pub fn poll(&mut self, inputs: &mut Vec<Input>) -> Result<(), Error> {
        for _ in 0..POLL_FRAMES {
            let now = self.clock.now_us();
            match &mut self.driver {
                Driver::Elan(device) => match device.poll().map_err(Error::Elan)? {
                    None => break,
                    Some(elan::Frame::Absolute(frame)) => {
                        let touch =
                            Touch::of(frame.contacts.iter().flatten().map(|c| (c.x, c.y_top_down)));
                        self.translator.frame(touch, frame.buttons & 7, now, inputs);
                    }
                    Some(elan::Frame::Trackpoint(frame)) => {
                        if let Some((dx, dy)) = frame.motion
                            && (dx, dy) != (0, 0)
                        {
                            inputs.push(Input::Motion { dx, dy });
                        }
                        self.translator.buttons(frame.buttons & 7, inputs);
                    }
                },
                Driver::Cypress(device) => match device.poll().map_err(Error::Cypress)? {
                    None => break,
                    Some(frame) => {
                        let touch = Touch::of(frame.contacts.iter().flatten().map(|c| (c.x, c.y)));
                        self.translator.frame(touch, frame.buttons, now, inputs);
                    }
                },
                Driver::CypressPip(device) => match device.poll().map_err(Error::CypressPip)? {
                    None => break,
                    Some(cyapa::pip::Frame::Touches { touches, .. }) => {
                        let touch = Touch::of(
                            touches
                                .active
                                .iter()
                                .flatten()
                                .filter(|c| c.kind == cyapa::pip::ContactType::Finger)
                                .map(|c| (c.x, c.y)),
                        );
                        let buttons = self.translator.held();
                        self.translator.frame(touch, buttons, now, inputs);
                    }
                    Some(cyapa::pip::Frame::Buttons { buttons, .. }) => {
                        self.translator.buttons(buttons & 7, inputs);
                    }
                    Some(_) => {}
                },
                Driver::ElanSmbus(device) => match device.poll().map_err(Error::Elan)? {
                    None => break,
                    Some(elan::Frame::Absolute(frame)) => {
                        let touch =
                            Touch::of(frame.contacts.iter().flatten().map(|c| (c.x, c.y_top_down)));
                        self.translator.frame(touch, frame.buttons & 7, now, inputs);
                    }
                    Some(elan::Frame::Trackpoint(frame)) => {
                        if let Some((dx, dy)) = frame.motion
                            && (dx, dy) != (0, 0)
                        {
                            inputs.push(Input::Motion { dx, dy });
                        }
                        self.translator.buttons(frame.buttons & 7, inputs);
                    }
                },
                Driver::Rmi(device) => {
                    let poll = device.poll_input();
                    let (_, maximum_y) = rmi_maximum(device);
                    let polled = !poll.reports.is_empty();
                    for report in poll.reports {
                        let buttons = self.translator.held();
                        // The InterTouch platform data flips y, which RMI4
                        // reports bottom-up.
                        let touch = match report {
                            FunctionReport::F11(frame) => Touch::of(
                                frame
                                    .contacts
                                    .iter()
                                    .filter(|c| matches!(c.state, 1 | 2))
                                    .map(|c| (c.x, maximum_y.saturating_sub(c.y))),
                            ),
                            FunctionReport::F12(frame) => Touch::of(
                                frame
                                    .contacts
                                    .iter()
                                    .filter(|c| RMI_FINGERS.contains(&c.object_type))
                                    .map(|c| (c.x, maximum_y.saturating_sub(c.y))),
                            ),
                            FunctionReport::F30(frame) => {
                                self.translator.buttons(rmi_buttons(&frame.buttons), inputs);
                                continue;
                            }
                            FunctionReport::F3A(frame) => {
                                let pressed =
                                    frame.buttons.iter().map(|b| (b.gpio_index, b.pressed));
                                self.translator.buttons(rmi_buttons_of(pressed), inputs);
                                continue;
                            }
                            FunctionReport::F03(_) | FunctionReport::F1A(_) => continue,
                        };
                        self.translator.frame(touch, buttons, now, inputs);
                    }
                    if let Some(error) = poll.error {
                        return Err(Error::Rmi(error));
                    }
                    if !polled {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the pad stopped after a failure and is no longer read.
    pub fn stopped(&self) -> bool {
        match &self.driver {
            Driver::Elan(device) => device.state() == elan::State::Uncertain,
            Driver::Cypress(device) => device.state() == cyapa::State::Uncertain,
            Driver::CypressPip(device) => device.state() == cyapa::pip::State::Uncertain,
            Driver::ElanSmbus(device) => device.state() == elan::State::Uncertain,
            Driver::Rmi(device) => device.control_state() == rmi::ControlState::Uncertain,
        }
    }
}

/// The pad's scale, from its F11 or F12 sensor's maximum and size.
fn rmi_geometry(device: &RmiSmbus) -> Geometry {
    let maximum = rmi_maximum(device);
    let mm = device
        .f11_capabilities()
        .filter_map(|c| c.dimensions_mm)
        .chain(
            device
                .f12_capabilities()
                .map(|c| c.dimensions_mm)
                .filter_map(|(x, y)| Some((x?, y?))),
        )
        .next()
        .unwrap_or((0, 0));
    Geometry::size(maximum, mm)
}

/// The pad's maximum x and y, from its F11 or F12 sensor.
fn rmi_maximum(device: &RmiSmbus) -> (u16, u16) {
    device
        .f11_capabilities()
        .map(|c| c.maximum)
        .chain(device.f12_capabilities().filter_map(|c| c.maximum))
        .next()
        .unwrap_or((0, 0))
}

/// Mouse buttons from F30 GPIO buttons: the first three valid buttons are
/// left, right and middle, as `rmi_f30_map_gpios` maps them from BTN_LEFT.
fn rmi_buttons(buttons: &[rmi::f30::Button]) -> u8 {
    rmi_buttons_of(buttons.iter().map(|b| (b.gpio_index, b.pressed)))
}

/// Mouse buttons from GPIO buttons in their pin order, the first three left,
/// right and middle.
fn rmi_buttons_of(buttons: impl Iterator<Item = (u8, bool)>) -> u8 {
    buttons
        .take(3)
        .enumerate()
        .filter(|(_, (_, pressed))| *pressed)
        .fold(0, |mask, (bit, _)| mask | 1 << bit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 100 by 58 mm pad of 1200 by 700 units, 12 units per millimetre, as
    /// an ELAN pad states its resolution.
    fn small() -> Translator {
        Translator::new(Geometry::resolution((1200, 700), (12, 12)))
    }

    /// A 100 by 60 mm pad of 3000 by 1800 units, as a Cypress or RMI4 pad
    /// states its size.
    fn fine() -> Translator {
        Translator::new(Geometry::size((3000, 1800), (100, 60)))
    }

    fn one(x: i32, y: i32) -> Option<Touch> {
        Some(Touch { x, y, fingers: 1 })
    }

    fn two(x: i32, y: i32) -> Option<Touch> {
        Some(Touch { x, y, fingers: 2 })
    }

    /// The motion of `steps` frames 10 ms apart, each `step` units.
    fn drag(
        pad: &mut Translator,
        touch: fn(i32, i32) -> Option<Touch>,
        step: (i32, i32),
        steps: i32,
    ) -> Vec<Input> {
        let mut inputs = Vec::new();
        for n in 0..=steps {
            pad.frame(
                touch(100 + n * step.0, 600 - n * step.1),
                0,
                n as u64 * 10_000,
                &mut inputs,
            );
        }
        inputs
    }

    fn motion(inputs: &[Input]) -> (i32, i32) {
        inputs.iter().fold((0, 0), |(x, y), input| match input {
            Input::Motion { dx, dy } => (x + i32::from(*dx), y + i32::from(*dy)),
            _ => (x, y),
        })
    }

    fn wheels(inputs: &[Input]) -> (i32, i32) {
        inputs.iter().fold((0, 0), |(w, p), input| match input {
            Input::MouseWheel(d) => (w + i32::from(*d), p),
            Input::MouseHWheel(d) => (w, p + i32::from(*d)),
            _ => (w, p),
        })
    }

    #[test]
    fn the_same_finger_travel_moves_the_pointer_the_same_on_every_pad() {
        // 50 mm right and 20 mm up, in 100 frames: 600 and 240 units on the
        // small pad, 1500 and 600 on the fine one. 50 mm at 400 counts per
        // inch is 787 counts, 20 mm 314.
        let small = motion(&drag(&mut small(), one, (6, 0), 100));
        let fine = motion(&drag(&mut fine(), one, (15, 0), 100));
        assert_eq!(small, (787, 0));
        assert_eq!(small, fine);
        let small = motion(&drag(&mut self::small(), one, (0, 12), 20));
        let fine = motion(&drag(&mut self::fine(), one, (0, 30), 20));
        assert_eq!(small, (0, -314));
        assert_eq!(small, fine);
    }

    #[test]
    fn slow_travel_keeps_its_remainders() {
        // One unit a frame on the fine pad is 0.525 counts: 200 frames, 6.67
        // mm, move 104 counts, none lost to rounding.
        let mut pad = fine();
        let inputs = drag(&mut pad, one, (1, 0), 200);
        assert_eq!(motion(&inputs), (104, 0));
        assert!(
            inputs
                .iter()
                .all(|i| matches!(i, Input::Motion { dx: 1, dy: 0 }))
        );
    }

    #[test]
    fn a_pad_without_a_stated_size_is_assumed_69_mm_wide() {
        let mut pad = Translator::new(Geometry::resolution((1380, 1000), (0, 0)));
        // 690 units is half the assumed 69 mm: 34.5 mm, 543 counts.
        assert_eq!(motion(&drag(&mut pad, one, (69, 0), 10)), (543, 0));
    }

    #[test]
    fn two_fingers_scroll_by_their_travel_on_every_pad() {
        // 25 mm away from the user is ten detents up; 10 mm right four
        // detents right, on both pads.
        for (pad, scale) in [(&mut small(), 12), (&mut fine(), 30)] {
            let inputs = drag(pad, two, (0, scale / 2), 50);
            assert_eq!(wheels(&inputs), (10, 0));
            assert_eq!(motion(&inputs), (0, 0), "no pointer motion");
        }
        for (pad, scale) in [(&mut small(), 12), (&mut fine(), 30)] {
            let inputs = drag(pad, two, (scale / 2, 0), 20);
            assert_eq!(wheels(&inputs), (0, 4));
        }
    }

    #[test]
    fn a_finger_landing_or_lifting_does_not_move_the_pointer() {
        let mut pad = small();
        let mut inputs = Vec::new();
        pad.frame(one(100, 100), 0, 0, &mut inputs);
        // The second finger moves the point to the middle of both.
        pad.frame(two(400, 100), 0, 300_000, &mut inputs);
        pad.frame(one(700, 100), 0, 600_000, &mut inputs);
        assert!(inputs.is_empty(), "{inputs:?}");
    }

    #[test]
    fn short_still_touches_tap_by_their_finger_count() {
        for (touch, button) in [(one as fn(i32, i32) -> Option<Touch>, 1), (two, 2)] {
            let mut pad = small();
            let mut inputs = Vec::new();
            pad.frame(touch(10, 10), 0, 0, &mut inputs);
            pad.frame(touch(11, 10), 0, 50_000, &mut inputs);
            pad.frame(None, 0, 100_000, &mut inputs);
            let tap: Vec<_> = inputs
                .into_iter()
                .filter(|i| matches!(i, Input::MouseButtons(_)))
                .collect();
            assert_eq!(tap, [Input::MouseButtons(button), Input::MouseButtons(0)]);
        }
    }

    #[test]
    fn long_or_moving_touches_do_not_tap() {
        let mut pad = small();
        let mut inputs = Vec::new();
        pad.frame(one(10, 10), 0, 0, &mut inputs);
        pad.frame(None, 0, 300_000, &mut inputs);
        assert!(inputs.is_empty(), "a long touch");
        // A quick 2 mm flick moves the pointer and does not click.
        pad.frame(one(10, 10), 0, 400_000, &mut inputs);
        pad.frame(one(34, 10), 0, 450_000, &mut inputs);
        pad.frame(None, 0, 500_000, &mut inputs);
        assert_eq!(inputs, [Input::Motion { dx: 31, dy: 0 }]);
    }

    #[test]
    fn touches_of_contacts_count_fingers_and_track_the_middle_of_two() {
        assert_eq!(Touch::of::<u16>([]), None);
        assert_eq!(Touch::of([(10u16, 20)]), one(10, 20));
        assert_eq!(Touch::of([(10u16, 20), (30, 40)]), two(20, 30));
        assert_eq!(Touch::of([(i32::MAX, 0), (i32::MAX, 0)]), two(i32::MAX, 0));
        assert_eq!(
            Touch::of([(10u16, 20), (30, 40), (0, 0)]),
            Some(Touch {
                x: 20,
                y: 30,
                fingers: 3
            })
        );
    }

    #[test]
    fn three_fingers_neither_move_nor_scroll() {
        let mut pad = small();
        let mut inputs = Vec::new();
        for n in 0..5 {
            let touch = Some(Touch {
                x: n * 100,
                y: n * 100,
                fingers: 3,
            });
            pad.frame(touch, 0, 300_000 * n as u64, &mut inputs);
        }
        assert!(inputs.is_empty());
    }

    #[test]
    fn rmi_gpio_buttons_are_left_right_and_middle_in_pin_order() {
        let pressed = |pins: &[(u8, bool)]| rmi_buttons_of(pins.iter().copied());
        assert_eq!(pressed(&[(0, true)]), 1);
        assert_eq!(pressed(&[(2, false), (5, true)]), 2);
        assert_eq!(pressed(&[(0, true), (1, true), (2, true), (3, true)]), 7);
    }

    #[test]
    fn button_changes_report_once_each() {
        let mut pad = small();
        let mut inputs = Vec::new();
        pad.frame(None, 1, 0, &mut inputs);
        pad.frame(None, 1, 1, &mut inputs);
        pad.buttons(0, &mut inputs);
        assert_eq!(inputs, [Input::MouseButtons(1), Input::MouseButtons(0)]);
    }

    #[test]
    fn a_lifted_finger_starts_motion_afresh() {
        let mut pad = small();
        let mut inputs = Vec::new();
        pad.frame(one(0, 0), 0, 0, &mut inputs);
        pad.frame(one(64, 0), 0, 300_000, &mut inputs);
        inputs.clear();
        pad.frame(None, 0, 600_000, &mut inputs);
        pad.frame(one(900, 0), 0, 900_000, &mut inputs);
        pad.frame(one(900, 0), 0, 1_200_000, &mut inputs);
        assert!(inputs.is_empty(), "no jump from the old position");
    }
}
