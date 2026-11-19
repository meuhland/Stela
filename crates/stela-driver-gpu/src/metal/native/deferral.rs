//! Pipeline creations deferred outside the service's turn
//! ([`crate::metal::deferred`], `service/shared.rs`).
//!
//! While a call may defer, no pipeline is created in its turn: even found in
//! a cache, a creation serializes and hashes each stage's NIR and loads the
//! shaders, which took a large shader 45-55 ms. The call fails with
//! `DEFERRED` and leaves a job that creates the pipeline from its stages'
//! NIR: looked up in the pipeline store's cache and, when it lacks the
//! pipeline, compiled into a scratch cache whose objects the store keeps
//! with the compile's cost (the backend's own cache without a store). NIR
//! to machine code, when no cache holds it, runs in the job too. A creation through a binary archive's
//! cache that adds what it lacks (`PipelineSource::Cached`, as an archived
//! pipeline's variants are) runs in a job through that same cache, which the
//! archive's destruction then leaves to the job ([`SharedCache`]). A lookup
//! that fails on a miss (`Archived`) costs the same key hashing, 6-25 ms in
//! a turn: it runs in a job too, which keeps its answer, the pipeline or
//! that the archive lacks it, and the call's next run takes the pipeline,
//! or goes on to its next source, with no driver work. The device looks in
//! a creation's every archive in the same turn (`Device::create_from`).
//! The job keeps the pipeline, and the call's next run takes it with no
//! driver work.
//!
//! A creation whose stages' NIR is small ([`IN_TURN_NIR_BYTES`]) is
//! looked up in the turn, in the pipeline store's cache or its archive's:
//! hashing that NIR costs a fraction of a millisecond, under what a job and
//! the calling thread's sleep until the call runs again cost. Found, the
//! call takes the pipeline in its turn. Missed, an archive lookup has its
//! answer, and a creation leaves its job, which compiles into a scratch
//! cache for the store with no second lookup, or through the archive's
//! cache.
//!
//! The job shares the compiler's NIR rather than copying it in the turn
//! (copies took 15-35 ms): it pins each stage ([`pin_nir`]), which the
//! driver only reads, until the call takes the pipeline. So a creation's
//! key, which holds its stages by their NIR handles, names the same shaders
//! for as long as its pipeline is kept: a NIR release elsewhere no longer
//! voids it. A key whose job failed creates in its turn, so no call defers
//! forever. Made pipelines no call takes are destroyed, the oldest once
//! [`KEPT`] wait.
//!
//! Pipeline creation is thread-safe and the caches are internally
//! synchronized (none is created `EXTERNALLY_SYNCHRONIZED`), so a job needs
//! no turn; the queue and command pools stay the turn's. The backend holds
//! the job lock while it destroys the device, so no job runs on a destroyed
//! one, and later jobs see it gone.
use super::{NativeBackend, failure, objects, raw};
use crate::metal::{
    compiler::{DEFERRED, PinnedNir, pin_nir},
    deferred::{
        Done, Job, JobLock, Made, Owner,
        kept::{Kept, Taken},
    },
    error::{Error, Result},
    vulkan::object::{ComputePipelineInfo, GraphicsPipelineInfo, PipelineCache, PipelineSource},
};
use alloc::{boxed::Box, collections::BTreeSet, format, sync::Arc, vec::Vec};
use ash::vk;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use stela_crypto::{Sha256, digest::Digest};

/// Pipeline work in the service's turns since the last [`take_turn_work`],
/// of one kind: how many, and the ticks of all and of the longest.
pub(super) struct Counted {
    count: AtomicU64,
    ticks: AtomicU64,
    longest: AtomicU64,
}

impl Counted {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            longest: AtomicU64::new(0),
        }
    }

    pub(super) fn note(&self, ticks: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.ticks.fetch_add(ticks, Ordering::Relaxed);
        self.longest.fetch_max(ticks, Ordering::Relaxed);
    }

    fn take(&self) -> [u64; 3] {
        [&self.count, &self.ticks, &self.longest].map(|counter| counter.swap(0, Ordering::Relaxed))
    }
}

/// Pipeline jobs a turn left, and the time to set each up (its key was
/// taken before): pinning its stages' NIR, which `copy_us` in the report
/// still names from when the stages were copied.
static LEFT: Counted = Counted::new();
/// Pipelines a turn created itself: none deferred, as a call that may not
/// defer, a full store's lookup or a key whose job failed creates.
static CREATED: Counted = Counted::new();
/// Binary archives a turn looked in for a pipeline, failing on a miss: the
/// creation's next source depends on the answer, so it stays in the turn.
static LOOKED: Counted = Counted::new();
/// Pipelines a turn found in the store's or an archive's cache
/// ([`IN_TURN_NIR_BYTES`]).
static FOUND: Counted = Counted::new();
/// Lookups in the store's or an archive's cache a turn made that missed:
/// an archive lookup's answer, or a creation then left as a job that
/// compiles.
static MISSED: Counted = Counted::new();

/// Counts a creation of `source` a turn made itself, which took `ticks`.
pub(super) fn note_created(source: PipelineSource, ticks: u64) {
    match source {
        PipelineSource::Archived(_) => LOOKED.note(ticks),
        _ => CREATED.note(ticks),
    }
}

/// Since the last call, cleared: the pipeline jobs turns left, the
/// pipelines turns created, the archive lookups turns made, and the store
/// lookups turns made that found the pipeline and that missed, each as
/// count, ticks in all and longest ticks.
pub fn take_turn_work() -> [[u64; 3]; 5] {
    [
        LEFT.take(),
        CREATED.take(),
        LOOKED.take(),
        FOUND.take(),
        MISSED.take(),
    ]
}

/// The most NIR, in bytes as the compiler's cache counts it, a creation's
/// stages hold for its lookup in the store's cache to run in the turn. The
/// driver serializes and hashes each stage's NIR for the lookup: a shader
/// that took 45-55 ms held megabytes, and the compositor's hold tens of
/// kilobytes, which take a few hundred microseconds or less.
pub(super) const IN_TURN_NIR_BYTES: usize = 256 << 10;

/// The most NIR one call's lookups in its turn hash in all: a commit that
/// meets many new pipelines looks up the first few and leaves jobs for the
/// rest.
pub(super) const IN_TURN_CALL_NIR_BYTES: usize = 4 * IN_TURN_NIR_BYTES;

/// The cache a creation of `source`, whose stages hold `nir_bytes`, is
/// looked up in during its turn, after lookups of `looked` bytes in the
/// same call: an archive's own, or for a compiled one the `store`'s; none
/// past [`IN_TURN_NIR_BYTES`] or [`IN_TURN_CALL_NIR_BYTES`].
pub(super) fn lookup_in_turn(
    source: PipelineSource,
    store: Option<PipelineCache>,
    nir_bytes: usize,
    looked: usize,
) -> Option<PipelineCache> {
    if nir_bytes > IN_TURN_NIR_BYTES || looked + nir_bytes > IN_TURN_CALL_NIR_BYTES {
        return None;
    }
    match source {
        PipelineSource::Compiled => store,
        PipelineSource::Cached(cache) | PipelineSource::Archived(cache) => Some(cache),
    }
}

/// Keys a backend remembers as failed; past this it forgets them all, which
/// costs at most one more job each.
const REMEMBERED: usize = 1 << 14;

/// Pipelines made for calls that have not run again; past this the oldest
/// is destroyed, and its call, run again, defers once more.
const KEPT: usize = 32;

/// Jobs one call leaves at most, well under [`KEPT`]: a commit that misses
/// more pipelines runs again for the rest.
const LEFT_PER_CALL: usize = 8;

#[derive(Default)]
pub(super) struct Deferral {
    /// Set while the call served now may defer.
    deferring: Option<JobLock>,
    /// The lock of the jobs this backend made.
    jobs: Option<JobLock>,
    /// The jobs the call served now left, at most [`LEFT_PER_CALL`].
    left: Vec<Job>,
    /// The NIR bytes the call served now looked up in its turn.
    looked: usize,
    /// Set once the device is destroyed.
    gone: Arc<AtomicBool>,
    /// The cache jobs fill while the store's does not grow.
    own: vk::PipelineCache,
    /// Keys whose calls create in their turns.
    failed: BTreeSet<[u8; 32]>,
    /// Pipelines jobs made, with their stages pinned.
    made: Kept<MadePipeline>,
    /// Archives' caches jobs were given, while their archives stay.
    shared: Vec<Arc<SharedCache>>,
}

impl Default for Kept<MadePipeline> {
    fn default() -> Self {
        Self::new(KEPT)
    }
}

/// What a job made for a creation, and the pins that keep its key's NIR
/// handles naming its stages until a call takes it.
struct MadePipeline {
    made: Answer,
    _pins: Vec<PinnedNir>,
}

/// A job's answer to a creation: the pipeline, or, for a lookup that fails
/// on a miss, that the archive lacks it.
#[derive(Clone, Copy)]
enum Answer {
    Pipeline(vk::Pipeline),
    NotArchived,
}

impl MadePipeline {
    /// The pipeline it holds, for its owner to destroy.
    fn pipeline(&self) -> Option<vk::Pipeline> {
        match self.made {
            Answer::Pipeline(pipeline) => Some(pipeline),
            Answer::NotArchived => None,
        }
    }
}

/// Kept pipelines are made under one generation: their pins, not a count
/// of NIR releases, keep their keys true.
const GENERATION: u64 = 0;

/// What a pipeline job made, for the backend whose `gone` it holds: the
/// pipeline, and for one the store's cache lacked, the scratch cache it
/// compiled into with that cache's data and the compile's cost. Dropped
/// unadopted, it destroys both unless the device is gone.
struct Ran {
    pipeline: Option<vk::Pipeline>,
    /// A lookup that failed on a miss: the archive lacks the pipeline.
    not_archived: bool,
    compiled: Option<Compiled>,
    pins: Vec<PinnedNir>,
    /// The archive's cache it created through, kept until it is adopted.
    _cache: Option<Arc<SharedCache>>,
    gone: Arc<AtomicBool>,
    device: ash::Device,
}

/// A binary archive's pipeline cache that jobs create through: destroyed by
/// the last of the archive and those jobs ([`NativeBackend::cache_in_use`]).
pub(super) struct SharedCache {
    cache: vk::PipelineCache,
    /// The archive is gone: the last holder destroys the cache.
    doomed: AtomicBool,
    gone: Arc<AtomicBool>,
    device: ash::Device,
}

impl Drop for SharedCache {
    fn drop(&mut self) {
        if self.doomed.load(Ordering::Acquire) && !self.gone.load(Ordering::Acquire) {
            // SAFETY: its archive and every job that used it are gone.
            unsafe { self.device.destroy_pipeline_cache(self.cache, None) };
        }
    }
}

/// A compile into a scratch cache, for the store to keep
/// ([`super::pipeline_store::Persisted::adopt`]).
struct Compiled {
    scratch: vk::PipelineCache,
    /// The scratch cache's data, read in the job.
    data: Option<Vec<u8>>,
    cost: super::pipeline_store::Cost,
}

impl Drop for Ran {
    fn drop(&mut self) {
        if self.gone.load(Ordering::Acquire) {
            return;
        }
        if let Some(pipeline) = self.pipeline.take() {
            // SAFETY: the job's pipeline, which nothing uses.
            unsafe { self.device.destroy_pipeline(pipeline, None) };
        }
        if let Some(compiled) = self.compiled.take() {
            // SAFETY: the job's scratch cache, which nothing uses.
            unsafe { self.device.destroy_pipeline_cache(compiled.scratch, None) };
        }
    }
}

/// Takes the hold `matches` finds out of `holds`: `Some` while a job holds
/// it too, `None` when nothing else does or there is none. Jobs take holds
/// only in a turn, as this runs, so none appears meanwhile.
fn still_held<T>(holds: &mut Vec<Arc<T>>, matches: impl Fn(&T) -> bool) -> Option<Arc<T>> {
    let at = holds.iter().position(|hold| matches(hold))?;
    let hold = holds.swap_remove(at);
    (Arc::strong_count(&hold) > 1).then_some(hold)
}

/// A pipeline creation a call asks for.
#[derive(Clone, Copy)]
pub(super) enum Wanted<'a> {
    Compute(&'a ComputePipelineInfo),
    Graphics(&'a GraphicsPipelineInfo),
}

impl Wanted<'_> {
    fn source(self) -> PipelineSource {
        match self {
            Self::Compute(info) => info.source,
            Self::Graphics(info) => info.source,
        }
    }

    /// The bytes its stages' NIR holds ([`IN_TURN_NIR_BYTES`]).
    fn nir_bytes(self) -> usize {
        let bytes = |code: crate::metal::vulkan::object::ShaderCode| {
            crate::metal::compiler::nir_bytes(code.0 as usize as *mut core::ffi::c_void)
        };
        match self {
            Self::Compute(info) => bytes(info.code),
            Self::Graphics(info) => info.stages.iter().map(|&(_, code)| bytes(code)).sum(),
        }
    }

    /// The digest of everything the creation reads: its state and its
    /// stages' NIR, by the compiler's handles, which stay while its cache
    /// keeps them.
    fn key(self) -> [u8; 32] {
        let text = match self {
            Self::Compute(info) => format!("compute {info:?}"),
            Self::Graphics(info) => format!("graphics {info:?}"),
        };
        Sha256::digest(text.as_bytes()).into()
    }
}

impl Deferral {
    pub(super) fn set(&mut self, deferring: Option<&JobLock>) {
        if let (Some(lock), None) = (deferring, &self.jobs) {
            self.jobs = Some(lock.clone());
        }
        self.deferring = deferring.cloned();
        self.left.clear();
        self.looked = 0;
    }

    pub(super) fn stop(&mut self) {
        if let Some(lock) = self.deferring.take() {
            lock.lock.set_may_defer(false);
        }
    }

    /// Whether the call served now runs again: it left a job, or a host
    /// call of it was withdrawn while it could
    /// ([`CompileLock::defer_for_wait`](crate::metal::compiler::CompileLock::defer_for_wait)).
    pub(super) fn deferred(&self) -> bool {
        !self.left.is_empty()
            || self
                .deferring
                .as_ref()
                .is_some_and(|lock| lock.lock.waited())
    }

    /// The oldest job the call left.
    pub(super) fn take(&mut self) -> Option<Job> {
        (!self.left.is_empty()).then(|| self.left.remove(0))
    }

    /// Whether the call served now may defer.
    pub(super) fn deferring(&self) -> bool {
        self.deferring.is_some()
    }

    /// Leaves `job` for the call served now, which then fails with
    /// `DEFERRED`; false, leaving nothing, unless the call may defer and has
    /// room for another job.
    pub(super) fn defer(&mut self, job: Job) -> bool {
        let Some(lock) = &self.deferring else {
            return false;
        };
        if self.left.len() >= LEFT_PER_CALL {
            return false;
        }
        lock.lock.set_deferred(true);
        self.left.push(job);
        true
    }

    /// The flag set once the device is destroyed, which a job checks before
    /// it uses the device.
    pub(super) fn gone(&self) -> Arc<AtomicBool> {
        self.gone.clone()
    }

    /// Waits out a running job and keeps later ones from the device, which
    /// is about to be destroyed.
    pub(super) fn retire(&self) {
        let held = self.jobs.as_ref().map(|jobs| jobs.lock.hold(jobs.relax));
        self.gone.store(true, Ordering::Release);
        drop(held);
    }

    /// # Safety
    /// The device is idle and no job runs ([`Self::retire`]).
    pub(super) unsafe fn destroy(&mut self, device: &ash::Device) {
        // Their archives' caches stay their owners'.
        self.shared.clear();
        for pipeline in self.made.drain().filter_map(|made| made.pipeline()) {
            // SAFETY: the caller's; no call took the pipeline.
            unsafe { device.destroy_pipeline(pipeline, None) };
        }
        if self.own != vk::PipelineCache::null() {
            // SAFETY: the caller's.
            unsafe { device.destroy_pipeline_cache(self.own, None) };
            self.own = vk::PipelineCache::null();
        }
    }

    fn remember(set: &mut BTreeSet<[u8; 32]>, key: [u8; 32]) {
        if set.len() >= REMEMBERED {
            set.clear();
        }
        set.insert(key);
    }
}

impl NativeBackend {
    /// A creation: the answer a job made for it (the pipeline, or for a
    /// lookup that fails on a miss, `NotArchived`), else, while the call may
    /// defer, `DEFERRED` with the job that makes it left. `None` creates in
    /// the call as without deferral.
    pub(super) fn deferred_creation(&mut self, wanted: Wanted<'_>) -> Option<Result<vk::Pipeline>> {
        let key = wanted.key();
        // The pins drop here: the key is spent.
        if let Taken::Fresh(made) | Taken::Stale(made) = self.deferral.made.take(&key, GENERATION) {
            return Some(match made.made {
                Answer::Pipeline(pipeline) => Ok(pipeline),
                Answer::NotArchived => Err(Error::NotArchived),
            });
        }
        let lock = self.deferral.deferring.clone()?;
        if self.deferral.failed.contains(&key) {
            return None;
        }
        if self.deferral.left.iter().any(|left| left.key == key) {
            return Some(Err(DEFERRED));
        }
        let source = wanted.source();
        let store = self.store.cache().map(|store| PipelineCache(raw(store)));
        let bytes = wanted.nir_bytes();
        let looked = match lookup_in_turn(source, store, bytes, self.deferral.looked) {
            Some(cache) => {
                self.deferral.looked += bytes;
                let started = (self.ticks)();
                let mut feedback = vk::PipelineCreationFeedback::default();
                let archived = PipelineSource::Archived(cache);
                let created = create(&self.device, self.layout, wanted, archived, &mut feedback);
                let ticks = (self.ticks)().wrapping_sub(started);
                let compiled = matches!(source, PipelineSource::Compiled);
                match created {
                    Ok(pipeline) => {
                        if compiled {
                            super::pipeline_store::note_lookup(true, feedback.duration);
                        }
                        FOUND.note(ticks);
                        return Some(Ok(pipeline));
                    }
                    Err(Error::NotArchived) => {
                        MISSED.note(ticks);
                        // A lookup's miss is its answer.
                        if let PipelineSource::Archived(_) = source {
                            return Some(Err(Error::NotArchived));
                        }
                        // The job compiles a store's miss into a scratch
                        // cache, counted there as the store's miss.
                        compiled
                    }
                    // Another failure is the job's to meet again and
                    // answer.
                    Err(_) => false,
                }
            }
            None => false,
        };
        // A call that goes on past a deferral, as a commit does to find
        // every pipeline it misses, leaves a job for each up to a bound; it
        // runs again, so past the bound nothing it makes would stay.
        if self.deferral.left.len() >= LEFT_PER_CALL {
            return Some(Err(DEFERRED));
        }
        let started = (self.ticks)();
        let job = self.pipeline_job(wanted, key, looked);
        LEFT.note((self.ticks)().wrapping_sub(started));
        self.deferral.left.push(job?);
        lock.lock.set_deferred(true);
        Some(Err(DEFERRED))
    }

    /// Keeps what a pipeline job made for its call's next run.
    pub(super) fn adopt_pipeline(&mut self, done: Done) {
        let Ok(mut ran) = done.made.downcast::<Ran>() else {
            return;
        };
        // A job of a device this backend replaced.
        if !Arc::ptr_eq(&ran.gone, &self.deferral.gone) {
            return;
        }
        let made = match ran.pipeline.take() {
            Some(pipeline) => Answer::Pipeline(pipeline),
            None if ran.not_archived => Answer::NotArchived,
            None => {
                Deferral::remember(&mut self.deferral.failed, done.key);
                return;
            }
        };
        if let Some(compiled) = ran.compiled.take() {
            // SAFETY: the job made the scratch cache on this device and is
            // done with it.
            unsafe {
                self.store
                    .adopt(&self.device, compiled.scratch, compiled.data, compiled.cost)
            };
        }
        let made = MadePipeline {
            made,
            _pins: core::mem::take(&mut ran.pins),
        };
        if let Some(pipeline) = self
            .deferral
            .made
            .keep(done.key, GENERATION, made)
            .and_then(|oldest| oldest.pipeline())
        {
            // SAFETY: the job's pipeline, which no call took.
            unsafe { self.device.destroy_pipeline(pipeline, None) };
        }
    }

    /// The cache jobs fill without a store, made at first use.
    fn own_cache(&mut self) -> Option<vk::PipelineCache> {
        if self.deferral.own == vk::PipelineCache::null() {
            let info = vk::PipelineCacheCreateInfo::default();
            // SAFETY: the create info outlives the call.
            self.deferral.own = unsafe { self.device.create_pipeline_cache(&info, None) }.ok()?;
        }
        Some(self.deferral.own)
    }

    /// Whether `cache`'s destruction waits for the jobs that create through
    /// it, the last of which destroys it; false when none holds it.
    pub(super) fn cache_in_use(&mut self, cache: vk::PipelineCache) -> bool {
        match still_held(&mut self.deferral.shared, |shared| shared.cache == cache) {
            Some(shared) => {
                shared.doomed.store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// A hold of the archive cache `cache` for a job.
    fn share_cache(&mut self, cache: vk::PipelineCache) -> Arc<SharedCache> {
        if let Some(shared) = self
            .deferral
            .shared
            .iter()
            .find(|shared| shared.cache == cache)
        {
            return shared.clone();
        }
        let shared = Arc::new(SharedCache {
            cache,
            doomed: AtomicBool::new(false),
            gone: self.deferral.gone.clone(),
            device: self.device.clone(),
        });
        self.deferral.shared.push(shared.clone());
        shared
    }

    /// A job that creates `wanted` from its stages' NIR, pinned, and keeps
    /// the pipeline, or `None` when no cache can be made. `looked` when the
    /// turn found the store's cache lacks it: the job compiles at once.
    fn pipeline_job(&mut self, wanted: Wanted<'_>, key: [u8; 32], looked: bool) -> Option<Job> {
        // The store's cache is looked in, and what it lacks compiles into a
        // scratch cache the store keeps from; without a store, the backend's
        // own cache takes it. An archive's creation or lookup goes through
        // the archive's cache alone, as in its turn: a lookup's miss is its
        // answer.
        let archive = match wanted.source() {
            PipelineSource::Cached(cache) | PipelineSource::Archived(cache) => {
                Some(self.share_cache(super::handle(cache.0)))
            }
            PipelineSource::Compiled => None,
        };
        let (first, into_store) = match self.store.cache() {
            _ if archive.is_some() => (wanted.source(), false),
            Some(store) => (PipelineSource::Archived(PipelineCache(raw(store))), true),
            None => (
                PipelineSource::Cached(PipelineCache(raw(self.own_cache()?))),
                false,
            ),
        };

        let source = first;
        let (owned, pins) = match wanted {
            Wanted::Compute(info) => (
                Owned::Compute(ComputePipelineInfo { source, ..*info }),
                alloc::vec![pin_nir(info.code)],
            ),
            Wanted::Graphics(info) => (
                Owned::Graphics(Box::new(GraphicsPipelineInfo {
                    source,
                    ..info.clone()
                })),
                info.stages.iter().map(|&(_, code)| pin_nir(code)).collect(),
            ),
        };
        let device = self.device.clone();
        let layout = self.layout;
        let gone = self.deferral.gone.clone();
        let from_archive = archive.is_some();
        Some(Job::new(key, Owner::Backend, move || {
            let wanted = owned.wanted();
            let mut feedback = vk::PipelineCreationFeedback::default();
            let mut not_archived = false;
            let mut compiled = None;
            let pipeline = (!gone.load(Ordering::Acquire))
                .then(|| {
                    let created = if looked && into_store {
                        Err(Error::NotArchived)
                    } else {
                        create(&device, layout, wanted, source, &mut feedback)
                    };
                    let scratch = || {
                        let info = vk::PipelineCacheCreateInfo::default();
                        // SAFETY: the create info outlives the call.
                        unsafe { device.create_pipeline_cache(&info, None) }.ok()
                    };
                    let missed = into_store && matches!(created, Err(Error::NotArchived));
                    let (created, hit) = match missed.then(scratch).flatten() {
                        Some(scratch) => {
                            let into = PipelineSource::Cached(PipelineCache(raw(scratch)));
                            let created = create(&device, layout, wanted, into, &mut feedback);
                            let cost = super::pipeline_store::Cost {
                                ns: feedback.duration,
                                heap: super::pipeline_store::heap_rise(),
                            };
                            // SAFETY: the job's own cache, which no
                            // creation uses now.
                            let data = unsafe { device.get_pipeline_cache_data(scratch) }.ok();
                            compiled = Some(Compiled {
                                scratch,
                                data,
                                cost,
                            });
                            (created, false)
                        }
                        // The store held it, or no scratch cache could be
                        // made: compiled without one, kept nowhere.
                        None if missed => {
                            let compiled = PipelineSource::Compiled;
                            (create(&device, layout, wanted, compiled, &mut feedback), false)
                        }
                        None => {
                            let hit = if into_store {
                                created.is_ok()
                            } else {
                                feedback.flags.contains(
                                    vk::PipelineCreationFeedbackFlags::VALID
                                        | vk::PipelineCreationFeedbackFlags::APPLICATION_PIPELINE_CACHE_HIT,
                                )
                            };
                            (created, hit)
                        }
                    };
                    // The store's lookups, not an archive's, as in a turn.
                    if !from_archive {
                        super::pipeline_store::note_lookup(hit, feedback.duration);
                    }
                    not_archived = from_archive && matches!(created, Err(Error::NotArchived));
                    created.ok()
                })
                .flatten();
            drop(owned);
            Box::new(Ran {
                pipeline,
                not_archived,
                compiled,
                pins,
                _cache: archive,
                gone,
                device,
            }) as Made
        }))
    }
}

/// A job's copy of a creation, naming its stages' pinned NIR.
enum Owned {
    Compute(ComputePipelineInfo),
    Graphics(Box<GraphicsPipelineInfo>),
}

// SAFETY: plain values but the NIR handles, whose shaders the job's pins
// keep alive and which the driver and every holder only read.
unsafe impl Send for Owned {}

impl Owned {
    fn wanted(&self) -> Wanted<'_> {
        match self {
            Self::Compute(info) => Wanted::Compute(info),
            Self::Graphics(info) => Wanted::Graphics(info),
        }
    }
}

/// Creates `wanted` with `source`'s flags through `cache`.
fn create(
    device: &ash::Device,
    layout: vk::PipelineLayout,
    wanted: Wanted<'_>,
    source: PipelineSource,
    feedback: &mut vk::PipelineCreationFeedback,
) -> Result<vk::Pipeline> {
    let cache = match source {
        PipelineSource::Archived(cache) | PipelineSource::Cached(cache) => super::handle(cache.0),
        PipelineSource::Compiled => vk::PipelineCache::null(),
    };
    let mut chained =
        vk::PipelineCreationFeedbackCreateInfo::default().pipeline_creation_feedback(feedback);
    match wanted {
        Wanted::Compute(info) => {
            let info = ComputePipelineInfo { source, ..*info };
            objects::compute_pipeline(&info, layout, |create| {
                let create = (*create).push_next(&mut chained);
                // SAFETY: the create info, its chain and the NIR outlive the
                // call; the cache is internally synchronized.
                unsafe { device.create_compute_pipelines(cache, &[create], None) }
                    .map(|pipelines| pipelines[0])
                    .map_err(|(_, error)| failure(error))
            })
        }
        Wanted::Graphics(info) => {
            let info = GraphicsPipelineInfo {
                source,
                ..info.clone()
            };
            objects::graphics_pipeline(&info, layout, |create| {
                let create = (*create).push_next(&mut chained);
                // SAFETY: as above.
                unsafe { device.create_graphics_pipelines(cache, &[create], None) }
                    .map(|pipelines| pipelines[0])
                    .map_err(|(_, error)| failure(error))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Counted, IN_TURN_CALL_NIR_BYTES, IN_TURN_NIR_BYTES, lookup_in_turn, still_held};
    use crate::metal::vulkan::object::{PipelineCache, PipelineSource};
    use alloc::{sync::Arc, vec};

    /// A creation of NIR up to the bound is looked up in its turn: a
    /// compiled one in the store's cache when there is a store, an
    /// archive's in the archive's own.
    #[test]
    fn a_small_creation_is_looked_up_in_its_turn() {
        let (store, archive) = (PipelineCache(3), PipelineCache(7));
        let small = 40 << 10;
        let large = IN_TURN_NIR_BYTES + 1;
        let compiled = PipelineSource::Compiled;
        assert_eq!(lookup_in_turn(compiled, Some(store), small, 0), Some(store));
        let bound = IN_TURN_NIR_BYTES;
        assert_eq!(lookup_in_turn(compiled, Some(store), bound, 0), Some(store));
        assert_eq!(lookup_in_turn(compiled, Some(store), large, 0), None);
        assert_eq!(lookup_in_turn(compiled, None, small, 0), None);
        for source in [
            PipelineSource::Archived(archive),
            PipelineSource::Cached(archive),
        ] {
            assert_eq!(lookup_in_turn(source, Some(store), small, 0), Some(archive));
            assert_eq!(lookup_in_turn(source, None, small, 0), Some(archive));
            assert_eq!(lookup_in_turn(source, None, large, 0), None);
        }
        // A call's lookups stop at their bound, after which its creations
        // leave jobs.
        let spent = IN_TURN_CALL_NIR_BYTES - small;
        assert_eq!(
            lookup_in_turn(compiled, Some(store), small, spent),
            Some(store)
        );
        assert_eq!(
            lookup_in_turn(compiled, Some(store), small, spent + 1),
            None
        );
    }

    #[test]
    fn counted_work_is_taken_once_with_its_longest() {
        let counted = Counted::new();
        counted.note(30);
        counted.note(70);
        assert_eq!(counted.take(), [2, 100, 70]);
        assert_eq!(counted.take(), [0, 0, 0]);
    }

    #[test]
    fn an_archive_cache_a_job_holds_outlives_its_archive_until_the_job_drops_it() {
        let job = Arc::new(7u32);
        let mut holds = vec![Arc::new(3u32), job.clone()];
        let kept = still_held(&mut holds, |&cache| cache == 7).expect("the job holds it");
        assert_eq!(holds.len(), 1, "the backend's hold is gone");
        drop(job);
        assert_eq!(Arc::strong_count(&kept), 1, "the last holder destroys it");
        // Nothing but the backend holds this one: destroyed at once.
        assert!(still_held(&mut holds, |&cache| cache == 3).is_none());
        assert!(holds.is_empty());
        // One no job was given is destroyed at once too.
        assert!(still_held(&mut holds, |&cache| cache == 9).is_none());
    }
}
