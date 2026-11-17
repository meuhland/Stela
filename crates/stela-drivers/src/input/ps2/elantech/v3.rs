// SPDX-License-Identifier: GPL-2.0-only
// Original notices are held in ../elantech.rs and NOTICE.
//! Elantech v3 touchpad packet decoding.
use super::*;

impl Engine {
    pub(super) fn report_v3(&mut self, now: u64, inputs: &mut Vec<Input>) -> Result<(), ErrorKind> {
        let c = self.capabilities.as_ref().unwrap();
        let p = self.packet;
        let debounce = p == [0xc4, 0xff, 0xff, 2, 0xff, 0xff];
        let head = if c.crc_enabled {
            match p[3] & 9 {
                8 => Some(true),
                9 => Some(false),
                _ => None,
            }
        } else if p[0] & 12 == 4 && p[3] & 0xcf == 2 {
            Some(true)
        } else if p[0] & 12 == 12 && p[3] & 0xce == 12 {
            Some(false)
        } else {
            None
        };
        if !debounce && head.is_none() {
            self.last_fault = Some((p, 6));
            return Err(ErrorKind::Elantech(Error::Packet));
        }
        let sequence = self.sequence;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(ErrorKind::Elantech(Error::SequenceExhausted))?;
        let fingers = p[0] >> 6;
        let point = (
            i32::from((u16::from(p[1] & 15) << 8) | u16::from(p[2])),
            i32::from(c.bounds.maximum.1)
                - i32::from((u16::from(p[4] & 15) << 8) | u16::from(p[5])),
        );
        let report = if debounce {
            Report::Debounce
        } else if fingers == 2 && head == Some(true) {
            self.head = Some((sequence, point));
            Report::Head { point }
        } else {
            let (primary, secondary, head_sequence) = if fingers == 2 {
                (
                    self.head.map(|(_, point)| point),
                    Some(point),
                    self.head.map(|(sequence, _)| sequence),
                )
            } else {
                ((fingers != 0).then_some(point), None, None)
            };
            let buttons = if c.buttonpad {
                u8::from(p[0] & 3 != 0)
            } else {
                p[0] & 3
            };
            // Semi-MT: two fingers move their middle, or the second
            // finger's point until a head reports the first.
            let touch = match (primary, secondary) {
                (Some(first), Some(second)) => Touch::of([first, second]),
                (first, second) => first.or(second).map(|(x, y)| Touch { x, y, fingers: 1 }),
            }
            .map(|touch| Touch { fingers, ..touch });
            self.touch(touch, buttons, now, inputs);
            Report::Contacts(Contact {
                fingers,
                primary,
                secondary,
                head_sequence,
                buttons,
                pressure: Some(Pressure::Observed {
                    pressure: (p[1] & 0xf0) | ((p[4] & 0xf0) >> 4),
                    width: ((p[0] & 0x30) >> 2) | ((p[3] & 0x30) >> 4),
                }),
            })
        };
        self.frames.push_back(Frame {
            raw: p,
            length: 6,
            native_sequence: sequence,
            report,
        });
        self.position = 0;
        Ok(())
    }
}
