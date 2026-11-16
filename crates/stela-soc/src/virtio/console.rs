//! The console device (virtio 1.2, 5.3): one port, its receive queue 0 and
//! transmit queue 1. No `MULTIPORT` and no `SIZE`; `EMERG_WRITE` is offered,
//! its configuration word taking one byte of output at a time.
//!
//! What the guest transmits goes to the boot log, a line at a time
//! ([`Console::take_lines`]): each line ends at `\n` (a `\r` before it is
//! dropped, as a tty writes `\r\n`) or at [`LINE_BYTES`], and is escaped
//! where it is not printable ASCII ([`escape`]). The log takes at most
//! [`RATE_LINES`] lines a second after a burst of [`BURST_LINES`], and at most
//! [`LINES_KEPT`] between two reports; the others are counted as dropped.
//!
//! What the guest receives comes from an [`Input`]: the host channel behind
//! the console. None exists yet, so a boot gives either [`Silent`] or the
//! test source [`Echo`], which greets the driver once at `DRIVER_OK` and
//! sends each transmitted line back prefixed with [`ECHO_PREFIX`]. Bytes
//! wait in a bounded buffer ([`Received`]) until the driver posts receive
//! buffers, and are dropped and counted past it.
use super::queue::Queue;
use super::{Served, VirtioDevice, ring_error};
use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};
use core::fmt::Write as _;
use stela_contracts::{dma::Memory, virtio::ids};

/// The receive queue (the guest's input) and the transmit queue.
pub const RECEIVE: usize = 0;
pub const TRANSMIT: usize = 1;
/// The most entries each queue takes.
pub const QUEUE_MAX: u16 = 128;
/// `VIRTIO_CONSOLE_F_EMERG_WRITE`: the `emerg_wr` configuration word.
pub const F_EMERG_WRITE: u64 = 1 << 2;
/// `emerg_wr`'s offset in the configuration (after `cols`, `rows` and
/// `max_nr_ports`), and the configuration's bytes.
const EMERG_WR: u64 = 8;
const CONFIG_BYTES: u64 = 12;
/// A line longer than this is logged in parts.
pub const LINE_BYTES: usize = 160;
/// Lines the log takes a second, after a burst of [`BURST_LINES`].
pub const RATE_LINES: u64 = 20;
pub const BURST_LINES: u64 = 40;
/// Lines kept for the log between two reports.
pub const LINES_KEPT: usize = 64;
/// Bytes waiting for the guest's receive buffers at most.
pub const RECEIVED_BYTES: usize = 4096;
/// What [`Echo`] puts before each line it sends back. It never echoes a
/// line that starts with it, so a guest tty that echoes its input back out
/// does not loop.
pub const ECHO_PREFIX: &[u8] = b"echo: ";
/// What [`Echo`] sends once, when the driver first sets `DRIVER_OK`.
pub const GREETING: &[u8] = b"stela: virtio console up, host echo on\n";
/// Chains one step takes from each queue at most, bounding its work under
/// the devices' lock.
const STEP_CHAINS: usize = 64;
/// The bytes one chain moves at most; longer transmit chains are refused.
const CHAIN_BYTES: usize = 64 * 1024;

/// The bytes waiting for the guest, at most [`RECEIVED_BYTES`].
#[derive(Debug, Default)]
pub struct Received {
    bytes: VecDeque<u8>,
    dropped: u64,
}

impl Received {
    /// Queues `bytes` for the guest; those past the bound are dropped.
    pub fn push(&mut self, bytes: &[u8]) {
        let room = RECEIVED_BYTES - self.bytes.len();
        let taken = bytes.len().min(room);
        self.bytes.extend(&bytes[..taken]);
        self.dropped += (bytes.len() - taken) as u64;
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Bytes dropped, the buffer being full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// A host channel feeding the guest's console input. A real one (a serial
/// port, a TCP socket, the Proxmox console) polls its source in
/// [`Self::poll`] and says it is [`Self::ready`] when it has bytes.
pub trait Input {
    /// The driver set `DRIVER_OK`.
    fn started(&mut self, _received: &mut Received) {}
    /// The guest transmitted `line` (its end left out, unescaped).
    fn transmitted(&mut self, _line: &[u8], _received: &mut Received) {}
    /// Gives what the host has for the guest now; each step asks.
    fn poll(&mut self, _received: &mut Received) {}
    /// Whether [`Self::poll`] has bytes with no guest action, which makes
    /// the device due.
    fn ready(&self) -> bool {
        false
    }
}

/// No input: the guest receives nothing.
pub struct Silent;

impl Input for Silent {}

/// The test source: [`GREETING`] once, and each transmitted line back.
pub struct Echo {
    greeted: bool,
}

impl Echo {
    pub fn new() -> Self {
        Self { greeted: false }
    }
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

impl Input for Echo {
    fn started(&mut self, received: &mut Received) {
        if !core::mem::replace(&mut self.greeted, true) {
            received.push(GREETING);
        }
    }

    fn transmitted(&mut self, line: &[u8], received: &mut Received) {
        if line.is_empty()
            || line.starts_with(ECHO_PREFIX)
            || line == &GREETING[..GREETING.len() - 1]
        {
            return;
        }
        received.push(ECHO_PREFIX);
        received.push(line);
        received.push(b"\n");
    }
}

/// `line` as the log shows it: printable ASCII as is, `\\`, `\t` and `\r`
/// as such, every other byte as `\xHH`.
pub fn escape(line: &[u8]) -> String {
    let mut text = String::with_capacity(line.len());
    for &byte in line {
        match byte {
            b'\\' => text.push_str("\\\\"),
            b'\t' => text.push_str("\\t"),
            b'\r' => text.push_str("\\r"),
            0x20..=0x7e => text.push(byte as char),
            _ => write!(text, "\\x{byte:02x}").unwrap(),
        }
    }
    text
}

/// The console's own counters, for the boot log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Bytes the guest transmitted, and the lines they made.
    pub transmitted: u64,
    pub lines: u64,
    /// Lines the log did not take (the rate, or too many between reports).
    pub dropped_lines: u64,
    /// Bytes the guest received, and those dropped before it could.
    pub received: u64,
    pub dropped_received: u64,
}

pub struct Console {
    input: Box<dyn Input>,
    received: Received,
    /// The line being transmitted.
    line: Vec<u8>,
    /// Lines for the log, escaped, since the last [`Self::take_lines`].
    lines: Vec<String>,
    /// The log's allowance in lines, and when it was last topped up.
    allowance: u64,
    topped: Option<u64>,
    counters: Counters,
    /// The last step found no receive buffer for the bytes waiting.
    starved: bool,
    /// The last step left transmit chains for the next.
    pending: bool,
    buffer: Vec<u8>,
}

impl Console {
    pub fn new(input: Box<dyn Input>) -> Self {
        Self {
            input,
            received: Received::default(),
            line: Vec::new(),
            lines: Vec::new(),
            allowance: BURST_LINES,
            topped: None,
            counters: Counters::default(),
            starved: false,
            pending: false,
            buffer: vec![0; CHAIN_BYTES],
        }
    }

    pub fn counters(&self) -> Counters {
        Counters {
            dropped_received: self.received.dropped(),
            ..self.counters
        }
    }

    /// Whether the log may take one more line at `now` (counter ticks).
    fn allowed(&mut self, now: u64) -> bool {
        let hz = crate::counter::FREQUENCY_HZ;
        let topped = *self.topped.get_or_insert(now);
        let earned = now.saturating_sub(topped).saturating_mul(RATE_LINES) / hz;
        if earned > 0 {
            self.allowance = (self.allowance + earned).min(BURST_LINES);
            self.topped = Some(topped + earned * hz / RATE_LINES);
        }
        if self.allowance == 0 || self.lines.len() >= LINES_KEPT {
            return false;
        }
        self.allowance -= 1;
        true
    }

    /// The line being transmitted ends: to the input, and to the log.
    fn end_line(&mut self, now: u64) {
        let mut line = core::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        self.input.transmitted(&line, &mut self.received);
        self.counters.lines += 1;
        if self.allowed(now) {
            self.lines.push(escape(&line));
        } else {
            self.counters.dropped_lines += 1;
        }
    }

    /// The guest's output, byte by byte.
    fn output(&mut self, bytes: &[u8], now: u64) {
        self.counters.transmitted += bytes.len() as u64;
        for &byte in bytes {
            if byte == b'\n' {
                self.end_line(now);
                continue;
            }
            self.line.push(byte);
            if self.line.len() >= LINE_BYTES {
                self.end_line(now);
            }
        }
    }

    fn transmit(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        event_idx: bool,
        now: u64,
        served: &mut Served,
    ) -> Result<(), &'static str> {
        let mut taken = 0;
        while taken < STEP_CHAINS {
            let Some(chain) = queue.pop(memory).map_err(ring_error)? else {
                break;
            };
            if !chain.all(false) {
                return Err("a console transmit buffer the device may write");
            }
            let bytes = chain.bytes() as usize;
            if bytes > CHAIN_BYTES {
                return Err("a console transmit chain past 64 KiB");
            }
            let mut data = core::mem::take(&mut self.buffer);
            chain
                .read(memory, 0, &mut data[..bytes])
                .map_err(ring_error)?;
            self.output(&data[..bytes], now);
            self.buffer = data;
            queue.push_used(memory, chain.head, 0).map_err(ring_error)?;
            served.chains += 1;
            taken += 1;
        }
        self.pending = taken == STEP_CHAINS && queue.pending(memory).map_err(ring_error)? != 0;
        if event_idx {
            if self.pending {
                queue.no_kick(memory).map_err(ring_error)?;
            } else {
                queue.want_kick(memory).map_err(ring_error)?;
            }
        }
        served.interrupt |= queue.publish(memory, event_idx).map_err(ring_error)?;
        Ok(())
    }

    fn receive(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        event_idx: bool,
        served: &mut Served,
    ) -> Result<(), &'static str> {
        let mut taken = 0;
        while !self.received.is_empty() && taken < STEP_CHAINS {
            let Some(chain) = queue.pop(memory).map_err(ring_error)? else {
                // No buffer: hear of the next, unless one came meanwhile.
                if event_idx && queue.want_kick_past_seen(memory).map_err(ring_error)? {
                    continue;
                }
                self.starved = true;
                break;
            };
            if !chain.all(true) {
                return Err("a console receive buffer the device may not write");
            }
            let bytes = (chain.bytes() as usize)
                .min(self.received.len())
                .min(CHAIN_BYTES);
            let data: Vec<u8> = self.received.bytes.drain(..bytes).collect();
            chain.write(memory, 0, &data).map_err(ring_error)?;
            queue
                .push_used(memory, chain.head, bytes as u32)
                .map_err(ring_error)?;
            self.counters.received += bytes as u64;
            served.chains += 1;
            served.bytes += bytes as u64;
            taken += 1;
        }
        if event_idx && !self.starved {
            queue.want_kick(memory).map_err(ring_error)?;
        }
        served.interrupt |= queue.publish(memory, event_idx).map_err(ring_error)?;
        Ok(())
    }
}

impl VirtioDevice for Console {
    fn name(&self) -> &'static str {
        "virtio-console"
    }

    fn id(&self) -> u32 {
        ids::CONSOLE as u32
    }

    fn features(&self) -> u64 {
        F_EMERG_WRITE
    }

    fn queue_max(&self) -> &'static [u16] {
        &[QUEUE_MAX, QUEUE_MAX]
    }

    fn config_read(&self, offset: u64, bytes: u64) -> Option<u64> {
        // cols, rows and max_nr_ports, none offered, read 0; emerg_wr too.
        (offset.checked_add(bytes)? <= CONFIG_BYTES).then_some(0)
    }

    fn config_write(&mut self, offset: u64, bytes: u64, value: u64) -> bool {
        if offset != EMERG_WR || bytes != 4 {
            return false;
        }
        // No clock here: an emergency line is logged whatever the rate.
        let topped = self.topped;
        self.output(&[value as u8], topped.unwrap_or(0));
        true
    }

    fn activate(&mut self, _features: u64) {
        self.input.started(&mut self.received);
    }

    fn reset(&mut self) {
        self.line.clear();
        self.received.bytes.clear();
        self.starved = false;
        self.pending = false;
    }

    fn due(&self) -> bool {
        self.pending || (!self.received.is_empty() && !self.starved) || self.input.ready()
    }

    fn take_lines(&mut self) -> Vec<String> {
        core::mem::take(&mut self.lines)
    }

    fn has_lines(&self) -> bool {
        !self.lines.is_empty()
    }

    fn counters(&self) -> Vec<(&'static str, u64)> {
        let counters = Console::counters(self);
        vec![
            ("tx_bytes", counters.transmitted),
            ("tx_lines", counters.lines),
            ("tx_dropped", counters.dropped_lines),
            ("rx_bytes", counters.received),
            ("rx_dropped", counters.dropped_received),
        ]
    }

    fn step(
        &mut self,
        queues: &mut [Option<Queue>],
        notified: u64,
        memory: &mut dyn Memory,
        event_idx: bool,
        now: u64,
    ) -> Result<Served, &'static str> {
        let mut served = Served::default();
        // Transmit first: an echo lands in what the guest receives next.
        if let Some(queue) = queues.get_mut(TRANSMIT).and_then(Option::as_mut) {
            self.transmit(queue, memory, event_idx, now, &mut served)?;
        }
        self.input.poll(&mut self.received);
        if notified & 1 << RECEIVE != 0 {
            self.starved = false;
        }
        if let Some(queue) = queues.get_mut(RECEIVE).and_then(Option::as_mut)
            && !self.received.is_empty()
            && !self.starved
        {
            self.receive(queue, memory, event_idx, &mut served)?;
        }
        Ok(served)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Access, MmioDevice, Width};
    use crate::virtio::mmio::Mmio;
    use core::sync::atomic::{AtomicBool, Ordering};
    use stela_contracts::dma::test_support::Ram;
    use stela_contracts::interrupt::{AicInput, InterruptSink, Wire};
    use stela_contracts::virtio::{features, mmio::*, ring, status};

    const RAM: u64 = 0x8_0000_0000;
    const IRQ: AicInput = AicInput::new(0, 0x2a3);
    const SIZE: u64 = 8;
    const BUFFERS: u64 = RAM + 0x1_0000;
    const SECOND: u64 = crate::counter::FREQUENCY_HZ;

    /// Queue `queue`'s descriptor table, available ring and used ring.
    fn areas(queue: usize) -> [u64; 3] {
        let base = RAM + 0x4000 * queue as u64;
        [base, base + 0x1000, base + 0x2000]
    }

    /// A buffer of the test's, 0x400 bytes apart.
    fn buffer(slot: u16) -> u64 {
        BUFFERS + 0x400 * u64::from(slot)
    }

    #[derive(Default)]
    struct Line {
        level: bool,
    }

    impl InterruptSink for Line {
        fn set_level(&mut self, wire: Wire, level: bool) {
            assert_eq!(wire, Wire::Aic(IRQ));
            self.level = level;
        }
    }

    struct Bench {
        device: Mmio,
        ram: Ram,
        line: Line,
        now: u64,
        /// Each queue's available index, and how far the test read its used ring.
        available: [u16; 2],
        harvested: [u16; 2],
    }

    impl Bench {
        fn new(input: Box<dyn Input>, notice: Option<&'static AtomicBool>) -> Self {
            Self {
                device: Mmio::new(Box::new(Console::new(input)), IRQ, notice),
                ram: Ram::new(RAM, 0x2_0000),
                line: Line::default(),
                now: 0,
                available: [0; 2],
                harvested: [0; 2],
            }
        }

        fn write(&mut self, register: u64, value: u32) {
            let mut access = Access::new(0, self.now, &mut self.line);
            self.device
                .write(register, Width::Word, u64::from(value), &mut access)
                .unwrap();
        }

        fn read(&mut self, register: u64) -> u32 {
            let mut access = Access::new(0, self.now, &mut self.line);
            self.device
                .read(register, Width::Word, &mut access)
                .unwrap() as u32
        }

        fn step(&mut self) {
            let mut access = Access::new(0, self.now, &mut self.line);
            self.device.step(&mut self.ram, &mut access);
        }

        /// The driver's start: both queues, then DRIVER_OK and a first step.
        fn start(&mut self, accepted: u64) {
            self.write(STATUS, 0);
            self.write(STATUS, (status::ACKNOWLEDGE | status::DRIVER) as u32);
            for select in 0..2 {
                self.write(DRIVER_FEATURES_SEL, select);
                self.write(DRIVER_FEATURES, (accepted >> (32 * select)) as u32);
            }
            let now = self.read(STATUS);
            self.write(STATUS, now | status::FEATURES_OK as u32);
            assert_ne!(self.read(STATUS) & status::FEATURES_OK as u32, 0);
            for queue in [RECEIVE, TRANSMIT] {
                self.write(QUEUE_SEL, queue as u32);
                self.write(QUEUE_NUM, SIZE as u32);
                for (low, address) in [QUEUE_DESC_LOW, QUEUE_AVAIL_LOW, QUEUE_USED_LOW]
                    .into_iter()
                    .zip(areas(queue))
                {
                    self.write(low, address as u32);
                    self.write(low + 4, (address >> 32) as u32);
                }
                self.write(QUEUE_READY, 1);
                assert_eq!(self.read(QUEUE_READY), 1);
            }
            let now = self.read(STATUS);
            self.write(STATUS, now | status::DRIVER_OK as u32);
            self.step();
        }

        /// Makes `slot`'s buffer of `bytes` the next available chain of
        /// `queue`, the device writing it or not.
        fn post(&mut self, queue: usize, slot: u16, bytes: u32, write: bool) {
            let [desc, avail, _] = areas(queue);
            let flags = if write { ring::DESC_F_WRITE } else { 0 };
            let descriptor = [
                buffer(slot).to_le_bytes().as_slice(),
                &bytes.to_le_bytes(),
                &flags.to_le_bytes(),
                &[0; 2],
            ]
            .concat();
            self.ram.write(desc + 16 * u64::from(slot), &descriptor);
            let index = self.available[queue];
            self.ram.write(
                avail + 4 + 2 * (u64::from(index) % SIZE),
                &slot.to_le_bytes(),
            );
            self.available[queue] = index.wrapping_add(1);
            self.ram
                .write(avail + 2, &self.available[queue].to_le_bytes());
        }

        fn notify(&mut self, queue: usize) {
            self.write(QUEUE_NOTIFY, queue as u32);
            self.step();
        }

        /// The guest writes `text` through transmit buffer `slot`.
        fn transmit(&mut self, slot: u16, text: &[u8]) {
            self.ram.write(buffer(slot), text);
            self.post(TRANSMIT, slot, text.len() as u32, false);
            self.notify(TRANSMIT);
        }

        /// The used entries of `queue` since the last call: (id, len).
        fn used(&mut self, queue: usize) -> Vec<(u32, u32)> {
            let [_, _, used] = areas(queue);
            let mut index = [0u8; 2];
            self.ram.read(used + 2, &mut index);
            let index = u16::from_le_bytes(index);
            let mut entries = Vec::new();
            while self.harvested[queue] != index {
                let mut entry = [0u8; 8];
                let at = u64::from(self.harvested[queue]) % SIZE;
                self.ram.read(used + 4 + 8 * at, &mut entry);
                entries.push((
                    u32::from_le_bytes(entry[..4].try_into().unwrap()),
                    u32::from_le_bytes(entry[4..].try_into().unwrap()),
                ));
                self.harvested[queue] = self.harvested[queue].wrapping_add(1);
            }
            entries
        }

        fn received(&mut self, slot: u16, bytes: u32) -> Vec<u8> {
            let mut data = vec![0u8; bytes as usize];
            self.ram.read(buffer(slot), &mut data);
            data
        }

        fn lines(&mut self) -> Vec<String> {
            self.device.take_lines()
        }

        fn counter(&self, name: &str) -> u64 {
            self.device
                .device_counters()
                .into_iter()
                .find(|(counter, _)| *counter == name)
                .unwrap()
                .1
        }
    }

    #[test]
    fn the_page_is_a_two_queue_console_with_emergency_writes() {
        let mut bench = Bench::new(Box::new(Silent), None);
        assert_eq!(bench.read(DEVICE_ID), ids::CONSOLE as u32);
        bench.write(DEVICE_FEATURES_SEL, 0);
        assert_eq!(
            u64::from(bench.read(DEVICE_FEATURES)),
            F_EMERG_WRITE | features::EVENT_IDX
        );
        for (queue, most) in [(0, u32::from(QUEUE_MAX)), (1, u32::from(QUEUE_MAX)), (2, 0)] {
            bench.write(QUEUE_SEL, queue);
            assert_eq!(bench.read(QUEUE_NUM_MAX), most);
        }
        let mut access = Access::new(0, 0, &mut bench.line);
        assert_eq!(
            bench.device.read(CONFIG, Width::Word, &mut access).unwrap(),
            0
        );
        assert_eq!(
            bench
                .device
                .read(CONFIG + 4, Width::Word, &mut access)
                .unwrap(),
            0
        );
        assert!(
            bench
                .device
                .read(CONFIG + 12, Width::Byte, &mut access)
                .is_err()
        );
        // emerg_wr: one byte of output a write, logged by the line.
        bench.start(features::VERSION_1 | F_EMERG_WRITE);
        for byte in b"panic!\n" {
            bench.write(CONFIG + 8, u32::from(*byte));
        }
        assert_eq!(bench.lines(), ["panic!"]);
        let mut access = Access::new(0, 0, &mut bench.line);
        assert!(
            bench
                .device
                .write(CONFIG, Width::Word, 1, &mut access)
                .is_err(),
            "cols is not the driver's"
        );
    }

    #[test]
    fn transmitted_lines_reach_the_log_escaped() {
        let notice: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let mut bench = Bench::new(Box::new(Silent), Some(notice));
        bench.start(features::VERSION_1);
        notice.store(false, Ordering::Release);
        bench.transmit(0, b"hello\r\n");
        assert!(notice.load(Ordering::Acquire), "the log hears of the line");
        assert_eq!(bench.used(TRANSMIT), [(0, 0)]);
        assert!(bench.line.level, "the driver hears its buffer is done");
        bench.write(INTERRUPT_ACK, INT_VRING as u32);
        // Unprintable bytes escaped; a line across two buffers is one line.
        bench.transmit(1, b"a\tb\x01\\c\xff\rd\n");
        bench.transmit(2, b"par");
        bench.transmit(3, b"tial\nnext");
        assert_eq!(
            bench.lines(),
            ["hello", "a\\tb\\x01\\\\c\\xff\\rd", "partial"]
        );
        // A long line is logged in parts of LINE_BYTES.
        let long = [b'x'; LINE_BYTES + 40];
        bench.transmit(4, &long);
        bench.transmit(5, b"\n");
        let lines = bench.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], format!("next{}", "x".repeat(LINE_BYTES - 4)));
        assert_eq!(lines[1], "x".repeat(44));
        assert_eq!(bench.counter("tx_lines"), 5);
        assert_eq!(bench.counter("tx_dropped"), 0);
        assert_eq!(bench.counter("tx_bytes"), 7 + 10 + 3 + 9 + 200 + 1);
        assert_eq!(bench.counter("rx_bytes"), 0, "no input");
    }

    #[test]
    fn the_log_takes_lines_at_its_rate_and_counts_the_rest() {
        let mut bench = Bench::new(Box::new(Silent), None);
        bench.start(features::VERSION_1);
        // A burst at once: BURST_LINES taken.
        bench.transmit(0, &b"line\n".repeat(50));
        assert_eq!(bench.lines().len() as u64, BURST_LINES);
        assert_eq!(bench.counter("tx_dropped"), 50 - BURST_LINES);
        // A second on, RATE_LINES more.
        bench.now += SECOND;
        bench.transmit(1, &b"line\n".repeat(30));
        assert_eq!(bench.lines().len() as u64, RATE_LINES);
        assert_eq!(
            bench.counter("tx_dropped"),
            50 - BURST_LINES + 30 - RATE_LINES
        );
        // Half a second earns half the rate.
        bench.now += SECOND / 2;
        bench.transmit(2, &b"line\n".repeat(30));
        assert_eq!(bench.lines().len() as u64, RATE_LINES / 2);
        // Lines the log has not taken are kept to LINES_KEPT.
        for slot in 3..5 {
            bench.now += 10 * SECOND;
            bench.transmit(slot, &b"line\n".repeat(BURST_LINES as usize));
        }
        assert_eq!(bench.lines().len(), LINES_KEPT);
        assert_eq!(bench.counter("tx_lines"), 50 + 30 + 30 + 2 * BURST_LINES);
    }

    #[test]
    fn echo_greets_once_and_sends_each_line_back() {
        let mut bench = Bench::new(Box::new(Echo::new()), None);
        bench.start(features::VERSION_1 | features::EVENT_IDX);
        // The greeting waits for a receive buffer, and the device for a kick.
        assert!(!bench.device.due());
        bench.post(RECEIVE, 0, 0x100, true);
        bench.post(RECEIVE, 1, 0x100, true);
        bench.notify(RECEIVE);
        assert_eq!(bench.used(RECEIVE), [(0, GREETING.len() as u32)]);
        assert_eq!(bench.received(0, GREETING.len() as u32), GREETING);
        // A line, then its echo in the next receive buffer, in one step.
        bench.transmit(0, b"hello\r\n");
        assert_eq!(bench.used(RECEIVE), [(1, 12)]);
        assert_eq!(bench.received(1, 12), b"echo: hello\n");
        assert_eq!(bench.lines(), ["hello"]);
        // The guest tty echoing what it received is not echoed again, nor
        // the greeting.
        bench.post(RECEIVE, 2, 0x100, true);
        bench.notify(RECEIVE);
        bench.transmit(1, b"echo: hello\r\n");
        bench.transmit(2, GREETING);
        assert!(bench.used(RECEIVE).is_empty());
        // Two lines' echoes share the buffer posted before them.
        bench.transmit(3, b"one\ntwo\n");
        assert_eq!(bench.used(RECEIVE), [(2, 20)]);
        assert_eq!(bench.received(2, 20), b"echo: one\necho: two\n");
        // With no receive buffer the echo waits; a buffer brings it.
        bench.transmit(4, b"three\n");
        assert!(bench.used(RECEIVE).is_empty());
        assert!(!bench.device.due(), "starved, it waits for a kick");
        bench.post(RECEIVE, 3, 4, true);
        bench.notify(RECEIVE);
        // A short buffer takes what fits; the rest waits for the next.
        assert_eq!(bench.used(RECEIVE), [(3, 4)]);
        assert_eq!(bench.received(3, 4), b"echo");
        bench.post(RECEIVE, 4, 0x100, true);
        bench.notify(RECEIVE);
        assert_eq!(bench.used(RECEIVE), [(4, 8)]);
        assert_eq!(bench.received(4, 8), b": three\n");
        assert_eq!(
            bench.counter("rx_bytes"),
            GREETING.len() as u64 + 12 + 20 + 12
        );
        // A reset and a new start on fresh rings: no second greeting.
        bench.write(STATUS, 0);
        bench.ram.write(RAM, &[0; 0x8000]);
        bench.available = [0; 2];
        bench.harvested = [0; 2];
        bench.start(features::VERSION_1);
        bench.post(RECEIVE, 0, 0x100, true);
        bench.notify(RECEIVE);
        assert!(bench.used(RECEIVE).is_empty());
    }

    #[test]
    fn what_waits_for_the_guest_is_bounded() {
        let mut received = Received::default();
        received.push(&[0; RECEIVED_BYTES - 10]);
        received.push(&[1; 30]);
        assert_eq!((received.len(), received.dropped()), (RECEIVED_BYTES, 20));
        assert_eq!(
            escape(b"ok \x7f\x00\xe2\x82\xac"),
            "ok \\x7f\\x00\\xe2\\x82\\xac"
        );
    }

    #[test]
    fn a_transmit_buffer_the_device_may_write_stops_it() {
        let mut bench = Bench::new(Box::new(Silent), None);
        bench.start(features::VERSION_1);
        bench.post(TRANSMIT, 0, 16, true);
        bench.notify(TRANSMIT);
        assert_eq!(
            bench.device.error(),
            Some("a console transmit buffer the device may write")
        );
        let mut bench = Bench::new(Box::new(Echo::new()), None);
        bench.start(features::VERSION_1);
        bench.post(RECEIVE, 0, 16, false);
        bench.notify(RECEIVE);
        assert_eq!(
            bench.device.error(),
            Some("a console receive buffer the device may not write")
        );
    }
}
