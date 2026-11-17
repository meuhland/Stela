//! A laptop panel's brightness through ACPI video, as Linux's ACPI video
//! driver sets it when it registers a panel's backlight
//! (`acpi_video_init_brightness`), which nouveau asks of it for a panel it
//! drives without a backlight of its own (`nouveau_acpi_video_register_backlight`).
//!
//! Each display output device with `_BCL` and `_BCM` keeps the level `_BQC`
//! (or `_BCQ`) reads where `_BCL` lists it and it is not zero; else, as
//! when the firmware or a GPU reset left an uninitialized or zero level,
//! it is set to the highest level `_BCL` lists. `_BQC` is first checked as
//! Linux checks it (`acpi_video_bqc_quirk`): a test level is set and read
//! back, and a `_BQC` that answers an index of `_BCL` instead of a level is
//! read as one, while one that answers neither is not trusted.
use crate::platform::acpi::{Argument, Error, Handle, Session, Value};
use alloc::{string::String, vec::Vec};

/// `_BCL`'s first two entries are the levels for AC power and for battery;
/// the levels a panel runs follow them (`ACPI_VIDEO_FIRST_LEVEL`).
const FIRST_LEVEL: usize = 2;

/// A `_BCL` list as Linux reads it (`acpi_video_get_levels`): its integer
/// entries as 32-bit levels, repeats after the first two dropped where
/// they follow each other; where the first two entries do not appear among
/// the rest, they are added to them; the levels sorted ascending where the
/// first of them is the highest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Levels {
    /// The AC and battery levels, then the levels the panel runs.
    levels: Vec<u64>,
    /// The highest level of all.
    max: u64,
    /// The panel's levels were listed highest first.
    reversed: bool,
}

impl Levels {
    /// `_BCL`'s entries, `None` where one is not an integer; `None` with
    /// fewer than two entries, which Linux refuses.
    pub fn parse(listed: &[Option<u64>]) -> Option<Self> {
        if listed.len() < FIRST_LEVEL {
            return None;
        }
        let mut levels: Vec<u64> = Vec::new();
        let mut max = 0;
        for &value in listed.iter().flatten() {
            let value = value & u64::from(u32::MAX);
            if levels.len() > FIRST_LEVEL && levels.last() == Some(&value) {
                continue;
            }
            levels.push(value);
            max = max.max(value);
        }
        if levels.len() < FIRST_LEVEL {
            return None;
        }
        let found = levels[FIRST_LEVEL..]
            .iter()
            .map(|&level| usize::from(level == levels[0]) + usize::from(level == levels[1]))
            .sum::<usize>();
        if found < FIRST_LEVEL {
            // Linux moves the levels up by the missing count, so the last
            // of the first two entries (or both) also lead the levels.
            let missing = FIRST_LEVEL - found;
            let added: Vec<u64> = levels[FIRST_LEVEL - missing..FIRST_LEVEL].to_vec();
            levels.splice(FIRST_LEVEL..FIRST_LEVEL, added);
        }
        let reversed = levels.get(FIRST_LEVEL) == Some(&max);
        if reversed {
            levels[FIRST_LEVEL..].sort_unstable();
        }
        Some(Self {
            levels,
            max,
            reversed,
        })
    }

    /// The levels the panel runs.
    fn steps(&self) -> &[u64] {
        &self.levels[FIRST_LEVEL..]
    }

    /// The level `_BQC` names by index of the panel's levels, as listed.
    fn indexed(&self, index: u64) -> Option<u64> {
        let steps = self.steps().len() as u64;
        let index = match self.reversed {
            true => steps.checked_sub(1)?.checked_sub(index)?,
            false => index,
        };
        self.steps().get(usize::try_from(index).ok()?).copied()
    }
}

/// A display output device's brightness methods.
pub trait Panel {
    /// What `_BQC` (or `_BCQ`) reads; `None` where the device has neither
    /// or its evaluation failed, which Linux then no longer asks.
    fn current(&mut self) -> Option<u64>;
    /// `_BCM` with `level`.
    fn set(&mut self, level: u64) -> Result<(), Error>;
}

/// What [`initialize`] set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brightness {
    pub level: u64,
    pub max: u64,
    /// The level read before, where `_BQC` was trusted.
    pub read: Option<u64>,
}

/// Set `panel` to the level read, else the highest of `levels`, as Linux
/// does when its ACPI video driver registers the panel's backlight.
pub fn initialize(levels: &Levels, panel: &mut impl Panel) -> Result<Brightness, Error> {
    let max = levels.max;
    let read = match panel.current() {
        Some(current) => trusted(levels, panel, current)?,
        None => None,
    };
    let level = read
        .filter(|&level| level != 0 && levels.steps().contains(&level))
        .unwrap_or(max);
    panel.set(level)?;
    Ok(Brightness { level, max, read })
}

/// The level `current`, which `_BQC` read first, names, once a test level
/// set and read back shows whether `_BQC` answers levels, indices or
/// neither (`acpi_video_bqc_quirk`); `None` for neither.
fn trusted(levels: &Levels, panel: &mut impl Panel, current: u64) -> Result<Option<u64>, Error> {
    let test = match current == levels.max {
        true => levels.steps().get(1).copied().unwrap_or(levels.max),
        false => levels.max,
    };
    panel.set(test)?;
    // A failed read gives the level last set, as Linux's does.
    let answered = panel.current().unwrap_or(test);
    if answered == test {
        return Ok(Some(current));
    }
    let count = levels.levels.len() as u64;
    let by_index = answered < count && levels.indexed(answered) == Some(test);
    Ok(by_index.then(|| levels.indexed(current)).flatten())
}

/// ACPI's methods on one display output device.
struct Device<'a> {
    handle: Handle<'a>,
    /// `_BQC`, else `_BCQ`, while it evaluates.
    query: Option<&'static str>,
}

impl Panel for Device<'_> {
    fn current(&mut self) -> Option<u64> {
        let name = self.query?;
        match self.handle.evaluate(Some(name), &[]) {
            Ok(Some(Value::Integer(level))) => Some(level & u64::from(u32::MAX)),
            _ => {
                self.query = None;
                None
            }
        }
    }

    fn set(&mut self, level: u64) -> Result<(), Error> {
        self.handle
            .invoke(Some("_BCM"), &[Argument::Integer(level)])
    }
}

/// What [`initialize_all`] did for one display output device.
#[derive(Debug)]
pub struct Outcome {
    pub path: String,
    /// The level set, or why none was: `_BCL` unreadable (`None`), or a
    /// refused `_BCM`.
    pub result: Result<Brightness, Option<Error>>,
}

/// Set each present display output device with `_BCL` and `_BCM` in
/// `session`'s namespace ([`initialize`]).
pub fn initialize_all(session: &Session) -> Result<Vec<Outcome>, Error> {
    let mut outcomes = Vec::new();
    for handle in session.all_devices()? {
        let has = |name: &str| handle.child(name).is_ok();
        if !has("_BCL") || !has("_BCM") {
            continue;
        }
        let path = handle.path().unwrap_or_default();
        let listed = match handle.evaluate(Some("_BCL"), &[]) {
            Ok(Some(Value::Package(values))) => values
                .iter()
                .map(|value| match value {
                    Value::Integer(level) => Some(*level),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let Some(levels) = Levels::parse(&listed) else {
            outcomes.push(Outcome {
                path,
                result: Err(None),
            });
            continue;
        };
        let query = ["_BQC", "_BCQ"].into_iter().find(|name| has(name));
        let mut device = Device { handle, query };
        outcomes.push(Outcome {
            path,
            result: initialize(&levels, &mut device).map_err(Some),
        });
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests;
