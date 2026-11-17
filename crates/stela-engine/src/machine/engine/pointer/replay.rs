//! The memo against a modelled desktop on several PEs, run by hand
//! (`memo_replay_of_threads_on_pes`), with the `PES PAC` census of each PE:
//! what the memo of one PE, and a memo all PEs share, would miss, by key,
//! operation and cause.
//!
//! The earlier model (`memo_replay_of_a_process_mix`) ran one PE, a few
//! processes with their own keys, call trees of 97 sites and depth 7 at
//! most. It predicted 5x fewer cipher runs; the guest gave 1.3x, and its
//! census found 49% of cipher runs new, 36% computed on another PE and 15%
//! lost from the PE's own memo, IB signings 70% of them, and thousands of
//! new key generations a beat. This model adds what the guest has and that
//! one lacked: B keys per key owner (a process or, as XNU 26A434 loads them
//! on each context switch, a thread), hundreds of threads moving between
//! eight PEs, call trees over a large shared library whose frames are deep
//! and whose sites repeat with a skewed (Zipf-like) frequency, and kernel
//! entries on each thread's own kernel stack.

use super::*;
use alloc::{format, string::String};

/// One run of the model.
#[derive(Clone, Copy, Debug)]
struct Desktop {
    pes: usize,
    threads: u64,
    /// Threads per key owner: 1 for keys per thread.
    threads_per_owner: u64,
    /// Distinct call sites of the shared library.
    sites: u64,
    /// Distinct call paths the threads run, from the library's sites.
    paths: u64,
    /// Paths each thread runs most (its loops).
    hot: u64,
    /// The deepest call tree.
    depth: u64,
    /// Call trees a time slice runs.
    calls: u64,
    slices: u64,
    /// Sets this run's keys apart from other runs' in the census's table,
    /// which every run in the process shares.
    salt: u64,
}

/// What a run gives, over all PEs.
#[derive(Debug, Default)]
struct Outcome {
    operations: u64,
    fills: u64,
    /// Fills by key and operation ([`census::Census::fills`]).
    by_key: [[u64; census::OPERATIONS]; pointer::TAG_WORDS],
    causes: [u64; census::CAUSES],
    generations: u64,
    /// Fills a memo every PE shares (per-key generations in one table of
    /// the same size per PE) would still take: the per-PE misses no other
    /// PE's memo holds under the same key words.
    shared_fills: u64,
}

/// A 64-bit finalizer: fixed values from a path, level or site.
fn mix(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^ value >> 33
}

/// A xorshift generator: the model needs only spread, not quality.
struct Spread(u64);

impl Spread {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A site index in `0..sites`, small indices far more frequent.
    fn skewed(&mut self, sites: u64) -> u64 {
        let uniform = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        // P(index < x) grows as log x: Zipf-like over `sites`.
        let index = (sites as f64).powf(uniform) as u64;
        index.saturating_sub(1).min(sites - 1)
    }
}

fn words(salt: u64, word: u64) -> Option<[u64; 2]> {
    let word = word ^ salt;
    Some([word, word.rotate_left(17) ^ 0x5bd1_e995])
}

impl Desktop {
    fn user(&self, thread: u64) -> PointerInputs {
        let owner = thread / self.threads_per_owner;
        PointerInputs {
            keys: [
                words(self.salt, 1),
                words(self.salt, 0x1000 + owner),
                words(self.salt, 3),
                words(self.salt, 0x10_0000 + owner),
                words(self.salt, 5),
            ],
            version: pauth::Version::Classic,
            tcr: 17 | 25 << 16 | 1 << 37 | 1 << 51,
        }
    }

    fn kernel(&self, thread: u64) -> PointerInputs {
        let owner = thread / self.threads_per_owner;
        PointerInputs {
            keys: [
                words(self.salt, 9),
                words(self.salt, 10),
                words(self.salt, 11),
                words(self.salt, 0x10_0000 + owner),
                words(self.salt, 5),
            ],
            ..self.user(thread)
        }
    }

    fn run(&self) -> Outcome {
        let mut services: Vec<Box<PointerService>> = (0..self.pes)
            .map(|pe| {
                let mut service = Box::new(PointerService::new());
                service.census = Some(Box::new(census::Census::new(pe)));
                service
            })
            .collect();
        let mut spread = Spread(0x9e37_79b9_7f4a_7c15);
        let mut shared = Shared::new(self.pes);
        let mut outcome = Outcome::default();
        for _ in 0..self.slices {
            for pe in 0..self.pes {
                // Mostly a thread of this PE's own share; one slice in
                // four a thread from anywhere, which moves it here.
                let thread = if spread.next().is_multiple_of(4) {
                    spread.next() % self.threads
                } else {
                    (spread.next() % (self.threads / self.pes as u64)) * self.pes as u64 + pe as u64
                };
                let stack = 0x16f_0000_0000 - thread * 0x8_0000;
                let kernel_stack = 0xffff_fe30_0000_0000 + thread * 0x4000;
                services[pe].close();
                services[pe].open(self.user(thread));
                for call in 0..self.calls {
                    // A call path of the shared pool, the frequent ones
                    // far more often: its sites and frame sizes are fixed,
                    // so on one thread's stack its (LR, SP) pairs repeat.
                    // Mostly one of the thread's own hot paths (its loops),
                    // else any path of the pool.
                    let path = if spread.next().is_multiple_of(8) {
                        spread.skewed(self.paths)
                    } else {
                        mix(thread << 16 ^ spread.skewed(self.hot)) % self.paths
                    };
                    let depth = 1 + mix(path) % self.depth;
                    let mut sp = stack;
                    for level in 0..depth {
                        let step = mix(path ^ level << 48);
                        let site = 0x1_8000_0000 + 4 * (step % self.sites);
                        sp -= 0x20 + 0x10 * (mix(site) % 8);
                        self.frame(
                            &mut services,
                            &mut shared,
                            pe,
                            Key::Ib,
                            site,
                            sp,
                            &mut outcome,
                        );
                    }
                    if call % 8 == 7 {
                        services[pe].close();
                        services[pe].open(self.kernel(thread));
                        let system_call = spread.skewed(256);
                        for level in 0..6u64 {
                            let step = mix(system_call ^ level << 48 ^ 1 << 63);
                            let site = 0xffff_fe00_0800_0000 + 4 * (step % (self.sites / 4));
                            let sp = kernel_stack - 0x80 * level;
                            self.frame(
                                &mut services,
                                &mut shared,
                                pe,
                                Key::Ib,
                                site,
                                sp,
                                &mut outcome,
                            );
                            self.frame(
                                &mut services,
                                &mut shared,
                                pe,
                                Key::Ia,
                                site + 8,
                                sp,
                                &mut outcome,
                            );
                        }
                        services[pe].close();
                        services[pe].open(self.user(thread));
                    }
                }
            }
        }
        for service in &services {
            outcome.fills += service.fills;
            let census = service.census.as_ref().unwrap();
            for (key, fills) in census.fills.iter().enumerate() {
                for (operation, count) in fills.iter().enumerate() {
                    outcome.by_key[key][operation] += count;
                }
            }
            for (cause, count) in census.causes.iter().enumerate() {
                outcome.causes[cause] += count;
            }
            outcome.generations += census.generations;
        }
        outcome
    }

    /// A frame on `pe`: sign on entry, authenticate on return, through the
    /// PE's memo, and through `shared`, one memo of all PEs' entries.
    #[allow(clippy::too_many_arguments)]
    fn frame(
        &self,
        services: &mut [Box<PointerService>],
        shared: &mut Shared,
        pe: usize,
        key: Key,
        lr: u64,
        sp: u64,
        outcome: &mut Outcome,
    ) {
        let words = services[pe]
            .context
            .as_ref()
            .and_then(|context| context.words(key).copied())
            .expect("the key");
        let signed = match services[pe].result(PointerOperation::Sign, key, lr, sp) {
            Some(Ok((signed, _))) => signed,
            other => panic!("{other:?}"),
        };
        if !shared.access(words, SIGNED, lr, sp) {
            outcome.shared_fills += 1;
            shared.access(words, AUTHENTICATED, signed, sp);
        }
        let operation = PointerOperation::Authenticate { combined: true };
        services[pe].result(operation, key, signed, sp);
        if !shared.access(words, AUTHENTICATED, signed, sp) {
            outcome.shared_fills += 1;
        }
        outcome.operations += 2;
    }
}

const SIGNED: u64 = 1;
const AUTHENTICATED: u64 = 2;

/// A memo every PE shares, as large as all PEs' memos together, two ways to
/// a set, entries told apart by their key's words and kind.
struct Shared {
    /// Tag, pointer and modifier of each entry.
    entries: Vec<[u64; 3]>,
    shift: u32,
}

impl Shared {
    fn new(pes: usize) -> Self {
        let entries = pointer::MEMO_ENTRIES * pes.next_power_of_two();
        Self {
            entries: vec![[0; 3]; entries],
            shift: 64 - (entries / 2).trailing_zeros(),
        }
    }

    /// Whether the memo holds this result; it does after.
    fn access(&mut self, words: [u64; 2], kind: u64, pointer: u64, modifier: u64) -> bool {
        let tag = (words[0] ^ words[1].rotate_left(29)).wrapping_mul(0x9e37_79b9_7f4a_7c15) | kind;
        let set = ((pointer ^ modifier.rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            >> self.shift) as usize
            * 2;
        let entry = [tag, pointer, modifier];
        if self.entries[set] == entry {
            return true;
        }
        if self.entries[set + 1] == entry {
            self.entries.swap(set, set + 1);
            return true;
        }
        self.entries[set + 1] = self.entries[set];
        self.entries[set] = entry;
        false
    }
}

use stela_contracts::pauth::Key;

fn report(desktop: &Desktop, outcome: &Outcome) -> String {
    let fills = outcome.fills.max(1) as f64;
    let ib = outcome.by_key[1];
    format!(
        "REPLAY threads={} per_owner={} paths={} hot={} depth={} operations={} per_operation={:.4} shared_per_operation={:.4} ib_sign={:.2} ib_auth={:.2} new={:.2} here={:.2} other={:.2} generations={}",
        desktop.threads,
        desktop.threads_per_owner,
        desktop.paths,
        desktop.hot,
        desktop.depth,
        outcome.operations,
        outcome.fills as f64 / outcome.operations as f64,
        outcome.shared_fills as f64 / outcome.operations as f64,
        ib[census::SIGN] as f64 / fills,
        ib[census::AUTHENTICATE] as f64 / fills,
        outcome.causes[0] as f64 / fills,
        outcome.causes[1] as f64 / fills,
        outcome.causes[2] as f64 / fills,
        outcome.generations,
    )
}

/// Cipher runs per PAC operation of the modelled desktop, for keys per
/// process and per thread, few and many threads, small and large
/// libraries, with the census split; and what a memo shared by all PEs
/// would still run. Prints one line per model.
#[test]
#[ignore = "manual: opt-in"]
fn memo_replay_of_threads_on_pes() {
    extern crate std;
    let base = Desktop {
        pes: 8,
        threads: 512,
        threads_per_owner: 1,
        sites: 1 << 16,
        paths: 1 << 12,
        hot: 32,
        depth: 24,
        calls: 256,
        slices: 150,
        salt: 0,
    };
    for (run, desktop) in [
        // Few threads with process keys and few paths: the old model's
        // world, where the memo works.
        Desktop {
            threads: 32,
            threads_per_owner: 8,
            paths: 1 << 8,
            depth: 8,
            ..base
        },
        Desktop {
            threads: 128,
            threads_per_owner: 8,
            ..base
        },
        Desktop {
            threads: 128,
            ..base
        },
        Desktop {
            threads_per_owner: 8,
            ..base
        },
        Desktop { ..base },
        Desktop { hot: 128, ..base },
        Desktop {
            threads: 2048,
            ..base
        },
    ]
    .into_iter()
    .enumerate()
    {
        let desktop = Desktop {
            salt: (run as u64 + 1) << 40,
            ..desktop
        };
        let outcome = desktop.run();
        std::println!("{}", report(&desktop, &outcome));
    }
}
