//! Stela's virtio devices and its own virtio kext, StelaVirtIO, which a
//! `--virtio` boot gives the guest: the entropy device at
//! `/arm-io/stela-virtio0` on the virtio-mmio transport
//! (`stela_soc::virtio`), fed from the boot's seeded random stream, and the
//! kext linked into the auxiliary collection beside StelaGPU. The kext is in
//! the image only with the `virtio-kext` feature, as `include_bytes!` needs
//! `xtask guest`'s output; unlike StelaGPU it needs no `nvk`.
//!
//! The boot log has a `VIRTIO` line for the device's placement, each thing
//! its driver does (status changes, the features agreed, the queues'
//! places, the first notification, errors) soon after it happens, and the
//! device's counters at each service report while they move.
use super::{output, parallel};
use alloc::{boxed::Box, string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_contracts::interrupt::AicInput;
use stela_contracts::virtio::status;
use stela_loader::{apple_device_tree::DeviceTree, aux_kc::Kext, stela_gpu::Placement};
use stela_soc::machine::{AppleDevices, VirtioReport};
use stela_soc::virtio::{mmio::Event, rng::Rng};

/// `xtask guest`'s objects of the StelaVirtIO kext.
#[cfg(feature = "virtio-kext")]
mod kext {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/guest/StelaVirtIO.kext.rs"
    ));
}

/// Where the installed system would keep the bundle; the kernel loads it
/// from the auxiliary collection, so the path is nominal.
#[cfg(feature = "virtio-kext")]
const BUNDLE_PATH: &str = "/Library/Extensions/StelaVirtIO.kext";

/// Whether this image carries the kext.
pub(super) const EMBEDDED: bool = cfg!(feature = "virtio-kext");

/// The entropy device's node index.
const RNG: usize = 0;

/// Set when a device has events for the log.
static EVENTS: AtomicBool = AtomicBool::new(false);

/// The kexts a boot links for its virtio devices: StelaVirtIO when the boot
/// asks for them (`--virtio`) and the image carries it, else none, so a
/// boot without `--virtio` links exactly what it did before.
pub(super) fn kexts(wanted: bool) -> Vec<Kext<'static>> {
    #[cfg(feature = "virtio-kext")]
    if wanted {
        return alloc::vec![Kext {
            info: kext::INFO,
            objects: kext::OBJECTS.to_vec(),
            bundle_path: BUNDLE_PATH.into(),
            trusted_code: kext::TRUSTED_CODE.to_vec(),
        }];
    }
    let _ = wanted;
    Vec::new()
}

/// Places the entropy device's node, after the GPU gates and StelaNet: it
/// takes the next free window and AIC input. A tree with no room has none.
pub(super) fn place(tree: &mut DeviceTree) -> Option<Placement> {
    match stela_loader::virtio::place_virtio(tree, RNG) {
        Ok(placement) => Some(placement),
        Err(error) => {
            output::arguments(format_args!("VIRTIO rng not placed: {error}\n"));
            None
        }
    }
}

/// Puts the entropy device where its node says, its AIC input split into
/// die and input by the board's `per_die`.
pub(super) fn add(devices: &mut AppleDevices, placement: Placement, per_die: u32) {
    let irq = AicInput::new(
        (placement.irq / per_die) as u8,
        (placement.irq % per_die) as u16,
    );
    let source = Box::new(|bytes: &mut [u8]| crate::random::fill(bytes).is_ok());
    devices
        .add_virtio_mmio(
            placement.physical,
            irq,
            Box::new(Rng::new(source)),
            Some(&EVENTS),
        )
        .unwrap_or_else(|error| output::fatal(&error));
    output::arguments(format_args!(
        "VIRTIO rng /arm-io/{} at {:#x} aic input {:#x}{}\n",
        stela_loader::virtio::name(RNG),
        placement.physical,
        placement.irq,
        if EMBEDDED {
            ""
        } else {
            " (no StelaVirtIO kext in this image)"
        }
    ));
}

/// Logs what the devices' drivers did since the last call; `counters` adds
/// each device's counters, when they moved. Without `counters` it looks
/// only when a device has events, and never waits for the devices' lock.
pub(super) fn report(devices: &parallel::Board, counters: bool) {
    if !counters && !EVENTS.load(Ordering::Acquire) {
        return;
    }
    let reports = devices.try_with(|devices| {
        devices.has_virtio().then(|| {
            EVENTS.store(false, Ordering::Release);
            devices.virtio_reports(counters)
        })
    });
    for report in reports.flatten().unwrap_or_default() {
        log(&report);
    }
}

/// The device's name in the log: `virtio-rng` is `rng`.
fn short(name: &str) -> &str {
    name.strip_prefix("virtio-").unwrap_or(name)
}

fn log(report: &VirtioReport) {
    let name = short(report.name);
    for event in &report.events {
        match *event {
            Event::Status { from, to } => output::arguments(format_args!(
                "VIRTIO {name} status {from:#x} -> {to:#x} ({})\n",
                status_names(to)
            )),
            Event::Features {
                offered,
                driver,
                accepted,
            } => output::arguments(format_args!(
                "VIRTIO {name} features {driver:#x} of {offered:#x} {}\n",
                if accepted { "accepted" } else { "refused" }
            )),
            Event::Queue {
                index,
                size,
                desc,
                avail,
                used,
            } => output::arguments(format_args!(
                "VIRTIO {name} queue {index}: {size} entries, desc {desc:#x} avail {avail:#x} \
                 used {used:#x}\n"
            )),
            Event::QueueRefused { index, why } => {
                output::arguments(format_args!("VIRTIO {name} queue {index} refused: {why}\n"))
            }
            Event::Notified { index } => {
                output::arguments(format_args!("VIRTIO {name} queue {index} first notify\n"))
            }
            Event::Reset => output::arguments(format_args!("VIRTIO {name} reset\n")),
            Event::Error(why) => output::arguments(format_args!(
                "VIRTIO {name} error, the driver must reset it: {why}\n"
            )),
        }
    }
    if report.dropped_events != 0 {
        output::arguments(format_args!(
            "VIRTIO {name} {} more events not kept\n",
            report.dropped_events
        ));
    }
    if report.fresh {
        let stats = report.stats;
        output::arguments(format_args!(
            "VIRTIO {name} served={} chains={} notifies={} interrupts={} status={:#x} \
             features={:#x}{}\n",
            stats.bytes,
            stats.chains,
            stats.notifies,
            stats.interrupts,
            report.status,
            report.accepted,
            report
                .error
                .map(|why| alloc::format!(" error={why}"))
                .unwrap_or_default()
        ));
    }
}

/// The status bits' names, `|`-joined.
fn status_names(bits: u32) -> String {
    let names: Vec<&str> = status::LAYOUT
        .iter()
        .filter(|&&(_, bit)| u64::from(bits) & bit != 0)
        .map(|&(name, _)| name)
        .collect();
    if names.is_empty() {
        String::from("reset")
    } else {
        names.join("|")
    }
}
