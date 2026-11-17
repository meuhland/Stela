//! Native code given the service's cipher computes exactly what the
//! engine's exit computes ([`PointerService::result`] with the reference
//! cipher of the same keys), for every key, form and PAC version, under
//! TCRs with and without TBI and TBID, for EL0, EL1 and EL2 address
//! layouts, both key enables and a closed key: thunks and blocks complete
//! signings, successful authentications and PACGA without the service, and
//! leave the rest to it, which completes or refuses them as before.

use super::*;
use alloc::format;
use stela_compiler::{
    GuestAddress, GuestState, TargetCpu,
    block::{
        BlockProfile, CompilationPlan, ExitKind, Features,
        PointerAuthenticationModifier as Modifier, PointerAuthenticationOperation as Operation,
    },
    x86_64::{NativeSystem, block_entry, pinned},
};

/// Executable copies of native code, unmapped on drop.
struct Mapped {
    page: *mut core::ffi::c_void,
    bytes: usize,
}

unsafe extern "C" {
    fn mmap(
        address: *mut core::ffi::c_void,
        bytes: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut core::ffi::c_void;
    fn mprotect(address: *mut core::ffi::c_void, bytes: usize, protection: i32) -> i32;
    fn munmap(address: *mut core::ffi::c_void, bytes: usize) -> i32;
}

#[cfg(target_os = "macos")]
const MAP_ANONYMOUS: i32 = 0x1000;
#[cfg(target_os = "linux")]
const MAP_ANONYMOUS: i32 = 0x20;

impl Mapped {
    /// `body` entered as the engine enters native code
    /// (`pinned::entered`).
    fn new(body: &[u8]) -> Self {
        let code = pinned::entered(body);
        let bytes = code.len().next_multiple_of(4096);
        // SAFETY: private anonymous memory, RX after the copy.
        unsafe {
            let page = mmap(core::ptr::null_mut(), bytes, 3, 2 | MAP_ANONYMOUS, -1, 0);
            assert_ne!(page as usize, usize::MAX);
            core::ptr::copy_nonoverlapping(code.as_ptr(), page.cast(), code.len());
            assert_eq!(mprotect(page, bytes, 5), 0);
            Self { page, bytes }
        }
    }

    /// Run it on `state`: the exit's PC and kind.
    fn run(&self, state: &mut GuestState) -> (u64, u64) {
        // SAFETY: the code is the block ABI's; it touches only `state`, what
        // its anchors name (the service, its memo), and the stack, and calls
        // the service.
        let exit = unsafe { block_entry::enter(self.page.cast(), state, 0, 0) };
        (exit as u64, (exit >> 64) as u64)
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: mapped by `new`, no longer running.
        unsafe { assert_eq!(munmap(self.page, self.bytes), 0) };
    }
}

/// The cipher's place, or `None` on a host without AES-NI, whose tests
/// then have nothing to compare.
fn cipher() -> Option<Place> {
    let place = native_cipher(TargetCpu::detect());
    if place.is_none() {
        extern crate std;
        std::eprintln!("no AES-NI: the native cipher is not used on this host");
    }
    place
}

/// TCRs of the regimes the engine runs: EL1&0 with 39- and 47-bit ranges,
/// TBI0 alone (macOS EL0 data), TBI0 with TBID0, both TBIs, neither (a
/// signing then selects by bit 63), and TBI1 with TBID1, as an EL2 host
/// regime may.
const TCRS: [u64; 6] = [
    25 | 25 << 16 | 1 << 37,
    17 | 25 << 16 | 1 << 37 | 1 << 51,
    17 | 17 << 16 | 1 << 37 | 1 << 38,
    25 | 25 << 16,
    16 | 39 << 16 | 1 << 38 | 1 << 52,
    39 | 16 << 16 | 1 << 38,
];

const VERSIONS: [pauth::Version; 4] = [
    pauth::Version::Classic,
    pauth::Version::Enhanced,
    pauth::Version::Faulting,
    pauth::Version::FaultingCombined,
];

/// Pointers of EL0 and of the kernel, tagged, all ones, zero and garbage,
/// before signing.
const POINTERS: [u64; 8] = [
    0x0000_0001_0000_8000,
    0x0000_7fff_ffff_f000,
    0xffff_fe00_1234_5678,
    0xffff_ff80_0012_3450,
    0x5a00_0001_0000_8000,
    u64::MAX,
    0,
    0x1234_5678_9abc_def0,
];

const KEYS: [PointerAuthenticationKey; 4] = [
    PointerAuthenticationKey::Ia,
    PointerAuthenticationKey::Ib,
    PointerAuthenticationKey::Da,
    PointerAuthenticationKey::Db,
];

/// A thunk's form: a signing into X3, a plain authentication of X3, a
/// branch authentication of X3 (the combined form), each with X5 as the
/// modifier, or PACGA of X3 and X5 into X4.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Sign,
    Authenticate,
    Branch,
    Generic,
}

fn operation(kind: Kind, key: PointerAuthenticationKey) -> Operation {
    let modifier = Modifier::Register(5);
    match kind {
        Kind::Sign => Operation::Sign {
            key,
            destination: 3,
            modifier,
        },
        Kind::Authenticate => Operation::Authenticate {
            key,
            destination: 3,
            modifier,
        },
        Kind::Branch => Operation::Branch {
            key,
            target: 3,
            modifier,
            link: false,
        },
        Kind::Generic => Operation::GenericCode(stela_compiler::block::pauth_memory::GenericCode {
            destination: 4,
            source: 3,
            modifier: 5,
        }),
    }
}

/// A key's enable: open and enabled, disabled, or its tag word closed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Enable {
    Enabled,
    Disabled,
    Closed,
}

const PC: u64 = 0x4000;

/// Thunks compute what the engine computes: signings, successful
/// authentications and PACGA without a service call, failures and
/// non-canonical signings through it, as before.
#[test]
fn thunks_compute_what_the_engine_computes() {
    let Some(cipher) = cipher() else { return };
    let mut thunks = Vec::new();
    for kind in [Kind::Sign, Kind::Authenticate, Kind::Branch] {
        for key in KEYS {
            let form = pointer::Form::of(operation(kind, key)).expect("a thunk form");
            let thunk = pointer::thunk(
                form,
                GuestAddress(PC),
                TAGS,
                SERVICE as usize as u64,
                Some(cipher),
            );
            thunks.push((kind, Some(key), Mapped::new(&thunk.code)));
        }
    }
    let generic =
        pointer::Form::of(operation(Kind::Generic, PointerAuthenticationKey::Ia)).unwrap();
    let thunk = pointer::thunk(
        generic,
        GuestAddress(PC),
        TAGS,
        SERVICE as usize as u64,
        Some(cipher),
    );
    thunks.push((Kind::Generic, None, Mapped::new(&thunk.code)));
    let (mut computed, mut serviced) = (0u64, 0u64);
    for tcr in TCRS {
        for version in VERSIONS {
            let inputs = super::tests::inputs(version, tcr);
            let mut native = Box::new(PointerService::new());
            let mut engine = PointerService::new();
            native.open(inputs);
            engine.open(inputs);
            let mut state = GuestState::default();
            state.anchors[Anchor::Pointer] = native.anchors().0;
            for (kind, key, code) in &thunks {
                for enable in [Enable::Enabled, Enable::Disabled, Enable::Closed] {
                    if key.is_none() && enable == Enable::Disabled {
                        continue;
                    }
                    native.close_native();
                    match (key, enable) {
                        (_, Enable::Closed) => {}
                        (Some(key), enable) => {
                            native.reopen(key_of(*key), enable == Enable::Enabled, false)
                        }
                        (None, _) => native.reopen_generic(false),
                    }
                    // The memo a usable tag word allocates.
                    state.anchors[Anchor::Memo] = native.anchors().1;
                    for (step, &base) in POINTERS.iter().enumerate() {
                        let modifier = 0xffff_fe10_0000_0000 ^ (step as u64) << 4 ^ tcr;
                        let mut operands = vec![base];
                        if let Some(key) = key {
                            let signed =
                                engine.result(PointerOperation::Sign, key_of(*key), base, modifier);
                            if let Some(Ok((signed, _))) = signed {
                                operands.extend([signed, signed ^ 1 << 50, signed ^ 1 << 62]);
                            }
                        }
                        for operand in operands {
                            let expected =
                                expect(&mut engine, *kind, *key, enable, operand, modifier);
                            state.x[3] = operand;
                            state.x[4] = 0x4444;
                            state.x[5] = modifier;
                            let fills = native.fills;
                            let exit = code.run(&mut state);
                            let what = format!(
                                "{version:?} tcr={tcr:#x} {kind:?} {key:?} {enable:?} {operand:#x}"
                            );
                            let destination = if matches!(kind, Kind::Generic) { 4 } else { 3 };
                            match (expected, kind) {
                                (Some(value), Kind::Branch) => {
                                    assert_eq!(exit, (value, ExitKind::Indirect as u64), "{what}");
                                }
                                (Some(value), _) => {
                                    assert_eq!(
                                        exit,
                                        (PC + 4, ExitKind::Fallthrough as u64),
                                        "{what}"
                                    );
                                    assert_eq!(state.x[destination], value, "{what}");
                                }
                                (None, _) => {
                                    assert_eq!(
                                        exit,
                                        (PC, ExitKind::PointerAuthentication as u64),
                                        "{what}"
                                    );
                                    assert_eq!(state.x[3], operand, "{what}");
                                }
                            }
                            // Completions the cipher gives never reach
                            // the service.
                            let canonical = enable == Enable::Disabled
                                || native_completes(&native, *kind, *key, operand);
                            if expected.is_some() && canonical {
                                assert_eq!(native.fills, fills, "{what}: a service call");
                                computed += 1;
                            } else if native.fills != fills {
                                serviced += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    // Both paths ran: the corrupted signings and the authentications of
    // unsigned pointers among the operands fail, through the service.
    assert!(
        computed > 10_000 && serviced > 0,
        "{computed} computed, {serviced} serviced"
    );
}

/// Native transitions switch the tag words between two exception levels
/// of one memo generation whose enables differ (`transition::check_pointer`
/// copies the tag words the engine reopened for each), without the service.
/// Thunks and blocks compiled once then give, at each level, what the engine
/// gives there: the pointer unchanged where the key is disabled, its
/// signing or authentication where it is enabled, in any order of switches.
/// Before, a disabled key's zero layouts stayed after a switch to the level
/// where it is enabled, and an enabled key's layouts after a switch back, so
/// a process whose keys are disabled at EL0 got signed pointers.
#[test]
fn tag_words_switched_natively_keep_each_levels_enables() {
    use stela_contracts::pauth::Key;
    let Some(cipher) = cipher() else { return };
    let mut thunks = Vec::new();
    for kind in [Kind::Sign, Kind::Authenticate, Kind::Branch] {
        for key in KEYS {
            let form = pointer::Form::of(operation(kind, key)).unwrap();
            let thunk = pointer::thunk(
                form,
                GuestAddress(PC),
                TAGS,
                SERVICE as usize as u64,
                Some(cipher),
            );
            thunks.push((kind, key, Mapped::new(&thunk.code)));
        }
    }
    for tcr in TCRS {
        for version in VERSIONS {
            let inputs = super::tests::inputs(version, tcr);
            let mut native = Box::new(PointerService::new());
            let mut engine = PointerService::new();
            native.open(inputs);
            engine.open(inputs);
            // The kernel's level: every key enabled. The process's: IA and
            // DA disabled (a process without arm64e's JOP keys), IB and DB
            // enabled; then the reverse.
            let level = |native: &mut PointerService, disabled: [bool; 4]| {
                native.close_native();
                for (key, disabled) in [Key::Ia, Key::Ib, Key::Da, Key::Db]
                    .into_iter()
                    .zip(disabled)
                {
                    native.reopen(key, !disabled, false);
                }
                native.tags
            };
            let kernel = level(&mut native, [false; 4]);
            let process = level(&mut native, [true, false, true, false]);
            let other = level(&mut native, [false, true, false, true]);
            let mut state = GuestState::default();
            state.anchors[Anchor::Pointer] = native.anchors().0;
            state.anchors[Anchor::Memo] = native.anchors().1;
            for (round, tags) in [kernel, process, kernel, other, process, other, kernel]
                .into_iter()
                .enumerate()
            {
                // As a native transition writes them.
                native.tags = tags;
                for (kind, key, code) in &thunks {
                    let index = usize::from(pointer::key_index(*key));
                    let enable = if tags[index] == pointer::DISABLED {
                        Enable::Disabled
                    } else {
                        Enable::Enabled
                    };
                    let pointer = POINTERS[round % 4];
                    let modifier = 0x0000_0001_6fd0_0000 + round as u64 * 0x40;
                    let signed = match engine.result(
                        PointerOperation::Sign,
                        key_of(*key),
                        pointer,
                        modifier,
                    ) {
                        Some(Ok((signed, _))) => signed,
                        other => panic!("{other:?}"),
                    };
                    let operand = if matches!(kind, Kind::Sign) {
                        pointer
                    } else {
                        signed
                    };
                    let expected =
                        expect(&mut engine, *kind, Some(*key), enable, operand, modifier);
                    state.x[3] = operand;
                    state.x[5] = modifier;
                    let exit = code.run(&mut state);
                    let what = format!(
                        "{version:?} tcr={tcr:#x} round {round} {kind:?} {key:?} {enable:?}"
                    );
                    match (expected, kind) {
                        (Some(value), Kind::Branch) => {
                            assert_eq!(exit, (value, ExitKind::Indirect as u64), "{what}")
                        }
                        (Some(value), _) => {
                            assert_eq!(exit, (PC + 4, ExitKind::Fallthrough as u64), "{what}");
                            assert_eq!(state.x[3], value, "{what}");
                        }
                        // A pointer outside the regime's range fails its
                        // authentication, at either level.
                        (None, _) => {
                            assert_eq!(exit, (PC, ExitKind::PointerAuthentication as u64), "{what}")
                        }
                    }
                }
            }
        }
    }
}

/// What the engine's exit gives for `kind` on `operand`: the completed
/// value, or `None` for what the native code leaves to the engine.
fn expect(
    engine: &mut PointerService,
    kind: Kind,
    key: Option<PointerAuthenticationKey>,
    enable: Enable,
    operand: u64,
    modifier: u64,
) -> Option<u64> {
    match (enable, key) {
        (Enable::Closed, _) => None,
        (Enable::Disabled, _) => Some(operand),
        (Enable::Enabled, None) => engine.generic_code(operand, modifier),
        (Enable::Enabled, Some(key)) => {
            let operation = match kind {
                Kind::Sign => PointerOperation::Sign,
                Kind::Authenticate => PointerOperation::Authenticate { combined: false },
                _ => PointerOperation::Authenticate { combined: true },
            };
            match engine.result(operation, key_of(key), operand, modifier) {
                Some(Ok((value, true))) => Some(value),
                _ => None,
            }
        }
    }
}

/// Whether the cipher alone answers `kind` on `operand`: PACGA, any
/// authentication, and a signing whose pointer is canonical in its layout.
fn native_completes(
    service: &PointerService,
    kind: Kind,
    key: Option<PointerAuthenticationKey>,
    operand: u64,
) -> bool {
    let (Kind::Sign, Some(key)) = (kind, key) else {
        return true;
    };
    let layout = service.cipher.layouts[usize::from(pointer::key_index(key))][0];
    let extension = layout.extension[usize::from(operand & layout.select != 0)];
    operand & extension == 0 || operand & extension == extension
}

/// Blocks given the cipher complete PAC instructions in line with the
/// engine's results and leave at the first one the engine does not
/// complete, its result unwritten.
#[test]
fn blocks_compute_what_the_engine_computes() {
    use stela_contracts::pauth::Key;
    let Some(cipher) = cipher() else { return };
    // PACIA X0, X1; AUTDB X2, X3; PACGA X4, X5, X6; PACDB X7, X8;
    // AUTIB X9, X10; ADD X11, X11, #1.
    const WORDS: [u32; 6] = [
        0xdac1_0020,
        0xdac1_1c62,
        0x9ac6_30a4,
        0xdac1_0d07,
        0xdac1_1549,
        0x9100_056b,
    ];
    let source: Vec<u8> = WORDS.iter().flat_map(|word| word.to_le_bytes()).collect();
    let options = stela_compiler::Options::new(
        BlockProfile::new(Features::SYSTEM_MODE),
        TargetCpu::detect(),
    )
    .with_native_system(NativeSystem {
        pointer: Some(cells(TargetCpu::detect())),
        ..NativeSystem::NONE
    });
    assert_eq!(
        cells(TargetCpu::detect()).cipher,
        Some(cipher.offset as u32)
    );
    let block = CompilationPlan::new(&source, GuestAddress(PC), &options)
        .unwrap()
        .compile()
        .unwrap();
    assert_eq!(block.source_bytes, source.len());
    let code = Mapped::new(&block.native.x86_64_code);
    for tcr in TCRS {
        for version in VERSIONS {
            let inputs = super::tests::inputs(version, tcr);
            let mut native = Box::new(PointerService::new());
            let mut engine = PointerService::new();
            native.open(inputs);
            engine.open(inputs);
            for key in [Key::Ia, Key::Ib, Key::Da, Key::Db] {
                native.reopen(key, true, false);
            }
            native.reopen_generic(false);
            let (service, memo) = native.anchors();
            let mut state = GuestState::default();
            state.anchors[Anchor::Pointer] = service;
            state.anchors[Anchor::Memo] = memo;
            for (step, &pointer) in POINTERS.iter().enumerate() {
                let modifier = 0x0000_0001_6fd0_0000 + step as u64 * 0x30;
                let signed = |engine: &mut PointerService, key| match engine.result(
                    PointerOperation::Sign,
                    key,
                    pointer,
                    modifier,
                ) {
                    Some(Ok((signed, _))) => signed,
                    other => panic!("{other:?}"),
                };
                let db = signed(&mut engine, Key::Db);
                let ib = signed(&mut engine, Key::Ib);
                // A corrupted IB signing every other step.
                let ib = if step % 2 == 1 { ib ^ 1 << 52 } else { ib };
                let mut x = [0u64; 12];
                x[1] = modifier;
                x[0] = pointer;
                x[2] = db;
                x[3] = modifier;
                (x[4], x[5], x[6]) = (pointer, pointer ^ modifier, modifier);
                (x[7], x[8]) = (pointer, modifier + 8);
                (x[9], x[10]) = (ib, modifier);
                state.x[..12].copy_from_slice(&x);
                // The engine's results, in order, up to the first it does
                // not complete.
                let mut expected = x;
                let mut stop = None;
                let steps: [(usize, Option<Key>, PointerOperation, usize, usize); 5] = [
                    (0, Some(Key::Ia), PointerOperation::Sign, 0, 1),
                    (
                        1,
                        Some(Key::Db),
                        PointerOperation::Authenticate { combined: false },
                        2,
                        3,
                    ),
                    (2, None, PointerOperation::Sign, 4, 5),
                    (3, Some(Key::Db), PointerOperation::Sign, 7, 8),
                    (
                        4,
                        Some(Key::Ib),
                        PointerOperation::Authenticate { combined: false },
                        9,
                        10,
                    ),
                ];
                for (at, key, operation, target, from) in steps {
                    let result = match key {
                        Some(key) => {
                            match engine.result(operation, key, expected[target], expected[from]) {
                                Some(Ok((value, true))) => Some(value),
                                _ => None,
                            }
                        }
                        None => engine.generic_code(expected[from], expected[6]),
                    };
                    match result {
                        Some(value) => {
                            let destination = if key.is_none() { 4 } else { target };
                            expected[destination] = value;
                        }
                        None => {
                            stop = Some(at);
                            break;
                        }
                    }
                }
                let exit = code.run(&mut state);
                let what = format!("{version:?} tcr={tcr:#x} {pointer:#x}");
                match stop {
                    Some(at) => assert_eq!(
                        exit,
                        (PC + 4 * at as u64, ExitKind::PointerAuthentication as u64),
                        "{what}"
                    ),
                    None => {
                        expected[11] += 1;
                        assert_eq!(exit.0, PC + 4 * WORDS.len() as u64, "{what}");
                    }
                }
                assert_eq!(state.x[..12], expected, "{what}");
            }
        }
    }
}

/// The schedule native code runs gives what the engine's cipher gives.
#[test]
fn schedules_follow_the_keys_words() {
    if cipher().is_none() {
        return;
    }
    let mut service = PointerService::new();
    let inputs = super::tests::inputs(pauth::Version::Enhanced, TCRS[0]);
    service.open(inputs);
    let context = service.context.as_ref().unwrap();
    for (index, key) in [
        stela_contracts::pauth::Key::Ia,
        stela_contracts::pauth::Key::Ib,
        stela_contracts::pauth::Key::Da,
        stela_contracts::pauth::Key::Db,
    ]
    .into_iter()
    .enumerate()
    {
        let words = context.words(key).unwrap();
        assert_eq!(service.cipher.schedules[index][0], *words);
        assert_eq!(
            service.cipher.schedules[index],
            pauth::keyed::schedule(words)
        );
    }
    assert_eq!(service.cipher.classic, 0);
    assert_eq!(service.cipher.enhanced, u64::MAX);
}

/// What a PAC operation costs natively, run by hand in a release build: an
/// IB signing (PACIB X3, X5) and its authentication, each through a thunk
/// that computes it, one that finds it in the memo (a hit on a warm memo,
/// the best case a lookup has) and one that misses the memo and calls the
/// service. Prints nanoseconds per run, with the entry around each run,
/// and host instructions per operation, single-stepped, past that entry.
#[test]
#[ignore = "manual: opt-in"]
fn native_cipher_timing() {
    extern crate std;
    use std::time::Instant;
    const RUNS: u64 = 1 << 20;
    let Some(cipher) = cipher() else { return };
    let tcr = 25 | 25 << 16 | 1 << 37;
    let mut service = Box::new(PointerService::new());
    service.open(super::tests::inputs(pauth::Version::Classic, tcr));
    service.reopen(stela_contracts::pauth::Key::Ib, true, false);
    let (anchor, memo) = service.anchors();
    let mut state = GuestState::default();
    state.anchors[Anchor::Pointer] = anchor;
    state.anchors[Anchor::Memo] = memo;
    let key = PointerAuthenticationKey::Ib;
    let thunk = |kind, cipher| {
        let form = pointer::Form::of(operation(kind, key)).unwrap();
        Mapped::new(
            &pointer::thunk(
                form,
                GuestAddress(PC),
                TAGS,
                SERVICE as usize as u64,
                cipher,
            )
            .code,
        )
    };
    let (computed_sign, computed_auth) = (
        thunk(Kind::Sign, Some(cipher)),
        thunk(Kind::Authenticate, Some(cipher)),
    );
    let memo_sign = thunk(Kind::Sign, None);
    #[cfg(target_os = "linux")]
    let memo_auth = thunk(Kind::Authenticate, None);
    // The entry alone: a block of one ADD.
    let add = 0x9100_056bu32.to_le_bytes();
    let options = stela_compiler::Options::new(
        BlockProfile::new(Features::SYSTEM_MODE),
        TargetCpu::detect(),
    )
    .with_native_system(NativeSystem {
        pointer: Some(cells(TargetCpu::detect())),
        ..NativeSystem::NONE
    });
    let empty = CompilationPlan::new(&add, GuestAddress(PC), &options)
        .unwrap()
        .compile()
        .unwrap();
    let empty = Mapped::new(&empty.native.x86_64_code);
    let operands = |run: u64| {
        (
            0xffff_ff80_0800_0000 + (run % 4096) * 4,
            0xffff_fe10_0000_0000 - run * 16,
        )
    };
    let signed = |service: &mut PointerService, run| {
        let (pointer, modifier) = operands(run);
        match service.result(
            PointerOperation::Sign,
            stela_contracts::pauth::Key::Ib,
            pointer,
            modifier,
        ) {
            Some(Ok((signed, _))) => signed,
            other => panic!("{other:?}"),
        }
    };
    let time = |code: &Mapped, state: &mut GuestState, operand: &dyn Fn(u64) -> (u64, u64)| {
        let started = Instant::now();
        for run in 0..RUNS {
            let (pointer, modifier) = operand(run);
            state.x[3] = pointer;
            state.x[5] = modifier;
            core::hint::black_box(code.run(state));
        }
        started.elapsed().as_nanos() as f64 / RUNS as f64
    };
    let fresh = |run: u64| operands(run + 7 * RUNS);
    let entry = time(&empty, &mut state, &fresh);
    let computed = time(&computed_sign, &mut state, &fresh);
    let fills = service.fills;
    let missed = time(&memo_sign, &mut state, &fresh);
    let misses = service.fills - fills;
    // A warm memo: one operand pair, found every time.
    let same = |_| operands(1);
    let _ = time(&memo_sign, &mut state, &same);
    let hit = time(&memo_sign, &mut state, &same);
    let pairs: Vec<(u64, u64)> = (0..4096)
        .map(|run| (signed(&mut service, run), operands(run).1))
        .collect();
    let authentic = |run: u64| pairs[(run % 4096) as usize];
    let computed_authentication = time(&computed_auth, &mut state, &authentic);
    std::println!(
        "NATIVE ns_per_run entry={entry:.1} computed_sign={computed:.1} memo_hit_sign={hit:.1} memo_miss_sign={missed:.1} ({misses} service fills) computed_auth={computed_authentication:.1}"
    );
    #[cfg(target_os = "linux")]
    {
        let count = |code: &Mapped, state: &mut GuestState, (pointer, modifier): (u64, u64)| {
            state.x[3] = pointer;
            state.x[5] = modifier;
            super::tests::steps(|| {
                core::hint::black_box(code.run(state));
            })
        };
        let base = count(&empty, &mut state, operands(1));
        let computed = count(&computed_sign, &mut state, operands(3 * RUNS)) - base;
        let missed = count(&memo_sign, &mut state, operands(3 * RUNS + 1)) - base;
        let hit = count(&memo_sign, &mut state, operands(1)) - base;
        let computed_authentication = count(&computed_auth, &mut state, pairs[5]) - base;
        let hit_authentication = count(&memo_auth, &mut state, pairs[4095]) - base;
        std::println!(
            "NATIVE instructions_past_entry computed_sign={computed} memo_hit_sign={hit} memo_miss_sign={missed} computed_auth={computed_authentication} memo_hit_auth={hit_authentication} (an ADD block's run: {base})"
        );
    }
}
