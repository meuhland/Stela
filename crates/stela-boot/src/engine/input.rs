//! The host's keyboards and pointers, read between engine slices and fed to
//! the native USB peripheral the guest enumerates: every input device probe
//! found, whatever its transport ([`stela_drivers::input::InputDevice`]).
//!
//! Their drivers reach registers and DMA pages through the loader's
//! identity root, as the storage services do, so each poll runs in that
//! root (`storage::RestoreRoot`). The guest sees an input change at
//! its peripheral's next interrupt poll after the slice it arrived in. A
//! device that gave no input for a few seconds is polled every
//! [`IDLE_MILLIS`], so its first input waits that long. A device whose
//! polls cost more than a [`COST_SHARE`]th of that is polled as much less
//! often, but at most [`SLOW_MILLIS`] apart when idle and [`ACTIVE_MILLIS`]
//! when active, which bound its first input's wait while the host drivers'
//! lock is free. A posted GPU call is pumped between devices
//! (`gpu::pump_posted`).
//!
//! No call waits for the host drivers' lock for longer than
//! [`STARVED_WAIT_MICROS`]: the turn may be a step of a PE that runs guest
//! code, and another processor can hold the lock for longer than one exit
//! may take. A poll, an LED change or a stopped device's removal that finds
//! the lock busy stays due for the next turn. A device holds its input
//! meanwhile as far as its hardware does: a USB device one report in its
//! queued transfer and what it keeps itself behind the endpoint's NAKs, a
//! PS/2 device its own buffer behind the controller's.
//!
//! Each device's first polling and LED failure is reported once. A device
//! that stops is reported and dropped, so re-plugged keyboards do not pile
//! up as stopped devices polled for the rest of the boot, and the guest
//! gets a release of every key and button it held ([`HeldInputs`]). The
//! guest has one keyboard and one pointer for all host devices, so each
//! device's keys and buttons are kept apart and joined: a touchpad's report
//! does not let go of a mouse's drag, nor one keyboard's Shift the other's.
//!
//! Each read notes, for the first key press and the first pointer motion it
//! returns, the TSC when its poll saw the device's transfer complete and
//! how long after the device's previous poll that came; the delivery then
//! follows that input to the screen ([`stela_soc::input_latency`]), and the
//! gaps the polls achieved are reported (`INPUT POLL`).
use super::storage::{Inputs, RestoreRoot};
use crate::output;
use alloc::vec::Vec;
use stela_drivers::input::{HeldInputs, Input, InputDevice};
use stela_soc::input_latency::{self, Kind};
use stela_soc::machine::AppleDevices;

/// What was reported of one device, each kind once, and when it was read.
#[derive(Clone, Copy, Default)]
struct Reported {
    /// The device's identity while it is read ([`HeldInputs`]).
    id: u64,
    failed: bool,
    /// Its first USB transfer error, retried, was reported.
    transfer_error: bool,
    leds: bool,
    /// The keyboard LEDs last wanted have not reached the device yet.
    leds_due: bool,
    /// The counter when the device was last polled, and when a poll last
    /// found input.
    looked: u64,
    active: u64,
    /// Its polls' recent cost in counter ticks, without lock waits: an
    /// eighth of each poll joins seven eighths of the last.
    cost: u64,
    /// Its polls since the last [`TIMES_SECONDS`] report.
    times: Times,
}

/// One device's polls between reports, in 24 MHz counter ticks: how many,
/// the waits for the host-drivers lock before them and the polls' own time,
/// each in total and at most, and the turns a busy lock skipped it.
#[derive(Clone, Copy, Default)]
struct Times {
    polls: u64,
    waited: u64,
    longest_wait: u64,
    polled: u64,
    longest_poll: u64,
    skipped: u64,
}

/// How often the input devices' poll times are reported.
const TIMES_SECONDS: u64 = 10;

/// How often an idle device is polled. A poll can be a port or register
/// access a hypervisor takes to its device model, which waits there for
/// the model's lock, and the boot PE serves the host between slices of
/// guest code, holding back its guest, the devices' deadlines and the GPUs'
/// work. A device holds what arrives meanwhile: a USB device in its
/// transfer, a PS/2 device in its own buffer while the controller's is
/// full. USB low-speed keyboards and mice are polled as rarely.
const IDLE_MILLIS: u64 = 8;
/// How long a busy host-drivers lock may leave a due device unpolled before
/// a poll waits for the lock, for [`STARVED_WAIT_MICROS`] at most. A disk
/// command can hold it for tens of milliseconds; below this a due device is
/// polled at the next turn the lock is free.
const STARVED_MILLIS: u64 = 32;
/// How long a turn waits for the host drivers' lock for a device unpolled
/// past [`STARVED_MILLIS`], once for all its devices: about a service
/// step's length. The wait shows a holder that works in steps (the GPUs'
/// pump) that the lock is wanted, so it lets go at its next step; a holder
/// that keeps it longer leaves the poll due for the next turn.
const STARVED_WAIT_MICROS: u64 = 1000;
/// How long a device is polled at every service turn after it last gave
/// input, so a pointer's motion keeps its full rate.
const ACTIVE_SECONDS: u64 = 5;
/// A device's polls may take at most one part in this of the boot PE: a
/// poll costing more is spaced this many times its cost apart. A port read
/// that costs a microsecond on hardware can wait milliseconds for a
/// hypervisor's device-model lock; the boot PE's time is the guest's.
const COST_SHARE: u64 = 32;
/// The longest an idle device waits for its next poll however costly its
/// polls are, which its first input can wait.
const SLOW_MILLIS: u64 = 250;
/// The longest an active device waits for its next poll.
const ACTIVE_MILLIS: u64 = 32;

pub struct HostInput {
    inputs: Inputs,
    /// What was reported of each of `inputs.devices`, in the same order.
    reported: Vec<Reported>,
    /// The first input refusal, reported once.
    refused: bool,
    /// The guest's keyboard LEDs as last sent to the host keyboards.
    leds: Option<u8>,
    /// The guest's keyboard LEDs as the last delivery found them.
    wanted_leds: Option<u8>,
    /// The counter at the last poll-time report.
    reported_times: u64,
    /// What each device holds down, released when it stops.
    held: HeldInputs,
    /// The next device's identity.
    next_id: u64,
    /// Host keyboard and pointer changes given to the guest since boot
    /// (`INPUT reports`).
    delivered: Delivered,
    /// Each device's polls since the last report, in the same order as
    /// `reported` (`INPUT POLL`).
    polls: Vec<Polls>,
    /// The inputs of the last read to follow to the screen, by
    /// [`Kind`]: where each is in what the read returned, when it was
    /// read and how long after its device's previous poll
    /// ([`stela_soc::input_latency`]).
    followed: [Option<Followed>; 2],
}

/// An input of a read to follow to the screen ([`HostInput::followed`]).
#[derive(Clone, Copy)]
struct Followed {
    index: usize,
    read_at: u64,
    poll_gap: u64,
}

/// One device's polls since the last report, on the TSC: when it was last
/// polled, the gaps between its polls, and those before a poll that found
/// input, which a report waited in the host controller at most.
#[derive(Default)]
struct Polls {
    last: u64,
    gaps: Vec<u64>,
    found: Vec<u64>,
}

/// The poll gaps a report keeps of each device; later ones are left out.
const POLL_GAPS: usize = 4096;

impl Polls {
    /// A poll that began at TSC `at`; `found` when it read input. The gap
    /// before it, none for the device's first poll.
    fn polled(&mut self, at: u64, found: bool) -> u64 {
        let gap = if self.last == 0 {
            0
        } else {
            at.wrapping_sub(self.last)
        };
        self.last = at;
        if gap != 0 {
            if self.gaps.len() < POLL_GAPS {
                self.gaps.push(gap);
            }
            if found && self.found.len() < POLL_GAPS {
                self.found.push(gap);
            }
        }
        gap
    }
}

/// Host input changes given to the guest's peripheral since boot, by kind:
/// a key, or any pointer change (motion, position, buttons, wheels).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Delivered {
    keyboard: u64,
    pointer: u64,
}

impl Delivered {
    fn count(&mut self, input: &Input) {
        match input {
            Input::Key { .. } => self.keyboard += 1,
            _ => self.pointer += 1,
        }
    }
}

impl HostInput {
    pub fn new(inputs: Inputs) -> Self {
        for device in &inputs.devices {
            output::arguments(format_args!("INPUT host device {}\n", device.name()));
        }
        if inputs.devices.is_empty() {
            output::arguments(format_args!(
                "INPUT no host keyboard or pointer: the peripheral stays idle\n"
            ));
        }
        let reported = (0..inputs.devices.len() as u64)
            .map(|id| Reported {
                id,
                ..Reported::default()
            })
            .collect();
        Self {
            next_id: inputs.devices.len() as u64,
            polls: inputs.devices.iter().map(|_| Polls::default()).collect(),
            reported,
            inputs,
            refused: false,
            leds: None,
            wanted_leds: None,
            reported_times: 0,
            held: HeldInputs::new(),
            delivered: Delivered::default(),
            followed: [None; 2],
        }
    }

    /// Read `devices` too from now on: keyboards and pointers that arrived.
    pub fn add(&mut self, devices: Vec<InputDevice>) {
        for device in devices {
            output::arguments(format_args!("INPUT host device {}\n", device.name()));
            self.inputs.devices.push(device);
            self.reported.push(Reported {
                id: self.next_id,
                ..Reported::default()
            });
            self.polls.push(Polls::default());
            self.next_id += 1;
        }
    }

    fn deliver(&mut self, devices: &mut AppleDevices, input: Input) {
        self.delivered.count(&input);
        if let Err(error) = devices.input(input)
            && !core::mem::replace(&mut self.refused, true)
        {
            output::arguments(format_args!("INPUT refused {input:?}: {error}\n"));
        }
    }

    /// Deliver what [`Self::read`] read from the host's devices, and note
    /// the guest's lock-key LEDs for the next read to send. The inputs the
    /// read chose to follow to the screen are followed from here, unless
    /// one of their kind still is ([`stela_soc::input_latency::begin`]).
    pub fn deliver_all(&mut self, devices: &mut AppleDevices, inputs: Vec<Input>) {
        let followed = core::mem::take(&mut self.followed);
        for (index, input) in inputs.into_iter().enumerate() {
            if let Some(kind) = Kind::of(&input)
                && let Some(chosen) = followed[kind as usize].filter(|f| f.index == index)
            {
                input_latency::begin(kind, chosen.read_at, chosen.poll_gap);
            }
            self.deliver(devices, input);
        }
        self.wanted_leds = devices.keyboard_leds();
    }

    /// Read the host's input devices, without the guest's devices, in the
    /// order probe found them. The guest's lock-key LEDs reach each host
    /// keyboard first, as Linux `hidinput_led_worker` sends an input
    /// device's LED changes.
    pub fn read(
        &mut self,
        #[cfg(feature = "diagnostics")] turn: &mut super::service::InputTurn,
    ) -> Vec<Input> {
        if let Some(leds) = self.wanted_leds
            && self.leds != Some(leds)
        {
            self.leds = Some(leds);
            #[cfg(feature = "diagnostics")]
            {
                turn.led_changed = true;
            }
            for reported in &mut self.reported {
                reported.leds_due = true;
            }
        }
        if let Some(leds) = self.leds {
            for (device, reported) in self.inputs.devices.iter_mut().zip(&mut self.reported) {
                if !reported.leds_due || device.stopped() {
                    continue;
                }
                let set = {
                    // SAFETY: as below, for the device's controller. A busy
                    // lock leaves the change due for the next turn.
                    let Some(_root) = (unsafe { RestoreRoot::try_enter(self.inputs.root) }) else {
                        continue;
                    };
                    device.set_leds(leds)
                };
                reported.leds_due = false;
                if let Err(error) = set
                    && !core::mem::replace(&mut reported.leds, true)
                {
                    output::arguments(format_args!("INPUT {} LEDs: {error}\n", device.name()));
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            turn.led_done = super::host::rdtsc();
            turn.devices = self.inputs.devices.len();
        }
        let mut pending = Vec::new();
        let mut stopped = false;
        let now = super::host::fresh_counter();
        let millis = super::host::COUNTER_HZ / 1000;
        let starved = millis * STARVED_MILLIS;
        // Whether this turn may still wait for the lock.
        let mut may_wait = true;
        self.followed = [None; 2];
        let devices = self
            .inputs
            .devices
            .iter_mut()
            .zip(&mut self.reported)
            .zip(&mut self.polls);
        for ((device, reported), polls) in devices {
            // A stopped device a busy lock kept from being dropped.
            if device.stopped() {
                stopped = true;
                continue;
            }
            let active =
                now.wrapping_sub(reported.active) < super::host::COUNTER_HZ * ACTIVE_SECONDS;
            let interval = if active {
                (reported.cost * COST_SHARE).min(millis * ACTIVE_MILLIS)
            } else {
                (reported.cost * COST_SHARE).clamp(millis * IDLE_MILLIS, millis * SLOW_MILLIS)
            };
            let since = now.wrapping_sub(reported.looked);
            if since < interval {
                continue;
            }
            let before = pending.len();
            let entering = super::host::fresh_counter();
            let entered;
            let began;
            let polled = {
                // SAFETY: the loader root `storage::open` captured, which
                // maps this device.
                let root = unsafe {
                    if since >= interval + starved && core::mem::take(&mut may_wait) {
                        RestoreRoot::enter_within(self.inputs.root, STARVED_WAIT_MICROS)
                    } else {
                        RestoreRoot::try_enter(self.inputs.root)
                    }
                };
                let Some(_root) = root else {
                    reported.times.skipped += 1;
                    continue;
                };
                entered = super::host::fresh_counter();
                began = super::host::rdtsc();
                device.poll(&mut pending)
            };
            // The poll saw the device's transfers complete: what it read
            // was the host's from here.
            let read_at = super::host::rdtsc();
            self.held.polled(reported.id, &mut pending, before);
            let poll_gap = polls.polled(began, pending.len() != before);
            for (offset, input) in pending[before..].iter().enumerate() {
                if let Some(kind) = Kind::of(input)
                    && self.followed[kind as usize].is_none()
                {
                    self.followed[kind as usize] = Some(Followed {
                        index: before + offset,
                        read_at,
                        poll_gap,
                    });
                }
            }
            let times = &mut reported.times;
            let (waited, took) = (
                entered.wrapping_sub(entering),
                super::host::fresh_counter().wrapping_sub(entered),
            );
            times.polls += 1;
            times.waited += waited;
            times.longest_wait = times.longest_wait.max(waited);
            times.polled += took;
            times.longest_poll = times.longest_poll.max(took);
            reported.cost = reported.cost - reported.cost / 8 + took / 8;
            reported.looked = now;
            if pending.len() != before {
                reported.active = now;
            }
            if let Err(error) = polled
                && !core::mem::replace(&mut reported.failed, true)
            {
                output::arguments(format_args!("INPUT {}: {error}\n", device.name()));
            }
            // A USB HID interface's first transfer error, which it retries:
            // once, so a boot's log says what its endpoints met.
            if !reported.transfer_error
                && device.transfer_errors().0 != 0
                && let Some(error) = device.first_error()
            {
                reported.transfer_error = true;
                output::arguments(format_args!(
                    "INPUT {} transfer error, retried: {error}\n",
                    device.name()
                ));
            }
            stopped |= device.stopped();
            // A poll can take milliseconds in a hypervisor: a posted GPU call
            // does not wait for the remaining devices too.
            super::gpu::pump_posted();
        }
        if stopped {
            self.remove_stopped(&mut pending);
        }
        if now.wrapping_sub(self.reported_times) >= super::host::COUNTER_HZ * TIMES_SECONDS {
            self.reported_times = now;
            self.report_times();
        }
        pending
    }

    /// `INPUT reports keyboard=N pointer=N devices=N usb_errors=N
    /// usb_retry_windows=N`: the host keyboard and pointer changes given to
    /// the guest since boot, which grow as the host's keys and motion reach
    /// it; the devices still read; and their USB HID transfer errors and
    /// spent retry windows, each recovered. Then `INPUT TIMES
    /// name:polls/waited_us/longest_wait_us/polled_us/longest_poll_us/
    /// skipped …`: each device's polls since the last report, the
    /// host-drivers lock waits before them, and their own time; `INPUT
    /// POLL` ([`Self::report_polls`]); and an `INPUT LATENCY` line for each
    /// kind of input followed to the screen in the window
    /// ([`stela_soc::input_latency`]).
    fn report_times(&mut self) {
        let (errors, windows) = self
            .inputs
            .devices
            .iter()
            .map(InputDevice::transfer_errors)
            .fold((0, 0), |(e, w), (de, dw)| (e + de, w + dw));
        output::arguments(format_args!(
            "INPUT reports keyboard={} pointer={} devices={} usb_errors={errors} \
             usb_retry_windows={windows}\n",
            self.delivered.keyboard,
            self.delivered.pointer,
            self.inputs.devices.len(),
        ));
        let micros = |ticks: u64| ticks / (super::host::COUNTER_HZ / 1_000_000);
        let mut line = alloc::string::String::from("INPUT TIMES");
        for (device, reported) in self.inputs.devices.iter().zip(&mut self.reported) {
            let t = core::mem::take(&mut reported.times);
            let _ = core::fmt::Write::write_fmt(
                &mut line,
                format_args!(
                    " {}:{}/{}/{}/{}/{}/{}",
                    device.name().replace(' ', "_"),
                    t.polls,
                    micros(t.waited),
                    micros(t.longest_wait),
                    micros(t.polled),
                    micros(t.longest_poll),
                    t.skipped,
                ),
            );
        }
        output::arguments(format_args!("{line}\n"));
        self.report_polls();
        for kind in Kind::ALL {
            let report = input_latency::take(kind);
            if !report.is_empty() {
                output::arguments(format_args!("{report}\n"));
            }
        }
    }

    /// `INPUT POLL name:endpoint_us=N:gap_us=p50/p90/max:found_gap_us=
    /// p50/p90/max …`: each device's interrupt endpoint's service interval
    /// as its descriptor asks (0 for a device that has none), the gaps its
    /// polls since the last report achieved, and the gaps before the polls
    /// that found input: a report waits in the host controller for up to
    /// that long, as the host is polled, not interrupted.
    fn report_polls(&mut self) {
        let hz = u128::from(input_latency::hz().max(1));
        let mut line = alloc::string::String::from("INPUT POLL");
        for (device, polls) in self.inputs.devices.iter().zip(&mut self.polls) {
            let percentiles = |gaps: &mut Vec<u64>| {
                let [p50, p90, max] = input_latency::percentiles(gaps);
                gaps.clear();
                [p50, p90, max].map(|ticks| u128::from(ticks) * 1_000_000 / hz)
            };
            let gaps = percentiles(&mut polls.gaps);
            let found = percentiles(&mut polls.found);
            let _ = core::fmt::Write::write_fmt(
                &mut line,
                format_args!(
                    " {}:endpoint_us={}:gap_us={}/{}/{}:found_gap_us={}/{}/{}",
                    device.name().replace(' ', "_"),
                    device.interval_us().unwrap_or(0),
                    gaps[0],
                    gaps[1],
                    gaps[2],
                    found[0],
                    found[1],
                    found[2],
                ),
            );
        }
        output::arguments(format_args!("{line}\n"));
    }

    /// Report and drop every device that stopped, appending to `pending` a
    /// release of every key and button it held, at once even while a busy
    /// lock keeps the device. Dropping one can stop its transfers, so it
    /// runs in the loader's root as a poll does; a device a busy lock keeps
    /// is dropped at a later turn, unpolled meanwhile.
    fn remove_stopped(&mut self, pending: &mut Vec<Input>) {
        let mut index = 0;
        while index < self.inputs.devices.len() {
            if !self.inputs.devices[index].stopped() {
                index += 1;
                continue;
            }
            self.held.stopped(self.reported[index].id, pending);
            // SAFETY: the loader root `storage::open` captured, which maps
            // this device; the processor's pinned record is active.
            let Some(root) = (unsafe { RestoreRoot::try_enter(self.inputs.root) }) else {
                index += 1;
                continue;
            };
            let device = self.inputs.devices.remove(index);
            self.reported.remove(index);
            self.polls.remove(index);
            let name = device.name();
            let why = device
                .first_error()
                .map_or_else(alloc::string::String::new, |error| {
                    alloc::format!(": {error}")
                });
            drop(device);
            drop(root);
            output::arguments(format_args!("INPUT {name} stopped and removed{why}\n"));
        }
    }
}
