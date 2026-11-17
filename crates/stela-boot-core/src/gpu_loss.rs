//! The end of a macOS session whose GPU is lost for good. A lost GPU's
//! service reopens its device for the next client; while that works, macOS
//! goes on. When it does not and no other GPU serves Metal, every new
//! WindowServer is refused and dies, and the screen stays frozen: Stela then
//! stops as a boot without a GPU does, with which GPU was lost and what its
//! kernel driver said of it.
use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// The failed reopens in a row after which a lost GPU is taken not to
/// reopen, when its kernel driver did not say that it stopped answering:
/// the close that found it lost and the next client's connection.
pub const REOPEN_ATTEMPTS: u32 = 2;

/// What a kernel driver's line says, after its device's name, of a GPU that
/// left the bus (nouveau's `nvkm_device_gone`): after it no reopen can work.
const SILENT: &str = ": stopped answering";

/// A GPU that serves Metal, as [`verdict`] reads it.
#[derive(Clone, Copy, Debug)]
pub struct Gpu<'a> {
    /// Its PCI address, as the boot log names it.
    pub name: &'a str,
    /// Whether its device is lost now.
    pub lost: bool,
    /// The reopens that failed in a row since the loss.
    pub failed_reopens: u32,
    /// The last such reopen's error.
    pub error: Option<&'a str>,
    /// Its kernel driver's line that it stopped answering, if it gave one.
    pub silent: Option<&'a str>,
}

impl Gpu<'_> {
    /// Whether the GPU is lost and will not reopen: a reopen failed after
    /// its driver said it stopped answering, or [`REOPEN_ATTEMPTS`] failed
    /// in a row. A lost GPU no reopen was tried on yet may still come back.
    fn gone(&self) -> bool {
        self.lost
            && self.failed_reopens
                >= if self.silent.is_some() {
                    1
                } else {
                    REOPEN_ATTEMPTS
                }
    }
}

/// Why the boot stops, once no GPU of `gpus` (those that serve Metal) can
/// serve macOS again; `None` while any of them works or may still reopen,
/// and for a boot that has none.
pub fn verdict(gpus: &[Gpu<'_>]) -> Option<String> {
    if gpus.is_empty() || !gpus.iter().all(Gpu::gone) {
        return None;
    }
    let each: Vec<String> = gpus
        .iter()
        .map(|gpu| {
            let tries = match gpu.failed_reopens {
                1 => String::from("1 try"),
                tries => format!("{tries} tries"),
            };
            let error = gpu.error.unwrap_or("no error named");
            let said = match gpu.silent {
                Some(line) => format!("Its driver said: {line}"),
                None => String::from("Its driver did not say that it left the bus"),
            };
            format!(
                "the GPU {} was lost while macOS ran and did not reopen ({tries}, last: \
                 {error}). {said}",
                gpu.name
            )
        })
        .collect();
    Some(format!(
        "macOS cannot run without a GPU: {}",
        each.join("; ")
    ))
}

/// The kernel drivers' lines that a GPU stopped answering, the first of each
/// GPU kept for the stop's reason. Written once per slot; read without a
/// lock.
pub struct SilentLines<const SLOTS: usize, const BYTES: usize> {
    lines: [Line<BYTES>; SLOTS],
}

struct Line<const BYTES: usize> {
    /// [`FREE`], [`WRITING`] or [`READY`].
    state: AtomicU8,
    length: AtomicUsize,
    bytes: [AtomicU8; BYTES],
}

const FREE: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

impl<const SLOTS: usize, const BYTES: usize> Default for SilentLines<SLOTS, BYTES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const SLOTS: usize, const BYTES: usize> SilentLines<SLOTS, BYTES> {
    pub const fn new() -> Self {
        Self {
            lines: [const {
                Line {
                    state: AtomicU8::new(FREE),
                    length: AtomicUsize::new(0),
                    bytes: [const { AtomicU8::new(0) }; BYTES],
                }
            }; SLOTS],
        }
    }

    /// Keeps `line` if it says a device stopped answering and a slot is
    /// free, cut to the slot's size at a character boundary; whether it
    /// says so.
    pub fn observe(&self, line: &str) -> bool {
        if !line.contains(SILENT) {
            return false;
        }
        let mut end = line.len().min(BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        for slot in &self.lines {
            if slot
                .state
                .compare_exchange(FREE, WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                for (byte, value) in slot.bytes.iter().zip(line[..end].bytes()) {
                    byte.store(value, Ordering::Relaxed);
                }
                slot.length.store(end, Ordering::Relaxed);
                slot.state.store(READY, Ordering::Release);
                break;
            }
        }
        true
    }

    /// The kept line that names the GPU at `address`.
    pub fn of(&self, address: &str) -> Option<String> {
        self.lines.iter().find_map(|slot| {
            if slot.state.load(Ordering::Acquire) != READY {
                return None;
            }
            let length = slot.length.load(Ordering::Relaxed);
            let bytes: Vec<u8> = slot.bytes[..length]
                .iter()
                .map(|byte| byte.load(Ordering::Relaxed))
                .collect();
            String::from_utf8(bytes)
                .ok()
                .filter(|line| line.contains(address))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GONE: &str = "nouveau 0000:02:00.0: stopped answering: BOOT_0 read ffffffff for 1004 \
                        ms; config vendor ffff command ffff status ffff pmcsr ffff link status \
                        ffff (vendor ffff link status ffff when it began, 0 silences ended \
                        before); its waits end at once from here";

    fn gpu(lost: bool, failed_reopens: u32, silent: Option<&'static str>) -> Gpu<'static> {
        Gpu {
            name: "0000:02:00.0",
            lost,
            failed_reopens,
            error: (failed_reopens != 0).then_some("DeviceLost"),
            silent,
        }
    }

    #[test]
    fn a_working_gpu_or_one_that_may_still_reopen_goes_on() {
        assert_eq!(verdict(&[]), None);
        assert_eq!(verdict(&[gpu(false, 0, None)]), None);
        // Lost, its connections still open: no reopen was tried.
        assert_eq!(verdict(&[gpu(true, 0, None)]), None);
        assert_eq!(verdict(&[gpu(true, 0, Some(GONE))]), None);
        // One failed reopen with no word from the driver may be a passing
        // refusal: the next client tries again.
        assert_eq!(verdict(&[gpu(true, 1, None)]), None);
        // A reopen that gave a device counts none, whatever came before.
        assert_eq!(verdict(&[gpu(false, 0, Some(GONE))]), None);
    }

    #[test]
    fn a_gpu_off_the_bus_ends_the_session_at_its_first_failed_reopen() {
        let reason = verdict(&[gpu(true, 1, Some(GONE))]).expect("stops");
        assert!(reason.starts_with(
            "macOS cannot run without a GPU: the GPU 0000:02:00.0 was lost while macOS ran and \
             did not reopen (1 try, last: DeviceLost). Its driver said: nouveau 0000:02:00.0: \
             stopped answering: BOOT_0 read ffffffff for 1004 ms; config vendor ffff"
        ));
    }

    #[test]
    fn a_gpu_its_driver_said_nothing_of_ends_it_at_the_second_failed_reopen() {
        let reason = verdict(&[gpu(true, REOPEN_ATTEMPTS, None)]).expect("stops");
        assert!(reason.contains("(2 tries, last: DeviceLost)"));
        assert!(reason.ends_with("Its driver did not say that it left the bus"));
    }

    #[test]
    fn another_gpu_that_serves_keeps_the_session() {
        let other = Gpu {
            name: "0000:03:00.0",
            ..gpu(false, 0, None)
        };
        assert_eq!(verdict(&[gpu(true, 3, Some(GONE)), other]), None);
        let other = Gpu {
            lost: true,
            failed_reopens: 2,
            error: Some("OutOfMemory"),
            ..other
        };
        let reason = verdict(&[gpu(true, 3, Some(GONE)), other]).expect("stops");
        assert!(reason.contains("the GPU 0000:02:00.0 was lost"));
        assert!(reason.contains("; the GPU 0000:03:00.0 was lost"));
        assert!(reason.contains("(2 tries, last: OutOfMemory)"));
    }

    #[test]
    fn the_first_silent_line_of_each_gpu_is_kept_and_found_by_its_address() {
        let lines = SilentLines::<2, 64>::new();
        assert!(
            !lines.observe(
                "nouveau 0000:02:00.0: answers again: BOOT_0 read ffffffff for 30 to 40 ms"
            )
        );
        // A status's description is not the driver's word on its device.
        assert!(!lines.observe("nouveau 0000:02:00.0: rpc: the GPU stopped answering on the bus"));
        assert_eq!(lines.of("0000:02:00.0"), None);
        assert!(lines.observe(GONE));
        // Cut to the slot, whole characters only.
        let kept = lines.of("0000:02:00.0").expect("kept");
        assert_eq!(kept, &GONE[..64]);
        assert_eq!(lines.of("0000:03:00.0"), None);
        assert!(lines.observe("amdgpu 0000:03:00.0: stopped answering: é"));
        assert!(lines.of("0000:03:00.0").expect("kept").ends_with('é'));
        // A third line says so, with no slot left for it.
        assert!(lines.observe("xe 0000:04:00.0: stopped answering"));
        assert_eq!(lines.of("0000:04:00.0"), None);
        let cut = SilentLines::<1, 41>::new();
        assert!(cut.observe("amdgpu 0000:03:00.0: stopped answering: é"));
        assert!(cut.of("0000:03:00.0").expect("kept").ends_with(": "));
    }
}
