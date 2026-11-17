//! ACM's environment variables, after the reference executor's table
//! `__envTable` (26B123 VA 0xfffffe000cb3d520: 0x4f slots of 0x98 bytes,
//! `findSlot` 0xfffffe00092c0138 takes slot `id` when its first word is
//! `id`), `Env_GetVariable` 0xfffffe00092c0aa8 and
//! `Env_SetVariableWithParams` 0xfffffe00092bf728.
//!
//! A slot holds its data type at +4 (1: a boolean, exactly one byte), its
//! flags at +8, the callers that may read it at +0x10 and set it at +0x18
//! (`Permissions_VerifyCaller` 0xfffffe00092a7074: no common bit is -1),
//! and its hooks: an access check on reads (+0x20) and sets (+0x28), a
//! policy a set must satisfy (+0x30, +0x38), a data provider (+0x40, also
//! registered by `InitCredentialEngine` 0xfffffe0009294a40 for 22, 25, 27,
//! 29, 36, 69 and 75, and by `Timer_Init` for 47), a default (+0x48), a
//! maximum age (+0x58), a validation (+0x60) and set and read callbacks
//! (+0x68, +0x70). Flags: 0x2 the data may not be changed or cleared once
//! set ("forbidden to override existing data", -1), 0x4 it lives in the
//! persistent locker (`readPersistentVarsFromLocker`), 0x20 only a data
//! provider answers it ("slot->dataProviderCB", -5).
//!
//! **Callers.** Variables an AP process sets and reads after the kernel
//! checked its entitlement (`AppleCredentialManagerShared::
//! _checkRequiredCommandEntitlements`, e.g. 39 with
//! `securestore.macbuddy.allow`, 72 with `oti.entropy.allow`) name the
//! callers 0xfffff, and SEP-internal ones name high bits only (0x08…: ACM
//! itself, 0x20…). This model takes the AP's requests as a caller every
//! 0xfffff bit stands for: a mask with all of them passes, one with none is
//! -1, and one with some of them is outside the model (which AP caller sepOS
//! assigns is not recovered).
//!
//! **Model.** Plain stored data, as `setData` keeps it, with the executor's
//! checks. Hooks, the locker and data providers are outside the model: each
//! use is named once and refused.
use alloc::{format, string::String, vec::Vec};
use zeroize::Zeroizing;

/// kACMErrorNotFound: `findSlot` found no slot.
const NOT_FOUND: i32 = -7;
/// `Permissions_VerifyCaller` with no common caller bit.
const PERMISSION: i32 = -1;
/// kACMErrorBadParameter.
const BAD_PARAMETER: i32 = -3;
/// A provider-only variable with no provider.
const NO_PROVIDER: i32 = -5;
/// "forbidden to override existing data" / "... to clear existing data".
const WRITE_ONCE: i32 = -1;
/// `kACMEnvironmentVariableMaxDataLength`.
const MAX_DATA: usize = 0x80;

const ONCE: u8 = 0x2;
const LOCKER: u8 = 0x4;
const PROVIDED_ONLY: u8 = 0x20;

const GET_CHECK: u16 = 1 << 0;
const SET_CHECK: u16 = 1 << 1;
const POLICY: u16 = 1 << 2;
const POLICY_SET: u16 = 1 << 3;
const PROVIDER: u16 = 1 << 4;
const DEFAULT: u16 = 1 << 5;
const MAX_AGE: u16 = 1 << 6;
const VALIDATE: u16 = 1 << 7;

/// The callers the AP's requests stand for.
const AP_CALLERS: u64 = 0xf_ffff;

#[derive(Clone, Copy)]
struct Slot {
    boolean: bool,
    flags: u8,
    read: u64,
    write: u64,
    hooks: u16,
}

const fn slot(boolean: bool, flags: u8, read: u64, write: u64, hooks: u16) -> Option<Slot> {
    Some(Slot {
        boolean,
        flags,
        read,
        write,
        hooks,
    })
}

/// `__envTable` by variable; None where `findSlot` finds no slot.
#[rustfmt::skip]
const SLOTS: [Option<Slot>; 0x4f] = [
    slot(false, 0x0, 0x0, 0x0, 0), // 0
    slot(false, 0x3, u64::MAX, 0x10_0000_0000, 0), // 1
    slot(false, 0x2, u64::MAX, 0x10_0000_0000, 0), // 2
    slot(true, 0x3, u64::MAX, 0xf_0000_0000, 0), // 3
    slot(true, 0x3, u64::MAX, 0xf_0000_0000, 0), // 4
    slot(true, 0x2, u64::MAX, 0xf000_0010_0000_0000, 0), // 5
    slot(false, 0x0, u64::MAX, 0x2_0900_0000_0000, VALIDATE), // 6
    slot(true, 0x2, u64::MAX, 0x2000_0000_0000_0000, 0), // 7
    slot(false, 0x2, u64::MAX, 0x0800_0000_0000_0000, 0), // 8
    slot(false, 0x2, u64::MAX, 0x0800_0000_0000_0000, 0), // 9
    slot(false, 0x1, 0x0800_0000_0000_0000, 0x0800_0000_0000_0000, 0), // 10
    slot(false, 0x1, 0x0800_0000_0000_0000, 0x0800_0000_0000_0000, 0), // 11
    slot(false, 0x1, 0x4000_0000_0000, 0x0800_0000_0000_0000, 0), // 12
    slot(true, 0x14, u64::MAX, 0xffff_ffff, POLICY), // 13
    slot(true, 0x4, u64::MAX, u64::MAX, 0), // 14
    slot(false, 0x4, u64::MAX, u64::MAX, 0), // 15
    slot(false, 0x9, 0xf_ffff, 0xf_ffff, 0), // 16
    slot(false, 0x1, 0xf_ffff, 0xf_ffff, 0), // 17
    slot(false, 0x9, 0x0800_0000_0000_0000, 0x0800_0000_0000_0000, 0), // 18
    slot(false, 0x1, 0x0800_0000_0000_0000, 0x0800_0000_0000_0000, 0), // 19
    slot(false, 0x9, 0xf_ffff, 0xf_ffff, 0), // 20
    slot(true, 0x1, u64::MAX, 0x0800_0000_0000_0000, 0), // 21
    slot(true, 0x2, u64::MAX, 0xf000_0000_0000_0000, PROVIDER), // 22
    slot(true, 0x2, u64::MAX, 0x2000_0000_0000_0000, 0), // 23
    slot(false, 0x2, u64::MAX, 0x2000_0000_0000_0000, 0), // 24
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 25
    slot(true, 0x2, u64::MAX, 0x2000_0000_0000_0000, 0), // 26
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 27
    slot(true, 0x3, 0x0800_0000_0000_0000, 0x0800_0000_0000_0000, 0), // 28
    slot(true, 0x20, u64::MAX, 0x0, PROVIDER), // 29
    slot(false, 0x2, u64::MAX, 0x2_0900_0000_0000, VALIDATE), // 30
    slot(false, 0x0, 0x2000_8000_0000_0000, 0x8000_0000_0000, 0), // 31
    None, // 32
    slot(false, 0x2, u64::MAX, 0x2000_0000_0000_0000, 0), // 33
    slot(false, 0x0, u64::MAX, 0x2000_1200_0100_0000, 0), // 34
    slot(true, 0x1, u64::MAX, 0x300_0000, 0), // 35
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 36
    slot(false, 0x14, u64::MAX, 0xffff_ffff, POLICY), // 37
    slot(true, 0x0, u64::MAX, 0x2, 0), // 38
    slot(false, 0x49, 0xf_ffff, 0xf_ffff, 0), // 39
    slot(true, 0x0, u64::MAX, 0x100_0000, 0), // 40
    slot(true, 0x84, u64::MAX, 0xffff_ffff, POLICY), // 41
    slot(true, 0x0, u64::MAX, 0xffff_ffff, 0), // 42
    slot(true, 0x84, u64::MAX, 0xffff_ffff, POLICY), // 43
    slot(false, 0x20, u64::MAX, 0x0, 0), // 44
    slot(false, 0x4, u64::MAX, 0xffff_ffff, VALIDATE), // 45
    slot(true, 0x0, u64::MAX, 0xffff_ffff, 0), // 46
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 47
    slot(false, 0x4, 0x2000_0000_0000_0000, 0x2000_0000_0000_0000, 0), // 48
    slot(false, 0xc, 0x2000_0000_0000_0000, 0x2000_0000_0000_0000, 0), // 49
    slot(false, 0x4, u64::MAX, 0x2000_0000_0000_0000, 0), // 50
    slot(false, 0x20, u64::MAX, 0x0, 0), // 51
    slot(false, 0xe, 0xffff_ffff, 0xffff_ffff, GET_CHECK | SET_CHECK), // 52
    slot(true, 0x14, u64::MAX, 0xffff_ffff, POLICY), // 53
    slot(true, 0x14, u64::MAX, 0xffff_ffff, POLICY), // 54
    slot(true, 0x14, u64::MAX, 0xffff_ffff, POLICY), // 55
    slot(true, 0x2, u64::MAX, 0x0800_0000_0000_0000, 0), // 56
    slot(true, 0x2, u64::MAX, 0x0800_0000_0000_0000, 0), // 57
    slot(true, 0x2, u64::MAX, 0x0800_0000_0000_0000, 0), // 58
    slot(true, 0x2, u64::MAX, 0x0800_0000_0000_0000, 0), // 59
    slot(true, 0x14, u64::MAX, 0xffff_ffff, POLICY), // 60
    slot(true, 0x84, u64::MAX, 0xffff_ffff, POLICY), // 61
    slot(false, 0x0, u64::MAX, 0x2_0900_0000_0000, 0), // 62
    slot(false, 0x4, u64::MAX, 0x2000_0000_0000_0002, 0), // 63
    slot(true, 0x20, u64::MAX, 0x0, 0), // 64
    slot(false, 0x0, u64::MAX, 0x800_0000_0000, 0), // 65
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 66
    slot(false, 0x4, u64::MAX, 0xffff_ffff, 0), // 67
    slot(false, 0x8, 0x2000_0000_0000_0000, 0x0800_0000_ff00_0000, MAX_AGE), // 68
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 69
    slot(false, 0x2, u64::MAX, 0x2_0900_0000_0000, 0), // 70
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 71
    slot(false, 0xe, 0xf_ffff, 0xf_ffff, 0), // 72
    slot(true, 0x14, u64::MAX, 0xf_ffff, POLICY | POLICY_SET), // 73
    slot(true, 0x14, u64::MAX, 0xf_ffff, POLICY | DEFAULT), // 74
    slot(false, 0x20, u64::MAX, 0x0, PROVIDER), // 75
    None, // 76
    None, // 77
    slot(false, 0x2, u64::MAX, 0x2000_0000_0000_0000, VALIDATE), // 78
];

/// The one-byte variables `InitCredentialEngine` sets ([`Variables::new`]):
/// 5, 6, the biometry type `biometryTypePresent` reads (0: none), 7, 26,
/// 33 (1) and 34.
const INITIAL: [(u32, u8); 6] = [(5, 0), (6, 0), (7, 0), (26, 0), (33, 1), (34, 0)];

/// Why an environment request has no answer.
pub(super) enum Refused {
    /// The executor's own ACM error code.
    Acm(i32),
    /// Outside the model: the variable and what it needs, named once.
    Unmodeled(u32, &'static str),
}

impl From<i32> for Refused {
    fn from(code: i32) -> Self {
        Self::Acm(code)
    }
}

impl Refused {
    /// The once-per-boot line for an unmodeled use.
    pub(super) fn line(variable: u32, what: &str) -> String {
        format!("environment variable {variable} needs {what}, which is not modeled")
    }
}

/// The variables' stored data, as ACM keeps it for this boot.
pub(super) struct Variables {
    data: Vec<(u32, Zeroizing<Vec<u8>>)>,
}

impl Variables {
    pub(super) fn new() -> Self {
        Self {
            data: INITIAL
                .iter()
                .map(|&(variable, value)| (variable, Zeroizing::new([value].to_vec())))
                .collect(),
        }
    }

    fn slot(variable: u32) -> Result<Slot, Refused> {
        SLOTS
            .get(variable as usize)
            .copied()
            .flatten()
            .ok_or(Refused::Acm(NOT_FOUND))
    }

    fn stored(&self, variable: u32) -> Option<&[u8]> {
        self.data
            .iter()
            .find(|(id, _)| *id == variable)
            .map(|(_, data)| data.as_slice())
    }

    /// `Env_GetVariable`: the variable's data, empty when it holds none.
    pub(super) fn get(&self, variable: u32) -> Result<Vec<u8>, Refused> {
        let slot = Self::slot(variable)?;
        allowed(variable, slot.read)?;
        if slot.hooks & GET_CHECK != 0 {
            return Err(Refused::Unmodeled(variable, "a read access check"));
        }
        if slot.flags & LOCKER != 0 {
            return Err(Refused::Unmodeled(variable, "the persistent locker"));
        }
        if slot.hooks & MAX_AGE != 0 {
            return Err(Refused::Unmodeled(variable, "a maximum data age"));
        }
        let stored = self.stored(variable);
        // A provider answers a provider-only variable always, and another
        // one while it holds no data (+0xd40).
        if slot.hooks & PROVIDER != 0 && (slot.flags & PROVIDED_ONLY != 0 || stored.is_none()) {
            return Err(Refused::Unmodeled(variable, "its data provider"));
        }
        if slot.flags & PROVIDED_ONLY != 0 {
            return Err(NO_PROVIDER.into());
        }
        match stored {
            Some(data) => Ok(data.to_vec()),
            None if slot.hooks & DEFAULT != 0 => {
                Err(Refused::Unmodeled(variable, "its default callback"))
            }
            // `*outSize = slot->dataSize`, 0 (+0x118).
            None => Ok(Vec::new()),
        }
    }

    /// `Env_SetVariableWithParams`: stores `data`, or clears the variable
    /// for None.
    pub(super) fn set(&mut self, variable: u32, data: Option<&[u8]>) -> Result<(), Refused> {
        // Checked before the slot (+0x8c..+0xac).
        if data.is_some_and(|data| data.is_empty() || data.len() > MAX_DATA) {
            return Err(BAD_PARAMETER.into());
        }
        let slot = Self::slot(variable)?;
        allowed(variable, slot.write)?;
        if slot.hooks & SET_CHECK != 0 {
            return Err(Refused::Unmodeled(variable, "a set access check"));
        }
        if let Some(data) = data
            && slot.boolean
            && data.len() != 1
        {
            // "slot->dataType != kEnvVariableDataTypeBool || dataSize == 1".
            return Err(BAD_PARAMETER.into());
        }
        if slot.hooks & VALIDATE != 0 {
            return Err(Refused::Unmodeled(variable, "a data validation"));
        }
        if slot.hooks & (POLICY | POLICY_SET) != 0 {
            return Err(Refused::Unmodeled(variable, "a policy verification"));
        }
        if slot.flags & LOCKER != 0 {
            return Err(Refused::Unmodeled(variable, "the persistent locker"));
        }
        if slot.hooks & MAX_AGE != 0 {
            return Err(Refused::Unmodeled(variable, "a maximum data age"));
        }
        let at = self.data.iter().position(|(id, _)| *id == variable);
        if slot.flags & ONCE != 0
            && let Some(at) = at
            && data != Some(self.data[at].1.as_slice())
        {
            return Err(WRITE_ONCE.into());
        }
        match (at, data) {
            (Some(at), Some(data)) => self.data[at].1 = Zeroizing::new(data.to_vec()),
            (None, Some(data)) => self.data.push((variable, Zeroizing::new(data.to_vec()))),
            (Some(at), None) => {
                self.data.remove(at);
            }
            (None, None) => {}
        }
        Ok(())
    }
}

/// `Permissions_VerifyCaller` for the AP's requests ([`AP_CALLERS`]).
fn allowed(variable: u32, callers: u64) -> Result<(), Refused> {
    match callers & AP_CALLERS {
        AP_CALLERS => Ok(()),
        0 => Err(PERMISSION.into()),
        _ => Err(Refused::Unmodeled(variable, "a caller class of its own")),
    }
}
