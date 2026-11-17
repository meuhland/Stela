use super::super::Strides;
use crate::metal::{
    abi::{
        CpuCacheMode, HazardTrackingMode, ResourceOptions, StorageMode, SwizzleChannels,
        TextureType, TextureUsage,
    },
    command_buffer::blit::Region,
    device::{
        TextureId,
        fixture::{MockBackend, TestDevice, device_on},
    },
    pixel_format::PixelFormat,
    resource::TextureDescriptor,
    vulkan::{ImageAspects, backend::Object, command::Command},
};
use alloc::vec::Vec;

fn texture(
    device: &mut TestDevice,
    storage_mode: StorageMode,
    pixel_format: PixelFormat,
    width: u64,
) -> TextureId {
    let descriptor = TextureDescriptor {
        texture_type: TextureType::D2,
        pixel_format,
        width,
        height: 2,
        depth: 1,
        mipmap_level_count: 1,
        sample_count: 1,
        array_length: 1,
        options: ResourceOptions {
            cpu_cache_mode: CpuCacheMode::DefaultCache,
            storage_mode,
            hazard_tracking_mode: HazardTrackingMode::Default,
        },
        usage: TextureUsage::SHADER_READ,
        swizzle: SwizzleChannels::IDENTITY,
    };
    device.new_texture(&descriptor).unwrap()
}

fn whole(width: u64) -> Region {
    Region {
        slice: 0,
        level: 0,
        origin: [0; 3],
        size: [width, 2, 1],
    }
}

#[test]
fn a_shared_texture_is_read_through_host_memory_a_gpu_copy_filled() {
    let mut device = device_on(MockBackend::new());
    let texture = texture(&mut device, StorageMode::Shared, PixelFormat::Rgba8Unorm, 2);
    let tight = Strides {
        bytes_per_row: 8,
        bytes_per_image: 0,
    };
    let written: Vec<u8> = (1..=16).collect();
    device
        .replace_region(texture, &whole(2), &written, tight)
        .unwrap();
    let allocated = device.memory().allocated;
    // Rows 12 bytes apart: the 4 between them stay the caller's.
    let strided = Strides {
        bytes_per_row: 12,
        ..tight
    };
    let read_back = device
        .begin_read_back(5, texture, &whole(2), strided, 20)
        .unwrap()
        .expect("only host image copies reach the image");
    let image = device.texture_image(texture).unwrap();
    let copied = device
        .backend
        .submissions
        .iter()
        .flatten()
        .flat_map(|segment| &segment.commands)
        .any(|command| matches!(command, Command::CopyImageToBuffer { src, .. } if *src == image));
    assert!(copied, "the GPU copies the image");
    assert!(
        !device.read_back_complete(&read_back).unwrap(),
        "readable only once the copy completed"
    );
    device.backend.finish();
    assert!(device.read_back_complete(&read_back).unwrap());
    let mut read = [0xee; 20];
    read_back
        .write_out::<()>(|offset, run| {
            read[offset as usize..][..run.len()].copy_from_slice(run);
            Ok(())
        })
        .unwrap();
    assert_eq!(read[..8], written[..8]);
    assert_eq!(read[8..12], [0xee; 4]);
    assert_eq!(read[12..], written[8..]);
    device.end_read_back(read_back);
    assert_eq!(device.memory().allocated, allocated, "its memory is freed");
    assert!(
        device
            .backend
            .destroyed
            .iter()
            .any(|object| matches!(object, Object::Buffer(_)))
    );
}

#[test]
fn a_read_back_decodes_what_the_image_stores() {
    let mut device = device_on(MockBackend::new());
    let texture = texture(&mut device, StorageMode::Shared, PixelFormat::Bgr10Xr, 1);
    let tight = Strides {
        bytes_per_row: 4,
        bytes_per_image: 0,
    };
    // Red 1023, green 894, blue 384; then every channel at 0.
    let pack = |red: u32, green: u32, blue: u32| blue | green << 10 | red << 20 | 3 << 30;
    let written: Vec<u8> = [pack(1023, 894, 384), pack(0, 0, 0)]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect();
    device
        .replace_region(texture, &whole(1), &written, tight)
        .unwrap();
    let read_back = device
        .begin_read_back(5, texture, &whole(1), tight, 8)
        .unwrap()
        .unwrap();
    device.backend.finish();
    assert!(device.read_back_complete(&read_back).unwrap());
    let mut runs = Vec::new();
    read_back
        .write_out::<()>(|offset, run| {
            runs.push((offset, run.to_vec()));
            Ok(())
        })
        .unwrap();
    assert_eq!(runs, [(0, written)], "packed rows are one run");
    device.end_read_back(read_back);
}

#[test]
fn a_texture_with_a_cpu_copy_needs_no_read_back() {
    let mut backend = MockBackend::new();
    backend.physical.as_mut().unwrap().features.host_image_copy = false;
    let mut device = device_on(backend);
    let texture = texture(
        &mut device,
        StorageMode::Managed,
        PixelFormat::Rgba8Unorm,
        2,
    );
    let tight = Strides {
        bytes_per_row: 8,
        bytes_per_image: 0,
    };
    let submitted = device.backend.submissions.len();
    assert!(
        device
            .begin_read_back(5, texture, &whole(2), tight, 16)
            .unwrap()
            .is_none()
    );
    assert_eq!(device.backend.submissions.len(), submitted);
}

/// The commands of every submission so far, in order.
fn submitted(device: &TestDevice) -> Vec<Command> {
    device
        .backend
        .submissions
        .iter()
        .flatten()
        .flat_map(|segment| segment.commands.iter().cloned())
        .collect()
}

#[test]
fn a_shared_depth_texture_reads_back_what_the_gpu_wrote_after_the_cpu() {
    let mut device = device_on(MockBackend::new());
    let texture = texture(
        &mut device,
        StorageMode::Shared,
        PixelFormat::Depth32Float,
        2,
    );
    let tight = Strides {
        bytes_per_row: 8,
        bytes_per_image: 0,
    };
    let written: Vec<u8> = [0.25f32, 0.5, 0.75, 1.0]
        .iter()
        .flat_map(|depth| depth.to_le_bytes())
        .collect();
    device
        .replace_region(texture, &whole(2), &written, tight)
        .unwrap();
    // A read outside a read-back would see the CPU's writes alone.
    let mut read = [0; 16];
    assert!(matches!(
        device.get_bytes(texture, &whole(2), &mut read, tight),
        Err(crate::metal::error::Error::Unsupported(_))
    ));
    let image = device.texture_image(texture).unwrap();
    // What a render pass stored into the image since.
    let rendered: Vec<u8> = [0.125f32, 0.5, 0.625, 0.0]
        .iter()
        .flat_map(|depth| depth.to_le_bytes())
        .collect();
    device.backend.image_bytes.insert(image.0, rendered.clone());
    let read_back = device
        .begin_read_back(5, texture, &whole(2), tight, 16)
        .unwrap()
        .expect("a shared depth texture is read by a GPU copy");
    // The CPU's write reaches the image first, then the image is copied
    // out of its depth aspect.
    let commands = submitted(&device);
    let upload = commands.iter().position(|command| {
        matches!(command, Command::CopyBufferToImage { dst, region, .. }
            if *dst == image && region.subresource.aspects == ImageAspects::DEPTH)
    });
    let copy = commands.iter().position(|command| {
        matches!(command, Command::CopyImageToBuffer { src, region, .. }
            if *src == image && region.subresource.aspects == ImageAspects::DEPTH)
    });
    assert!(
        matches!((upload, copy), (Some(upload), Some(copy)) if upload < copy),
        "{commands:?}"
    );
    device.backend.finish();
    assert!(device.read_back_complete(&read_back).unwrap());
    let mut runs = Vec::new();
    read_back
        .write_out::<()>(|offset, run| {
            runs.push((offset, run.to_vec()));
            Ok(())
        })
        .unwrap();
    assert_eq!(runs, [(0, rendered)]);
    device.end_read_back(read_back);
}

#[test]
fn cpu_copies_of_depth_and_stencil_textures_keep_to_one_aspect() {
    let mut device = device_on(MockBackend::new());
    // Depth16Unorm texels two bytes wide: a write starting at an odd texel
    // is not a copy Vulkan takes for depth.
    let narrow = texture(
        &mut device,
        StorageMode::Shared,
        PixelFormat::Depth16Unorm,
        4,
    );
    let one = Region {
        slice: 0,
        level: 0,
        origin: [1, 0, 0],
        size: [1, 1, 1],
    };
    let packed = Strides {
        bytes_per_row: 2,
        bytes_per_image: 0,
    };
    assert!(matches!(
        device.replace_region(narrow, &one, &[1, 2], packed),
        Err(crate::metal::error::Error::Unsupported(_))
    ));
    let even = Region {
        origin: [2, 1, 0],
        ..one
    };
    device
        .replace_region(narrow, &even, &[1, 2], packed)
        .unwrap();
    // Stencil8: one byte a texel, its stencil aspect.
    let stencil = texture(&mut device, StorageMode::Shared, PixelFormat::Stencil8, 4);
    device
        .replace_region(
            stencil,
            &whole(4),
            &[7; 8],
            Strides {
                bytes_per_row: 4,
                bytes_per_image: 0,
            },
        )
        .unwrap();
    let read_back = device
        .begin_read_back(
            5,
            stencil,
            &whole(4),
            Strides {
                bytes_per_row: 4,
                bytes_per_image: 0,
            },
            8,
        )
        .unwrap()
        .unwrap();
    let image = device.texture_image(stencil).unwrap();
    assert!(submitted(&device).iter().any(|command| {
        matches!(command, Command::CopyImageToBuffer { src, region, .. }
            if *src == image && region.subresource.aspects == ImageAspects::STENCIL)
    }));
    device.backend.finish();
    device.end_read_back(read_back);
    // A combined format's CPU texels have no layout: blits copy its aspects.
    let combined = texture(
        &mut device,
        StorageMode::Shared,
        PixelFormat::Depth32FloatStencil8,
        2,
    );
    assert!(matches!(
        device.cpu_bands(
            combined,
            &whole(2),
            Strides {
                bytes_per_row: 16,
                bytes_per_image: 0,
            }
        ),
        Err(crate::metal::error::Error::Unsupported(_))
    ));
}
