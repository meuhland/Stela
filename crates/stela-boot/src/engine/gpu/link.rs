//! The PCI Express link above a GPU, for the boot log: both ends' ASPM,
//! Clock Power Management, LTR and L1 PM Substates as the firmware left
//! them and after the claim's reset, and the port's view when the GPU stops
//! answering or answers again, which tells a link that went down from a
//! port that lost power with it. The port is read through a read-only
//! mapping of its configuration kept for the boot; nothing is written.
use crate::{native_mmio, output};
use alloc::{format, string::String};
use stela_drivers::bus::pci::{Address, ConfigSpace, link_report, owned::ObservedConfig};

/// The PCI Express port directly above a GPU, kept read-only.
pub(in crate::engine) struct Port {
    address: Address,
    config: ObservedConfig,
}

impl Port {
    /// `port <address> <its report>`, or why it could not be read.
    fn report(&self) -> String {
        match link_report::read(&self.config) {
            Ok(end) => format!("port {} {end}", self.address),
            Err(error) => format!("port {} unreadable: {error:?}", self.address),
        }
    }
}

/// The port above the GPU at `address`, mapped read-only for the boot;
/// `None` for one on a root bus, and, said in a line, for one whose port
/// cannot be mapped.
pub(in crate::engine) fn port_above(
    mmio: &'static native_mmio::Source,
    address: Address,
) -> Option<Port> {
    let bridge = mmio.parent_bridge(address)?;
    let refused = |why: &dyn core::fmt::Debug| {
        output::arguments(format_args!(
            "GPU {address}: its port {bridge} is not read for the link lines: {why:?}\n"
        ));
    };
    // SAFETY: the mount's fabric owns the bridge's configuration and keeps
    // other writers out; this mapping only reads it, for the boot.
    let (window, mapping) = match unsafe { mmio.observe_config_page(bridge) } {
        Ok(observed) => observed,
        Err(error) => {
            refused(&error);
            return None;
        }
    };
    // SAFETY: the source issued this exact UC read-only page of `bridge`.
    match unsafe { ObservedConfig::attach(bridge, window, mapping) } {
        Ok(config) => Some(Port {
            address: bridge,
            config,
        }),
        Err((error, mut mapping)) => {
            // SAFETY: the refused attachment published no view or reader.
            let _ = unsafe { mapping.retire() };
            refused(&error);
            None
        }
    }
}

/// `GPU <address> link <when>: GPU <its report>; port <address> <its
/// report>`, from the GPU's configuration `gpu` and the port above it.
pub(in crate::engine) fn log_link<C: ConfigSpace + ?Sized>(
    address: Address,
    when: &str,
    gpu: &C,
    port: Option<&Port>,
) {
    let own = match link_report::read(gpu) {
        Ok(end) => format!("{end}"),
        Err(error) => format!("unreadable: {error:?}"),
    };
    let above = port.map_or_else(|| String::from("no port above it is read"), Port::report);
    output::arguments(format_args!(
        "GPU {address} link {when}: GPU {own}; {above}\n"
    ));
}

/// `GPU <name> <event>; port <address> <its report>`: what the port above
/// a GPU whose driver said `event` of it reads now, which tells whether the
/// port and its link are still up.
#[cfg(feature = "nvk")]
pub(in crate::engine) fn log_port_at(name: &str, event: &str, port: Option<&Port>) {
    let above = port.map_or_else(|| String::from("no port above it is read"), Port::report);
    output::arguments(format_args!("GPU {name} {event}; {above}\n"));
}
