//! The compiler's caches and deferred compilations, over a service that
//! counts its calls.
extern crate std;

use super::*;
use crate::metal::{
    library::tests::fixture,
    nvidia::{CHIPS, fixture::nvk},
};
use alloc::vec;
use core::cell::Cell;

std::thread_local! {
    /// Reflections, compilations and releases the service made.
    static CALLS: Cell<(u32, u32, u32)> = const { Cell::new((0, 0, 0)) };
}

fn calls() -> (u32, u32, u32) {
    CALLS.get()
}

std::thread_local! {
    /// Serialized NIR the hosted image deserialized.
    static DESERIALIZED: Cell<u32> = const { Cell::new(0) };
}

/// The hosted image's NIR deserializer: bytes that start with `N` are NIR,
/// each at an address of its own; others are not.
pub(super) fn deserialize(blob: &[u8]) -> Option<*mut c_void> {
    (blob.first() == Some(&b'N')).then(|| {
        DESERIALIZED.set(DESERIALIZED.get() + 1);
        ptr::without_provenance_mut(0x10_0000 * DESERIALIZED.get() as usize)
    })
}

/// The NIR cache's key of `entry` compiled with `specialization`.
fn key_of(
    compiler: &mut AirCompiler,
    library: u64,
    entry: &str,
    specialization: &Specialization,
) -> [u8; 32] {
    compiler
        .plan(library, entry, specialization, None, None, None)
        .unwrap()
        .key
        .unwrap()
}

std::thread_local! {
    /// For the test that runs jobs on another thread: the compile lock and
    /// whether a thread is inside the service, which LLVM, built without
    /// threads, needs to be one at a time and only under the lock.
    static EXCLUSIVE: core::cell::RefCell<Option<(alloc::sync::Arc<CompileLock>, alloc::sync::Arc<core::sync::atomic::AtomicBool>)>> =
        const { core::cell::RefCell::new(None) };
}

/// Enters the service as LLVM would: alone, under the lock.
fn enter_service() {
    EXCLUSIVE.with_borrow(|exclusive| {
        let Some((lock, inside)) = exclusive else {
            return;
        };
        assert!(
            lock.is_held(),
            "the service entered without the compile lock"
        );
        assert!(
            !inside.swap(true, core::sync::atomic::Ordering::AcqRel),
            "two processors in the service at once"
        );
        for _ in 0..2000 {
            core::hint::spin_loop();
        }
        inside.store(false, core::sync::atomic::Ordering::Release);
    });
}

unsafe extern "C" fn reflect(
    _: *const c_void,
    _: usize,
    _: *const c_char,
    _: *const ffi::ConstantValue,
    _: u32,
    reflection: *mut ffi::Reflection,
    _: *mut ffi::AirError,
) -> bool {
    enter_service();
    let (reflected, compiled, released) = CALLS.get();
    CALLS.set((reflected + 1, compiled, released));
    let mut answer = ffi::Reflection::ZERO;
    // A kernel.
    answer.stage = 3;
    // SAFETY: the compiler passes its reflection record.
    unsafe { *reflection = answer };
    true
}

unsafe extern "C" fn reflect_bindings(
    _: *const c_void,
    _: usize,
    _: *const c_char,
    _: *const ffi::ConstantValue,
    _: u32,
    _: *mut ffi::Bindings,
    _: *mut ffi::AirError,
) -> bool {
    false
}

unsafe extern "C" fn sampler_states(
    _: *const c_void,
    _: usize,
    _: *mut u64,
    count: *mut u32,
    _: *mut ffi::AirError,
) -> bool {
    // SAFETY: the compiler passes its count.
    unsafe { *count = 0 };
    true
}

std::thread_local! {
    /// Stitched modules and function listings the service made.
    static STITCHES: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

unsafe extern "C" fn functions(
    _: *const c_void,
    _: usize,
    _: ffi::VisitFunction,
    _: *mut c_void,
    _: *mut ffi::AirError,
) -> bool {
    let (stitched, listed) = STITCHES.get();
    STITCHES.set((stitched, listed + 1));
    true
}

/// The module every stitch makes: bare bitcode.
const STITCHED_AIR: &[u8] = b"BC\xc0\xde";

unsafe extern "C" fn stitch(
    _: *const ffi::StitchSource,
    _: u32,
    _: *const ffi::StitchGraph,
    _: u32,
    visit: ffi::VisitBytes,
    data: *mut c_void,
    _: *mut ffi::AirError,
) -> bool {
    enter_service();
    let (stitched, listed) = STITCHES.get();
    STITCHES.set((stitched + 1, listed));
    // SAFETY: the callback takes the bytes for the call.
    unsafe { visit(data, STITCHED_AIR.as_ptr().cast(), STITCHED_AIR.len()) };
    true
}

/// Each compilation's NIR is at its own address.
unsafe extern "C" fn compile(
    _: *const c_void,
    _: usize,
    _: *const c_char,
    _: *const ffi::Target,
    _: *const ffi::Specialization,
    _: *const ffi::Reflection,
    _: *mut ffi::AirError,
) -> *mut c_void {
    let (reflected, compiled, released) = CALLS.get();
    CALLS.set((reflected, compiled + 1, released));
    ptr::without_provenance_mut(0x1000 * (compiled as usize + 1))
}

unsafe extern "C" fn tessellation_stages(
    _: *const ffi::Target,
    _: *const ffi::Tessellation,
    _: *mut *mut c_void,
) {
}

unsafe extern "C" fn kernel(_: *const c_void) -> *mut c_void {
    ptr::null_mut()
}

unsafe extern "C" fn release(_: *mut c_void) {
    let (reflected, compiled, released) = CALLS.get();
    CALLS.set((reflected, compiled, released + 1));
}

unsafe extern "C" fn buffer_reads(_: *const c_void) -> u32 {
    0
}

fn caching() -> (AirCompiler, u64) {
    let service = Service {
        reflect,
        reflect_bindings,
        sampler_states,
        functions,
        exports: functions,
        stitch,
        compile,
        tessellation_stages,
        accumulate_kernel: kernel,
        select_kernel: kernel,
        resolve_draws_kernel: kernel,
        resolve_dispatches_kernel: kernel,
        store_texels_kernel: kernel,
        load_texels_kernel: kernel,
        average_samples_kernel: kernel,
        decode_etc_kernel: kernel,
        decode_pvrtc_kernel: kernel,
        reorder_blocks_kernel: kernel,
        decode_astc_kernel: kernel,
        release,
        buffer_reads,
        nir_options: NonNull::dangling(),
    };
    // SAFETY: the fake service never reads the options.
    let mut compiler = unsafe { AirCompiler::new(service, &nvk(&CHIPS[0], 8 << 30)) };
    compiler.keep_compilations(NIR_BUDGET);
    let library = compiler.add_library(fixture(false)).unwrap();
    (compiler, library)
}

#[test]
fn a_function_made_again_reflects_nothing() {
    let (mut compiler, library) = caching();
    // A function without values, and one with: its declaration and the
    // specialization reflect once each.
    compiler.constants(library, "kernel").unwrap();
    compiler.prepare_function(library, "kernel", None).unwrap();
    let values = vec![(0, vec![1])];
    compiler
        .prepare_function(library, "kernel", Some(values.clone()))
        .unwrap();
    assert_eq!(calls().0, 2);
    // Another library of the same bytes, as another process loads it.
    let again = compiler.add_library(fixture(false)).unwrap();
    compiler.constants(again, "kernel").unwrap();
    compiler
        .prepare_function(again, "kernel", Some(values))
        .unwrap();
    assert_eq!(calls().0, 2);
}

#[test]
fn a_compilation_made_again_is_lent_from_the_cache() {
    let (mut compiler, library) = caching();
    let specialization = Specialization {
        workgroup_size: Some([64, 1, 1]),
        ..Specialization::default()
    };
    let first = compiler
        .compile(library, "kernel", &specialization)
        .unwrap();
    compiler.release(first.code);
    let again = compiler.add_library(fixture(false)).unwrap();
    let second = compiler.compile(again, "kernel", &specialization).unwrap();
    assert_eq!(first, second);
    compiler.release(second.code);
    // Another shape compiles.
    let other = Specialization {
        workgroup_size: Some([32, 1, 1]),
        ..specialization
    };
    let third = compiler.compile(library, "kernel", &other).unwrap();
    assert_ne!(third.code, first.code);
    compiler.release(third.code);
    // The service compiled twice and released nothing the cache holds.
    assert_eq!((calls().1, calls().2), (2, 0));
}

#[test]
fn a_deferred_compilation_runs_outside_and_the_call_finds_it() {
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    compiler.set_deferring(true);
    let specialization = Specialization::default();
    assert_eq!(
        compiler.compile(library, "kernel", &specialization),
        Err(DEFERRED)
    );
    assert!(lock.deferred());
    let job = compiler.take_deferred().unwrap();
    assert!(!lock.deferred());
    assert_eq!(job.name(), "kernel");
    assert_eq!(calls().1, 0);
    let finished = job.run_outside(&lock.hold(core::hint::spin_loop));
    assert_eq!(calls().1, 1);
    compiler.adopt(finished);
    // The call's next run finds the shader without compiling.
    let shader = compiler
        .compile(library, "kernel", &specialization)
        .unwrap();
    compiler.release(shader.code);
    compiler.end_call();
    assert_eq!((calls().1, calls().2), (1, 0));
    assert!(compiler.report().contains("nir_hits=1"));
}

/// A call that goes on past a deferral, as a commit does, leaves a job for
/// each compilation it misses, up to the bound, and compiles none in its
/// turn; its next run finds them and leaves a job for the rest.
#[test]
fn a_call_that_goes_on_past_a_deferral_leaves_a_job_for_each_compilation() {
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    let shapes: Vec<_> = (1..=LEFT_PER_CALL as u32 + 1)
        .map(|width| Specialization {
            workgroup_size: Some([width, 1, 1]),
            ..Specialization::default()
        })
        .collect();
    compiler.set_deferring(true);
    for shape in &shapes {
        assert_eq!(compiler.compile(library, "kernel", shape), Err(DEFERRED));
    }
    // Its key again leaves nothing more.
    assert_eq!(
        compiler.compile(library, "kernel", &shapes[0]),
        Err(DEFERRED)
    );
    let jobs: Vec<_> = core::iter::from_fn(|| compiler.take_job()).collect();
    assert_eq!(jobs.len(), LEFT_PER_CALL);
    compiler.end_call();
    assert_eq!(calls().1, 0, "nothing compiled in the turn");
    for job in jobs {
        let done = job.run(&lock.hold(core::hint::spin_loop));
        compiler.adopt_job(done);
    }
    assert_eq!(calls().1, LEFT_PER_CALL as u32);

    compiler.set_deferring(true);
    for shape in &shapes[..LEFT_PER_CALL] {
        let shader = compiler.compile(library, "kernel", shape).unwrap();
        compiler.release(shader.code);
    }
    assert_eq!(
        compiler.compile(library, "kernel", &shapes[LEFT_PER_CALL]),
        Err(DEFERRED)
    );
    assert!(compiler.take_job().is_some() && compiler.take_job().is_none());
    compiler.end_call();
    assert_eq!(calls().1, LEFT_PER_CALL as u32);
}

#[test]
fn a_deferred_reflection_runs_outside_and_the_call_finds_it() {
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    let reflected = calls().0;
    compiler.set_deferring(true);
    let constants = Some(vec![(0, vec![1, 0, 0, 0])]);
    assert_eq!(
        compiler
            .prepare_function(library, "kernel", constants.clone())
            .err(),
        Some(DEFERRED)
    );
    assert!(lock.deferred() && Compiler::deferred(&compiler));
    let job = compiler.take_job().unwrap();
    compiler.end_call();
    assert_eq!(calls().0, reflected);
    let done = job.run(&lock.hold(core::hint::spin_loop));
    assert_eq!(calls().0, reflected + 1);
    compiler.adopt_job(done);
    // The call's next run finds the reflection without the service.
    compiler.set_deferring(true);
    let prepared = compiler
        .prepare_function(library, "kernel", constants)
        .unwrap();
    assert_eq!(prepared.constants, vec![(0, vec![1, 0, 0, 0])]);
    assert!(compiler.take_job().is_none());
    assert_eq!(calls().0, reflected + 1);
}

#[test]
fn a_deferred_stitch_runs_outside_and_the_call_finds_it_with_its_functions() {
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    let sources = [LinkedFunction {
        library,
        entry: String::from("kernel"),
        constants: vec![(0, vec![1, 0, 0, 0])],
        alias: None,
    }];
    let graphs = [StitchedGraph {
        name: "stitched".into(),
        nodes: vec![
            StitchedNode::Input(0),
            StitchedNode::Call {
                function: 0,
                arguments: vec![0],
            },
        ],
        output: Some(1),
        kernel: false,
    }];
    compiler.set_deferring(true);
    assert_eq!(compiler.stitch(&sources, &graphs), Err(DEFERRED));
    assert!(lock.deferred() && Compiler::deferred(&compiler));
    let job = compiler.take_job().unwrap();
    compiler.end_call();
    assert_eq!(STITCHES.get(), (0, 0), "nothing ran in the turn");
    let done = job.run(&lock.hold(core::hint::spin_loop));
    assert_eq!(STITCHES.get(), (1, 1), "the job stitched and listed");
    compiler.adopt_job(done);

    // The call's next run takes the module and its listing.
    compiler.set_deferring(true);
    assert_eq!(compiler.stitch(&sources, &graphs).unwrap(), STITCHED_AIR);
    assert_eq!(compiler.functions(STITCHED_AIR), Ok(Vec::new()));
    assert!(compiler.take_job().is_none());
    compiler.end_call();
    assert_eq!(STITCHES.get(), (1, 1));

    // Without deferring, the turn stitches and lists itself.
    compiler.set_deferring(false);
    assert_eq!(compiler.stitch(&sources, &graphs).unwrap(), STITCHED_AIR);
    assert_eq!(compiler.functions(STITCHED_AIR), Ok(Vec::new()));
    assert_eq!(STITCHES.get(), (2, 2));
}

#[test]
fn a_call_that_stopped_deferring_compiles_in_its_turn() {
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    compiler.set_deferring(true);
    // A commit that changed an indirect command buffer cannot run again.
    compiler.stop_deferring();
    let shader = compiler
        .compile(library, "kernel", &Specialization::default())
        .unwrap();
    compiler.release(shader.code);
    assert!(!lock.deferred() && compiler.take_deferred().is_none());
    assert_eq!(calls().1, 1);
}

#[test]
fn without_kept_compilations_nothing_defers_or_stays() {
    let (mut compiler, library) = caching();
    compiler.nir = None;
    compiler.set_deferring(true);
    let shader = compiler
        .compile(library, "kernel", &Specialization::default())
        .unwrap();
    compiler.release(shader.code);
    assert_eq!((calls().1, calls().2), (1, 1));
}

#[test]
fn reflections_in_turns_and_in_jobs_take_the_service_one_at_a_time() {
    use core::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(std::thread::yield_now);
    let inside = alloc::sync::Arc::new(AtomicBool::new(false));
    let exclusive = (lock.clone(), inside.clone());
    EXCLUSIVE.set(Some(exclusive.clone()));
    let (jobs, queue) = mpsc::channel::<Job>();
    let (finished, done) = mpsc::channel::<Done>();
    let worker = std::thread::spawn(move || {
        EXCLUSIVE.set(Some(exclusive));
        let lock = EXCLUSIVE.with_borrow(|exclusive| exclusive.as_ref().unwrap().0.clone());
        for job in queue {
            finished
                .send(job.run(&lock.hold(std::thread::yield_now)))
                .unwrap();
        }
    });
    let constants = |value: u32| Some(vec![(0, value.to_le_bytes().to_vec())]);
    for round in 0..200 {
        // A call that may defer leaves its reflection as a job.
        compiler.set_deferring(true);
        let deferred = compiler.prepare_function(library, "kernel", constants(round));
        assert_eq!(deferred.err(), Some(DEFERRED));
        jobs.send(compiler.take_job().unwrap()).unwrap();
        compiler.end_call();
        // A call that may not reflects in its turn, while the job may run.
        compiler.set_deferring(false);
        compiler
            .prepare_function(library, "kernel", constants(1000 + round))
            .unwrap();
        compiler.end_call();
        for done in done.try_iter() {
            compiler.adopt_job(done);
        }
    }
    drop(jobs);
    worker.join().unwrap();
    for done in done.try_iter() {
        compiler.adopt_job(done);
    }
    EXCLUSIVE.set(None);
    // Each job's reflection is found without the service.
    compiler.set_deferring(true);
    for round in 0..200 {
        compiler
            .prepare_function(library, "kernel", constants(round))
            .unwrap();
    }
    assert!(compiler.take_job().is_none());
}

#[test]
fn a_failed_try_leaves_the_holders_lock_held() {
    let lock = CompileLock::new();
    let held = lock.hold(core::hint::spin_loop);
    assert!(lock.try_hold().is_none());
    assert!(lock.is_held());
    drop(held);
    assert!(lock.try_hold().is_some());
    assert!(!lock.is_held());
}

#[test]
fn an_evicted_compilation_is_found_again_in_what_this_boot_serialized() {
    fn recall(key: &[u8; 32]) -> Option<Vec<u8>> {
        (key == &[2; 32]).then(|| vec![2, 2])
    }
    static STORED: [u8; 1] = [1];
    let (mut compiler, _) = caching();
    // As `persist` sets them where the image serializes NIR.
    compiler.stored.insert([1; 32], &STORED);
    compiler.recall = Some(recall);
    // An earlier boot's comes first, then this boot's, else none.
    let earlier = compiler.serialized(&[1; 32]).unwrap();
    assert_eq!((&*earlier.0, earlier.1), (&[1u8][..], false));
    let recalled = compiler.serialized(&[2; 32]).unwrap();
    assert_eq!((&*recalled.0, recalled.1), (&[2u8, 2][..], true));
    assert!(compiler.serialized(&[3; 32]).is_none());
    assert!(compiler.report().ends_with("recalled=0"));
}

#[test]
fn a_deferring_call_leaves_stored_nir_to_its_job_which_deserializes_it() {
    static STORED: [u8; 2] = *b"N1";
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    let specialization = Specialization::default();
    let key = key_of(&mut compiler, library, "kernel", &specialization);
    compiler.stored.insert(key, &STORED);
    compiler.set_deferring(true);
    // The turn neither deserializes nor compiles.
    assert_eq!(
        compiler.compile(library, "kernel", &specialization),
        Err(DEFERRED)
    );
    assert_eq!((DESERIALIZED.get(), calls().1), (0, 0));
    let job = compiler.take_deferred().unwrap();
    let finished = job.run_outside(&lock.hold(core::hint::spin_loop));
    // The job deserialized in place of compiling.
    assert_eq!((DESERIALIZED.get(), calls().1), (1, 0));
    compiler.adopt(finished);
    let shader = compiler
        .compile(library, "kernel", &specialization)
        .unwrap();
    assert_eq!(shader.code.0, 0x10_0000);
    compiler.release(shader.code);
    compiler.end_call();
    assert_eq!(DESERIALIZED.get(), 1);
    assert!(
        compiler
            .report()
            .contains("compiled=0 restored=1/1 deferred=1")
    );
}

std::thread_local! {
    /// The one key whose NIR this boot's store recalls as NIR.
    static RECALLED: Cell<[u8; 32]> = const { Cell::new([0; 32]) };
}

#[test]
fn a_job_deserializes_this_boots_nir_and_compiles_what_does_not_deserialize() {
    fn recall(key: &[u8; 32]) -> Option<Vec<u8>> {
        Some(match *key == RECALLED.get() {
            true => b"N2".to_vec(),
            false => b"X".to_vec(),
        })
    }
    let (mut compiler, library) = caching();
    let lock = compiler.compile_lock(core::hint::spin_loop);
    compiler.recall = Some(recall);
    let shapes = [32, 64].map(|width| Specialization {
        workgroup_size: Some([width, 1, 1]),
        ..Specialization::default()
    });
    RECALLED.set(key_of(&mut compiler, library, "kernel", &shapes[0]));
    for specialization in &shapes {
        compiler.set_deferring(true);
        assert_eq!(
            compiler.compile(library, "kernel", specialization),
            Err(DEFERRED)
        );
        let job = compiler.take_deferred().unwrap();
        compiler.end_call();
        compiler.adopt(job.run_outside(&lock.hold(core::hint::spin_loop)));
    }
    // The first deserialized, the second's bytes are no NIR: it compiled.
    assert_eq!((DESERIALIZED.get(), calls().1), (1, 1));
    let report = compiler.report();
    assert!(
        report.contains("compiled=1 restored=0/0 deferred=2"),
        "{report}"
    );
    assert!(report.ends_with("recalled=1"), "{report}");
}

#[test]
fn a_call_that_does_not_defer_deserializes_stored_nir_in_its_turn() {
    static STORED: [u8; 2] = *b"N1";
    let (mut compiler, library) = caching();
    let specialization = Specialization::default();
    let key = key_of(&mut compiler, library, "kernel", &specialization);
    compiler.stored.insert(key, &STORED);
    let shader = compiler
        .compile(library, "kernel", &specialization)
        .unwrap();
    assert_eq!((DESERIALIZED.get(), calls().1), (1, 0));
    compiler.release(shader.code);
    assert!(
        compiler
            .report()
            .contains("compiled=0 restored=1/1 deferred=0")
    );
}

std::thread_local! {
    /// What the compiler gave the next boot: keys and blobs.
    static KEPT: core::cell::RefCell<Vec<([u8; 32], Vec<u8>)>> =
        const { core::cell::RefCell::new(Vec::new()) };
}

fn keep(key: &[u8; 32], blob: &[u8]) {
    KEPT.with_borrow_mut(|kept| kept.push((*key, blob.to_vec())));
}

fn recall_none(_: &[u8; 32]) -> Option<Vec<u8>> {
    None
}

/// The digest a reflection of `entry` with `values` is kept by.
fn reflection_key(
    compiler: &AirCompiler,
    library: u64,
    entry: &str,
    values: Option<&[(u32, Vec<u8>)]>,
) -> [u8; 32] {
    let selected = select(&compiler.libraries, library, entry).unwrap();
    reflection_digest(&(
        selected.module.slice_hash,
        String::from(entry),
        values.map(<[_]>::to_vec),
    ))
}

#[test]
fn a_reflection_record_packs_small_and_unpacks_the_same() {
    let mut record = ffi::Reflection::zeroed();
    record.stage = 3;
    record.threadgroup_bytes = 0x1234_5678_9abc;
    record.buffer_count = 2;
    record.buffers[1].index = 7;
    record.names[..5].copy_from_slice(b"hello");
    record.names[ffi::NAME_BYTES - 1] = 9;
    let packed = record.pack();
    assert!(packed.len() < 128, "{} bytes", packed.len());
    let unpacked = ffi::Reflection::unpack(&packed).unwrap();
    assert_eq!(unpacked.pack(), packed);
    assert_eq!(
        (
            unpacked.stage,
            unpacked.threadgroup_bytes,
            unpacked.buffers[1].index
        ),
        (3, 0x1234_5678_9abc, 7)
    );
    assert_eq!(&unpacked.names[..5], b"hello");
    assert_eq!(unpacked.names[ffi::NAME_BYTES - 1], 9);
    let empty = ffi::Reflection::zeroed();
    assert_eq!(
        ffi::Reflection::unpack(&empty.pack()).unwrap().pack(),
        empty.pack()
    );
    // Damaged or of another layout: none.
    assert!(ffi::Reflection::unpack(&packed[..packed.len() - 1]).is_none());
    assert!(ffi::Reflection::unpack(&[1, 0, 0, 0]).is_none());
    let mut past = packed.clone();
    past.extend_from_slice(&[0xff, 0xff, 0, 0, 1, 0, 0, 0, 1]);
    assert!(ffi::Reflection::unpack(&past).is_none());
}

/// A boot keeps each reflection, in a call's turn or in a job, under the
/// digest its job is known by; a later boot decodes it in the turn of a
/// call that may defer: no service call, no job.
#[test]
fn a_reflection_an_earlier_boot_kept_decodes_without_the_service_or_a_job() {
    KEPT.with_borrow_mut(Vec::clear);
    let (mut compiler, library) = caching();
    compiler.persist = Some(keep);
    let lock = compiler.compile_lock(core::hint::spin_loop);
    let values = vec![(0, vec![1, 0, 0, 0])];
    // In the turn.
    compiler.prepare_function(library, "kernel", None).unwrap();
    // In a job.
    compiler.set_deferring(true);
    assert_eq!(
        compiler
            .prepare_function(library, "kernel", Some(values.clone()))
            .err(),
        Some(DEFERRED)
    );
    let job = compiler.take_job().unwrap();
    compiler.end_call();
    compiler.adopt_job(job.run(&lock.hold(core::hint::spin_loop)));
    let kept = KEPT.take();
    let declared = reflection_key(&compiler, library, "kernel", None);
    let specialized = reflection_key(&compiler, library, "kernel", Some(&values));
    assert_eq!(
        kept.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        [declared, specialized]
    );
    // The next boot.
    let reflected = calls().0;
    let (mut warm, library) = caching();
    let blobs: Vec<&'static [u8]> = kept
        .iter()
        .map(|(_, blob)| &*alloc::boxed::Box::leak(blob.clone().into_boxed_slice()))
        .collect();
    warm.stored.insert(declared, blobs[0]);
    warm.stored.insert(specialized, blobs[1]);
    warm.persist = Some(keep);
    warm.recall = Some(recall_none);
    warm.set_deferring(true);
    let prepared = warm
        .prepare_function(library, "kernel", Some(values))
        .unwrap();
    assert_eq!(prepared.constants, vec![(0, vec![1, 0, 0, 0])]);
    warm.prepare_function(library, "kernel", None).unwrap();
    assert!(warm.take_job().is_none());
    assert_eq!(calls().0, reflected, "the service reflected nothing");
    assert!(KEPT.take().is_empty(), "nothing kept twice");
    let report = warm.report();
    assert!(
        report.contains("reflected=0 reflections=2 reflections_restored=2"),
        "{report}"
    );
}

/// A kept reflection that does not unpack is reflected again, as with none.
#[test]
fn a_damaged_kept_reflection_reflects_again() {
    static DAMAGED: [u8; 3] = [1, 2, 3];
    let (mut compiler, library) = caching();
    let key = reflection_key(&compiler, library, "kernel", None);
    compiler.stored.insert(key, &DAMAGED);
    let reflected = calls().0;
    compiler.prepare_function(library, "kernel", None).unwrap();
    assert_eq!(calls().0, reflected + 1);
    assert!(compiler.report().contains("reflections_restored=0"));
}
