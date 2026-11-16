//! The pipeline store's objects: what a driver's pipeline cache holds, each
//! kept with what compiling it cost, and the layouts they travel in.
//!
//! A driver's cache data (`vkGetPipelineCacheData`, Mesa's
//! `vk_common_GetPipelineCacheData`) is a 32-byte header naming the GPU, an
//! object count and the objects: each a type, a key and its data, with Mesa's
//! blob alignment (a word on four bytes, data on eight from the start). The
//! store keeps the objects one by one, so it can choose which to keep.
//!
//! On the file, objects go in groups ([`group`]): a group is one journal blob
//! that holds whole records, each an object with the compile it came from
//! ([`Unit`]) and that compile's [`Cost`], shared out by bytes. A group is
//! written in one commit beside everything committed, so a cut write leaves
//! the store as it was.
//!
//! The boot hands the device every group's objects as one cache payload
//! ([`seal`]): the driver's layout, which seeds its cache as it is and whose
//! trailing bytes it ignores, then a table naming each object's group, unit
//! and cost, then a trailer.
use alloc::vec::Vec;

/// The driver's cache header: size, version, vendor, device and UUID.
pub const CACHE_HEADER_BYTES: usize = 32;
/// The alignment of an object's data from the start of cache data.
const CACHE_DATA_ALIGN: usize = 8;
/// A group's first bytes.
const GROUP_MAGIC: [u8; 8] = *b"STLGPIP1";
/// A group's head: magic, the cache header, count and a zero word.
const GROUP_HEAD_BYTES: usize = 8 + CACHE_HEADER_BYTES + 8;
/// A group record's head: type, key and data lengths, a zero word, unit,
/// compile time and heap rise.
const RECORD_HEAD_BYTES: usize = 40;
/// The payload trailer's magic, its last bytes.
const TRAILER_MAGIC: [u8; 8] = *b"STLGCOST";
/// The trailer: object count, a zero word, the driver's layout's length,
/// magic.
const TRAILER_BYTES: usize = 24;
/// A table row: group, unit, compile time and heap rise.
const ROW_BYTES: usize = 32;

/// What a compile cost: its time in the driver and how far it raised the
/// heap above where it started.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cost {
    pub ns: u64,
    pub heap: u64,
}

impl Cost {
    /// Compiles past this time or heap rise are heavy: kept first, as one
    /// whose recompile could take the heap's last room.
    const HEAVY_NS: u64 = 1_000_000_000;
    const HEAVY_HEAP: u64 = 128 << 20;

    /// The cost as one number: nanoseconds, a byte of heap rise counting as
    /// one.
    pub fn value(&self) -> u128 {
        u128::from(self.ns) + u128::from(self.heap)
    }

    pub fn heavy(&self) -> bool {
        self.ns >= Self::HEAVY_NS || self.heap >= Self::HEAVY_HEAP
    }

    /// `self` shared out by `part` of `whole` bytes.
    pub fn share(&self, part: usize, whole: usize) -> Self {
        let whole = whole.max(1) as u128;
        let part = part as u128;
        Self {
            ns: (u128::from(self.ns) * part / whole) as u64,
            heap: (u128::from(self.heap) * part / whole) as u64,
        }
    }

    pub fn add(&mut self, other: Self) {
        self.ns = self.ns.saturating_add(other.ns);
        self.heap = self.heap.saturating_add(other.heap);
    }
}

/// The compile an object came from: objects of one unit are kept or dropped
/// together, as a pipeline needs each of them.
pub type Unit = u64;

/// One object of a driver's pipeline cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object<'a> {
    pub kind: u32,
    pub key: &'a [u8],
    pub data: &'a [u8],
}

impl Object<'_> {
    /// Its bytes in a group: what keeping it costs the file and the heap.
    pub fn bytes(&self) -> usize {
        RECORD_HEAD_BYTES + self.key.len() + self.data.len()
    }
}

/// An object with where it is kept and what it cost.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Kept<'a> {
    pub object: Object<'a>,
    pub unit: Unit,
    pub cost: Cost,
}

/// Reads `bytes` from `data` at `*at`, moving past them.
fn take<'a>(data: &'a [u8], at: &mut usize, bytes: usize) -> Option<&'a [u8]> {
    let taken = data.get(*at..at.checked_add(bytes)?)?;
    *at += bytes;
    Some(taken)
}

fn word(data: &[u8], at: &mut usize) -> Option<u32> {
    Some(u32::from_le_bytes(take(data, at, 4)?.try_into().ok()?))
}

fn double(data: &[u8], at: &mut usize) -> Option<u64> {
    Some(u64::from_le_bytes(take(data, at, 8)?.try_into().ok()?))
}

fn align(at: usize, to: usize) -> usize {
    at.div_ceil(to) * to
}

/// The header and objects of driver cache data `data`, in its order;
/// `None` when it is not whole.
pub fn objects(data: &[u8]) -> Option<(&[u8], Vec<Object<'_>>)> {
    let mut at = 0;
    let header = take(data, &mut at, CACHE_HEADER_BYTES)?;
    let count = word(data, &mut at)? as usize;
    let mut objects = Vec::new();
    for _ in 0..count {
        at = align(at, 4);
        let kind = word(data, &mut at)?;
        let key_bytes = word(data, &mut at)? as usize;
        let data_bytes = word(data, &mut at)? as usize;
        let key = take(data, &mut at, key_bytes)?;
        at = align(at, CACHE_DATA_ALIGN);
        let bytes = take(data, &mut at, data_bytes)?;
        objects.push(Object {
            kind,
            key,
            data: bytes,
        });
    }
    Some((header, objects))
}

/// Driver cache data of `header` and `objects`, as the driver lays it out.
pub fn cache_data<'a>(header: &[u8], objects: impl IntoIterator<Item = Object<'a>>) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(header);
    data.extend_from_slice(&[0; 4]);
    let mut count = 0u32;
    for object in objects {
        data.resize(align(data.len(), 4), 0);
        data.extend_from_slice(&object.kind.to_le_bytes());
        data.extend_from_slice(&(object.key.len() as u32).to_le_bytes());
        data.extend_from_slice(&(object.data.len() as u32).to_le_bytes());
        data.extend_from_slice(object.key);
        data.resize(align(data.len(), CACHE_DATA_ALIGN), 0);
        data.extend_from_slice(object.data);
        count += 1;
    }
    data[CACHE_HEADER_BYTES..CACHE_HEADER_BYTES + 4].copy_from_slice(&count.to_le_bytes());
    data
}

/// A group of `kept` objects of cache header `header`: one journal blob.
pub fn group<'a>(header: &[u8], kept: impl IntoIterator<Item = Kept<'a>>) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&GROUP_MAGIC);
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(&[0; 8]);
    let mut count = 0u32;
    for Kept { object, unit, cost } in kept {
        bytes.extend_from_slice(&object.kind.to_le_bytes());
        bytes.extend_from_slice(&(object.key.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(object.data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&unit.to_le_bytes());
        bytes.extend_from_slice(&cost.ns.to_le_bytes());
        bytes.extend_from_slice(&cost.heap.to_le_bytes());
        bytes.extend_from_slice(object.key);
        bytes.extend_from_slice(object.data);
        count += 1;
    }
    bytes[8 + CACHE_HEADER_BYTES..12 + CACHE_HEADER_BYTES].copy_from_slice(&count.to_le_bytes());
    bytes
}

/// The cache header and records of group `bytes`; `None` when it is not a
/// whole group.
pub fn read_group(bytes: &[u8]) -> Option<(&[u8], Vec<Kept<'_>>)> {
    let mut at = 0;
    if take(bytes, &mut at, 8)? != GROUP_MAGIC {
        return None;
    }
    let header = take(bytes, &mut at, CACHE_HEADER_BYTES)?;
    let count = word(bytes, &mut at)? as usize;
    word(bytes, &mut at)?;
    debug_assert_eq!(at, GROUP_HEAD_BYTES);
    let mut kept = Vec::new();
    for _ in 0..count {
        let kind = word(bytes, &mut at)?;
        let key_bytes = word(bytes, &mut at)? as usize;
        let data_bytes = word(bytes, &mut at)? as usize;
        word(bytes, &mut at)?;
        let unit = double(bytes, &mut at)?;
        let ns = double(bytes, &mut at)?;
        let heap = double(bytes, &mut at)?;
        let key = take(bytes, &mut at, key_bytes)?;
        let data = take(bytes, &mut at, data_bytes)?;
        kept.push(Kept {
            object: Object { kind, key, data },
            unit,
            cost: Cost { ns, heap },
        });
    }
    Some((header, kept))
}

/// The bytes a group of objects of `bytes` in all takes.
pub fn group_bytes(bytes: usize) -> usize {
    GROUP_HEAD_BYTES + bytes
}

/// An object of the payload [`seal`] makes, with the file group it is in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placed<'a> {
    pub kept: Kept<'a>,
    /// The offset of its group in the file.
    pub group: u64,
}

/// The cache payload the device seeds from: driver cache data of `header`
/// and `placed`'s objects, then the table of their groups, units and costs
/// and the trailer.
pub fn seal<'a>(header: &[u8], placed: &[Placed<'a>]) -> Vec<u8> {
    let mut payload = cache_data(header, placed.iter().map(|placed| placed.kept.object));
    let data = payload.len() as u64;
    for placed in placed {
        payload.extend_from_slice(&placed.group.to_le_bytes());
        payload.extend_from_slice(&placed.kept.unit.to_le_bytes());
        payload.extend_from_slice(&placed.kept.cost.ns.to_le_bytes());
        payload.extend_from_slice(&placed.kept.cost.heap.to_le_bytes());
    }
    payload.extend_from_slice(&(placed.len() as u32).to_le_bytes());
    payload.extend_from_slice(&[0; 4]);
    payload.extend_from_slice(&data.to_le_bytes());
    payload.extend_from_slice(&TRAILER_MAGIC);
    payload
}

/// The driver cache data of a payload [`seal`] made and its objects, placed;
/// `None` when it is not one.
pub fn open(payload: &[u8]) -> Option<(&[u8], Vec<Placed<'_>>)> {
    let trailer = payload.len().checked_sub(TRAILER_BYTES)?;
    let mut at = trailer;
    let count = word(payload, &mut at)? as usize;
    word(payload, &mut at)?;
    let data_bytes = usize::try_from(double(payload, &mut at)?).ok()?;
    if take(payload, &mut at, 8)? != TRAILER_MAGIC
        || data_bytes.checked_add(count.checked_mul(ROW_BYTES)?)? != trailer
    {
        return None;
    }
    let data = &payload[..data_bytes];
    let (_, objects) = objects(data)?;
    if objects.len() != count {
        return None;
    }
    let mut at = data_bytes;
    let placed = objects
        .into_iter()
        .map(|object| {
            let group = double(payload, &mut at)?;
            let unit = double(payload, &mut at)?;
            let ns = double(payload, &mut at)?;
            let heap = double(payload, &mut at)?;
            Some(Placed {
                kept: Kept {
                    object,
                    unit,
                    cost: Cost { ns, heap },
                },
                group,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some((data, placed))
}

/// A unit for a compile whose first object has key `key`.
pub fn unit_of(key: &[u8]) -> Unit {
    use stela_crypto::{Sha256, digest::Digest};
    let digest = Sha256::digest(key);
    u64::from_le_bytes(digest[..8].try_into().expect("eight bytes"))
}
