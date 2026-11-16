use super::*;
use crate::metal::nvidia::{CHIPS, fixture};
use alloc::{boxed::Box, vec};

const COMPILER: &[u8] = b"compiler build a";

fn gpu() -> Identity {
    fixture::nvk(&CHIPS[0], 16 << 30).identity
}

/// A store of `payload` for `key`, leaked as the boot's bytes are.
fn stored(key: &Key, payload: &[u8]) -> &'static [u8] {
    let mut bytes = header(key, payload).to_vec();
    bytes.extend_from_slice(payload);
    Box::leak(bytes.into_boxed_slice())
}

#[test]
fn the_key_names_the_compiler_build_and_every_gpu_identity_field() {
    let base = Key::new(COMPILER, &gpu());
    assert_eq!(base, Key::new(COMPILER, &gpu()));
    let changed: [fn(&mut Identity); 5] = [
        |gpu| gpu.vendor_id ^= 1,
        |gpu| gpu.device_id ^= 1,
        |gpu| gpu.driver_version ^= 1,
        |gpu| gpu.driver_id = crate::metal::vulkan::DriverId::MesaRadv,
        |gpu| gpu.pipeline_cache_uuid[15] ^= 1,
    ];
    for change in changed {
        let mut other = gpu();
        change(&mut other);
        assert_ne!(Key::new(COMPILER, &other), base);
    }
    assert_ne!(Key::new(b"compiler build b", &gpu()), base);
    // A length prefix keeps identities from running into the GPU's fields.
    assert_ne!(Key::new(b"", &gpu()), base);
}

#[test]
fn a_sealed_store_opens_for_its_key_alone() {
    let key = Key::new(COMPILER, &gpu());
    let bytes = stored(&key, b"vk pipeline cache data");
    assert_eq!(stored_bytes(&bytes[..HEADER_BYTES]), Ok(bytes.len() as u64));
    assert_eq!(
        payload(bytes, &key, MAX_BYTES),
        Ok(&b"vk pipeline cache data"[..])
    );
    let other = Key::new(b"compiler build b", &gpu());
    assert_eq!(payload(bytes, &other, MAX_BYTES), Err(Rejection::Stale));
}

#[test]
fn blank_foreign_damaged_and_oversized_stores_are_refused() {
    let key = Key::new(COMPILER, &gpu());
    let good = stored(&key, &[7; 300]).to_vec();
    assert_eq!(payload(&[], &key, MAX_BYTES), Err(Rejection::Blank));
    assert_eq!(payload(&[0; 4096], &key, MAX_BYTES), Err(Rejection::Blank));
    assert_eq!(
        payload(&good[..40], &key, MAX_BYTES),
        Err(Rejection::Format)
    );

    let mut magic = good.clone();
    magic[0] ^= 1;
    assert_eq!(payload(&magic, &key, MAX_BYTES), Err(Rejection::Format));
    let mut version = good.clone();
    version[8] = 2;
    assert_eq!(payload(&version, &key, MAX_BYTES), Err(Rejection::Format));

    let mut flipped = good.clone();
    flipped[HEADER_BYTES + 100] ^= 1;
    assert_eq!(payload(&flipped, &key, MAX_BYTES), Err(Rejection::Damaged));
    let mut checksum = good.clone();
    checksum[CHECKSUM_AT] ^= 1;
    assert_eq!(payload(&checksum, &key, MAX_BYTES), Err(Rejection::Damaged));

    // A file cut short, a bound below the store, and a length past any file.
    assert_eq!(
        payload(&good[..good.len() - 1], &key, MAX_BYTES),
        Err(Rejection::Length)
    );
    assert_eq!(
        payload(&good, &key, good.len() as u64 - 1),
        Err(Rejection::Length)
    );
    assert_eq!(
        payload(&good, &key, good.len() as u64).map(<[u8]>::len),
        Ok(300)
    );
    let mut huge = good.clone();
    huge[LENGTH_AT..CHECKSUM_AT].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(stored_bytes(&huge), Err(Rejection::Length));
    huge[LENGTH_AT..CHECKSUM_AT].copy_from_slice(&MAX_BYTES.to_le_bytes());
    assert_eq!(stored_bytes(&huge), Err(Rejection::Length));
}

/// Driver cache data of one compile: `objects` of (kind, key fill, data
/// bytes), under the fixture's header.
fn compile_data(objects: &[(u32, u8, usize)]) -> Vec<u8> {
    let keys: Vec<Vec<u8>> = objects.iter().map(|&(_, key, _)| vec![key; 20]).collect();
    let datas: Vec<Vec<u8>> = objects
        .iter()
        .map(|&(_, key, bytes)| vec![key.wrapping_mul(3); bytes])
        .collect();
    entries::cache_data(
        &HEADER,
        objects
            .iter()
            .enumerate()
            .map(|(index, &(kind, _, _))| Object {
                kind,
                key: &keys[index],
                data: &datas[index],
            }),
    )
}

/// The driver cache header the fixtures use.
const HEADER: [u8; entries::CACHE_HEADER_BYTES] = [0x11; entries::CACHE_HEADER_BYTES];

fn ms(ms: u64) -> Cost {
    Cost {
        ns: ms * 1_000_000,
        heap: 0,
    }
}

#[test]
fn driver_cache_data_reads_back_with_its_alignment() {
    // Keys of odd lengths put each object's data on eight bytes and each
    // word on four, from the start, as the driver lays them out.
    let keys: [&[u8]; 3] = [b"k", b"key-five", b"seven!!"];
    let datas: [&[u8]; 3] = [b"abc", b"", b"0123456789"];
    let data = entries::cache_data(
        &HEADER,
        (0..3).map(|index| Object {
            kind: index as u32,
            key: keys[index],
            data: datas[index],
        }),
    );
    // Header, count; type, key and data lengths, key "k", pad to 56.
    assert_eq!(&data[32..36], &3u32.to_le_bytes());
    assert_eq!(&data[56..59], b"abc");
    let (header, objects) = entries::objects(&data).unwrap();
    assert_eq!(header, &HEADER);
    assert_eq!(objects.len(), 3);
    for (index, object) in objects.iter().enumerate() {
        assert_eq!(
            (object.kind, object.key, object.data),
            (index as u32, keys[index], datas[index])
        );
    }
    // Cut short anywhere, it is not whole.
    for cut in [10, 35, 40, data.len() - 1] {
        assert!(entries::objects(&data[..cut]).is_none());
    }
}

#[test]
fn groups_and_sealed_payloads_keep_each_objects_unit_and_cost() {
    let data = compile_data(&[(1, 1, 100), (2, 2, 7)]);
    let (_, objects) = entries::objects(&data).unwrap();
    let kept: Vec<Kept> = objects
        .iter()
        .enumerate()
        .map(|(index, &object)| Kept {
            object,
            unit: 40 + index as u64,
            cost: Cost {
                ns: 5 + index as u64,
                heap: 9,
            },
        })
        .collect();
    let group = entries::group(&HEADER, kept.iter().copied());
    let (header, read) = entries::read_group(&group).unwrap();
    assert_eq!((header, &read[..]), (&HEADER[..], &kept[..]));
    assert!(entries::read_group(&group[..group.len() - 1]).is_none());

    let placed: Vec<Placed> = kept
        .iter()
        .map(|&kept| Placed {
            kept,
            group: 0x2_0000,
        })
        .collect();
    let payload = entries::seal(&HEADER, &placed);
    let (cache, opened) = entries::open(&payload).unwrap();
    // The driver reads the leading cache data and stops at its count.
    assert_eq!(entries::objects(cache).unwrap().1, objects);
    assert_eq!(entries::objects(&payload).unwrap().1, objects);
    assert_eq!(opened, placed);
    // Plain cache data is not a sealed payload.
    assert!(entries::open(&data).is_none());
}

#[test]
fn units_are_kept_by_cost_per_byte_heavy_first_and_whole() {
    let gpu = gpu();
    // Room for 3000 bytes of objects.
    let mut store = Store::new(&[], COMPILER, 2 * 3000 + 2 * GROUP_BYTES, 3000);
    assert_eq!(store.budget(), 3000);
    let claim = store.claim(&gpu).unwrap();
    // Cheap per byte, dear per byte, and a heavy compile: costly in all
    // but cheap per byte.
    store.compiled(&claim, &compile_data(&[(1, 1, 900)]), ms(10));
    store.compiled(&claim, &compile_data(&[(1, 2, 400), (2, 3, 400)]), ms(80));
    store.compiled(&claim, &compile_data(&[(1, 4, 1500)]), ms(1500));
    let unit = |key: u8| entries::unit_of(&[key; 20]);
    let kept = store.kept_units();
    // The heavy one first, then the dearest per byte; the cheapest no
    // longer fits.
    assert_eq!(kept, [unit(4), unit(2)].into_iter().collect());
    // A yet dearer compile of one byte more than the room left displaces
    // nothing it outranks less than: the cheap one is gone, the others stay.
    store.compiled(&claim, &compile_data(&[(1, 5, 100)]), ms(500));
    let kept = store.kept_units();
    assert!(kept.contains(&unit(5)) && kept.contains(&unit(4)) && kept.contains(&unit(2)));
    assert!(!kept.contains(&unit(1)));
    // A unit's objects go together: both of unit 2 are in one group.
    let update = store.update().unwrap();
    let (_, group) = entries::read_group(update.group.as_ref().unwrap()).unwrap();
    let units: Vec<Unit> = group.iter().map(|kept| kept.unit).collect();
    assert_eq!(units.iter().filter(|&&u| u == unit(2)).count(), 2);
    assert!(!units.contains(&unit(1)));
}

#[test]
fn a_compiles_cost_is_shared_by_bytes_and_an_object_needed_again_gains() {
    let gpu = gpu();
    let mut store = Store::new(&[], COMPILER, MAX_BYTES, 1 << 20);
    let claim = store.claim(&gpu).unwrap();
    store.compiled(
        &claim,
        &compile_data(&[(1, 1, 300 - 60), (2, 2, 100 - 60)]),
        Cost {
            ns: 4000,
            heap: 400,
        },
    );
    let costs =
        |store: &Store| -> Vec<Cost> { store.objects.values().map(|held| held.cost).collect() };
    assert_eq!(
        costs(&store),
        [
            Cost {
                ns: 3000,
                heap: 300
            },
            Cost {
                ns: 1000,
                heap: 100
            }
        ]
    );
    // Another compile that made object 2 again adds to it.
    store.compiled(
        &claim,
        &compile_data(&[(2, 2, 100 - 60)]),
        Cost { ns: 50, heap: 0 },
    );
    assert_eq!(
        costs(&store)[1],
        Cost {
            ns: 1050,
            heap: 100
        }
    );
    // A release ends what the claim may add.
    store.release(&claim);
    store.compiled(&claim, &compile_data(&[(3, 3, 10)]), ms(1));
    assert_eq!(store.objects.len(), 2);
}

/// Writes `update` as the boot would: its group at `at`, its removals and
/// NIR done.
fn write(store: &mut Store, files: &mut Vec<(u32, u64, Vec<u8>)>, update: &Update, at: u64) {
    files.retain(|(_, offset, _)| !update.remove.contains(offset));
    if let Some(group) = &update.group {
        files.push((kinds::GROUP, at, group.to_vec()));
    }
    if !update.nir.is_empty() {
        files.push((kinds::NIR, at + 1, update.nir.to_vec()));
    }
    store.written(
        update,
        Written {
            group: update.group.as_ref().map(|_| at),
            removed: true,
            nir: true,
        },
    );
}

#[test]
fn what_is_kept_is_written_in_groups_read_back_and_dropped_for_dearer_compiles() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    let bound = 2 * 4000 + 2 * GROUP_BYTES + (64 << 10);
    let mut store = Store::new(&[], COMPILER, bound, 4000);
    let claim = store.claim(&gpu).unwrap();
    let mut file = Vec::new();
    store.compiled(&claim, &compile_data(&[(1, 1, 1000), (2, 2, 500)]), ms(30));
    store.compiled(&claim, &compile_data(&[(1, 3, 1000)]), ms(10));
    let first = store.update().unwrap();
    assert!(first.remove.is_empty());
    write(&mut store, &mut file, &first, 0x2_0000);
    assert!(store.update().is_none(), "everything is written");

    // The next boot reads both back, with their units and costs.
    let stored: &'static [u8] = Box::leak(
        assemble(
            &key,
            file.iter()
                .map(|(kind, at, bytes)| (*kind, *at, &bytes[..])),
        )
        .into_boxed_slice(),
    );
    let mut next = Store::new(stored, COMPILER, bound, 4000);
    let claim = next.claim(&gpu).unwrap();
    assert_eq!(
        entries::objects(claim.seed.bytes()).unwrap().1.len(),
        3,
        "the driver is seeded with every object"
    );
    assert_eq!(next.kept_units(), store.kept_units());
    assert!(next.update().is_none(), "nothing new");

    // A dearer compile that leaves room for only one of them: the cheaper
    // is dropped. Its group holds a kept one too, so the kept one is
    // written again into the new group and the old group removed with it,
    // in one update.
    next.compiled(&claim, &compile_data(&[(1, 4, 1600)]), ms(400));
    let unit = |key: u8| entries::unit_of(&[key; 20]);
    assert!(!next.kept_units().contains(&unit(3)));
    let update = next.update().unwrap();
    assert_eq!(update.remove, [0x2_0000]);
    let (_, group) = entries::read_group(update.group.as_ref().unwrap()).unwrap();
    let mut units: Vec<Unit> = group.iter().map(|kept| kept.unit).collect();
    units.dedup();
    assert_eq!(units, [unit(4), unit(1)]);
    // Unit 1's cost stays as it was written, shared by bytes.
    assert_eq!(group[1].cost, ms(30).share(1060, 1060 + 560));
    write(&mut next, &mut file, &update, 0x3_0000);
    assert!(next.update().is_none());
    assert_eq!(file.len(), 1);
}

#[test]
fn a_group_with_nothing_kept_goes_first_in_an_update_of_its_own() {
    let gpu = gpu();
    let bound = 2 * 2000 + 2 * GROUP_BYTES + (64 << 10);
    let mut store = Store::new(&[], COMPILER, bound, 2000);
    let claim = store.claim(&gpu).unwrap();
    let mut file = Vec::new();
    store.compiled(&claim, &compile_data(&[(1, 1, 1800)]), ms(1));
    let update = store.update().unwrap();
    write(&mut store, &mut file, &update, 0x2_0000);
    store.compiled(&claim, &compile_data(&[(1, 2, 1800)]), ms(100));
    // Removal alone: the new group is written in the next update, so no
    // commit is ever without what it keeps.
    let removal = store.update().unwrap();
    assert_eq!(
        (removal.remove.as_slice(), removal.group.is_none()),
        (&[0x2_0000][..], true)
    );
    write(&mut store, &mut file, &removal, 0);
    let added = store.update().unwrap();
    assert!(added.remove.is_empty() && added.group.is_some());
    // A removal that was not made (a dropped blob keeps every committed
    // one) is offered again.
    let mut again = Store::new(&[], COMPILER, bound, 2000);
    let claim = again.claim(&gpu).unwrap();
    again.compiled(&claim, &compile_data(&[(1, 1, 1800)]), ms(1));
    let update = again.update().unwrap();
    again.written(
        &update,
        Written {
            group: Some(0x2_0000),
            removed: true,
            nir: true,
        },
    );
    again.compiled(&claim, &compile_data(&[(1, 2, 1800)]), ms(100));
    let removal = again.update().unwrap();
    again.written(&removal, Written::default());
    assert_eq!(again.update().unwrap().remove, [0x2_0000]);
}

#[test]
fn a_full_file_drops_a_lower_ranked_group_for_a_dearer_one_and_never_the_reverse() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    // Stored NIR fills the file but for one group's room.
    let bound = 2 * GROUP_BYTES + (64 << 10);
    let mut nir = Vec::new();
    push_entry(
        &mut nir,
        &[1; 32],
        &vec![1; bound as usize - (12 << 10) - 36],
    );
    let stored = sealed(&key, &[], &nir);
    for (dearer, removed) in [(50, true), (1, false)] {
        let mut store = Store::new(stored, COMPILER, bound, 1 << 20);
        let claim = store.claim(&gpu).unwrap();
        store.compiled(&claim, &compile_data(&[(1, 1, 5000)]), ms(10));
        let update = store.update().unwrap();
        store.written(
            &update,
            Written {
                group: Some(0x2_0000),
                removed: true,
                nir: true,
            },
        );
        store.compiled(&claim, &compile_data(&[(1, 2, 5000)]), ms(dearer));
        let update = store.update();
        if removed {
            // The dearer group has no room: the cheaper one goes first.
            assert_eq!(update.unwrap().remove, [0x2_0000]);
        } else {
            // A cheaper compile finds no room and removes nothing.
            assert!(update.is_none());
        }
    }
}

#[test]
fn many_small_groups_merge_and_updates_come_to_an_end() {
    let gpu = gpu();
    let mut store = Store::new(&[], COMPILER, MAX_BYTES, 64 << 20);
    let claim = store.claim(&gpu).unwrap();
    let mut file = Vec::new();
    let mut at = 0x2_0000;
    // One small group per update, as compiles come one at a time.
    for key in 0..(MAX_GROUPS + 8) as u8 {
        store.compiled(
            &claim,
            &compile_data(&[(1, key, 100)]),
            ms(1 + u64::from(key)),
        );
        let update = store.update().unwrap();
        // Past the bound, the new group takes the smallest ones in.
        if store.groups.len() > MAX_GROUPS {
            assert!(update.remove.len() >= 2, "{:?}", update.remove);
        }
        write(&mut store, &mut file, &update, at);
        at += 0x1000;
        assert!(store.groups.len() <= MAX_GROUPS + 1);
    }
    // Nothing is left to write, as the boot finds at a stop.
    assert!(store.update().is_none());
    // Every kept object is on the file once.
    assert_eq!(
        store
            .objects
            .values()
            .filter(|held| held.group.is_some())
            .count(),
        MAX_GROUPS + 8
    );
}

#[test]
fn a_cache_of_the_earlier_layout_seeds_and_is_kept_at_no_known_cost() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    let legacy = compile_data(&[(1, 1, 100), (2, 2, 50)]);
    let file = [(kinds::CACHE, 0x2_0000, &legacy[..])];
    let stored: &'static [u8] = Box::leak(assemble(&key, file).into_boxed_slice());
    let mut store = Store::new(stored, COMPILER, MAX_BYTES, 1 << 20);
    let claim = store.claim(&gpu).unwrap();
    assert_eq!(entries::objects(claim.seed.bytes()).unwrap().1.len(), 2);
    assert_eq!(store.groups.len(), 1);
    assert!(
        store
            .objects
            .values()
            .all(|held| held.cost == Cost::default())
    );
    assert!(store.update().is_none(), "kept where it is");
}

#[test]
fn a_claim_seeds_from_a_matching_store_and_reports_any_other() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    let mut store = Store::new(stored(&key, b"cached"), COMPILER, MAX_BYTES, MAX_BYTES);
    let claim = store.claim(&gpu).unwrap();
    assert_eq!(claim.seed.bytes(), b"cached");
    assert_eq!(claim.refused, None);

    let mut blank = Store::new(&[], COMPILER, MAX_BYTES, MAX_BYTES);
    let claim = blank.claim(&gpu).unwrap();
    assert!(claim.seed.bytes().is_empty() && claim.refused.is_none());

    let other = Key::new(b"an older build", &gpu);
    let mut stale = Store::new(stored(&other, b"old"), COMPILER, MAX_BYTES, MAX_BYTES);
    let claim = stale.claim(&gpu).unwrap();
    assert!(claim.seed.bytes().is_empty());
    assert_eq!(claim.refused, Some(Rejection::Stale));

    let mut small = Store::new(stored(&key, &[1; 64]), COMPILER, HEADER_BYTES as u64, 0);
    assert_eq!(small.claim(&gpu).unwrap().refused, Some(Rejection::Length));
}

#[test]
fn the_newest_holder_adds_and_a_reopened_device_is_seeded_with_everything() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    let mut store = Store::new(&[], COMPILER, MAX_BYTES, 1 << 20);
    assert!(store.update().is_none(), "nothing holds it");
    let first = store.claim(&gpu).unwrap();
    assert!(store.update().is_none(), "nothing was added");
    store.compiled(&first, &compile_data(&[(1, 1, 10)]), ms(5));

    // Another GPU has no store while the first holds it.
    let mut other = gpu.clone();
    other.device_id ^= 1;
    assert!(store.claim(&other).is_none());

    // A reopened device of the same GPU takes over, seeded with what the
    // store holds; the first adds nothing more.
    let second = store.claim(&gpu).unwrap();
    assert_eq!(entries::objects(second.seed.bytes()).unwrap().1.len(), 1);
    store.compiled(&first, &compile_data(&[(1, 2, 10)]), ms(5));
    store.release(&first);
    store.compiled(&second, &compile_data(&[(1, 3, 10)]), ms(5));

    let update = store.update().unwrap();
    assert_eq!(update.key, key);
    assert_eq!(
        entries::read_group(update.group.as_ref().unwrap())
            .unwrap()
            .1
            .len(),
        2
    );
    // Offered again until written, and not after.
    assert!(store.update().is_some());
    store.written(
        &update,
        Written {
            group: Some(0x2_0000),
            removed: true,
            nir: true,
        },
    );
    assert!(store.update().is_none());

    // Released, the store serves another GPU.
    let mut store = Store::new(&[], COMPILER, MAX_BYTES, 1 << 20);
    let claim = store.claim(&gpu).unwrap();
    store.release(&claim);
    assert!(store.claim(&other).is_some());
}

/// The layout a boot reads back: header, cache, NIR.
fn sealed(key: &Key, cache: &[u8], nir: &[u8]) -> &'static [u8] {
    let mut bytes = header_with_nir(key, cache, nir).to_vec();
    bytes.extend_from_slice(cache);
    bytes.extend_from_slice(nir);
    Box::leak(bytes.into_boxed_slice())
}

#[test]
fn nir_compiled_this_boot_is_offered_once_and_read_back_after_the_cache() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    let mut store = Store::new(&[], COMPILER, MAX_BYTES, 1 << 20);
    let claim = store.claim(&gpu).unwrap();
    store.add_nir([1; 32], b"first nir");
    store.add_nir([2; 32], b"second");
    store.compiled(&claim, &compile_data(&[(1, 1, 20)]), ms(5));
    let update = store.update().unwrap();
    let entries: Vec<_> = NirEntries(&update.nir).collect();
    assert_eq!(
        entries,
        [([1; 32], &b"first nir"[..]), ([2; 32], &b"second"[..])]
    );
    let mut file = Vec::new();
    write(&mut store, &mut file, &update, 0x2_0000);
    store.add_nir([4; 32], b"later");
    let later = store.update().unwrap();
    assert!(later.group.is_none(), "the objects are written");
    assert_eq!(
        NirEntries(&later.nir)
            .map(|(key, _)| key)
            .collect::<Vec<_>>(),
        [[4; 32]]
    );

    let bytes: &'static [u8] = Box::leak(
        assemble(
            &key,
            file.iter()
                .map(|(kind, at, bytes)| (*kind, *at, &bytes[..])),
        )
        .into_boxed_slice(),
    );
    assert_eq!(stored_bytes(bytes), Ok(bytes.len() as u64));
    assert!(payload(bytes, &key, MAX_BYTES).is_ok());

    // The next boot's device holds it and offers only what is new; the
    // whole NIR is the stored entries and the added ones.
    let mut next = Store::new(bytes, COMPILER, MAX_BYTES, 1 << 20);
    assert!(next.stored_nir().next().is_none(), "no device claimed it");
    next.claim(&gpu).unwrap();
    assert_eq!(next.stored_nir().count(), 2);
    next.add_nir([1; 32], b"first nir");
    next.add_nir([3; 32], b"third");
    let update = next.update().unwrap();
    assert!(update.group.is_none());
    assert_eq!(
        NirEntries(&update.nir)
            .map(|(key, _)| key)
            .collect::<Vec<_>>(),
        [[3; 32]]
    );
    let keys: Vec<_> = NirEntries(&next.all_nir(&key))
        .map(|(key, _)| key)
        .collect();
    assert_eq!(keys, [[1; 32], [2; 32], [3; 32]]);
}

#[test]
fn nir_is_kept_only_while_the_file_can_hold_it_beside_the_objects() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    // A stored entry of 4 bytes; the objects' budget, room for two groups,
    // the alignment and room for one more entry of 64.
    let mut nir = Vec::new();
    push_entry(&mut nir, &[1; 32], &[1; 4]);
    let bytes = sealed(&key, &[], &nir);
    let budget = 3 * BLOB_ALIGN;
    let bound = 2 * budget + 2 * GROUP_BYTES + 4 * BLOB_ALIGN;
    let room = bound - budget - 2 * GROUP_BYTES - 2 * BLOB_ALIGN - nir.len() as u64;
    let filler = room as usize - 36 - (36 + 64);
    let mut store = Store::new(bytes, COMPILER, bound, budget);
    assert_eq!(store.budget(), budget);
    store.claim(&gpu).unwrap();
    store.add_nir([2; 32], &vec![2; filler]);
    store.add_nir([3; 32], &[3; 64]);
    store.add_nir([4; 32], &[4; 1]);
    let update = store.update().unwrap();
    let keys: Vec<_> = NirEntries(&update.nir).map(|(key, _)| key).collect();
    assert_eq!(keys, [[2; 32], [3; 32]]);
    // The budget never takes more than half of what the groups' room leaves.
    let small = Store::new(&[], COMPILER, 2 * GROUP_BYTES + 1000, u64::MAX);
    assert_eq!(small.budget(), 500);
}

#[test]
fn damaged_nir_is_dropped_and_the_cache_kept() {
    let gpu = gpu();
    let key = Key::new(COMPILER, &gpu);
    let mut nir = Vec::new();
    push_entry(&mut nir, &[1; 32], b"nir");
    let mut bytes = header_with_nir(&key, b"cache", &nir).to_vec();
    bytes.extend_from_slice(b"cache");
    bytes.extend_from_slice(&nir);
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    assert_eq!(payload(&bytes, &key, MAX_BYTES), Ok(&b"cache"[..]));
    assert!(nir_section(&bytes, &key, MAX_BYTES).is_empty());
}

#[test]
fn lookups_count_hits_and_compiles_apart_and_clear_when_taken() {
    let lookups = Lookups::new();
    lookups.note(true, 10);
    lookups.note(false, 300);
    lookups.note(false, 1_000);
    lookups.note(true, 20);
    assert_eq!(
        lookups.take(),
        LookupCounts {
            hits: 2,
            misses: 2,
            hit_ns: 30,
            miss_ns: 1_300,
            miss_longest_ns: 1_000,
        }
    );
    assert_eq!(lookups.take(), LookupCounts::default());
}

/// The store's lock, which a job's NIR restore takes ([`recall_nir`]) and
/// whose holder allocates, counts as held for as long as it is: a platform
/// that runs jobs in slices ends none inside it.
#[test]
fn the_store_lock_counts_as_held_while_a_job_could_reach_it() {
    extern crate std;
    use core::cell::Cell;
    std::thread_local! {
        static HELD: Cell<u32> = const { Cell::new(0) };
    }
    fn enter() {
        HELD.with(|held| held.set(held.get() + 1));
    }
    fn leave() {
        HELD.with(|held| held.set(held.get() - 1));
    }
    crate::metal::deferred::set_lock_hooks(enter, leave);
    let inside = with_store(|_| HELD.with(Cell::get));
    assert_eq!(inside, 1);
    assert_eq!(HELD.with(Cell::get), 0);
    // The job's own path through it.
    assert_eq!(recall_nir(&[9; 32]), None);
    assert_eq!(HELD.with(Cell::get), 0);
}
