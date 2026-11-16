//! Compiled pipelines kept across boots: the pipeline store.
//!
//! The boot reads the store's file before the device opens and hands its
//! bytes here ([`provide`]). The first backend to open claims it and, when
//! the key matches, seeds the pipeline cache its compiled pipelines read
//! from it. A pipeline that cache lacks compiles into an empty cache of its
//! own ([`Persisted::scratch`]), so the objects it made are known, and the
//! store keeps them with what the compile cost: the driver's time and the
//! heap's rise ([`Cost`]). The store's cache then takes them for the rest of
//! the boot.
//!
//! What is kept is a choice by cost ([`Store::kept_units`]): the objects of
//! one compile (a unit) are kept or dropped together, heavy compiles first,
//! then by cost per byte, within a budget of bytes the file and the heap can
//! hold. A unit is never dropped for one that costs less per byte. The boot
//! writes what to keep ([`update`]) while the guest runs and as it stops: a
//! group of objects at a time ([`entries::group`]), and groups that hold
//! nothing kept removed, each update in one commit beside what is committed
//! (`stela_formats::journal::plan_beside`), so a cut write leaves the store
//! as it was.
//!
//! A store is a header, a payload, then the AIR compiler's NIR by the
//! digest of what each compilation read ([`NirEntries`]), which a warm boot
//! deserializes instead of lowering AIR again; the header gives the NIR's
//! length and digest after the payload's, zero for none. The payload is the
//! driver's cache data of every kept object and a table of their groups,
//! units and costs ([`entries::seal`]). The header names a [`Key`]: the build
//! identity of the compiler that turns NIR into machine code, and the GPU's
//! vendor, device, driver and pipeline cache UUID. NVK's pipeline cache UUID
//! is the same for every Stela build (`compat/mesa/build_id.c`), so only the
//! build identity tells builds apart. Inside the cache NVK keys each shader
//! by a digest of its serialized NIR, the pipeline state and the layout, so
//! a function's AIR, constants, formats and linked functions select their
//! own entries. A store of another key, a damaged one or one past the bound
//! is reported once and ignored: its pipelines compile again.
use crate::metal::vulkan::physical::Identity;
use alloc::{sync::Arc, vec::Vec};
use ash::vk;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use stela_crypto::{Sha256, digest::Digest};

pub mod entries;
pub use entries::Cost;
use entries::{Kept, Object, Placed, Unit};

/// A store's first bytes.
pub const MAGIC: [u8; 8] = *b"STLGPIPE";
/// The header layout this build reads and writes.
pub const VERSION: u32 = 1;
/// The header: magic, version, a zero word, the key, the payload's length
/// and its SHA-256, then the NIR's length and its SHA-256, zeros for none.
pub const HEADER_BYTES: usize = 128;
const KEY_AT: usize = 16;
const LENGTH_AT: usize = 48;
const CHECKSUM_AT: usize = 56;
const NIR_LENGTH_AT: usize = 88;
const NIR_CHECKSUM_AT: usize = 96;
/// The alignment of each blob in the boot's file (the journal's `ALIGN`):
/// the cache and the NIR each round up to it there.
const BLOB_ALIGN: u64 = 4096;
/// The most bytes of NIR a store keeps.
pub const MAX_NIR_BYTES: u64 = 96 << 20;
/// The largest store kept, header included, whatever the file allows: the
/// boot holds its bytes and the driver a cache of them for the whole boot.
pub const MAX_BYTES: u64 = 256 << 20;

/// The kinds of the blobs of the boot's journal file.
pub mod kinds {
    /// A driver's cache data whole: the earlier layout, read only.
    pub const CACHE: u32 = 1;
    /// A piece of NIR entries ([`super::NirEntries`]).
    pub const NIR: u32 = 2;
    /// A group of pipeline objects ([`super::entries::group`]).
    pub const GROUP: u32 = 3;
}

/// The store a device reads, for `key`, from the blobs of the boot's file:
/// each its kind, its offset in the file and its bytes. Header, the payload
/// of every group's objects with their groups and costs ([`entries::seal`]),
/// NIR. A cache of the earlier layout counts as a group of objects of
/// unknown cost; an object in two groups is held from the first.
pub fn assemble<'a>(key: &Key, blobs: impl IntoIterator<Item = (u32, u64, &'a [u8])>) -> Vec<u8> {
    let mut header: Option<&[u8]> = None;
    let mut placed: Vec<Placed<'_>> = Vec::new();
    let mut seen = alloc::collections::BTreeSet::new();
    let mut nir = Vec::new();
    for (kind, offset, bytes) in blobs {
        let (found, kept) = match kind {
            kinds::NIR => {
                nir.extend_from_slice(bytes);
                continue;
            }
            kinds::GROUP => match entries::read_group(bytes) {
                Some(group) => group,
                None => continue,
            },
            kinds::CACHE => match entries::objects(bytes) {
                Some((found, objects)) => (
                    found,
                    objects
                        .into_iter()
                        .map(|object| Kept {
                            object,
                            unit: entries::unit_of(object.key),
                            cost: Cost::default(),
                        })
                        .collect(),
                ),
                None => continue,
            },
            _ => continue,
        };
        if *header.get_or_insert(found) != found {
            continue;
        }
        for kept in kept {
            if seen.insert((kept.object.kind, kept.object.key)) {
                placed.push(Placed {
                    kept,
                    group: offset,
                });
            }
        }
    }
    let payload = match header {
        Some(header) => entries::seal(header, &placed),
        None => Vec::new(),
    };
    let mut store = header_with_nir(key, &payload, &nir).to_vec();
    store.extend_from_slice(&payload);
    store.extend_from_slice(&nir);
    store
}

/// What a store's pipelines are valid for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Key(pub [u8; 32]);

impl Key {
    /// The key of pipelines the compiler of build identity `compiler`
    /// builds for `gpu`.
    pub fn new(compiler: &[u8], gpu: &Identity) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(MAGIC);
        hasher.update(VERSION.to_le_bytes());
        hasher.update((compiler.len() as u64).to_le_bytes());
        hasher.update(compiler);
        hasher.update(gpu.vendor_id.to_le_bytes());
        hasher.update(gpu.device_id.to_le_bytes());
        hasher.update(gpu.driver_id.raw().to_le_bytes());
        hasher.update(gpu.driver_version.to_le_bytes());
        hasher.update(gpu.pipeline_cache_uuid);
        Self(hasher.finalize().into())
    }
}

/// Why a store's bytes seed nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// A blank file: nothing was kept yet.
    Blank,
    /// Not a store, or one of another header version.
    Format,
    /// Another compiler build's or GPU's pipelines.
    Stale,
    /// A payload longer than the file or the bound.
    Length,
    /// A payload whose checksum does not match.
    Damaged,
}

impl Rejection {
    fn describe(self) -> &'static str {
        match self {
            Self::Blank => "it is blank",
            Self::Format => "it is not a pipeline store of this version",
            Self::Stale => "it holds another compiler build's or GPU's pipelines",
            Self::Length => "its length is past the file or the bound",
            Self::Damaged => "its checksum does not match",
        }
    }
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The bytes a store whose file begins with `header` takes, header
/// included, for a reader that has only its first [`HEADER_BYTES`].
pub fn stored_bytes(header: &[u8]) -> Result<u64, Rejection> {
    let head = &header[..header.len().min(HEADER_BYTES)];
    if head.iter().all(|&byte| byte == 0) {
        return Err(Rejection::Blank);
    }
    let header = header.get(..HEADER_BYTES).ok_or(Rejection::Format)?;
    if header[..8] != MAGIC || word(header, 8) != VERSION {
        return Err(Rejection::Format);
    }
    let length = u64::from_le_bytes(header[LENGTH_AT..CHECKSUM_AT].try_into().expect("a length"));
    let nir = nir_length(header);
    length
        .checked_add(nir)
        .and_then(|length| length.checked_add(HEADER_BYTES as u64))
        .filter(|&bytes| bytes <= MAX_BYTES)
        .ok_or(Rejection::Length)
}

fn nir_length(header: &[u8]) -> u64 {
    u64::from_le_bytes(
        header[NIR_LENGTH_AT..NIR_CHECKSUM_AT]
            .try_into()
            .expect("a length"),
    )
}

/// The header of a store of `payload` for `key`.
pub fn header(key: &Key, payload: &[u8]) -> [u8; HEADER_BYTES] {
    header_with_nir(key, payload, &[])
}

/// The header of a store of `payload` and then `nir` for `key`.
pub fn header_with_nir(key: &Key, payload: &[u8], nir: &[u8]) -> [u8; HEADER_BYTES] {
    let mut header = [0; HEADER_BYTES];
    header[..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[KEY_AT..LENGTH_AT].copy_from_slice(&key.0);
    header[LENGTH_AT..CHECKSUM_AT].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[CHECKSUM_AT..CHECKSUM_AT + 32].copy_from_slice(&Sha256::digest(payload));
    if !nir.is_empty() {
        header[NIR_LENGTH_AT..NIR_CHECKSUM_AT].copy_from_slice(&(nir.len() as u64).to_le_bytes());
        header[NIR_CHECKSUM_AT..NIR_CHECKSUM_AT + 32].copy_from_slice(&Sha256::digest(nir));
    }
    header
}

/// The NIR of `stored`, whose payload [`payload`] accepted for `key`
/// within `bound`: empty for none, or for NIR that is damaged, which then
/// compiles again.
pub fn nir_section<'a>(stored: &'a [u8], key: &Key, bound: u64) -> &'a [u8] {
    let Ok(payload) = payload(stored, key, bound) else {
        return &[];
    };
    let start = HEADER_BYTES + payload.len();
    let length = nir_length(stored);
    let Some(nir) = usize::try_from(length)
        .ok()
        .and_then(|length| stored.get(start..start.checked_add(length)?))
    else {
        return &[];
    };
    if Sha256::digest(nir)[..] != stored[NIR_CHECKSUM_AT..NIR_CHECKSUM_AT + 32] {
        return &[];
    }
    nir
}

/// A store's NIR: entries of a 32-byte key, a 32-bit little-endian length
/// and that many bytes of serialized NIR.
pub struct NirEntries<'a>(pub &'a [u8]);

impl<'a> Iterator for NirEntries<'a> {
    type Item = ([u8; 32], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let key: [u8; 32] = self.0.get(..32)?.try_into().ok()?;
        let length = u32::from_le_bytes(self.0.get(32..36)?.try_into().ok()?) as usize;
        let blob = self.0.get(36..36usize.checked_add(length)?)?;
        self.0 = &self.0[36 + length..];
        Some((key, blob))
    }
}

/// Appends an entry of `key` and `blob` to a store's NIR.
fn push_entry(nir: &mut Vec<u8>, key: &[u8; 32], blob: &[u8]) {
    nir.extend_from_slice(key);
    nir.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    nir.extend_from_slice(blob);
}

/// The payload of `stored`, a store's file or its first bytes, when it holds
/// `key`'s pipelines within `bound` bytes.
pub fn payload<'a>(stored: &'a [u8], key: &Key, bound: u64) -> Result<&'a [u8], Rejection> {
    let bytes = stored_bytes(stored)?;
    if stored[KEY_AT..LENGTH_AT] != key.0 {
        return Err(Rejection::Stale);
    }
    if bytes > bound.min(MAX_BYTES) || bytes > stored.len() as u64 {
        return Err(Rejection::Length);
    }
    // The cache's bytes, which the NIR follows.
    let cache = bytes - nir_length(stored) - HEADER_BYTES as u64;
    let payload = &stored[HEADER_BYTES..HEADER_BYTES + cache as usize];
    if Sha256::digest(payload)[..] != stored[CHECKSUM_AT..CHECKSUM_AT + 32] {
        return Err(Rejection::Damaged);
    }
    Ok(payload)
}

/// The bytes of objects one update writes as a group, unless a single
/// compile's objects take more. Every group fits beside the kept ones:
/// the file keeps room for two beside the objects' budget and the NIR.
pub const GROUP_BYTES: u64 = 4 << 20;
/// Groups past which an update merges the smallest: each is a blob of the
/// journal, whose commits name a bounded number.
const MAX_GROUPS: usize = 128;

/// The bytes a backend seeds its cache with.
#[derive(Clone, Debug)]
pub enum Seed {
    Empty,
    /// The driver cache data the boot read.
    Stored(&'static [u8]),
    /// Every object the store holds, for a reopened device.
    Kept(Arc<Vec<u8>>),
}

impl Seed {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Stored(bytes) => bytes,
            Self::Kept(bytes) => bytes,
        }
    }
}

/// A backend's hold on the store.
#[derive(Debug)]
pub struct Claim {
    generation: u64,
    pub key: Key,
    pub seed: Seed,
    /// Why the stored bytes seed nothing, unless they are blank.
    pub refused: Option<Rejection>,
}

/// An object's type and key: what the driver finds it by.
type ObjectKey = (u32, Vec<u8>);

/// An object's data: the boot's bytes, or compiled this boot.
#[derive(Debug)]
enum Data {
    Stored(&'static [u8]),
    New(Vec<u8>),
}

impl Data {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Stored(bytes) => bytes,
            Self::New(bytes) => bytes,
        }
    }
}

/// An object the store holds.
#[derive(Debug)]
struct Held {
    data: Data,
    unit: Unit,
    cost: Cost,
    /// The offset of the group on the file that holds it.
    group: Option<u64>,
}

/// A group on the file: its bytes and objects.
#[derive(Debug)]
struct Group {
    bytes: u64,
    objects: Vec<ObjectKey>,
}

/// A unit's rank: heavy first, then by cost per byte, then by unit.
#[derive(Clone, Copy, Debug)]
struct Rank {
    unit: Unit,
    cost: Cost,
    bytes: u64,
}

impl Rank {
    /// Whether `self` comes before `other`: kept first.
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        other
            .cost
            .heavy()
            .cmp(&self.cost.heavy())
            .then_with(|| {
                // Cost per byte, compared without a division.
                let ours = self.cost.value() * u128::from(other.bytes.max(1));
                let theirs = other.cost.value() * u128::from(self.bytes.max(1));
                theirs.cmp(&ours)
            })
            .then(self.unit.cmp(&other.unit))
    }
}

/// The boot's store: what it read, what was compiled since, and what of both
/// the boot wrote back ([`Store::update`]).
#[derive(Debug)]
pub struct Store {
    compiler: Vec<u8>,
    /// The file's data area, which bounds the stored bytes.
    bound: u64,
    /// The most bytes of objects kept: the file holds them, and the driver
    /// a copy in the heap for the whole boot.
    budget: u64,
    stored: &'static [u8],
    owner: Option<(u64, Key)>,
    claims: u64,
    /// Keys of the NIR added this boot that is written.
    written_nir: alloc::collections::BTreeSet<[u8; 32]>,
    /// The key the stored bytes matched at a claim, and their NIR.
    matched: Option<Key>,
    /// The keys of the matched NIR, verified once at the claim, so that
    /// adding NIR does not hash the stored file again.
    stored_keys: alloc::collections::BTreeSet<[u8; 32]>,
    /// NIR compiled this boot, by key, and its bytes.
    nir: alloc::collections::BTreeMap<[u8; 32], Vec<u8>>,
    nir_bytes: u64,
    /// The driver's cache header for the claimed GPU.
    header: Option<Vec<u8>>,
    objects: alloc::collections::BTreeMap<ObjectKey, Held>,
    groups: alloc::collections::BTreeMap<u64, Group>,
    /// New objects since a reopened device's seed was made.
    added: bool,
}

impl Store {
    /// A store of `stored`, its file's bytes (empty for a blank one), for
    /// the compiler of build identity `compiler`, in a file whose data area
    /// is `bound` bytes, keeping at most `budget` bytes of objects.
    pub fn new(stored: &'static [u8], compiler: &[u8], bound: u64, budget: u64) -> Self {
        let bound = bound.min(MAX_BYTES);
        Self {
            compiler: compiler.to_vec(),
            bound,
            budget: budget.min(bound.saturating_sub(2 * GROUP_BYTES) / 2),
            stored,
            owner: None,
            claims: 0,
            written_nir: alloc::collections::BTreeSet::new(),
            matched: None,
            stored_keys: alloc::collections::BTreeSet::new(),
            nir: alloc::collections::BTreeMap::new(),
            nir_bytes: 0,
            header: None,
            objects: alloc::collections::BTreeMap::new(),
            groups: alloc::collections::BTreeMap::new(),
            added: false,
        }
    }

    /// The most bytes of objects kept.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// The stored NIR, for the device whose claim matched the store.
    pub fn stored_nir(&self) -> NirEntries<'static> {
        NirEntries(match self.matched {
            Some(key) => nir_section(self.stored, &key, self.bound),
            None => &[],
        })
    }

    /// Keeps `blob`, NIR of `key` compiled this boot, for the next, while
    /// it fits [`Self::nir_room`].
    pub fn add_nir(&mut self, key: [u8; 32], blob: &[u8]) {
        let bytes = blob.len() as u64 + 36;
        if self.matched.is_none() && self.owner.is_none()
            || self.nir_bytes + bytes > self.nir_room()
            || self.stored_keys.contains(&key)
        {
            return;
        }
        if self.nir.insert(key, blob.to_vec()).is_none() {
            self.nir_bytes += bytes;
        }
    }

    /// The bytes of NIR added this boot that the file can still hold: its
    /// data area less the objects' budget, room for two groups beside them
    /// and the stored NIR, and at most [`MAX_NIR_BYTES`]. NIR past it could
    /// never be written beside the objects, and would only be held in the
    /// heap.
    fn nir_room(&self) -> u64 {
        let held =
            self.budget + 2 * GROUP_BYTES + 2 * BLOB_ALIGN + self.stored_nir().0.len() as u64;
        self.bound.saturating_sub(held).min(MAX_NIR_BYTES)
    }

    /// The serialized NIR of `key` that [`Self::add_nir`] kept this boot.
    pub fn recall_nir(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        self.nir.get(key).cloned()
    }

    /// The store for a backend of `gpu`. A newer backend of the same key
    /// replaces the holder, as a reopened device does; another GPU's while
    /// one holds it, has none.
    pub fn claim(&mut self, gpu: &Identity) -> Option<Claim> {
        let key = Key::new(&self.compiler, gpu);
        if self.owner.is_some_and(|(_, owner)| owner != key) {
            return None;
        }
        self.claims += 1;
        self.owner = Some((self.claims, key));
        let stored = payload(self.stored, &key, self.bound);
        if stored.is_ok() && self.matched != Some(key) {
            self.matched = Some(key);
            self.stored_keys = self.stored_nir().map(|(stored, _)| stored).collect();
            if let Ok(payload) = stored {
                self.hold_stored(payload);
            }
        }
        let (seed, refused) = match stored {
            _ if self.added && self.header.is_some() => {
                (Seed::Kept(Arc::new(self.cache_data())), None)
            }
            Ok(payload) => (
                Seed::Stored(entries::open(payload).map_or(payload, |(data, _)| data)),
                None,
            ),
            Err(Rejection::Blank) => (Seed::Empty, None),
            Err(rejection) => (Seed::Empty, Some(rejection)),
        };
        Some(Claim {
            generation: self.claims,
            key,
            seed,
            refused,
        })
    }

    /// Holds the objects of the stored payload, each in its group; driver
    /// cache data without the store's table holds objects of no group and
    /// no known cost.
    fn hold_stored(&mut self, payload: &'static [u8]) {
        let (header, placed) = match entries::open(payload) {
            Some((data, placed)) => (data.get(..entries::CACHE_HEADER_BYTES), placed),
            None => match entries::objects(payload) {
                Some((header, objects)) => (
                    Some(header),
                    objects
                        .into_iter()
                        .map(|object| Placed {
                            kept: Kept {
                                object,
                                unit: entries::unit_of(object.key),
                                cost: Cost::default(),
                            },
                            group: u64::MAX,
                        })
                        .collect(),
                ),
                None => (None, Vec::new()),
            },
        };
        self.header = header.map(<[u8]>::to_vec);
        for Placed { kept, group } in placed {
            let key = (kept.object.kind, kept.object.key.to_vec());
            if self.objects.contains_key(&key) {
                continue;
            }
            let group = (group != u64::MAX).then_some(group);
            if let Some(offset) = group {
                let entry = self.groups.entry(offset).or_insert(Group {
                    bytes: 0,
                    objects: Vec::new(),
                });
                entry.bytes += kept.object.bytes() as u64;
                entry.objects.push(key.clone());
            }
            self.objects.insert(
                key,
                Held {
                    data: Data::Stored(kept.object.data),
                    unit: kept.unit,
                    cost: kept.cost,
                    group,
                },
            );
        }
    }

    /// Driver cache data of every object held.
    fn cache_data(&self) -> Vec<u8> {
        entries::cache_data(
            self.header.as_deref().unwrap_or(&[]),
            self.objects.iter().map(|((kind, key), held)| Object {
                kind: *kind,
                key,
                data: held.data.bytes(),
            }),
        )
    }

    /// Keeps the objects of `data`, the driver cache data of one compile
    /// that cost `cost`, its cost shared out by their bytes. An object held
    /// already gains its share: more compiles need it.
    pub fn compiled(&mut self, claim: &Claim, data: &[u8], cost: Cost) {
        if !self.holds(claim) {
            return;
        }
        let Some((header, objects)) = entries::objects(data) else {
            return;
        };
        if objects.is_empty() {
            return;
        }
        if self.header.is_none() {
            self.header = Some(header.to_vec());
        }
        let whole: usize = objects.iter().map(Object::bytes).sum();
        let unit = entries::unit_of(objects[0].key);
        for object in objects {
            let share = cost.share(object.bytes(), whole);
            let key = (object.kind, object.key.to_vec());
            match self.objects.get_mut(&key) {
                Some(held) => held.cost.add(share),
                None => {
                    self.objects.insert(
                        key,
                        Held {
                            data: Data::New(object.data.to_vec()),
                            unit,
                            cost: share,
                            group: None,
                        },
                    );
                    self.added = true;
                }
            }
        }
    }

    /// Ends `claim`'s hold, when it still holds the store.
    pub fn release(&mut self, claim: &Claim) {
        if self.holds(claim) {
            self.owner = None;
        }
    }

    fn holds(&self, claim: &Claim) -> bool {
        self.owner
            .is_some_and(|(generation, _)| generation == claim.generation)
    }

    /// Each unit's rank, kept ones first.
    fn ranks(&self) -> Vec<Rank> {
        let mut units: alloc::collections::BTreeMap<Unit, Rank> =
            alloc::collections::BTreeMap::new();
        for ((_, key), held) in &self.objects {
            let rank = units.entry(held.unit).or_insert(Rank {
                unit: held.unit,
                cost: Cost::default(),
                bytes: 0,
            });
            rank.cost.add(held.cost);
            rank.bytes += (40 + key.len() + held.data.bytes().len()) as u64;
        }
        let mut ranks: Vec<Rank> = units.into_values().collect();
        ranks.sort_by(Rank::cmp);
        ranks
    }

    /// The units kept: whole units by rank while they fit the budget; a
    /// unit that does not fit is passed over, never one before it.
    pub fn kept_units(&self) -> alloc::collections::BTreeSet<Unit> {
        let mut used = 0;
        self.ranks()
            .into_iter()
            .filter(|rank| {
                let fits = used + rank.bytes <= self.budget;
                if fits {
                    used += rank.bytes;
                }
                fits
            })
            .map(|rank| rank.unit)
            .collect()
    }

    /// The bytes of `key`'s object in a group.
    fn object_bytes(&self, key: &ObjectKey) -> u64 {
        self.objects.get(key).map_or(0, |held| {
            (40 + key.1.len() + held.data.bytes().len()) as u64
        })
    }

    /// The file's bytes in use, as the store knows them: its groups and the
    /// NIR, each blob rounded to its alignment.
    fn used(&self) -> u64 {
        let aligned = |bytes: u64| bytes.div_ceil(BLOB_ALIGN) * BLOB_ALIGN;
        let groups: u64 = self
            .groups
            .values()
            .map(|group| aligned(entries::group_bytes(group.bytes as usize) as u64))
            .sum();
        let written: u64 = self
            .nir
            .iter()
            .filter(|(key, _)| self.written_nir.contains(*key))
            .map(|(_, blob)| blob.len() as u64 + 36)
            .sum();
        groups + aligned(self.stored_nir().0.len() as u64) + written
    }

    /// What to write next, unless nothing is new, for the key the store is
    /// held for:
    /// - groups that hold no kept object, or that are too large to write
    ///   again and hold one that is not kept, are removed, in an update of
    ///   their removal alone;
    /// - else kept objects on no group, by rank, become a new group of at
    ///   most [`GROUP_BYTES`], with a group that holds objects not kept, or
    ///   the smallest groups when there are many, written into it and
    ///   removed in the same commit;
    /// - when the file lacks room for the new group, the group of the
    ///   lowest rank is removed first, if it ranks below the new one: an
    ///   object is never dropped for one of a lower rank;
    /// - NIR compiled this boot is appended.
    pub fn update(&mut self) -> Option<Update> {
        let key = self.owner.map(|(_, key)| key).or(self.matched)?;
        let ranks = self.ranks();
        let kept = self.kept_units();
        let position: alloc::collections::BTreeMap<Unit, usize> = ranks
            .iter()
            .enumerate()
            .map(|(index, rank)| (rank.unit, index))
            .collect();
        // What is kept nowhere and wanted nowhere is only the driver's now.
        self.objects
            .retain(|_, held| held.group.is_some() || kept.contains(&held.unit));
        let wanted = |store: &Self, object: &ObjectKey| {
            store
                .objects
                .get(object)
                .is_some_and(|held| kept.contains(&held.unit))
        };
        let wanted_bytes = |store: &Self, group: &Group| -> u64 {
            group
                .objects
                .iter()
                .filter(|object| wanted(store, object))
                .map(|object| store.object_bytes(object))
                .sum()
        };
        let dead: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, group)| {
                let bytes = wanted_bytes(self, group);
                bytes == 0 || bytes > GROUP_BYTES && bytes < group.bytes
            })
            .map(|(&offset, _)| offset)
            .collect();
        if !dead.is_empty() {
            return Some(Update::removing(key, dead));
        }
        // Kept objects on no group, by their unit's rank.
        let mut pending: Vec<ObjectKey> = self
            .objects
            .iter()
            .filter(|(_, held)| held.group.is_none() && kept.contains(&held.unit))
            .map(|(object, _)| object.clone())
            .collect();
        pending.sort_by_key(|object| position[&self.objects[object].unit]);
        let mut chosen: Vec<ObjectKey> = Vec::new();
        let mut bytes = 0;
        let mut last_unit = None;
        for object in pending {
            let unit = self.objects[&object].unit;
            let size = self.object_bytes(&object);
            if bytes + size > GROUP_BYTES && last_unit != Some(unit) && !chosen.is_empty() {
                break;
            }
            bytes += size;
            last_unit = Some(unit);
            chosen.push(object);
        }
        // A group partly kept, then the smallest when there are many, is
        // written again into this one. Merging the smallest must join two or
        // more, or the count of groups would not fall.
        let partial: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, group)| wanted_bytes(self, group) < group.bytes)
            .map(|(&offset, _)| offset)
            .collect();
        let mut smallest: Vec<u64> = Vec::new();
        if self.groups.len() > MAX_GROUPS {
            let mut by_size: Vec<(u64, u64)> = self
                .groups
                .iter()
                .filter(|(offset, _)| !partial.contains(offset))
                .map(|(&offset, group)| (group.bytes, offset))
                .collect();
            by_size.sort_unstable();
            smallest = by_size.into_iter().map(|(_, offset)| offset).collect();
        }
        let mut compacted = Vec::new();
        let mut merged = 0;
        for (offset, is_small) in partial
            .iter()
            .map(|&offset| (offset, false))
            .chain(smallest.iter().map(|&offset| (offset, true)))
        {
            let size = wanted_bytes(self, &self.groups[&offset]);
            if bytes + size > GROUP_BYTES {
                if is_small {
                    break;
                }
                continue;
            }
            bytes += size;
            compacted.push((offset, is_small));
            merged += usize::from(is_small);
        }
        if merged == 1 && chosen.is_empty() && compacted.len() == 1 {
            compacted.clear();
            bytes = 0;
        }
        let mut compacted: Vec<u64> = compacted.into_iter().map(|(offset, _)| offset).collect();
        for offset in &compacted {
            chosen.extend(
                self.groups[offset]
                    .objects
                    .iter()
                    .filter(|object| wanted(self, object))
                    .cloned(),
            );
        }
        // Room beside what is committed, short of what the update frees
        // only once it commits.
        let aligned = |bytes: u64| bytes.div_ceil(BLOB_ALIGN) * BLOB_ALIGN;
        let needed = aligned(entries::group_bytes(bytes as usize) as u64);
        if !chosen.is_empty() && self.used() + needed > self.bound {
            let best = chosen
                .iter()
                .map(|object| position[&self.objects[object].unit])
                .min()
                .unwrap_or(usize::MAX);
            let victim = self
                .groups
                .iter()
                .filter(|(offset, _)| !compacted.contains(offset))
                .map(|(&offset, group)| {
                    let top = group
                        .objects
                        .iter()
                        .filter_map(|object| self.objects.get(object))
                        .map(|held| position.get(&held.unit).copied().unwrap_or(usize::MAX))
                        .min()
                        .unwrap_or(usize::MAX);
                    (top, offset)
                })
                .max();
            if let Some((top, offset)) = victim
                && top > best
            {
                return Some(Update::removing(key, alloc::vec![offset]));
            }
            // No room, and nothing ranked lower to make it: NIR alone.
            chosen.clear();
            compacted.clear();
        }
        if self.header.is_none() {
            chosen.clear();
            compacted.clear();
        }
        let mut nir = Vec::new();
        let mut nir_keys = Vec::new();
        for (entry, blob) in &self.nir {
            if !self.written_nir.contains(entry) {
                push_entry(&mut nir, entry, blob);
                nir_keys.push(*entry);
            }
        }
        if chosen.is_empty() && nir.is_empty() {
            return None;
        }
        let group = (!chosen.is_empty()).then(|| {
            let header = self.header.as_deref().unwrap_or(&[]);
            Arc::new(entries::group(
                header,
                chosen.iter().map(|object| {
                    let held = &self.objects[object];
                    Kept {
                        object: Object {
                            kind: object.0,
                            key: &object.1,
                            data: held.data.bytes(),
                        },
                        unit: held.unit,
                        cost: held.cost,
                    }
                }),
            ))
        });
        Some(Update {
            key,
            remove: compacted,
            group,
            group_objects: chosen,
            nir: Arc::new(nir),
            nir_keys,
        })
    }

    /// `update` was written as `written` says: its removed groups are gone,
    /// its group is where it went, and its NIR is not offered again.
    pub fn written(&mut self, update: &Update, written: Written) {
        let removed = if written.removed {
            &update.remove[..]
        } else {
            &[]
        };
        for offset in removed {
            if let Some(group) = self.groups.remove(offset) {
                for object in group.objects {
                    if let Some(held) = self.objects.get_mut(&object)
                        && held.group == Some(*offset)
                    {
                        held.group = None;
                    }
                }
            }
        }
        if let (Some(at), Some(group)) = (written.group, &update.group) {
            for object in &update.group_objects {
                if let Some(held) = self.objects.get_mut(object) {
                    held.group = Some(at);
                }
            }
            self.groups.insert(
                at,
                Group {
                    bytes: group.len().saturating_sub(entries::group_bytes(0)) as u64,
                    objects: update.group_objects.clone(),
                },
            );
        }
        if written.nir {
            self.written_nir.extend(update.nir_keys.iter().copied());
        }
    }

    /// What the store keeps: its units and theirs bytes, the heavy ones
    /// among them, and its groups on the file.
    pub fn summary(&self) -> Summary {
        let kept = self.kept_units();
        let ranks = self.ranks();
        let kept_ranks = ranks.iter().filter(|rank| kept.contains(&rank.unit));
        Summary {
            units: ranks.len(),
            kept: kept.len(),
            kept_bytes: kept_ranks.clone().map(|rank| rank.bytes).sum(),
            heavy: kept_ranks.filter(|rank| rank.cost.heavy()).count(),
            groups: self.groups.len(),
            budget: self.budget,
        }
    }

    /// Every NIR entry the store holds for `key`, stored and added, as one
    /// section: what a store rewritten whole keeps.
    pub fn all_nir(&self, key: &Key) -> Vec<u8> {
        let stored = match self.matched {
            Some(matched) if matched == *key => nir_section(self.stored, key, self.bound),
            _ => &[],
        };
        let mut nir = stored.to_vec();
        for (entry, blob) in &self.nir {
            push_entry(&mut nir, entry, blob);
        }
        nir
    }
}

/// What the boot writes next for the store ([`Store::update`]), in one
/// commit beside what is committed.
#[derive(Debug)]
pub struct Update {
    pub key: Key,
    /// Groups removed, by offset in the file.
    pub remove: Vec<u64>,
    /// A group of objects added ([`entries::group`]), shared with the write
    /// rather than copied.
    pub group: Option<Arc<Vec<u8>>>,
    /// NIR entries ([`NirEntries`]) added to the written ones.
    pub nir: Arc<Vec<u8>>,
    group_objects: Vec<ObjectKey>,
    nir_keys: Vec<[u8; 32]>,
}

/// How an [`Update`] was written: where its group went, whether its
/// removals were made (a dropped blob keeps every committed one), and
/// whether its NIR was written.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Written {
    pub group: Option<u64>,
    pub removed: bool,
    pub nir: bool,
}

/// What a store keeps ([`Store::summary`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    /// Compiles it knows of, and those it keeps.
    pub units: usize,
    pub kept: usize,
    pub kept_bytes: u64,
    /// Kept compiles that are heavy ([`Cost::heavy`]).
    pub heavy: usize,
    /// Groups on the file.
    pub groups: usize,
    pub budget: u64,
}

impl Update {
    fn removing(key: Key, remove: Vec<u64>) -> Self {
        Self {
            key,
            remove,
            group: None,
            nir: Arc::new(Vec::new()),
            group_objects: Vec::new(),
            nir_keys: Vec::new(),
        }
    }
}

/// Pipelines created through the store's cache since the last
/// [`Lookups::take`]: those the cache held and those compiled, with the
/// driver's creation time of each kind and the longest compile.
pub struct Lookups {
    hits: AtomicU64,
    misses: AtomicU64,
    hit_ns: AtomicU64,
    miss_ns: AtomicU64,
    miss_longest_ns: AtomicU64,
}

/// What [`Lookups::take`] answers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LookupCounts {
    pub hits: u64,
    pub misses: u64,
    pub hit_ns: u64,
    pub miss_ns: u64,
    pub miss_longest_ns: u64,
}

impl Lookups {
    pub const fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            hit_ns: AtomicU64::new(0),
            miss_ns: AtomicU64::new(0),
            miss_longest_ns: AtomicU64::new(0),
        }
    }

    /// A creation of `ns` nanoseconds, `hit` when the cache held it.
    pub fn note(&self, hit: bool, ns: u64) {
        if hit {
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.hit_ns.fetch_add(ns, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            self.miss_ns.fetch_add(ns, Ordering::Relaxed);
            self.miss_longest_ns.fetch_max(ns, Ordering::Relaxed);
        }
    }

    /// The counts since the last call, cleared.
    pub fn take(&self) -> LookupCounts {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        LookupCounts {
            hits: take(&self.hits),
            misses: take(&self.misses),
            hit_ns: take(&self.hit_ns),
            miss_ns: take(&self.miss_ns),
            miss_longest_ns: take(&self.miss_longest_ns),
        }
    }
}

impl Default for Lookups {
    fn default() -> Self {
        Self::new()
    }
}

static LOOKUPS: Lookups = Lookups::new();

/// The boot's pipeline creations through the store's cache since the last
/// call: what the platform reports (`GPU STORE`).
pub fn take_lookups() -> LookupCounts {
    LOOKUPS.take()
}

/// Counts a creation through the store's cache.
pub(super) fn note_lookup(hit: bool, ns: u64) {
    LOOKUPS.note(hit, ns);
}

/// The boot's store, behind a lock no interrupt handler takes.
struct Cell {
    held: AtomicBool,
    store: UnsafeCell<Option<Store>>,
}
// SAFETY: `with_store` holds `held` for every access.
unsafe impl Sync for Cell {}
static STORE: Cell = Cell {
    held: AtomicBool::new(false),
    store: UnsafeCell::new(None),
};

/// Runs `f` on the store, under its lock. A deferred job reaches it
/// ([`recall_nir`], from its restore), and `f` allocates: the lock is
/// counted ([`deferred::hold_lock`]), so no job's slice ends while it holds
/// it and leaves the turns and the boot's writer spinning.
///
/// [`deferred::hold_lock`]: crate::metal::deferred::hold_lock
fn with_store<R>(f: impl FnOnce(&mut Option<Store>) -> R) -> R {
    let _held = crate::metal::deferred::hold_lock();
    while STORE
        .held
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        while STORE.held.load(Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }
    // SAFETY: `held` excludes every other access until it is released.
    let result = f(unsafe { &mut *STORE.store.get() });
    STORE.held.store(false, Ordering::Release);
    result
}

/// Give the devices that open later the store the boot read: `stored`, the
/// file's bytes, for the compiler of build identity `compiler`, in a file
/// whose data area is `bound` bytes, keeping at most `budget` bytes of
/// objects. Without it compiled pipelines keep no cache. Answers the
/// objects' budget the file leaves.
pub fn provide(stored: &'static [u8], compiler: &[u8], bound: u64, budget: u64) -> u64 {
    let store = Store::new(stored, compiler, bound, budget);
    let budget = store.budget();
    with_store(|slot| *slot = Some(store));
    budget
}

/// What the boot writes next for its store, while the guest runs and as it
/// stops ([`Store::update`]).
pub fn update() -> Option<Update> {
    with_store(|slot| slot.as_mut()?.update())
}

/// `update` was written as `written` says.
pub fn written(update: &Update, written: Written) {
    with_store(|slot| {
        if let Some(store) = slot {
            store.written(update, written);
        }
    });
}

/// What the boot's store keeps ([`Store::summary`]).
pub fn summary() -> Option<Summary> {
    with_store(|slot| slot.as_ref().map(Store::summary))
}

/// Measures how far the heap rose during the job that runs a compile: the
/// platform's, answering zero outside a measured job.
static HEAP_METER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Sets the meter a compile's heap rise is read from.
pub fn set_heap_meter(meter: fn() -> u64) {
    HEAP_METER.store(meter as usize, Ordering::Release);
}

/// The heap's rise during the running job so far, or zero.
pub(super) fn heap_rise() -> u64 {
    let meter = HEAP_METER.load(Ordering::Acquire);
    if meter == 0 {
        return 0;
    }
    // SAFETY: only `set_heap_meter` stores a nonzero value, a `fn() -> u64`.
    let meter: fn() -> u64 = unsafe { core::mem::transmute(meter) };
    meter()
}

/// Every NIR entry the boot's store holds for `key` ([`Store::all_nir`]).
pub fn all_nir(key: &Key) -> Vec<u8> {
    with_store(|slot| {
        slot.as_ref()
            .map(|store| store.all_nir(key))
            .unwrap_or_default()
    })
}

/// The NIR the store holds for the device that claimed it, by key.
pub fn stored_nir() -> Vec<([u8; 32], &'static [u8])> {
    with_store(|slot| {
        slot.as_ref()
            .map(|store| store.stored_nir().collect())
            .unwrap_or_default()
    })
}

/// Keeps `blob`, NIR of `key` compiled this boot, for the next boot.
pub fn add_nir(key: &[u8; 32], blob: &[u8]) {
    with_store(|slot| {
        if let Some(store) = slot {
            store.add_nir(*key, blob);
        }
    });
}

/// The serialized NIR of `key` compiled this boot, while the store keeps it.
pub fn recall_nir(key: &[u8; 32]) -> Option<Vec<u8>> {
    with_store(|slot| slot.as_ref()?.recall_nir(key))
}

/// The cache a backend's compiled pipelines read and fill, seeded from the
/// store.
pub(super) struct Persisted {
    claim: Option<Claim>,
    cache: vk::PipelineCache,
    log: fn(&str),
}

impl Persisted {
    /// No store: compiled pipelines keep no cache.
    pub(super) fn none(log: fn(&str)) -> Self {
        Self {
            claim: None,
            cache: vk::PipelineCache::null(),
            log,
        }
    }

    /// Claims the boot's store for `gpu` and seeds a cache from it. A seed
    /// the driver refuses leaves the cache empty.
    ///
    /// # Safety
    ///
    /// `device` is the backend's live device, whose physical device `gpu`
    /// describes.
    pub(super) unsafe fn open(device: &ash::Device, gpu: &Identity, log: fn(&str)) -> Self {
        let Some(mut claim) = with_store(|slot| slot.as_mut()?.claim(gpu)) else {
            return Self::none(log);
        };
        if let Some(rejection) = claim.refused {
            log(&alloc::format!(
                "pipeline store ignored: {}; its pipelines compile again",
                rejection.describe()
            ));
        }
        let seed = core::mem::replace(&mut claim.seed, Seed::Empty);
        let create = |data: &[u8]| {
            let info = vk::PipelineCacheCreateInfo::default().initial_data(data);
            // SAFETY: the create info and the data outlive the call; the
            // driver ignores data whose header is not its own.
            unsafe { device.create_pipeline_cache(&info, None) }
        };
        let cache = create(seed.bytes()).or_else(|error| {
            if seed.bytes().is_empty() {
                return Err(error);
            }
            log(&alloc::format!(
                "pipeline store refused by the driver ({error:?}); its pipelines compile again"
            ));
            create(&[])
        });
        match cache {
            Ok(cache) => Self {
                claim: Some(claim),
                cache,
                log,
            },
            Err(error) => {
                log(&alloc::format!(
                    "pipeline store unused: no pipeline cache ({error:?}); pipelines compile \
                     every boot"
                ));
                with_store(|slot| {
                    if let Some(store) = slot {
                        store.release(&claim);
                    }
                });
                Self::none(log)
            }
        }
    }

    /// The cache a compiled pipeline is looked up in, holding every
    /// pipeline the store keeps and every one compiled this boot.
    pub(super) fn cache(&self) -> Option<vk::PipelineCache> {
        self.claim.is_some().then_some(self.cache)
    }

    /// An empty cache a pipeline the store's cache lacks compiles into, so
    /// that what it compiled can be told apart and kept with its cost
    /// ([`Self::adopt`]). `None` without a store or a cache.
    pub(super) fn scratch(&self, device: &ash::Device) -> Option<vk::PipelineCache> {
        self.claim.as_ref()?;
        let info = vk::PipelineCacheCreateInfo::default();
        // SAFETY: the create info outlives the call.
        unsafe { device.create_pipeline_cache(&info, None) }.ok()
    }

    /// Keeps what a compile into `scratch` made, whose driver cache data is
    /// `data` (read now when `None`) and which cost `cost`: the store keeps
    /// its objects by their cost, and the store's cache takes them for the
    /// rest of the boot. Destroys `scratch`.
    ///
    /// # Safety
    ///
    /// `device` is the live device `scratch` was made on by
    /// [`Self::scratch`], and no pipeline creation uses `scratch`.
    pub(super) unsafe fn adopt(
        &mut self,
        device: &ash::Device,
        scratch: vk::PipelineCache,
        data: Option<Vec<u8>>,
        cost: Cost,
    ) {
        // SAFETY: the caller's live, unused cache.
        let data = data.or_else(|| unsafe { device.get_pipeline_cache_data(scratch) }.ok());
        if let (Some(claim), Some(data)) = (&self.claim, &data) {
            with_store(|slot| {
                if let Some(store) = slot {
                    store.compiled(claim, data, cost);
                }
            });
        }
        if self.claim.is_some() {
            // SAFETY: both caches are this device's and alive; the store's
            // cache is merged into only here, on the backend's own turn.
            if let Err(error) = unsafe { device.merge_pipeline_caches(self.cache, &[scratch]) } {
                (self.log)(&alloc::format!(
                    "pipeline store: a compiled pipeline was not merged ({error:?}); it \
                     compiles again when asked again"
                ));
            }
        }
        // SAFETY: the caller's cache, which nothing uses any more.
        unsafe { device.destroy_pipeline_cache(scratch, None) };
    }

    /// Releases the store and destroys the cache.
    ///
    /// # Safety
    ///
    /// `device` is the live device the cache was created on, and no
    /// pipeline creation uses the cache.
    pub(super) unsafe fn destroy(&mut self, device: &ash::Device) {
        if let Some(claim) = self.claim.take() {
            with_store(|slot| {
                if let Some(store) = slot {
                    store.release(&claim);
                }
            });
            // SAFETY: the caller's guarantees.
            unsafe { device.destroy_pipeline_cache(self.cache, None) };
            self.cache = vk::PipelineCache::null();
        }
    }
}

#[cfg(test)]
mod tests;
