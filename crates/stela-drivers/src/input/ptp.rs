// SPDX-License-Identifier: GPL-2.0-only
// Linux hid-multitouch.c reference: Copyright (c) 2010-2012 Stephane
// Chatty, Benjamin Tissoires and Ecole Nationale de l'Aviation Civile.
// See NOTICE.
//! Precision touchpads behind HID: a touchpad application (Digitizers 05h)
//! that lists its fingers' contacts, with an Input Mode feature in a device
//! configuration application. Such a pad starts in its firmware's mouse
//! mode; set to touchpad mode as Linux `mt_set_modes` sets it, it reports
//! each finger, and [`Translator`] turns finger travel into motion and two
//! fingers into scrolling at the pad's physical size, as the other
//! touchpads here.
//!
//! A frame is one report, or a first report whose contact count says how
//! many contacts follow and continuation reports with a count of 0, as
//! `mt_touch_report` assembles hybrid frames. The pad lists every contact
//! of a frame, so a contact left out of a frame is lifted.
use super::{
    Input,
    touchpad::{Axis, Geometry, Touch, Translator},
};
use crate::usb::hid::{
    Descriptor, Frame, ReportType,
    descriptor::{ReportField, Value},
    events::{Contact, Event},
};
use alloc::vec::Vec;

const TOUCH_PAD: u32 = 0x000d_0005;
const CONFIGURATION: u32 = 0x000d_000e;
const FINGER: u32 = 0x000d_0022;
const INPUT_MODE: u32 = 0x000d_0052;
const SURFACE_SWITCH: u32 = 0x000d_0057;
const BUTTON_SWITCH: u32 = 0x000d_0058;
const X: u32 = 0x0001_0030;
const Y: u32 = 0x0001_0031;
const BUTTON_1: u32 = 0x0009_0001;
/// hid-multitouch's `MT_INPUTMODE_TOUCHPAD`.
const MODE_TOUCHPAD: i64 = 3;
/// Contacts one frame holds at most, as hid-multitouch's slot limit.
const MAX_CONTACTS: usize = 250;

/// The application collection `collection` belongs to.
fn application(descriptor: &Descriptor, mut collection: Option<usize>) -> Option<usize> {
    while let Some(index) = collection {
        let c = descriptor.collections.get(index)?;
        if c.kind == 1 {
            return Some(index);
        }
        collection = c.parent;
    }
    None
}

/// Whether `collection` lies in a Finger logical collection.
fn in_finger(descriptor: &Descriptor, mut collection: Option<usize>) -> bool {
    while let Some(index) = collection {
        let Some(c) = descriptor.collections.get(index) else {
            return false;
        };
        if c.usage == FINGER {
            return true;
        }
        if c.kind == 1 {
            return false;
        }
        collection = c.parent;
    }
    false
}

/// The millimetres a field's physical range spans, from its unit: SI
/// linear (centimetres) or English linear (inches), length to the first
/// power, scaled by its unit exponent.
fn millimetres(field: &ReportField) -> Option<u16> {
    let extent = i128::from(field.physical_maximum) - i128::from(field.physical_minimum);
    if extent <= 0 {
        return None;
    }
    // Micrometres per unit of the field's system.
    let per_unit: i128 = match field.unit & 0xff {
        0x11 => 10_000,
        0x13 => 25_400,
        _ => return None,
    };
    let exponent = i32::from(field.unit_exponent);
    let mut um = extent * per_unit;
    if exponent >= 0 {
        um = um.checked_mul(10i128.checked_pow(exponent as u32)?)?;
    } else {
        um /= 10i128.checked_pow(exponent.unsigned_abs())?;
    }
    u16::try_from((um + 500) / 1000).ok().filter(|mm| *mm != 0)
}

/// One axis's scale from its field: its logical range over its physical
/// size, when both are stated.
fn axis(field: &ReportField) -> (u16, Option<Axis>) {
    let range = u16::try_from(field.maximum.saturating_sub(field.minimum).max(0)).unwrap_or(0);
    (
        range,
        millimetres(field).and_then(|mm| Axis::extent(range, mm)),
    )
}

/// One feature report that selects touchpad mode: its ID, its bytes after
/// the ID, and its fields' (bit offset, size, value) to set; every other
/// bit is 0.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mode {
    id: u8,
    bytes: usize,
    values: Vec<(usize, usize, i64)>,
}

/// What a descriptor describes of a precision touchpad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    application: usize,
    geometry: Geometry,
    modes: Vec<Mode>,
}

impl Profile {
    /// The touchpad the descriptor describes, if it is a precision
    /// touchpad: a touchpad application with fingers' X and Y, and an Input
    /// Mode feature to select it with.
    pub fn select(descriptor: &Descriptor) -> Option<Self> {
        let is = |collection: Option<usize>, usage: u32| {
            application(descriptor, collection)
                .is_some_and(|app| descriptor.collections[app].usage == usage)
        };
        let field = |usage: u32| {
            descriptor
                .reports
                .iter()
                .filter(|report| report.kind == ReportType::Input)
                .flat_map(|report| report.fields.iter())
                .find(|field| {
                    field.usages.contains(&usage)
                        && is(field.collection, TOUCH_PAD)
                        && in_finger(descriptor, field.collection)
                })
        };
        let (x, y) = (field(X)?, field(Y)?);
        let application = application(descriptor, x.collection)?;
        let (x_range, x_axis) = axis(x);
        let (y_range, y_axis) = axis(y);
        let mut modes = Vec::new();
        for report in descriptor
            .reports
            .iter()
            .filter(|report| report.kind == ReportType::Feature)
        {
            let mut values = Vec::new();
            for field in &report.fields {
                if !is(field.collection, CONFIGURATION) && !is(field.collection, TOUCH_PAD) {
                    continue;
                }
                for (element, usage) in field.usages.iter().enumerate().take(field.count) {
                    let value = match *usage {
                        INPUT_MODE => MODE_TOUCHPAD,
                        // Linux sets both switches on for a touchpad.
                        SURFACE_SWITCH | BUTTON_SWITCH => 1,
                        _ => continue,
                    };
                    if (field.minimum..=field.maximum).contains(&value) {
                        values.push((field.offset + element * field.size, field.size, value));
                    }
                }
            }
            if !values.is_empty() {
                modes.push(Mode {
                    id: report.id,
                    bytes: report.bits.div_ceil(8),
                    values,
                });
            }
        }
        // The mode switch is what makes contacts flow.
        if !modes.iter().any(|Mode { id, .. }| {
            descriptor.reports.iter().any(|report| {
                report.kind == ReportType::Feature
                    && report.id == *id
                    && report
                        .fields
                        .iter()
                        .any(|field| field.usages.contains(&INPUT_MODE))
            })
        }) {
            return None;
        }
        Some(Self {
            application,
            geometry: Geometry::new((x_range, y_range), x_axis, y_axis),
            modes,
        })
    }

    /// The feature reports that select touchpad mode, as (report ID,
    /// payload without the ID).
    pub fn mode_reports(&self) -> Vec<(u8, Vec<u8>)> {
        self.modes
            .iter()
            .map(|Mode { id, bytes, values }| {
                let mut payload = alloc::vec![0u8; *bytes];
                for &(offset, size, value) in values {
                    for bit in 0..size.min(64) {
                        if value >> bit & 1 != 0
                            && let Some(byte) = payload.get_mut((offset + bit) / 8)
                        {
                            *byte |= 1 << ((offset + bit) % 8);
                        }
                    }
                }
                (*id, payload)
            })
            .collect()
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }
}

/// A precision touchpad in touchpad mode.
pub struct Ptp {
    profile: Profile,
    translator: Translator,
    /// The contacts the frame being assembled expects, and those it has.
    expected: usize,
    received: Vec<Contact>,
}

impl Ptp {
    pub fn new(profile: Profile) -> Self {
        Self {
            translator: Translator::new(profile.geometry),
            profile,
            expected: 0,
            received: Vec::new(),
        }
    }

    /// One input report: `frame` as decoded, and the descriptor events the
    /// translator decoded from it. Reports of other applications are left
    /// to the shared translator.
    pub fn report(
        &mut self,
        descriptor: &Descriptor,
        frame: &Frame,
        events: &[Event],
        now_us: u64,
        inputs: &mut Vec<Input>,
    ) {
        let Some(packet) = events.iter().find_map(|event| match event {
            Event::Touch(packet) if packet.application == self.profile.application => Some(packet),
            _ => None,
        }) else {
            return;
        };
        let mut buttons = 0u8;
        for sample in &frame.samples {
            let Some(usage) = sample.usage else {
                continue;
            };
            if !(BUTTON_1..BUTTON_1 + 3).contains(&usage)
                || application(descriptor, sample.collection) != Some(self.profile.application)
            {
                continue;
            }
            if matches!(sample.value, Value::Scalar(value) if value != 0) && !sample.null {
                buttons |= 1 << (usage - BUTTON_1);
            }
        }
        let count = packet
            .contact_count
            .map_or(0, |count| usize::try_from(count).unwrap_or(0))
            .min(MAX_CONTACTS);
        if count != 0 {
            // A nonzero count always starts a frame; one still pending was
            // interrupted and is dropped.
            self.expected = count;
            self.received.clear();
        }
        for contact in &packet.contacts {
            if self.received.len() >= self.expected {
                break;
            }
            self.received.push(contact.clone());
        }
        if self.received.len() < self.expected {
            return;
        }
        // A complete frame, or a count of 0 with no frame pending: a
        // report of buttons alone, with no finger on the pad.
        let mut fingers: Vec<_> = self
            .received
            .drain(..)
            .filter(|contact| contact.tip == Some(true) && contact.confidence != Some(false))
            .filter_map(|contact| Some((contact.id, contact.x?, contact.y?)))
            .collect();
        self.expected = 0;
        fingers.sort_by_key(|(id, _, _)| *id);
        let clamp = |value: i64| value.clamp(i32::MIN.into(), i32::MAX.into()) as i32;
        let touch = Touch::of(fingers.into_iter().map(|(_, x, y)| (clamp(x), clamp(y))));
        self.translator.frame(touch, buttons, now_us, inputs);
    }

    /// Release the buttons and forget the fingers, as a stopped pad's.
    pub fn release(&mut self, inputs: &mut Vec<Input>) {
        self.expected = 0;
        self.received.clear();
        self.translator.release(inputs);
    }
}

#[cfg(test)]
mod tests;
