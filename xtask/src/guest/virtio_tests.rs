//! The StelaVirtIO kext's ring core (`guest/stela-virtio/kext/stela_vq.h`)
//! in host programs: its own tests, and a loopback against the device's
//! queue (`stela_soc::virtio::queue`), so the kext's rings and the device's
//! agree byte for byte. And the sound driver's sample path
//! (`guest/stela-virtio/kext/stela_pcm.h`): its clip, volume, mute and
//! meters, and the sample buffer's layout as the TX transfers read it.
use super::sdk;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use stela_contracts::dma::{Memory, test_support::Ram};
use stela_soc::virtio::queue::{Buffer, Queue};

/// Builds `guest/stela-virtio/kext/tests/virtqueue.c` with the `clang` on
/// PATH against the generated `stela/virtio.h`.
fn virtqueue(built: &Path) -> PathBuf {
    let clang = crate::tools::which("clang").expect("a clang on PATH for the kext's host tests");
    let include = built.join("include");
    fs::create_dir_all(include.join("stela")).unwrap();
    fs::write(include.join("stela/virtio.h"), sdk::virtio_header()).unwrap();
    let source = crate::tools::root().join("guest/stela-virtio/kext/tests/virtqueue.c");
    let program = built.join("virtqueue");
    crate::tools::exec(
        Command::new(clang)
            .args(["-std=c17", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(&include)
            .arg("-o")
            .arg(&program)
            .arg(source),
    )
    .unwrap();
    program
}

fn run(program: &Path, arguments: &[&std::ffi::OsStr]) -> String {
    let output = Command::new(program).args(arguments).output().unwrap();
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{printed}");
    printed
}

/// Builds `guest/stela-virtio/kext/tests/pcm.c` with the `clang` on PATH.
fn pcm(built: &Path) -> PathBuf {
    let clang = crate::tools::which("clang").expect("a clang on PATH for the kext's host tests");
    let source = crate::tools::root().join("guest/stela-virtio/kext/tests/pcm.c");
    let program = built.join("pcm");
    crate::tools::exec(
        Command::new(clang)
            .args(["-std=c17", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(&program)
            .arg(source)
            .arg("-lm"),
    )
    .unwrap();
    program
}

/// The engine's clip (float to S16, the volume, mute), its meters, and
/// the bytes the device reads for each frame clipped.
#[test]
fn the_kext_sample_path_passes_its_host_tests() {
    let built = tempfile::tempdir().unwrap();
    let program = pcm(built.path());
    assert_eq!(run(&program, &[]), "ok\n");
}

#[test]
fn the_kext_ring_core_passes_its_host_tests() {
    let built = tempfile::tempdir().unwrap();
    let program = virtqueue(built.path());
    assert_eq!(run(&program, &[]), "ok\n");
}

/// The loopback queue of `virtqueue.c`: its three areas, as the kext lays
/// them out and the device is told.
const SIZE: u64 = 8;
const DESC: u64 = 0x8000_0000;
const AVAIL: u64 = 0x8000_1000;
const USED: u64 = 0x8000_2000;
const IMAGE_BYTES: usize = 0x3000;

#[test]
fn the_kext_ring_and_the_device_queue_agree() {
    let built = tempfile::tempdir().unwrap();
    let program = virtqueue(built.path());
    let image = built.path().join("ring");
    assert_eq!(
        run(&program, &["image".as_ref(), image.as_os_str()]),
        "ok\n"
    );
    let bytes = fs::read(&image).unwrap();
    assert_eq!(bytes.len(), IMAGE_BYTES);
    let mut ram = Ram::new(DESC, IMAGE_BYTES);
    assert!(ram.write(DESC, &bytes));

    // The device takes the kext's three chains as the kext made them.
    let mut queue = Queue::split(DESC, AVAIL, USED, SIZE, 256).unwrap();
    queue.check(&mut ram).unwrap();
    assert_eq!(queue.pending(&mut ram).unwrap(), 3);
    let mut chains = Vec::new();
    while let Some(chain) = queue.pop(&mut ram).unwrap() {
        chains.push(chain);
    }
    let buffer = |address, bytes, write| Buffer {
        address,
        bytes,
        write,
    };
    assert_eq!(chains.len(), 3);
    assert_eq!(chains[0].head, 0);
    assert_eq!(
        chains[0].buffers,
        [
            buffer(0x8001_0000, 16, false),
            buffer(0x8001_0100, 64, true)
        ]
    );
    assert_eq!(chains[1].head, 2);
    assert_eq!(chains[1].buffers, [buffer(0x8002_0000, 4096, true)]);
    assert_eq!(chains[2].head, 3);
    assert_eq!(
        chains[2].buffers,
        [
            buffer(0x8003_0000, 8, false),
            buffer(0x8003_0010, 8, false),
            buffer(0x8003_0020, 8, false)
        ]
    );
    // The kext armed its interrupt at the first used entry (used_event 0).
    assert_eq!(queue.used_event(&mut ram).unwrap(), 0);

    // Used out of order: the second, the first, the third; a kick wanted at
    // the next chain; the interrupt the kext asked for.
    for (chain, written) in [(&chains[1], 4096), (&chains[0], 64), (&chains[2], 0)] {
        queue.push_used(&mut ram, chain.head, written).unwrap();
    }
    queue.want_kick(&mut ram).unwrap();
    assert!(queue.publish(&mut ram, true).unwrap());

    // The kext harvests them in that order, with those lengths.
    let mut after = vec![0u8; IMAGE_BYTES];
    assert!(ram.read(DESC, &mut after));
    fs::write(&image, &after).unwrap();
    assert_eq!(
        run(&program, &["harvest".as_ref(), image.as_os_str()]),
        "ok\n"
    );
}
