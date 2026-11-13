//! Code density measurements, run by hand: the native code blocks of a
//! sample compile to, and the chain machinery's pieces, as hex for offline
//! counting. Ignored tests, so `cargo test` never runs them; neither reads
//! anything a translation depends on.
use super::prepare::{Preparation, Prepared};
use alloc::{format, string::String, sync::Arc, vec::Vec};
use stela_compiler::{
    TargetCpu,
    x86_64::{MemoryChecks, NativeSystem, RangeCheck, chain},
};

/// Prepare each block of the sample `DENSITY_IN` names, one line each:
/// its hex PC, a weight and its hex bytes. `DENSITY_OUT` gets a line per
/// block: the PC, the weight, the source bytes it took, its hex code and
/// its source map (`pc:offset`), or why it was refused.
#[test]
#[ignore = "manual: opt-in"]
fn density_dump() {
    use std::io::Write;
    let input = std::env::var("DENSITY_IN").expect("DENSITY_IN names the sample");
    let output = std::env::var("DENSITY_OUT").expect("DENSITY_OUT names the output");
    let chip = crate::machine::test_chip();
    let checks = MemoryChecks::system(true, false, true, false)
        .with_range(RangeCheck::four_level(4).one_sided(true));
    let system = NativeSystem {
        exclusive: true,
        pointer: Some(super::pointer::CELLS),
        self_loops: true,
        ..NativeSystem::NONE
    };
    let preparation = Preparation::new(
        chip,
        Arc::new(chip.admit_table(0).expect("the test chip's admit table")),
        TargetCpu::detect(),
        true,
        checks,
        None,
        system,
    );
    let mut out = std::fs::File::create(output).expect("the output");
    let sample = std::fs::read_to_string(input).expect("the sample");
    for line in sample.lines() {
        let mut fields = line.split(' ');
        let (Some(pc), Some(weight), Some(hex)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let pc = u64::from_str_radix(pc, 16).expect("a hex PC");
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|at| u8::from_str_radix(&hex[2 * at..2 * at + 2], 16).expect("hex bytes"))
            .collect();
        match preparation.prepare_window(&bytes, pc, bytes.len(), true) {
            Some(Ok(Prepared::Block { compiled, .. })) => {
                let map: Vec<String> = compiled
                    .native
                    .source_map
                    .iter()
                    .map(|(pc, offset)| format!("{:x}:{offset}", pc.0))
                    .collect();
                writeln!(
                    out,
                    "{pc:x} {weight} {} {} {}",
                    compiled.source_bytes,
                    hex_of(&compiled.native.x86_64_code),
                    map.join(",")
                )
                .expect("written");
            }
            other => writeln!(out, "{pc:x} {weight} refused {other:?}").expect("written"),
        }
    }
}

/// The chain machinery's pieces to `DENSITY_OUT`, a line each: a link's
/// body, the cell and table probes, a stamped tail and a lookup tail.
#[test]
#[ignore = "manual: opt-in"]
fn chain_dump() {
    use stela_compiler::block::ExitKind;
    let output = std::env::var("DENSITY_OUT").expect("DENSITY_OUT names the output");
    let mut text = String::new();
    let (_, body) = chain::link(0x10_0000, chain::LINK_BYTES, 5, 7, 0x20_0000).expect("a link");
    text += &format!("link {}\n", hex_of(&body));
    let dispatcher = chain::lookup_dispatcher(0).expect("the probes");
    text += &format!("cell {}\n", hex_of(&dispatcher[..chain::PROBE_ENTRY]));
    let probe = &dispatcher[chain::PROBE_ENTRY..chain::STAMPING_PROBE_ENTRY];
    text += &format!("probe {}\n", hex_of(probe));
    let stamped = chain::stamped_tail(
        0x10_0000,
        0x30_0000,
        9,
        0x4000,
        ExitKind::Direct,
        5,
        7,
        0x20_0000,
        0x10_8000,
    )
    .expect("a stamped tail");
    text += &format!("stamped {}\n", hex_of(&stamped));
    let lookup =
        chain::lookup_tail(0x10_0000, ExitKind::Return, 5, 0x10_8000, 0).expect("a lookup tail");
    text += &format!("lookup {}\n", hex_of(&lookup));
    std::fs::write(output, text).expect("written");
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Page-relative coverage (`NativeSystem::page_relative`): the blocks of a
/// raw code file `PR_IN` at hex address `PR_PC`, prepared as the engine
/// prepares them, at their address and 4 GiB away. `PR_OUT` gets the counts
/// of blocks whose native code is the same at both, differs, or was
/// refused, and the words of the first differing blocks. Runs of
/// `PR_RUN` blocks (default 200) are taken at `PR_RUNS` (default 500)
/// places spread over the file.
#[test]
#[ignore = "manual: opt-in"]
fn page_relative_corpus() {
    use std::io::Write;
    let input = std::env::var("PR_IN").expect("PR_IN names the code");
    let output = std::env::var("PR_OUT").expect("PR_OUT names the output");
    let base = u64::from_str_radix(&std::env::var("PR_PC").expect("PR_PC: hex"), 16).expect("hex");
    let number = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let (run, runs) = (number("PR_RUN", 200), number("PR_RUNS", 500));
    let code = std::fs::read(input).expect("the code");
    let chip = crate::machine::test_chip();
    let checks = MemoryChecks::system(true, false, true, false)
        .with_range(RangeCheck::four_level(4).one_sided(true));
    let system = NativeSystem {
        exclusive: true,
        pointer: Some(super::pointer::CELLS),
        self_loops: true,
        // `PR_PLAIN` set: the control, ordinary code.
        page_relative: std::env::var_os("PR_PLAIN").is_none(),
        ..NativeSystem::NONE
    };
    let preparation = Preparation::new(
        chip,
        Arc::new(chip.admit_table(0).expect("the test chip's admit table")),
        TargetCpu::detect(),
        true,
        checks,
        None,
        system,
    );
    const PAGE: usize = 4096;
    const AWAY: u64 = 1 << 32;
    let prepare = |offset: usize, pc: u64| {
        // As the engine's: to the page's end, a block's bytes at most.
        let end = ((offset / PAGE + 1) * PAGE).min(offset + super::MAX_BLOCK_BYTES as usize);
        let window = &code[offset..end.min(code.len())];
        preparation.prepare_window(window, pc, window.len(), true)
    };
    let (mut same, mut differ, mut refused, mut samples) = (0usize, 0usize, 0usize, Vec::new());
    let mut reasons = alloc::collections::BTreeMap::<String, usize>::new();
    let stride = (code.len() / runs.max(1)).max(PAGE) & !3;
    for start in (0..code.len()).step_by(stride) {
        let mut offset = start;
        for _ in 0..run {
            if offset + 4 > code.len() {
                break;
            }
            let pc = base + offset as u64;
            let (here, there) = (prepare(offset, pc), prepare(offset, pc.wrapping_add(AWAY)));
            let taken = match (here, there) {
                (
                    Some(Ok(Prepared::Block { compiled: a, .. })),
                    Some(Ok(Prepared::Block { compiled: b, .. })),
                ) => {
                    if a.native.x86_64_code == b.native.x86_64_code {
                        same += 1;
                    } else {
                        differ += 1;
                        if samples.len() < 40 {
                            let words: Vec<String> = code[offset..offset + a.source_bytes]
                                .chunks(4)
                                .map(|word| {
                                    format!("{:08x}", u32::from_le_bytes(word.try_into().unwrap()))
                                })
                                .collect();
                            samples.push(format!("{pc:x} {}", words.join(" ")));
                        }
                    }
                    a.source_bytes.max(4)
                }
                (other, _) => {
                    refused += 1;
                    let reason = match other {
                        Some(Err(refusal)) => format!("{:?}", refusal.error.kind),
                        Some(Ok(Prepared::Checked(_))) => String::from("Checked"),
                        Some(Ok(Prepared::FloatingPointTrap)) => String::from("FloatingPointTrap"),
                        Some(Ok(Prepared::Block { .. })) => {
                            String::from("Block here, not 4 GiB away")
                        }
                        None => String::from("None"),
                    };
                    let reason: String = reason.chars().take(70).collect();
                    *reasons.entry(reason).or_insert(0usize) += 1;
                    4
                }
            };
            offset += taken;
        }
    }
    let total = same + differ;
    let mut out = std::fs::File::create(output).expect("the output");
    writeln!(
        out,
        "blocks={total} same={same} ({:.1}%) differ={differ} refused={refused}",
        100.0 * same as f64 / total.max(1) as f64
    )
    .expect("written");
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by_key(|(_, count)| core::cmp::Reverse(*count));
    for (reason, count) in reasons.iter().take(12) {
        writeln!(out, "refused {count} {reason}").expect("written");
    }
    for sample in samples {
        writeln!(out, "{sample}").expect("written");
    }
}
