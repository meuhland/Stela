//! StelaVirtIO, Stela's own virtio kext, which a `--virtio` boot links into
//! the auxiliary collection beside StelaGPU. It is in the image only with
//! the `virtio-kext` feature, as `include_bytes!` needs `xtask guest`'s
//! output; unlike StelaGPU it needs no `nvk`.
use alloc::vec::Vec;
use stela_loader::aux_kc::Kext;

/// `xtask guest`'s objects of the StelaVirtIO kext.
#[cfg(feature = "virtio-kext")]
mod kext {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/guest/StelaVirtIO.kext.rs"
    ));
}

/// Where the installed system would keep the bundle; the kernel loads it
/// from the auxiliary collection, so the path is nominal.
#[cfg(feature = "virtio-kext")]
const BUNDLE_PATH: &str = "/Library/Extensions/StelaVirtIO.kext";

/// Whether this image carries the kext.
pub(super) const EMBEDDED: bool = cfg!(feature = "virtio-kext");

/// The kexts a boot links for its virtio devices: StelaVirtIO when the boot
/// asks for them (`--virtio`) and the image carries it, else none, so a
/// boot without `--virtio` links exactly what it did before.
pub(super) fn kexts(wanted: bool) -> Vec<Kext<'static>> {
    #[cfg(feature = "virtio-kext")]
    if wanted {
        return alloc::vec![Kext {
            info: kext::INFO,
            objects: kext::OBJECTS.to_vec(),
            bundle_path: BUNDLE_PATH.into(),
            trusted_code: kext::TRUSTED_CODE.to_vec(),
        }];
    }
    let _ = wanted;
    Vec::new()
}
