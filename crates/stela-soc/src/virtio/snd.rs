//! The sound device (virtio 1.2, 5.14; `stela_contracts::virtio_snd`): one
//! output PCM stream, one jack and one channel map, on the control (0),
//! event (1), TX (2) and RX (3) queues. No device features; no input
//! stream, so the RX queue and the event queue are never used.
//!
//! **The stream.** 2 channels, S16 or S32, 48 or 44.1 kHz, in the states of
//! 5.14.6.6.1: `SET_PARAMS`, `PREPARE`, `START`, `STOP`, `RELEASE`; a request
//! the state does not allow answers `BAD_MSG`, parameters the device does
//! not offer `NOT_SUPP`. `JACK_INFO`, `PCM_INFO` and `CHMAP_INFO` describe
//! the jack, the stream and the map; every other request is `NOT_SUPP`.
//!
//! **Pacing.** Each TX transfer is a readable `virtio_snd_pcm_xfer`, the
//! frames, and a writable `virtio_snd_pcm_status`. The device takes it at
//! once and keeps it, queued after the ones before, and plays the queue at
//! the [`Sink`]'s clock: as the clock reaches a transfer it hands the sink
//! its frames, read from guest memory then, and as the clock passes its
//! last frame it completes it, `S_OK` with `latency_bytes` the bytes still
//! queued after it. The sink's clock runs on the host's counter
//! ([`Clock`]), so the guest's engine runs at the host's rate and sees its
//! own clock's error in the completions' timing, rather than the device
//! taking what the guest's time says. A clock that passes the queue's end
//! plays silence (an underrun), and the next transfer plays from where the
//! clock is. The step comes back for the next completion through
//! [`VirtioDevice::deadline`].
//!
//! **Log.** Each request the stream takes or refuses, its parameters and
//! state changes are notes for the boot log, and every [`REPORT_SECONDS`]
//! while it plays (and when it stops) a line with the frames, the
//! interval's peak and RMS, the underruns and the queued latency. The
//! sink's own notes (the capture's fingerprint) join them.
pub mod sink;
#[cfg(test)]
mod tests;

use super::queue::{Chain, Queue};
use super::{Served, VirtioDevice, ring_error};
use alloc::{boxed::Box, collections::VecDeque, format, string::String, vec, vec::Vec};
use sink::{Format, Sink};
use stela_contracts::{
    dma::Memory,
    virtio::ids,
    virtio_snd::{codes::*, messages::*, rate_hz, sample_bytes},
};

/// The most entries of the control, event, TX and RX queues.
pub const QUEUE_MAX: [u16; 4] = [64, 64, 256, 64];
/// The device's jacks, streams and channel maps.
pub const JACKS: u32 = 1;
pub const STREAMS: u32 = 1;
pub const CHMAPS: u32 = 1;
/// The stream's channels, formats and rates.
pub const CHANNELS: u8 = 2;
pub const FORMATS: u64 = 1 << FMT_S16 | 1 << FMT_S32;
pub const RATES: u64 = 1 << RATE_44100 | 1 << RATE_48000;
/// The jack's HDA pin default configuration: a line out at the rear, green
/// 1/8" jack, association 1 [mem: HDA 7.3.3.31's fields].
pub const JACK_DEFCONF: u32 = 0x0101_4010;
/// The PCM bytes one transfer carries at most.
pub const XFER_BYTES: u64 = 64 * 1024;
/// A control request's bytes the device reads at most.
const REQUEST_BYTES: usize = 64;
/// Chains one step takes from the control queue at most.
const STEP_REQUESTS: usize = 64;
/// How often the playing stream's line is logged, in host seconds.
pub const REPORT_SECONDS: u64 = 10;
/// Notes kept between two reports; later ones are counted.
const NOTES_KEPT: usize = 64;

/// The host's counter, in ticks of `crate::counter::FREQUENCY_HZ`: what the
/// sink's clock runs on. Without one the device uses the guest's counter.
pub type Clock = Box<dyn Fn() -> u64>;

/// The stream's states (virtio 1.2, 5.14.6.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Initial,
    ParamsSet,
    Prepared,
    Running,
    Stopped,
    Released,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::ParamsSet => "parameters set",
            Self::Prepared => "prepared",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Released => "released",
        }
    }
}

/// The parameters `SET_PARAMS` agreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub buffer_bytes: u32,
    pub period_bytes: u32,
    pub format: Format,
}

/// The device's own counters, for the boot log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Control requests answered, and those answered other than `S_OK`.
    pub requests: u64,
    pub refused: u64,
    /// Transfers completed `S_OK`, and those answered `BAD_MSG`.
    pub transfers: u64,
    pub bad_transfers: u64,
    /// Times the clock passed the queue's end while the stream ran.
    pub underruns: u64,
}

/// A transfer the device holds: its chain, its frames, and where they play.
#[derive(Debug, Clone)]
struct Transfer {
    chain: Chain,
    /// The readable bytes: the header, then the frames.
    readable: u64,
    frames: u64,
    /// The frame position of its first frame.
    start: u64,
    /// Its frames went to the sink.
    played: bool,
}

pub struct Snd {
    sink: Box<dyn Sink>,
    clock: Option<Clock>,
    hz: u64,
    state: State,
    params: Option<Params>,
    /// The transfers held, in play order.
    queue: VecDeque<Transfer>,
    /// The clock's position the device played to, and where the next
    /// transfer is queued.
    position: u64,
    queued_end: u64,
    /// The clock passed the queue's end, and no transfer came since.
    starved: bool,
    counters: Counters,
    notes: Vec<String>,
    dropped_notes: u64,
    /// The guest's counter and the host's at the last step.
    last: Option<(u64, u64)>,
    /// When the next report line is due, in host ticks, while it plays.
    report_at: Option<u64>,
    /// The control queue has requests a step left.
    control_pending: bool,
    buffer: Vec<u8>,
}

/// An answer to a control request: a status, and the info after it.
struct Answer {
    status: u64,
    body: Vec<u8>,
}

impl Answer {
    fn status(status: u64) -> Self {
        Self {
            status,
            body: Vec::new(),
        }
    }
}

fn word(bytes: &[u8], offset: u64) -> u32 {
    let offset = offset as usize;
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// A request code's name, for the log.
pub fn request_name(code: u64) -> &'static str {
    match code {
        R_JACK_INFO => "JACK_INFO",
        R_JACK_REMAP => "JACK_REMAP",
        R_PCM_INFO => "PCM_INFO",
        R_PCM_SET_PARAMS => "PCM_SET_PARAMS",
        R_PCM_PREPARE => "PCM_PREPARE",
        R_PCM_RELEASE => "PCM_RELEASE",
        R_PCM_START => "PCM_START",
        R_PCM_STOP => "PCM_STOP",
        R_CHMAP_INFO => "CHMAP_INFO",
        _ => "an unknown request",
    }
}

/// A status code's name, for the log.
pub fn status_name(status: u64) -> &'static str {
    match status {
        S_OK => "OK",
        S_BAD_MSG => "BAD_MSG",
        S_NOT_SUPP => "NOT_SUPP",
        S_IO_ERR => "IO_ERR",
        _ => "?",
    }
}

impl Snd {
    /// A device playing into `sink`, its clock on `clock` (the host's
    /// counter of `hz`), or on the guest's counter without one.
    pub fn new(sink: Box<dyn Sink>, clock: Option<Clock>, hz: u64) -> Self {
        Self {
            sink,
            clock,
            hz,
            state: State::Initial,
            params: None,
            queue: VecDeque::new(),
            position: 0,
            queued_end: 0,
            starved: false,
            counters: Counters::default(),
            notes: Vec::new(),
            dropped_notes: 0,
            last: None,
            report_at: None,
            control_pending: false,
            buffer: vec![0; XFER_BYTES as usize],
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn params(&self) -> Option<Params> {
        self.params
    }

    pub fn device_counters(&self) -> Counters {
        self.counters
    }

    /// The sink, for tests and the boot's description.
    pub fn sink(&self) -> &dyn Sink {
        &*self.sink
    }

    /// The bytes queued ahead of the position: the latency a completion
    /// reports.
    pub fn latency_bytes(&self) -> u64 {
        let frame = self.params.map_or(0, |params| params.format.frame_bytes());
        self.queued_end.saturating_sub(self.position) * u64::from(frame)
    }

    fn host(&self, now: u64) -> u64 {
        self.clock.as_ref().map_or(now, |clock| clock())
    }

    fn note(&mut self, note: String) {
        if self.notes.len() < NOTES_KEPT {
            self.notes.push(note);
        } else {
            self.dropped_notes += 1;
        }
    }

    /// The report line: what was played, the level since the last line,
    /// the underruns and the latency queued.
    fn report(&mut self) {
        let played = self.sink.played();
        let meter = self.sink.take_level();
        let latency = self.params.map_or(0, |params| {
            params
                .format
                .millis(self.latency_bytes() / u64::from(params.format.frame_bytes()))
        });
        let line = format!(
            "frames={} peak={} rms={} underruns={} silent={} latency_ms={}{}",
            played.frames,
            meter.peak_dbfs(),
            meter.rms_dbfs(),
            self.counters.underruns,
            played.silent,
            latency,
            self.sink
                .clock_source()
                .map(|source| format!(" clock={source}"))
                .unwrap_or_default(),
        );
        self.note(line);
    }

    /// The stream's answer to a PCM request in its state, and the state
    /// after it.
    fn transition(&mut self, code: u64, host: u64) -> u64 {
        let from = self.state;
        let to = match (code, from) {
            (R_PCM_PREPARE, State::ParamsSet | State::Prepared | State::Released) => {
                if self.queue.is_empty() {
                    self.position = 0;
                    self.queued_end = 0;
                    self.starved = false;
                }
                State::Prepared
            }
            (R_PCM_START, State::Prepared | State::Stopped) => {
                let Some(params) = self.params else {
                    return S_BAD_MSG;
                };
                self.sink.start(params.format, host, self.position);
                self.report_at = Some(host + REPORT_SECONDS * self.hz);
                State::Running
            }
            (R_PCM_STOP, State::Running) => {
                // The step played to `host` before it took the request.
                self.sink.stop(host);
                self.report_at = None;
                self.report();
                State::Stopped
            }
            (R_PCM_RELEASE, State::Prepared | State::Stopped) => {
                // The step completes the transfers held after the request.
                self.sink.release();
                State::Released
            }
            _ => return S_BAD_MSG,
        };
        self.state = to;
        self.note(format!(
            "stream 0 {}: {} -> {}",
            request_name(code),
            from.name(),
            to.name()
        ));
        S_OK
    }

    /// `SET_PARAMS`: the stream's format, buffer and period.
    fn set_params(&mut self, request: &[u8]) -> u64 {
        if !matches!(
            self.state,
            State::Initial | State::ParamsSet | State::Prepared | State::Released
        ) || !self.queue.is_empty()
        {
            return S_BAD_MSG;
        }
        let buffer_bytes = word(request, SET_PARAMS_BUFFER_BYTES);
        let period_bytes = word(request, SET_PARAMS_PERIOD_BYTES);
        let features = word(request, SET_PARAMS_FEATURES);
        let channels = request[SET_PARAMS_CHANNELS as usize];
        let format = request[SET_PARAMS_FORMAT as usize];
        let rate = request[SET_PARAMS_RATE as usize];
        let (Some(bytes), Some(hz)) = (sample_bytes(format), rate_hz(rate)) else {
            self.note(format!(
                "stream 0 PCM_SET_PARAMS refused (NOT_SUPP): format {format}, rate {rate}"
            ));
            return S_NOT_SUPP;
        };
        if features != 0 || channels != CHANNELS {
            self.note(format!(
                "stream 0 PCM_SET_PARAMS refused (NOT_SUPP): {channels} channels, features {features:#x}"
            ));
            return S_NOT_SUPP;
        }
        let format = Format {
            channels,
            sample_bytes: bytes as u8,
            rate: hz as u32,
        };
        let rates = self.sink.rates();
        if rates & 1 << rate == 0 {
            // A rate the device offers but its sink does not play: the
            // guest-audio ring is 48 kHz and has no resampler.
            let plays: Vec<String> = (0..64)
                .filter(|code| rates & 1 << code != 0)
                .filter_map(|code| rate_hz(code as u8))
                .map(|hz| format!("{hz} Hz"))
                .collect();
            self.note(format!(
                "stream 0 PCM_SET_PARAMS refused (NOT_SUPP): {format}: its sink plays {} only, \
                 with no resampler",
                plays.join(", ")
            ));
            return S_NOT_SUPP;
        }
        let frame = format.frame_bytes();
        if period_bytes == 0
            || !period_bytes.is_multiple_of(frame)
            || u64::from(period_bytes) > XFER_BYTES
            || buffer_bytes < period_bytes
            || !buffer_bytes.is_multiple_of(period_bytes)
        {
            self.note(format!(
                "stream 0 PCM_SET_PARAMS refused (BAD_MSG): buffer {buffer_bytes} bytes, period \
                 {period_bytes} bytes for {format}"
            ));
            return S_BAD_MSG;
        }
        let params = Params {
            buffer_bytes,
            period_bytes,
            format,
        };
        self.params = Some(params);
        let from = self.state;
        self.state = State::ParamsSet;
        let period_frames = u64::from(period_bytes / frame);
        let period_tenths = period_frames * 10_000 / u64::from(format.rate);
        self.note(format!(
            "stream 0 PCM_SET_PARAMS: {format}, buffer {buffer_bytes} bytes ({} periods), period \
             {period_bytes} bytes ({period_frames} frames, {}.{} ms): {} -> {}",
            buffer_bytes / period_bytes,
            period_tenths / 10,
            period_tenths % 10,
            from.name(),
            self.state.name()
        ));
        S_OK
    }

    /// An `*_INFO` query of `items` items of `item_bytes` each, filled by
    /// `item`.
    fn info(request: &[u8], items: u32, item_bytes: u64, item: impl Fn(u32, &mut [u8])) -> Answer {
        if request.len() < QUERY_INFO_BYTES as usize {
            return Answer::status(S_BAD_MSG);
        }
        let start = word(request, QUERY_START_ID);
        let count = word(request, QUERY_COUNT);
        let size = u64::from(word(request, QUERY_SIZE));
        if size < item_bytes || size > 256 || u64::from(start) + u64::from(count) > u64::from(items)
        {
            return Answer::status(S_BAD_MSG);
        }
        let mut body = vec![0; (u64::from(count) * size) as usize];
        for (index, slot) in body.chunks_mut(size as usize).enumerate() {
            item(start + index as u32, &mut slot[..item_bytes as usize]);
        }
        Answer { status: S_OK, body }
    }

    fn jack_info(_: u32, slot: &mut [u8]) {
        // hda_fn_nid 0, no features, the pin's configuration, no
        // capabilities, connected.
        slot[8..12].copy_from_slice(&JACK_DEFCONF.to_le_bytes());
        slot[16] = 1;
    }

    /// The stream's info, offering the `rates` its sink plays.
    fn pcm_info(rates: u64, slot: &mut [u8]) {
        let at = |offset: u64| offset as usize;
        slot[at(PCM_INFO_FORMATS)..at(PCM_INFO_FORMATS) + 8]
            .copy_from_slice(&FORMATS.to_le_bytes());
        slot[at(PCM_INFO_RATES)..at(PCM_INFO_RATES) + 8]
            .copy_from_slice(&(RATES & rates).to_le_bytes());
        slot[at(PCM_INFO_DIRECTION)] = D_OUTPUT as u8;
        slot[at(PCM_INFO_CHANNELS_MIN)] = CHANNELS;
        slot[at(PCM_INFO_CHANNELS_MAX)] = CHANNELS;
    }

    fn chmap_info(_: u32, slot: &mut [u8]) {
        let at = |offset: u64| offset as usize;
        slot[at(CHMAP_INFO_DIRECTION)] = D_OUTPUT as u8;
        slot[at(CHMAP_INFO_CHANNELS)] = CHANNELS;
        slot[at(CHMAP_INFO_POSITIONS)] = CHMAP_FL as u8;
        slot[at(CHMAP_INFO_POSITIONS) + 1] = CHMAP_FR as u8;
    }

    /// A control request's answer.
    fn request(&mut self, request: &[u8], host: u64) -> Answer {
        if request.len() < HDR_BYTES as usize {
            return Answer::status(S_BAD_MSG);
        }
        let code = u64::from(word(request, 0));
        match code {
            R_JACK_INFO => Self::info(request, JACKS, JACK_INFO_BYTES, Self::jack_info),
            R_PCM_INFO => {
                let rates = self.sink.rates();
                Self::info(request, STREAMS, PCM_INFO_BYTES, |_, slot| {
                    Self::pcm_info(rates, slot)
                })
            }
            R_CHMAP_INFO => Self::info(request, CHMAPS, CHMAP_INFO_BYTES, Self::chmap_info),
            R_PCM_SET_PARAMS | R_PCM_PREPARE | R_PCM_RELEASE | R_PCM_START | R_PCM_STOP => {
                let needed = if code == R_PCM_SET_PARAMS {
                    SET_PARAMS_BYTES
                } else {
                    PCM_HDR_BYTES
                };
                if request.len() < needed as usize || word(request, PCM_STREAM_ID) >= STREAMS {
                    return Answer::status(S_BAD_MSG);
                }
                let status = if code == R_PCM_SET_PARAMS {
                    self.set_params(request)
                } else {
                    self.transition(code, host)
                };
                if status != S_OK && code != R_PCM_SET_PARAMS {
                    self.note(format!(
                        "stream 0 {} refused ({}) while {}",
                        request_name(code),
                        status_name(status),
                        self.state.name()
                    ));
                }
                Answer::status(status)
            }
            _ => Answer::status(S_NOT_SUPP),
        }
    }

    /// Answers the control queue's requests.
    fn control(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        event_idx: bool,
        host: u64,
        served: &mut Served,
    ) -> Result<(), &'static str> {
        let mut taken = 0;
        while taken < STEP_REQUESTS {
            let Some(chain) = queue.pop(memory).map_err(ring_error)? else {
                break;
            };
            taken += 1;
            let readable: u64 = chain
                .buffers
                .iter()
                .filter(|buffer| !buffer.write)
                .map(|buffer| u64::from(buffer.bytes))
                .sum();
            let writable = chain.bytes() - readable;
            if writable < HDR_BYTES {
                return Err("a sound control request without room for its status");
            }
            let mut request = [0u8; REQUEST_BYTES];
            let length = (readable as usize).min(REQUEST_BYTES);
            chain
                .read(memory, 0, &mut request[..length])
                .map_err(ring_error)?;
            let mut answer = self.request(&request[..length], host);
            if (HDR_BYTES + answer.body.len() as u64) > writable {
                answer = Answer::status(S_BAD_MSG);
            }
            self.counters.requests += 1;
            if answer.status != S_OK {
                self.counters.refused += 1;
            }
            let mut response = (answer.status as u32).to_le_bytes().to_vec();
            response.extend_from_slice(&answer.body);
            chain
                .write(memory, readable, &response)
                .map_err(ring_error)?;
            queue
                .push_used(memory, chain.head, response.len() as u32)
                .map_err(ring_error)?;
            served.chains += 1;
            served.bytes += response.len() as u64;
        }
        self.control_pending =
            taken == STEP_REQUESTS && queue.pending(memory).map_err(ring_error)? != 0;
        if event_idx {
            if self.control_pending {
                queue.no_kick(memory).map_err(ring_error)?;
            } else {
                queue.want_kick(memory).map_err(ring_error)?;
            }
        }
        served.interrupt |= queue.publish(memory, event_idx).map_err(ring_error)?;
        Ok(())
    }

    /// Completes `transfer` with `status` and the latency queued after it.
    fn complete(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        transfer: &Transfer,
        status: u64,
    ) -> Result<(), &'static str> {
        let latency = if status == S_OK {
            self.counters.transfers += 1;
            self.latency_bytes()
        } else {
            self.counters.bad_transfers += 1;
            0
        };
        let mut answer = [0u8; PCM_STATUS_BYTES as usize];
        answer[..4].copy_from_slice(&(status as u32).to_le_bytes());
        answer[4..].copy_from_slice(&(latency.min(u64::from(u32::MAX)) as u32).to_le_bytes());
        transfer
            .chain
            .write(memory, transfer.readable, &answer)
            .map_err(ring_error)?;
        queue
            .push_used(memory, transfer.chain.head, PCM_STATUS_BYTES as u32)
            .map_err(ring_error)
    }

    /// Takes the TX queue's new transfers: each joins the queue, or is
    /// answered `BAD_MSG` at once.
    fn take(&mut self, queue: &mut Queue, memory: &mut dyn Memory) -> Result<(), &'static str> {
        while let Some(chain) = queue.pop(memory).map_err(ring_error)? {
            let readable: u64 = chain
                .buffers
                .iter()
                .filter(|buffer| !buffer.write)
                .map(|buffer| u64::from(buffer.bytes))
                .sum();
            if readable < PCM_XFER_BYTES || chain.bytes() - readable < PCM_STATUS_BYTES {
                return Err("a sound transfer without its header or status");
            }
            let mut header = [0u8; PCM_XFER_BYTES as usize];
            chain.read(memory, 0, &mut header).map_err(ring_error)?;
            let pcm = readable - PCM_XFER_BYTES;
            let mut transfer = Transfer {
                chain,
                readable,
                frames: 0,
                start: 0,
                played: false,
            };
            let frame = self
                .params
                .map_or(0, |params| u64::from(params.format.frame_bytes()));
            let takes = matches!(
                self.state,
                State::Prepared | State::Running | State::Stopped
            ) && u32::from_le_bytes(header) < STREAMS
                && frame != 0
                && pcm != 0
                && pcm <= XFER_BYTES
                && pcm.is_multiple_of(frame);
            if !takes {
                if self.counters.bad_transfers == 0 {
                    self.note(format!(
                        "stream 0 transfer refused (BAD_MSG): {pcm} bytes while {}",
                        self.state.name()
                    ));
                }
                self.complete(queue, memory, &transfer, S_BAD_MSG)?;
                continue;
            }
            transfer.frames = pcm / frame;
            transfer.start = self.queued_end.max(self.position);
            self.queued_end = transfer.start + transfer.frames;
            self.queue.push_back(transfer);
        }
        Ok(())
    }

    /// Plays the queue to the sink's clock at host tick `host`: each
    /// transfer's frames go to the sink as the clock reaches it, and it
    /// completes once the clock passed its last frame.
    fn play(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        host: u64,
    ) -> Result<(), &'static str> {
        if self.state != State::Running {
            return Ok(());
        }
        let target = self.sink.clock(host);
        while let Some(mut transfer) = self.queue.pop_front() {
            if target < transfer.start {
                self.queue.push_front(transfer);
                break;
            }
            if !transfer.played {
                let bytes = (transfer.readable - PCM_XFER_BYTES) as usize;
                let mut pcm = core::mem::take(&mut self.buffer);
                let read = transfer
                    .chain
                    .read(memory, PCM_XFER_BYTES, &mut pcm[..bytes])
                    .map_err(ring_error);
                if read.is_ok() {
                    self.sink.play(&pcm[..bytes]);
                }
                self.buffer = pcm;
                read?;
                transfer.played = true;
                self.starved = false;
            }
            let end = transfer.start + transfer.frames;
            if target < end {
                self.position = target;
                self.queue.push_front(transfer);
                return Ok(());
            }
            self.position = end;
            self.complete(queue, memory, &transfer, S_OK)?;
        }
        if self.queue.is_empty() && target > self.position {
            // Nothing to play: silence, and the next transfer plays from
            // where the clock is.
            self.sink.silence(target - self.position);
            if !self.starved {
                self.starved = true;
                self.counters.underruns += 1;
            }
            self.position = target;
            self.queued_end = self.queued_end.max(target);
        }
        Ok(())
    }

    /// Completes every transfer held: `RELEASE`, or a stream refused.
    fn complete_all(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
    ) -> Result<(), &'static str> {
        while let Some(transfer) = self.queue.pop_front() {
            self.complete(queue, memory, &transfer, S_OK)?;
        }
        Ok(())
    }
}

impl VirtioDevice for Snd {
    fn name(&self) -> &'static str {
        "virtio-snd"
    }

    fn id(&self) -> u32 {
        ids::SOUND as u32
    }

    fn queue_max(&self) -> &'static [u16] {
        &QUEUE_MAX
    }

    fn config_read(&self, offset: u64, bytes: u64) -> Option<u64> {
        if offset.checked_add(bytes)? > CONFIG_BYTES {
            return None;
        }
        let mut config = [0u8; CONFIG_BYTES as usize];
        config[CONFIG_JACKS as usize..][..4].copy_from_slice(&JACKS.to_le_bytes());
        config[CONFIG_STREAMS as usize..][..4].copy_from_slice(&STREAMS.to_le_bytes());
        config[CONFIG_CHMAPS as usize..][..4].copy_from_slice(&CHMAPS.to_le_bytes());
        let mut value = [0u8; 8];
        value[..bytes as usize]
            .copy_from_slice(&config[offset as usize..(offset + bytes) as usize]);
        Some(u64::from_le_bytes(value))
    }

    fn reset(&mut self) {
        if self.state == State::Running {
            let host = self.last.map_or(0, |(_, host)| host);
            self.sink.stop(host);
        }
        if self.state != State::Initial {
            self.note(format!("stream 0 reset while {}", self.state.name()));
        }
        self.sink.release();
        self.state = State::Initial;
        self.params = None;
        self.queue.clear();
        self.position = 0;
        self.queued_end = 0;
        self.starved = false;
        self.report_at = None;
        self.control_pending = false;
    }

    fn due(&self) -> bool {
        self.control_pending
    }

    fn deadline(&self) -> Option<u64> {
        if self.state != State::Running {
            return None;
        }
        let (guest, host) = self.last?;
        let next = self.queue.front()?;
        let due = self.sink.when(next.start + next.frames)?;
        Some(guest.saturating_add(due.saturating_sub(host)))
    }

    fn take_notes(&mut self) -> Vec<String> {
        self.notes.extend(self.sink.take_notes());
        let dropped = core::mem::take(&mut self.dropped_notes);
        let mut notes = core::mem::take(&mut self.notes);
        if dropped != 0 {
            notes.push(format!("{dropped} more notes not kept"));
        }
        notes
    }

    fn has_notes(&self) -> bool {
        !self.notes.is_empty()
    }

    fn counters(&self) -> Vec<(&'static str, u64)> {
        let played = self.sink.played();
        let mut counters = vec![
            ("frames", played.frames),
            ("silent", played.silent),
            ("underruns", self.counters.underruns),
            ("transfers", self.counters.transfers),
            ("bad_transfers", self.counters.bad_transfers),
            ("requests", self.counters.requests),
            ("refused", self.counters.refused),
        ];
        counters.extend(self.sink.counters());
        counters
    }

    fn take_host_due(&mut self) -> bool {
        self.sink.take_due()
    }

    fn step(
        &mut self,
        queues: &mut [Option<Queue>],
        notified: u64,
        memory: &mut dyn Memory,
        event_idx: bool,
        now: u64,
    ) -> Result<Served, &'static str> {
        let host = self.host(now);
        self.last = Some((now, host));
        // A sink whose clock follows its output looks at it first.
        self.sink.advance(host);
        let mut served = Served::default();
        let [control, _event, tx, ..] = queues else {
            return Ok(served);
        };
        // The clock first: what it passed completes, and a starved stream's
        // position catches up, so a new transfer plays from the clock on.
        if let Some(tx) = tx.as_mut() {
            self.play(tx, memory, host)?;
            self.take(tx, memory)?;
            self.play(tx, memory, host)?;
        }
        // Then the requests, at the same tick: STOP stops the clock where
        // the playing stopped.
        if let Some(control) = control.as_mut()
            && (notified & 1 << VQ_CONTROL != 0
                || self.control_pending
                || control.pending(memory).map_err(ring_error)? != 0)
        {
            self.control(control, memory, event_idx, host, &mut served)?;
        }
        if let Some(tx) = tx.as_mut() {
            if self.state == State::Released {
                self.complete_all(tx, memory)?;
            }
            if event_idx {
                tx.want_kick(memory).map_err(ring_error)?;
            }
            served.interrupt |= tx.publish(memory, event_idx).map_err(ring_error)?;
        }
        if self.report_at.is_some_and(|at| host >= at) {
            self.report();
            self.report_at = Some(host + REPORT_SECONDS * self.hz);
        }
        Ok(served)
    }
}
