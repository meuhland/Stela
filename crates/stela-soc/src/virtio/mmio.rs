//! The virtio-mmio version 2 transport (virtio 1.2, 4.2.2): one device's
//! register page, as `virtio_mmio.h` lays it out.
//!
//! The driver resets the device by writing 0 to `STATUS`, then sets
//! `ACKNOWLEDGE` and `DRIVER`, writes the features it accepts and sets
//! `FEATURES_OK`, which holds only for a subset of what the device offers
//! that has `VERSION_1`. It programs each queue's size and three areas and
//! sets `QUEUE_READY`, which holds only for a queue [`Queue::split`]
//! accepts, then sets `DRIVER_OK`, after which the device uses its ready
//! queues in the board's DMA step: once to check their areas are RAM, and
//! then whenever the driver notifies one or the device has work.
//!
//! `INTERRUPT_STATUS` drives the device's AIC input, a level: it rises with
//! the first bit raised and falls once `INTERRUPT_ACK` clears the last. A
//! ring that breaks the format, or areas that are not RAM, stop the device
//! instead of the machine: it forgets its queues, sets `DEVICE_NEEDS_RESET`
//! and raises `INT_CONFIG`, and the driver must reset it. Accesses no
//! register has, or of a width it lacks, are refused as every model's are.
//!
//! What the driver did (status changes, the features agreed, each queue's
//! places, its first notification, errors) is kept as [`Event`]s for the
//! boot log, and the device's work as [`Stats`].
use super::queue::Queue;
use super::{Queues, VirtioDevice, ring_error};
use crate::bus::{Access, Direction, MmioDevice, MmioError, Width, refuse};
use alloc::{boxed::Box, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_contracts::{
    bus::AdvancePolicy,
    dma::Memory,
    interrupt::{AicInput, Wire},
    virtio::{features, mmio::*, status},
};

/// The events kept between two reports; later ones are only counted.
const EVENTS_KEPT: usize = 64;

/// What the driver did with the device, for the boot log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `STATUS` changed from `from` to `to`, not by a reset.
    Status { from: u32, to: u32 },
    /// The driver set `FEATURES_OK` with `driver`, of `offered`; `accepted`
    /// tells whether the device kept the bit.
    Features {
        offered: u64,
        driver: u64,
        accepted: bool,
    },
    /// Queue `index` runs: `size` entries, its three areas.
    Queue {
        index: u16,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    },
    /// The driver set queue `index` ready, and the device refused it.
    QueueRefused { index: u16, why: &'static str },
    /// The driver's first notification of queue `index`.
    Notified { index: u16 },
    /// The driver reset the device.
    Reset,
    /// The device stopped: the driver must reset it.
    Error(&'static str),
}

/// What the device did for the guest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// `QUEUE_NOTIFY` writes.
    pub notifies: u64,
    /// Times the line rose.
    pub interrupts: u64,
    /// Chains the device used, and the bytes it wrote into them.
    pub chains: u64,
    pub bytes: u64,
}

/// One queue's registers, as the driver programmed them.
#[derive(Debug, Clone, Copy, Default)]
struct Programmed {
    num: u32,
    desc: u64,
    avail: u64,
    used: u64,
}

pub struct Mmio {
    name: &'static str,
    irq: AicInput,
    device: Box<dyn VirtioDevice>,
    /// `DEVICE_FEATURES`: the device's own bits and the transport's.
    offered: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    /// What the driver wrote to `DRIVER_FEATURES`.
    driver_features: u64,
    /// What `FEATURES_OK` agreed.
    accepted: u64,
    queue_sel: u32,
    programmed: Vec<Programmed>,
    /// The queues while they are ready.
    queues: Queues,
    status: u32,
    interrupt_status: u32,
    config_generation: u32,
    /// Queues notified since the last step, a bit each.
    notified: u64,
    /// Queues notified since the last reset, for their first notification.
    heard: u64,
    /// Whether the ready queues' areas were checked to be RAM.
    checked: bool,
    error: Option<&'static str>,
    events: Vec<Event>,
    dropped_events: u64,
    /// Set when an event is kept, for a boot that reports them.
    notice: Option<&'static AtomicBool>,
    stats: Stats,
    /// The stats as [`Self::stats_changed`] last saw them.
    reported: Stats,
}

impl Mmio {
    /// `device` on the transport, its interrupt at `irq`. `notice`, if
    /// given, is set whenever an event is kept.
    pub fn new(
        device: Box<dyn VirtioDevice>,
        irq: AicInput,
        notice: Option<&'static AtomicBool>,
    ) -> Self {
        let queues = device.queue_max().len();
        Self {
            name: device.name(),
            irq,
            offered: device.features() | features::TRANSPORT,
            device,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            accepted: 0,
            queue_sel: 0,
            programmed: vec![Programmed::default(); queues],
            queues: vec![None; queues],
            status: 0,
            interrupt_status: 0,
            config_generation: 0,
            notified: 0,
            heard: 0,
            checked: false,
            error: None,
            events: Vec::new(),
            dropped_events: 0,
            notice,
            stats: Stats::default(),
            reported: Stats::default(),
        }
    }

    /// The device's name, such as `virtio-rng`.
    pub fn device_name(&self) -> &'static str {
        self.name
    }

    pub fn device_id(&self) -> u32 {
        self.device.id()
    }

    pub fn status(&self) -> u32 {
        self.status
    }

    /// The features `FEATURES_OK` agreed.
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Whether the stats changed since the last call: what a periodic
    /// report shows.
    pub fn stats_changed(&mut self) -> bool {
        let changed = self.stats != self.reported;
        self.reported = self.stats;
        changed
    }

    /// Why the device last stopped itself.
    pub fn error(&self) -> Option<&'static str> {
        self.error
    }

    /// The events kept since the last call, and how many more were not.
    pub fn take_events(&mut self) -> (Vec<Event>, u64) {
        (
            core::mem::take(&mut self.events),
            core::mem::take(&mut self.dropped_events),
        )
    }

    fn event(&mut self, event: Event) {
        if self.events.len() < EVENTS_KEPT {
            self.events.push(event);
        } else {
            self.dropped_events += 1;
        }
        if let Some(notice) = self.notice {
            notice.store(true, Ordering::Release);
        }
    }

    /// Whether the device uses its queues: the driver set `DRIVER_OK`, and
    /// neither side gave up.
    pub fn running(&self) -> bool {
        self.status & status::DRIVER_OK as u32 != 0
            && self.status & (status::DEVICE_NEEDS_RESET | status::FAILED) as u32 == 0
    }

    /// Whether the DMA step has work: queues to check, a notification, or
    /// the device's own.
    pub fn due(&self) -> bool {
        self.running() && (!self.checked || self.notified != 0 || self.device.due())
    }

    /// Whether the device has work for the host's service; asking clears it.
    pub fn take_service_due(&mut self) -> bool {
        self.device.take_service_due()
    }

    /// The device behind the transport.
    pub fn device_mut(&mut self) -> &mut dyn VirtioDevice {
        &mut *self.device
    }

    pub fn device(&self) -> &dyn VirtioDevice {
        &*self.device
    }

    fn event_idx(&self) -> bool {
        self.accepted & features::EVENT_IDX != 0
    }

    /// The DMA step: checks the ready queues' areas once, then lets the
    /// device work on them, raising `INT_VRING` when the driver asked.
    pub fn step(&mut self, memory: &mut dyn Memory, access: &mut Access<'_>) {
        if !self.running() {
            return;
        }
        if !self.checked {
            let unusable = self
                .queues
                .iter()
                .flatten()
                .find_map(|queue| queue.check(memory).err());
            if let Some(error) = unusable {
                return self.fail(ring_error(error), access);
            }
            self.checked = true;
        }
        let notified = core::mem::take(&mut self.notified);
        let event_idx = self.event_idx();
        match self
            .device
            .step(&mut self.queues, notified, memory, event_idx, access.now)
        {
            Ok(served) => {
                self.stats.chains += served.chains;
                self.stats.bytes += served.bytes;
                if served.config {
                    self.config_generation = self.config_generation.wrapping_add(1);
                    self.raise(INT_CONFIG as u32, access);
                }
                if served.interrupt {
                    self.raise(INT_VRING as u32, access);
                }
            }
            Err(why) => self.fail(why, access),
        }
        if self.device.has_lines()
            && let Some(notice) = self.notice
        {
            notice.store(true, Ordering::Release);
        }
    }

    /// The device's own lines for the log since the last call.
    pub fn take_lines(&mut self) -> Vec<alloc::string::String> {
        self.device.take_lines()
    }

    /// The device's own counters, by name.
    pub fn device_counters(&self) -> Vec<(&'static str, u64)> {
        self.device.counters()
    }

    fn raise(&mut self, bits: u32, access: &mut Access<'_>) {
        if self.interrupt_status == 0 {
            self.stats.interrupts += 1;
        }
        self.interrupt_status |= bits;
        access.interrupts.set_level(Wire::Aic(self.irq), true);
    }

    /// Stops the device for `why`: it forgets its queues, so nothing reaches
    /// guest memory after this, and the driver hears it must reset it.
    fn fail(&mut self, why: &'static str, access: &mut Access<'_>) {
        self.queues.iter_mut().for_each(|queue| *queue = None);
        self.notified = 0;
        self.error = Some(why);
        let from = self.status;
        self.status |= status::DEVICE_NEEDS_RESET as u32;
        self.event(Event::Error(why));
        self.event(Event::Status {
            from,
            to: self.status,
        });
        if from & status::DRIVER_OK as u32 != 0 {
            self.raise(INT_CONFIG as u32, access);
        }
    }

    /// `STATUS` 0: the device forgets everything the driver set.
    fn reset(&mut self, access: &mut Access<'_>) {
        let was = self.status;
        self.device.reset();
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.accepted = 0;
        self.queue_sel = 0;
        self.programmed.fill(Programmed::default());
        self.queues.iter_mut().for_each(|queue| *queue = None);
        self.status = 0;
        self.interrupt_status = 0;
        self.notified = 0;
        self.heard = 0;
        self.checked = false;
        self.error = None;
        access.interrupts.set_level(Wire::Aic(self.irq), false);
        if was != 0 {
            self.event(Event::Reset);
        }
    }

    /// A `STATUS` write other than 0.
    fn set_status(&mut self, value: u32, access: &mut Access<'_>) {
        let from = self.status;
        // Only a reset clears DEVICE_NEEDS_RESET.
        let mut to = value | from & status::DEVICE_NEEDS_RESET as u32;
        let risen = to & !from;
        let rising = |bit: u64| risen & bit as u32 != 0;
        if rising(status::FEATURES_OK) {
            let driver = self.driver_features;
            let accepted =
                driver & !self.offered == 0 && driver & features::REQUIRED == features::REQUIRED;
            if accepted {
                self.accepted = driver;
            } else {
                to &= !(status::FEATURES_OK as u32);
            }
            self.event(Event::Features {
                offered: self.offered,
                driver,
                accepted,
            });
        }
        let starts = rising(status::DRIVER_OK);
        if rising(status::FAILED) {
            // The driver gave up: no queue is used after this.
            self.queues.iter_mut().for_each(|queue| *queue = None);
            self.notified = 0;
        }
        self.status = to;
        if to != from {
            self.event(Event::Status { from, to });
        }
        if starts {
            if to & status::FEATURES_OK as u32 == 0 {
                return self.fail("DRIVER_OK before FEATURES_OK", access);
            }
            self.checked = false;
            self.device.activate(self.accepted);
        }
    }

    /// The selected queue, if the device has it.
    fn selected(&self) -> Option<usize> {
        let index = self.queue_sel as usize;
        (index < self.programmed.len()).then_some(index)
    }

    /// `QUEUE_READY` 1 for queue `index`: it runs on the places programmed,
    /// or the device refuses it and the register reads 0.
    fn ready(&mut self, index: usize) {
        if self.queues[index].is_some() {
            return;
        }
        let place = self.programmed[index];
        let most = u64::from(self.device.queue_max()[index]);
        let queue = if self.status & status::FEATURES_OK as u32 == 0 {
            Err("a queue made ready before FEATURES_OK")
        } else {
            Queue::split(
                place.desc,
                place.avail,
                place.used,
                u64::from(place.num),
                most,
            )
        };
        match queue {
            Ok(queue) => {
                self.queues[index] = Some(queue);
                self.checked = false;
                self.event(Event::Queue {
                    index: index as u16,
                    size: place.num as u16,
                    desc: place.desc,
                    avail: place.avail,
                    used: place.used,
                });
            }
            Err(why) => self.event(Event::QueueRefused {
                index: index as u16,
                why,
            }),
        }
    }

    /// A write to one of the selected queue's places.
    fn program(
        &mut self,
        offset: u64,
        value: u32,
        access: &mut Access<'_>,
    ) -> Result<(), &'static str> {
        let Some(index) = self.selected() else {
            // No such queue: the write goes nowhere, as on hardware.
            return Ok(());
        };
        if self.queues[index].is_some() {
            self.fail("a ready queue's places changed", access);
            return Ok(());
        }
        let place = &mut self.programmed[index];
        let low = |word: &mut u64| *word = *word & !0xffff_ffff | u64::from(value);
        let high = |word: &mut u64| *word = *word & 0xffff_ffff | u64::from(value) << 32;
        match offset {
            QUEUE_NUM => place.num = value,
            QUEUE_DESC_LOW => low(&mut place.desc),
            QUEUE_DESC_HIGH => high(&mut place.desc),
            QUEUE_AVAIL_LOW => low(&mut place.avail),
            QUEUE_AVAIL_HIGH => high(&mut place.avail),
            QUEUE_USED_LOW => low(&mut place.used),
            QUEUE_USED_HIGH => high(&mut place.used),
            _ => return Err("no such queue register"),
        }
        Ok(())
    }

    fn word(&self, offset: u64) -> Option<u32> {
        let selected = self.selected();
        let place = selected.map(|index| self.programmed[index]);
        let half = |value: u64, select: u32| match select {
            0 => value as u32,
            1 => (value >> 32) as u32,
            _ => 0,
        };
        Some(match offset {
            MAGIC_VALUE => MAGIC as u32,
            VERSION => VERSION_2 as u32,
            DEVICE_ID => self.device.id(),
            VENDOR_ID => VENDOR as u32,
            DEVICE_FEATURES => half(self.offered, self.device_features_sel),
            DEVICE_FEATURES_SEL => self.device_features_sel,
            DRIVER_FEATURES => half(self.driver_features, self.driver_features_sel),
            DRIVER_FEATURES_SEL => self.driver_features_sel,
            QUEUE_SEL => self.queue_sel,
            QUEUE_NUM_MAX => selected.map_or(0, |index| u32::from(self.device.queue_max()[index])),
            QUEUE_NUM => place.map_or(0, |place| place.num),
            QUEUE_READY => selected.is_some_and(|index| self.queues[index].is_some()) as u32,
            QUEUE_DESC_LOW => place.map_or(0, |place| place.desc as u32),
            QUEUE_DESC_HIGH => place.map_or(0, |place| (place.desc >> 32) as u32),
            QUEUE_AVAIL_LOW => place.map_or(0, |place| place.avail as u32),
            QUEUE_AVAIL_HIGH => place.map_or(0, |place| (place.avail >> 32) as u32),
            QUEUE_USED_LOW => place.map_or(0, |place| place.used as u32),
            QUEUE_USED_HIGH => place.map_or(0, |place| (place.used >> 32) as u32),
            INTERRUPT_STATUS => self.interrupt_status,
            STATUS => self.status,
            // No shared memory region: all ones.
            SHM_SEL => 0,
            SHM_LEN_LOW | SHM_LEN_HIGH | SHM_BASE_LOW | SHM_BASE_HIGH => u32::MAX,
            CONFIG_GENERATION => self.config_generation,
            _ => return None,
        })
    }
}

impl MmioDevice for Mmio {
    fn advance_policy(&self) -> AdvancePolicy {
        AdvancePolicy::Passive
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn register_name(&self, offset: u64) -> Option<&'static str> {
        if offset >= CONFIG {
            return Some("CONFIG");
        }
        // The registers lie between BYTES and CONFIG in LAYOUT.
        LAYOUT
            .iter()
            .skip(1)
            .take_while(|&&(name, _)| name != "CONFIG")
            .find(|&&(_, value)| value == offset)
            .map(|&(name, _)| name)
    }

    fn read(&mut self, offset: u64, width: Width, _: &mut Access<'_>) -> Result<u64, MmioError> {
        let name = self.name;
        let refused = |reason| refuse(name, offset, width, Direction::Read, reason);
        if offset >= CONFIG {
            return self
                .device
                .config_read(offset - CONFIG, width.bytes())
                .ok_or_else(|| refused("past the configuration space"));
        }
        if width != Width::Word {
            return Err(refused("registers are 32-bit"));
        }
        self.word(offset)
            .map(u64::from)
            .ok_or_else(|| refused("no register to read"))
    }

    fn write(
        &mut self,
        offset: u64,
        width: Width,
        value: u64,
        access: &mut Access<'_>,
    ) -> Result<(), MmioError> {
        let name = self.name;
        let refused = |reason| refuse(name, offset, width, Direction::Write, reason);
        if offset >= CONFIG {
            return if self
                .device
                .config_write(offset - CONFIG, width.bytes(), value)
            {
                self.config_generation = self.config_generation.wrapping_add(1);
                if self.device.has_lines()
                    && let Some(notice) = self.notice
                {
                    notice.store(true, Ordering::Release);
                }
                Ok(())
            } else {
                Err(refused("the configuration space takes no such write"))
            };
        }
        if width != Width::Word {
            return Err(refused("registers are 32-bit"));
        }
        let value = value as u32;
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = value,
            DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            DRIVER_FEATURES => {
                if self.status & status::FEATURES_OK as u32 != 0 {
                    self.fail("the features changed after FEATURES_OK", access);
                } else {
                    match self.driver_features_sel {
                        0 => {
                            self.driver_features =
                                self.driver_features & !0xffff_ffff | u64::from(value);
                        }
                        1 => {
                            self.driver_features =
                                self.driver_features & 0xffff_ffff | u64::from(value) << 32;
                        }
                        // No feature lies past bit 63.
                        _ => {}
                    }
                }
            }
            QUEUE_SEL => self.queue_sel = value,
            QUEUE_NUM | QUEUE_DESC_LOW | QUEUE_DESC_HIGH | QUEUE_AVAIL_LOW | QUEUE_AVAIL_HIGH
            | QUEUE_USED_LOW | QUEUE_USED_HIGH => {
                self.program(offset, value, access).map_err(refused)?;
            }
            QUEUE_READY => {
                if let Some(index) = self.selected() {
                    match value {
                        0 => self.queues[index] = None,
                        1 => self.ready(index),
                        _ => return Err(refused("QUEUE_READY takes 0 or 1")),
                    }
                }
            }
            QUEUE_NOTIFY => {
                self.stats.notifies += 1;
                if (value as usize) < self.queues.len() {
                    let bit = 1u64 << value;
                    if self.heard & bit == 0 {
                        self.heard |= bit;
                        self.event(Event::Notified {
                            index: value as u16,
                        });
                    }
                    if self.queues[value as usize].is_some() {
                        self.notified |= bit;
                    }
                }
            }
            INTERRUPT_ACK => {
                self.interrupt_status &= !value;
                if self.interrupt_status == 0 {
                    access.interrupts.set_level(Wire::Aic(self.irq), false);
                }
            }
            STATUS => match value {
                0 => self.reset(access),
                value => self.set_status(value, access),
            },
            SHM_SEL => {}
            _ => return Err(refused("no register to write")),
        }
        Ok(())
    }
}
