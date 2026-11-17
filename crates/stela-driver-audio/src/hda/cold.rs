// SPDX-License-Identifier: GPL-2.0-only
//! Register-only containment of inherited conventional HDA DMA. This is an
//! engine observation, not a root, IRQ, firmware or worker retirement receipt.
use super::{Config, Registers, registers::*};
use crate::{Error, Phase, Timer};

/// A completed conventional HDA engine-stop observation. It deliberately
/// grants no original-root migration or DMA allocation permission.
pub struct Stopped {
    streams: u8,
}
impl Stopped {
    pub fn streams(&self) -> u8 {
        self.streams
    }
}

enum Width {
    Byte,
    Word,
    Dword,
}
struct Field {
    offset: usize,
    width: Width,
    mask: u32,
}

fn wait<R: Registers>(
    registers: &mut R,
    timer: &impl Timer,
    timeout: u32,
    field: Field,
    phase: Phase,
) -> Result<(), Error> {
    let Field {
        offset,
        width,
        mask,
    } = field;
    let start = timer.now_us();
    loop {
        let (value, failed) = match width {
            Width::Byte => {
                let v = registers.read8(offset);
                (u32::from(v), v == u8::MAX)
            }
            Width::Word => {
                let v = registers.read16(offset);
                (u32::from(v), v == u16::MAX)
            }
            Width::Dword => {
                let v = registers.read32(offset);
                (v, v == u32::MAX)
            }
        };
        if failed {
            return Err(Error::RegisterUnavailable { offset });
        }
        if value & mask == 0 {
            return Ok(());
        }
        if timer.now_us().wrapping_sub(start) >= u64::from(timeout) {
            return Err(Error::Timeout(phase));
        }
        timer.relax();
    }
}

/// Stop every inherited physical stream, CORB/RIRB and position DMA without
/// allocating, zeroing, rebasing or resetting inherited buffers or the link.
/// Controller/stream interrupt enables are disabled and read back separately.
///
/// The caller's register-provider ownership/exclusion and power prerequisites
/// remain necessary. The platform must independently retire old IRQ delivery,
/// firmware and CPU/work users and drain the original root before any binding.
/// A mask readback, stopped RUN bit or returned observation cannot replace
/// those proofs. Failure is uncertain; an owning lifecycle must latch failure
/// and retain the original config/MMIO/root/backing without replay or reset.
pub fn stop_inherited(
    registers: &mut impl Registers,
    timer: &impl Timer,
    config: &Config,
) -> Result<Stopped, Error> {
    timer.check().map_err(Error::MinimumDelay)?;
    if config.transition_timeout_us == 0 {
        return Err(Error::InvalidConfiguration);
    }
    let (major, minor) = (registers.read8(VMAJ), registers.read8(VMIN));
    if major != 1 || minor != 0 {
        return Err(Error::UnsupportedVersion { major, minor });
    }
    let gcap = registers.read16(GCAP);
    let streams = ((gcap >> 8) & 15) + ((gcap >> 12) & 15) + ((gcap >> 3) & 31);
    if streams == 0 || streams > 30 || registers.region_bytes() < 0x80 + usize::from(streams) * 0x20
    {
        return Err(Error::UnsupportedTopology);
    }
    let gctl = registers.read32(GCTL);
    if gctl == u32::MAX {
        return Err(Error::RegisterUnavailable { offset: GCTL });
    }
    registers.write32(INTCTL, 0);
    wait(
        registers,
        timer,
        config.transition_timeout_us,
        Field {
            offset: INTCTL,
            width: Width::Dword,
            mask: u32::MAX,
        },
        Phase::CommandStop,
    )?;
    registers.write16(WAKEEN, 0);
    wait(
        registers,
        timer,
        config.transition_timeout_us,
        Field {
            offset: WAKEEN,
            width: Width::Word,
            mask: u32::from(CODEC_MASK),
        },
        Phase::CommandStop,
    )?;
    registers.write32(GCTL, gctl & !GCTL_UNSOL);
    wait(
        registers,
        timer,
        config.transition_timeout_us,
        Field {
            offset: GCTL,
            width: Width::Dword,
            mask: GCTL_UNSOL,
        },
        Phase::CommandStop,
    )?;
    for index in 0..streams as u8 {
        let base = stream_base(index);
        let previous = registers.read8(base + SD_CTL);
        if previous == u8::MAX {
            return Err(Error::RegisterUnavailable {
                offset: base + SD_CTL,
            });
        }
        registers.write8(base + SD_CTL, previous & !(SD_RUN | SD_STATUS));
        wait(
            registers,
            timer,
            config.transition_timeout_us,
            Field {
                offset: base + SD_CTL,
                width: Width::Byte,
                mask: u32::from(SD_RUN | SD_STATUS),
            },
            Phase::StreamStop,
        )?;
        registers.write8(base + SD_STS, SD_STATUS);
        wait(
            registers,
            timer,
            config.transition_timeout_us,
            Field {
                offset: base + SD_STS,
                width: Width::Byte,
                mask: u32::from(SD_STATUS),
            },
            Phase::StreamStop,
        )?;
    }
    for (offset, mask) in [(CORBCTL, COMMAND_RUN | 1), (RIRBCTL, RIRB_RUN_STATUS)] {
        registers.write8(offset, 0);
        wait(
            registers,
            timer,
            config.transition_timeout_us,
            Field {
                offset,
                width: Width::Byte,
                mask: u32::from(mask),
            },
            Phase::CommandStop,
        )?;
    }
    for (offset, mask) in [
        (CORBSTS, CORB_MEMORY),
        (RIRBSTS, RIRB_RESPONSE | RIRB_OVERRUN),
    ] {
        registers.write8(offset, mask);
        wait(
            registers,
            timer,
            config.transition_timeout_us,
            Field {
                offset,
                width: Width::Byte,
                mask: u32::from(mask),
            },
            Phase::CommandStop,
        )?;
    }
    // DPLBASE bit0 enables position DMA. Clear it before changing the high
    // address; retain inherited backing through the independent original drain.
    for offset in [DPLBASE, DPUBASE] {
        registers.write32(offset, 0);
        wait(
            registers,
            timer,
            config.transition_timeout_us,
            Field {
                offset,
                width: Width::Dword,
                mask: u32::MAX,
            },
            Phase::CommandStop,
        )?;
    }
    Ok(Stopped {
        streams: streams as u8,
    })
}

/// What [`quiesce_for_power_off`] found and did, step by step. Each time is
/// from the step's first write to the readback that showed it done, `None`
/// when the readback still disagreed at the step's limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerOffQuiesce {
    /// GCAP read all ones: the controller did not answer, so nothing was
    /// written and every step is `None`.
    pub unanswered: bool,
    /// The stream descriptors whose RUN bit was set, by index.
    pub running_streams: u32,
    /// Every stream read back stopped (SDnCTL.RUN = 0).
    pub streams_us: Option<u64>,
    /// CORB and RIRB DMA read back stopped (CORBRUN = 0, RIRBDMAEN = 0).
    pub commands_us: Option<u64>,
    /// The controller read back in reset (GCTL.CRST = 0).
    pub reset_us: Option<u64>,
}

impl PowerOffQuiesce {
    /// Whether every step read back done.
    pub fn complete(&self) -> bool {
        !self.unanswered
            && self.streams_us.is_some()
            && self.commands_us.is_some()
            && self.reset_us.is_some()
    }
}

/// Polls `done` until it holds or `limit_us` passed since `start`: the time
/// it took, or `None`. An all-ones read never counts as done.
fn settle<R: Registers>(
    registers: &mut R,
    timer: &impl Timer,
    start: u64,
    limit_us: u64,
    mut done: impl FnMut(&mut R) -> bool,
) -> Option<u64> {
    loop {
        let elapsed = timer.now_us().wrapping_sub(start);
        if done(registers) {
            return Some(elapsed);
        }
        if elapsed >= limit_us {
            return None;
        }
        timer.relax();
    }
}

/// Bring a controller to rest before the machine turns off or restarts, as
/// Linux's `azx_stop_chip` then `azx_enter_link_reset`: interrupts off,
/// every running stream's RUN cleared and read back stopped, CORB and RIRB
/// DMA stopped, position DMA off, then the link put in reset (GCTL.CRST =
/// 0) and read back in reset. Each readback waits `limit_us` at most, and a
/// step that did not finish does not stop the next: whatever ran is stopped
/// as far as the controller lets it. Bus mastering is the caller's, after
/// this. Nothing is allocated, unmapped or retired.
pub fn quiesce_for_power_off(
    registers: &mut impl Registers,
    timer: &impl Timer,
    limit_us: u64,
) -> PowerOffQuiesce {
    let mut report = PowerOffQuiesce::default();
    let gcap = registers.read16(GCAP);
    if gcap == u16::MAX {
        report.unanswered = true;
        return report;
    }
    let streams = ((gcap >> 8) & 15) + ((gcap >> 12) & 15) + ((gcap >> 3) & 31);
    let streams = (0..streams.min(30) as u8)
        .filter(|&index| registers.region_bytes() >= stream_base(index) + 0x20)
        .count() as u8;
    registers.write32(INTCTL, 0);
    let start = timer.now_us();
    for index in 0..streams {
        let base = stream_base(index);
        let control = registers.read8(base + SD_CTL);
        if control == u8::MAX || control & (SD_RUN | SD_STATUS) == 0 {
            continue;
        }
        if control & SD_RUN != 0 {
            report.running_streams |= 1 << index;
        }
        registers.write8(base + SD_CTL, control & !(SD_RUN | SD_STATUS));
    }
    report.streams_us = settle(registers, timer, start, limit_us, |registers| {
        (0..streams).all(|index| {
            let control = registers.read8(stream_base(index) + SD_CTL);
            control != u8::MAX && control & SD_RUN == 0
        })
    });
    for index in 0..streams {
        if report.running_streams & 1 << index != 0 {
            registers.write8(stream_base(index) + SD_STS, SD_STATUS);
        }
    }
    let start = timer.now_us();
    registers.write8(CORBCTL, 0);
    registers.write8(RIRBCTL, 0);
    report.commands_us = settle(registers, timer, start, limit_us, |registers| {
        let (corb, rirb) = (registers.read8(CORBCTL), registers.read8(RIRBCTL));
        corb != u8::MAX && rirb != u8::MAX && (corb | rirb) & COMMAND_RUN == 0
    });
    // DPLBASE bit 0 enables position DMA; cleared before its high half.
    registers.write32(DPLBASE, 0);
    registers.write32(DPUBASE, 0);
    let start = timer.now_us();
    let gctl = registers.read32(GCTL);
    if gctl != u32::MAX && gctl & GCTL_RESET != 0 {
        registers.write32(GCTL, gctl & !GCTL_RESET);
    }
    report.reset_us = settle(registers, timer, start, limit_us, |registers| {
        let gctl = registers.read32(GCTL);
        gctl != u32::MAX && gctl & GCTL_RESET == 0
    });
    report
}

#[cfg(test)]
mod tests;
