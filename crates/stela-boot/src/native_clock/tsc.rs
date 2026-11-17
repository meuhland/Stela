// SPDX-License-Identifier: GPL-2.0-only
//! Native HPET power validation, TSC reads, the local deadline control and
//! device minimum delays.
mod deadline;
pub(super) use deadline::Control as DeadlineControl;
mod hpet_power;
mod minimum;
use core::arch::{asm, x86_64::__cpuid_count};
pub(super) use hpet_power::PowerControl;
pub(crate) use minimum::Minimum;
// Baseline CPUID is serializing on admitted x86-64. Do not presume
// AMD LFENCE ordering or use unchecked RDTSCP/TSC_AUX as an affinity lease.
pub(super) fn ordered() -> u64 {
    let _ = __cpuid_count(0, 0);
    let (low, high): (u32, u32);
    // SAFETY: only called after kept-CPU TSC feature admission.
    // Default memory clobber and both CPUIDs enclose the exact ordered sample.
    unsafe {
        asm!("rdtsc",out("eax")low,out("edx")high,options(nostack,preserves_flags));
    }
    let result = (u64::from(high) << 32) | u64::from(low);
    let _ = __cpuid_count(0, 0);
    result
}
/// The BSP's counter after every earlier instruction, without CPUID's VM
/// exit: LFENCE dispatch-serializes on Intel and on AMD with its MSR control
/// set, as Linux's rdtsc_ordered relies on. A core whose counter agrees with
/// the BSP's only through a software offset reads it moved by that offset
/// (`host::shared_tsc`): the clock's samples and a device wait's start and
/// end may be taken on different cores (the boot processor and the one
/// running Linux in its place), whose raw counters can be seconds apart.
pub(super) fn fenced() -> u64 {
    // SAFETY: LFENCE and RDTSC have no memory effects; x86-64 has both.
    let raw = unsafe {
        core::arch::x86_64::_mm_lfence();
        core::arch::x86_64::_rdtsc()
    };
    crate::engine::host::shared_tsc(raw)
}
