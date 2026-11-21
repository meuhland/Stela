use super::*;
use crate::metal::{
    abi::{
        CpuCacheMode, HazardTrackingMode, LoadAction, MultisampleDepthResolveFilter,
        MultisampleStencilResolveFilter, ResourceOptions, StorageMode, StoreAction,
        StoreActionOptions, SwizzleChannels, TextureType, VisibilityResultType,
    },
    command_buffer::blit::Region,
    device::{
        BufferId,
        fixture::{TestDevice, device},
        queue::{AttachmentRequest, RenderPassRequest},
        resources::Strides,
    },
    pipeline::MAX_COLOR_ATTACHMENTS,
    pixel_format::PixelFormat,
    resource::TextureDescriptor,
};

fn shared() -> ResourceOptions {
    ResourceOptions {
        cpu_cache_mode: CpuCacheMode::DefaultCache,
        storage_mode: StorageMode::Shared,
        hazard_tracking_mode: HazardTrackingMode::Default,
    }
}

fn descriptor(
    pixel_format: PixelFormat,
    width: u64,
    height: u64,
    usage: TextureUsage,
) -> TextureDescriptor {
    TextureDescriptor {
        texture_type: TextureType::D2,
        pixel_format,
        width,
        height,
        depth: 1,
        mipmap_level_count: 1,
        sample_count: 1,
        array_length: 1,
        options: shared(),
        usage,
        swizzle: SwizzleChannels::IDENTITY,
    }
}

fn pass(texture: TextureId, load_action: LoadAction) -> RenderPassRequest {
    let mut colors = [None; MAX_COLOR_ATTACHMENTS];
    colors[0] = Some(AttachmentRequest {
        texture,
        level: 0,
        slice: 0,
        depth_plane: 0,
        resolve: None,
        load_action,
        store_action: StoreAction::Store,
        store_action_options: StoreActionOptions::EMPTY,
        clear: [0.0; 4],
        surface: None,
    });
    RenderPassRequest {
        color_attachments: colors,
        depth_attachment: None,
        depth_resolve_filter: MultisampleDepthResolveFilter::Sample0,
        stencil_attachment: None,
        stencil_resolve_filter: MultisampleStencilResolveFilter::Sample0,
        visibility_result_buffer: None,
        visibility_result_type: VisibilityResultType::Reset,
        render_target_array_length: 0,
        render_target_width: 0,
        render_target_height: 0,
        default_raster_sample_count: 0,
        tile_memory: Default::default(),
        sample_positions: Vec::new(),
        rasterization_rate_map: false,
        counter_samples: Vec::new(),
    }
}

/// The copies between rows and aligned copies in the last submission's
/// first segment, in order: `true` into the copy, `false` back into the
/// rows, with each region's offset, row length and extent.
fn row_copies(device: &mut TestDevice) -> Vec<(bool, u64, u32, [u32; 3])> {
    device
        .backend()
        .submissions
        .last()
        .unwrap()
        .iter()
        .flat_map(|segment| segment.commands.iter())
        .filter_map(|command| match command {
            Command::CopyBufferToImage { region, .. } => Some((
                true,
                region.buffer_offset,
                region.buffer_row_length,
                region.extent,
            )),
            Command::CopyImageToBuffer { region, .. } => Some((
                false,
                region.buffer_offset,
                region.buffer_row_length,
                region.extent,
            )),
            _ => None,
        })
        .collect()
}

fn commit_empty(device: &mut TestDevice) {
    let commands = device.new_command_buffer();
    device.commit(commands).unwrap();
}

fn commit_pass(device: &mut TestDevice, texture: TextureId, load: LoadAction) {
    let mut commands = device.new_command_buffer();
    device
        .begin_render(&mut commands, &pass(texture, load))
        .unwrap();
    commands.end_encoding(&mut device.resolver()).unwrap();
    device.commit(commands).unwrap();
}

fn buffer(device: &mut TestDevice, length: u64) -> BufferId {
    device.new_buffer(length, shared()).unwrap()
}

/// WindowServer's '&b38' screen surfaces of a 3440 or 1680 wide screen:
/// the alpha plane's row is the width in bytes, which NVIDIA's linear
/// images cannot take. The texture is made, its alpha plane an aligned
/// copy every commit refreshes from the rows and a pass's encode of the
/// planes refreshes again; the color plane's rows fit and stay in place.
#[test]
fn screen_surfaces_of_odd_widths_sample_their_alpha_through_an_aligned_copy() {
    for (width, height, alpha_offset) in
        [(3440u32, 1440u32, 20_076_544u64), (1680, 1050, 7_227_392)]
    {
        let mut device = device();
        let length = alpha_offset + u64::from(width) * u64::from(height);
        let buffer = buffer(&mut device, length);
        let planes = [
            PlaneLayout {
                offset: 0,
                bytes_per_row: u64::from(width) * 4,
            },
            PlaneLayout {
                offset: alpha_offset,
                bytes_per_row: u64::from(width),
            },
        ];
        let surface = descriptor(
            PixelFormat::Rgb10A8_2pXr10,
            width.into(),
            height.into(),
            TextureUsage::SHADER_READ | TextureUsage::RENDER_TARGET,
        );
        let texture = device
            .new_extended_plane_texture(buffer, &surface, planes)
            .unwrap();
        assert!(device.has_aligned_copy(texture));
        let alpha = (true, alpha_offset, width, [width, height, 1]);
        commit_empty(&mut device);
        assert_eq!(row_copies(&mut device), [alpha], "{width}");
        // The encode writes the rows: they are copied in again after it,
        // and never back, as the planes are what the pass writes.
        commit_pass(&mut device, texture, LoadAction::Clear);
        assert_eq!(row_copies(&mut device), [alpha, alpha], "{width}");
        device.destroy_texture(texture).unwrap();
        commit_empty(&mut device);
        assert_eq!(row_copies(&mut device), [], "{width}");
    }
}

/// A buffer texture whose rows the GPU addresses in place is its linear
/// image, with no copies; one of 1004 BGRA8 pixels a row (4016 bytes) is an
/// aligned copy, refreshed by every commit and copied back by those that
/// write it.
#[test]
fn buffer_textures_of_unaligned_rows_copy_back_what_passes_write() {
    let mut device = device();
    let buffer = buffer(&mut device, 1 << 20);
    let usage = TextureUsage::SHADER_READ | TextureUsage::RENDER_TARGET;
    let fits = descriptor(PixelFormat::Bgra8Unorm, 1000, 10, usage);
    let fitting = device
        .new_texture_from_buffer(buffer, &fits, 0, 4096)
        .unwrap();
    assert!(!device.has_aligned_copy(fitting));
    let odd = descriptor(PixelFormat::Bgra8Unorm, 1004, 10, usage);
    let texture = device
        .new_texture_from_buffer(buffer, &odd, 65_536, 4016)
        .unwrap();
    assert!(device.has_aligned_copy(texture));
    let copy = (65_536, 1004, [1004, 10, 1]);
    commit_empty(&mut device);
    assert_eq!(row_copies(&mut device), [(true, copy.0, copy.1, copy.2)]);
    commit_pass(&mut device, texture, LoadAction::Load);
    assert_eq!(
        row_copies(&mut device),
        [
            (true, copy.0, copy.1, copy.2),
            (false, copy.0, copy.1, copy.2)
        ]
    );
    // A pass on the other texture writes only that one.
    commit_pass(&mut device, fitting, LoadAction::Load);
    assert_eq!(row_copies(&mut device), [(true, copy.0, copy.1, copy.2)]);
    // A pixel is a row's unit; an offset of whole pixels need not fit the
    // GPU's 32 bytes either.
    assert!(
        device
            .new_texture_from_buffer(buffer, &odd, 0, 4018)
            .is_err()
    );
    let offset = device
        .new_texture_from_buffer(buffer, &fits, 4100, 4096)
        .unwrap();
    assert!(device.has_aligned_copy(offset));
}

/// The CPU's copies of an aligned copy's texture go to and from its rows,
/// at their own pitch.
#[test]
fn cpu_copies_of_an_aligned_copy_convert_row_by_row_in_the_rows() {
    let mut device = device();
    let buffer = buffer(&mut device, 1 << 16);
    let odd = descriptor(PixelFormat::R8Unorm, 40, 3, TextureUsage::SHADER_READ);
    let texture = device
        .new_texture_from_buffer(buffer, &odd, 16, 48)
        .unwrap();
    assert!(device.has_aligned_copy(texture));
    let region = Region {
        slice: 0,
        level: 0,
        origin: [2, 1, 0],
        size: [3, 2, 1],
    };
    let tight = Strides {
        bytes_per_row: 3,
        bytes_per_image: 0,
    };
    let written = [1, 2, 3, 4, 5, 6];
    device
        .replace_region(texture, &region, &written, tight)
        .unwrap();
    let memory = device.resources.buffers.get(buffer.0).unwrap().memory;
    let rows: Vec<_> = device
        .backend()
        .writes
        .iter()
        .filter(|(written, ..)| *written == memory)
        .map(|(_, offset, bytes)| (*offset, bytes.clone()))
        .collect();
    // Row 1 starts 16 + 48 bytes in, row 2 48 after it; pixel 2 of each.
    assert_eq!(
        rows,
        [(66, alloc::vec![1, 2, 3]), (114, alloc::vec![4, 5, 6])]
    );
    let mut read = [0; 6];
    device
        .get_bytes(texture, &region, &mut read, tight)
        .unwrap();
    assert_eq!(read, written);
}

/// A two-plane surface whose planes' rows do not fit has an aligned copy
/// of each, refreshed by every commit.
#[test]
fn two_plane_surfaces_copy_each_unaligned_plane() {
    let mut device = device();
    let buffer = buffer(&mut device, 1 << 20);
    let surface = descriptor(
        PixelFormat::Rgb8_420_2p,
        1010,
        10,
        TextureUsage::SHADER_READ,
    );
    let texture = device
        .new_two_plane_texture(
            buffer,
            &surface,
            [
                PlaneLayout {
                    offset: 0,
                    bytes_per_row: 1040,
                },
                PlaneLayout {
                    offset: 1 << 16,
                    bytes_per_row: 1010,
                },
            ],
            0,
        )
        .unwrap();
    commit_empty(&mut device);
    assert_eq!(
        row_copies(&mut device),
        [
            (true, 0, 1040, [1010, 10, 1]),
            (true, 1 << 16, 505, [505, 5, 1])
        ]
    );
    device.destroy_texture(texture).unwrap();
}

/// Every connection's command buffer refreshes an aligned copy from its
/// rows, so the rows' buffer outlives the command buffers of other
/// connections that read it, though the buffer is its own connection's:
/// before, connection 1's release of the texture and then the buffer freed
/// the rows while connection 2's command buffer still copied from them.
#[test]
fn rows_another_connections_work_refreshes_from_outlive_it() {
    use crate::metal::{device::pins::Scope, vulkan::backend::Object};
    let mut device = device();
    let made = device.submitted_serial();
    let buffer = buffer(&mut device, 1 << 20);
    let (memory, ..) = device.buffer_memory(buffer).unwrap();
    let usage = TextureUsage::SHADER_READ;
    let odd = descriptor(PixelFormat::Bgra8Unorm, 1004, 10, usage);
    let texture = device
        .new_texture_from_buffer(buffer, &odd, 65_536, 4016)
        .unwrap();
    assert!(device.has_aligned_copy(texture));
    let commands = device.new_command_buffer();
    let other = device.commit_for(2, commands).unwrap();
    assert_eq!(row_copies(&mut device).len(), 1, "it reads the rows");
    let own = Scope::connection(1, made);
    device
        .release_for(own.clone(), |device| device.destroy_texture(texture))
        .unwrap();
    device
        .release_for(own, |device| device.destroy_buffer(buffer))
        .unwrap();
    assert!(
        !device.backend().destroyed.contains(&Object::Memory(memory)),
        "the rows stay while connection 2's command buffer runs"
    );
    device.backend().finish();
    assert_eq!(device.poll(), Ok(other));
    assert!(device.backend().destroyed.contains(&Object::Memory(memory)));
}

/// The planes of a 1920x1080 10-bit 4:2:0 surface, whose rows the GPU
/// could address in place, in a buffer.
const SURFACE_PLANES: [PlaneLayout; 2] = [
    PlaneLayout {
        offset: 0,
        bytes_per_row: 3840,
    },
    PlaneLayout {
        offset: 4 << 20,
        bytes_per_row: 3840,
    },
];

/// Both planes' copies refreshed from their rows.
const BOTH: [(bool, u64, u32, [u32; 3]); 2] = [
    (true, 0, 1920, [1920, 1080, 1]),
    (true, 4 << 20, 960, [960, 540, 1]),
];

/// The surface's ID in these tests.
const SURFACE: u32 = 42;

/// A buffer over a video frame's surface and the sampled texture over it,
/// kept in device memory.
fn video_frame(device: &mut TestDevice) -> (BufferId, TextureId) {
    let buffer = buffer(device, 8 << 20);
    let surface = descriptor(
        PixelFormat::Ycbcr10_420_2p,
        1920,
        1080,
        TextureUsage::SHADER_READ,
    );
    let texture = device
        .new_two_plane_texture(buffer, &surface, SURFACE_PLANES, 0)
        .unwrap();
    assert!(device.has_aligned_copy(texture));
    (buffer, texture)
}

/// Another texture over the same surface's first plane, one passes write.
fn plane_texture(device: &mut TestDevice, buffer: BufferId) -> TextureId {
    device
        .new_texture_from_buffer(
            buffer,
            &descriptor(
                PixelFormat::R16Unorm,
                1920,
                1080,
                TextureUsage::RENDER_TARGET,
            ),
            0,
            3840,
        )
        .unwrap()
}

fn note(texture: Option<TextureId>, seed: u32) -> SurfaceNote {
    SurfaceNote {
        texture,
        surface: SURFACE,
        seed,
    }
}

/// Commits, for connection `key`, a command buffer that uses `notes` and
/// writes `written`, and answers the copies between rows and copies it
/// made.
fn commit_uses(
    device: &mut TestDevice,
    key: u64,
    notes: &[SurfaceNote],
    written: &[TextureId],
) -> Vec<(bool, u64, u32, [u32; 3])> {
    let mut commands = device.new_command_buffer();
    commands.surfaces.extend_from_slice(notes);
    commands.written.extend_from_slice(written);
    device.commit_for(key, commands).unwrap();
    row_copies(device)
}

/// Uses `texture` at seeds 1, 2 and 3: its copy has then seen the seed
/// change twice, and trusts it.
fn trust_seed(device: &mut TestDevice, texture: TextureId) {
    for seed in 1..=3 {
        assert_eq!(
            commit_uses(device, 1, &[note(Some(texture), seed)], &[]),
            BOTH
        );
    }
    assert!(device.surface_watch(texture).unwrap().trusts_seed());
    assert_eq!(commit_uses(device, 1, &[note(Some(texture), 3)], &[]), []);
}

/// A video wallpaper's 10-bit 4:2:0 surface is sampled from device memory:
/// both planes' copies are refreshed only by a command buffer that uses the
/// texture; at each use until the surface's seed changed twice between uses,
/// as a decoder's pool surfaces' seeds do each time it fills them; from then
/// on only when the seed changed, or after a command buffer wrote the surface
/// through another object. Before, it was sampled from guest RAM: 10.6 ms
/// for one 1280x896 quad over a 3840x2160 surface.
#[test]
fn a_large_sampled_surface_is_copied_only_where_used_and_changed() {
    let mut device = device();
    let (buffer, texture) = video_frame(&mut device);
    let commit = |device: &mut TestDevice, notes: &[SurfaceNote], written: &[TextureId]| {
        commit_uses(device, 0, notes, written)
    };
    // Not used: nothing is copied.
    assert_eq!(commit(&mut device, &[], &[]), []);
    // Used at seed 7: copied at every use while the seed has not shown it
    // changes with the surface's writes.
    assert_eq!(commit(&mut device, &[note(Some(texture), 7)], &[]), BOTH);
    assert_eq!(commit(&mut device, &[note(Some(texture), 7)], &[]), BOTH);
    // The decoder filled it: a new seed, once and then again.
    assert_eq!(commit(&mut device, &[note(Some(texture), 8)], &[]), BOTH);
    assert_eq!(commit(&mut device, &[note(Some(texture), 8)], &[]), BOTH);
    assert_eq!(commit(&mut device, &[note(Some(texture), 9)], &[]), BOTH);
    // From then on, not again while the seed holds.
    assert_eq!(commit(&mut device, &[note(Some(texture), 9)], &[]), []);
    assert_eq!(commit(&mut device, &[note(Some(texture), 9)], &[]), []);
    // A view and the texture in one command buffer copy once.
    assert_eq!(
        commit(
            &mut device,
            &[note(Some(texture), 10), note(Some(texture), 10)],
            &[]
        ),
        BOTH
    );
    // Another texture over the same surface written by a command buffer:
    // the next use copies.
    let other = plane_texture(&mut device, buffer);
    assert_eq!(commit(&mut device, &[note(Some(other), 10)], &[other]), []);
    assert_eq!(commit(&mut device, &[note(Some(texture), 10)], &[]), BOTH);
    // Read, not written, it changes nothing.
    assert_eq!(commit(&mut device, &[note(Some(other), 10)], &[]), []);
    assert_eq!(commit(&mut device, &[note(Some(texture), 10)], &[]), []);
    // A buffer over the surface may be written by any shader.
    assert_eq!(commit(&mut device, &[note(None, 10)], &[]), []);
    assert_eq!(commit(&mut device, &[note(Some(texture), 10)], &[]), BOTH);
    // One command buffer that reads it and writes it through another
    // object copies before its work and leaves it stale for the next.
    assert_eq!(
        commit(
            &mut device,
            &[note(Some(texture), 10), note(Some(other), 10)],
            &[other]
        ),
        []
    );
    assert_eq!(commit(&mut device, &[note(Some(texture), 10)], &[]), BOTH);
    device.destroy_texture(other).unwrap();
    device.destroy_texture(texture).unwrap();
}

/// Parsec's capture surfaces: ScreenCaptureKit's service fills them again
/// for every frame without a write lock, so their seed holds while their
/// rows change, and no command buffer reports writing them. Their copies
/// are refreshed at every use, however long the seed holds. With the seed
/// trusted at its first value, VideoToolbox's conversion in parsecd read
/// the first frame forever and the encoder sent 418-byte P-frames.
#[test]
fn a_surface_whose_seed_never_changes_is_copied_at_every_use() {
    let mut device = device();
    let (_, texture) = video_frame(&mut device);
    for _ in 0..16 {
        assert_eq!(
            commit_uses(&mut device, 1, &[note(Some(texture), 5)], &[]),
            BOTH
        );
    }
    assert!(!device.surface_watch(texture).unwrap().trusts_seed());
    device.destroy_texture(texture).unwrap();
}

/// A command buffer of another connection that writes the surface through
/// its own texture leaves connection 1's copy stale, though the seed holds:
/// the device's state is the whole host's, by surface ID. Connection 2's
/// reads change nothing.
#[test]
fn another_connections_write_refreshes_every_connections_copy() {
    let mut device = device();
    let (buffer, texture) = video_frame(&mut device);
    trust_seed(&mut device, texture);
    let theirs = plane_texture(&mut device, buffer);
    assert_eq!(
        commit_uses(&mut device, 2, &[note(Some(theirs), 3)], &[]),
        []
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 3)], &[]),
        []
    );
    assert_eq!(
        commit_uses(&mut device, 2, &[note(Some(theirs), 3)], &[theirs]),
        []
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 3)], &[]),
        BOTH
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 3)], &[]),
        []
    );
    // Through a buffer over the surface, which shaders may write.
    assert_eq!(commit_uses(&mut device, 3, &[note(None, 3)], &[]), []);
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 3)], &[]),
        BOTH
    );
    device.destroy_texture(theirs).unwrap();
    device.destroy_texture(texture).unwrap();
}

/// A pool surface comes back with the same ID and a new frame: the decoder
/// that filled it changed its seed, so its copy is refreshed. A texture made
/// later over a surface of the same ID, in the slot of a destroyed one, knows
/// nothing of the old copy's seeds and copies at its uses.
#[test]
fn a_recycled_surface_is_copied_again() {
    let mut device = device();
    let (buffer, texture) = video_frame(&mut device);
    trust_seed(&mut device, texture);
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 4)], &[]),
        BOTH
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 4)], &[]),
        []
    );
    device.destroy_texture(texture).unwrap();
    let surface = descriptor(
        PixelFormat::Ycbcr10_420_2p,
        1920,
        1080,
        TextureUsage::SHADER_READ,
    );
    let again = device
        .new_two_plane_texture(buffer, &surface, SURFACE_PLANES, 0)
        .unwrap();
    assert_eq!(device.surface_watch(again), Some(Watch::default()));
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(again), 4)], &[]),
        BOTH
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(again), 4)], &[]),
        BOTH
    );
    device.destroy_texture(again).unwrap();
}

/// The CPU writes the surface. With a write lock its seed changes, and the
/// next use copies. Through `replaceRegion:` on another texture the guest
/// reported over the surface, the host's CPU copy changes no seed: it
/// leaves the surface's copies stale itself.
#[test]
fn cpu_writes_refresh_the_copies() {
    let mut device = device();
    let (buffer, texture) = video_frame(&mut device);
    trust_seed(&mut device, texture);
    // A write lock's unlock.
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 4)], &[]),
        BOTH
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 4)], &[]),
        []
    );
    // The host's CPU copy into another connection's texture over it.
    let theirs = plane_texture(&mut device, buffer);
    assert_eq!(
        commit_uses(&mut device, 2, &[note(Some(theirs), 4)], &[]),
        []
    );
    let region = Region {
        slice: 0,
        level: 0,
        origin: [0, 0, 0],
        size: [2, 1, 1],
    };
    let tight = Strides {
        bytes_per_row: 4,
        bytes_per_image: 0,
    };
    device
        .replace_region(theirs, &region, &[1, 2, 3, 4], tight)
        .unwrap();
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 4)], &[]),
        BOTH
    );
    assert_eq!(
        commit_uses(&mut device, 1, &[note(Some(texture), 4)], &[]),
        []
    );
    // Once destroyed, the texture's surface is forgotten.
    device.destroy_texture(theirs).unwrap();
    assert!(!device.resources.surface_textures.contains_key(&theirs.0));
    device.destroy_texture(texture).unwrap();
    assert!(device.resources.surface_textures.is_empty());
}

/// A commit refused after it planned its copies (its mappings fail, or its
/// submission) copies nothing: the copies' watches are left as they were,
/// and the next use copies again. Before, the refused use recorded the seed
/// as copied, and later uses at that seed read whatever the copy held.
#[test]
fn a_refused_commit_leaves_the_copies_to_copy_again() {
    let mut device = device();
    let (_, texture) = video_frame(&mut device);
    trust_seed(&mut device, texture);
    let before = device.surface_watch(texture);
    let notes = [note(Some(texture), 4)];
    let refreshes = |lead: &[Command]| {
        lead.iter()
            .filter(|command| matches!(command, Command::CopyBufferToImage { .. }))
            .count()
    };
    let (lead, _, _refused) = device.aligned_copy_commands(&[], &notes, &[]);
    assert_eq!(refreshes(&lead), 2);
    assert_eq!(device.surface_watch(texture), before);
    let (lead, _, uses) = device.aligned_copy_commands(&[], &notes, &[]);
    assert_eq!(refreshes(&lead), 2);
    device.apply_surface_uses(uses);
    let (lead, _, _) = device.aligned_copy_commands(&[], &notes, &[]);
    assert_eq!(refreshes(&lead), 0);
    device.destroy_texture(texture).unwrap();
}

/// A copy's watch: what each use copies, and when the seed is trusted.
#[test]
fn a_watch_trusts_a_seed_that_changed_twice() {
    let watch = Watch::default();
    // The first use copies, and so does each use at an unproven seed.
    let (copies, watch) = watch.used(SURFACE, 1);
    assert!(copies);
    let (copies, watch) = watch.used(SURFACE, 1);
    assert!(copies);
    let (copies, watch) = watch.used(SURFACE, 2);
    assert!(copies && !watch.trusts_seed());
    let (copies, watch) = watch.used(SURFACE, 2);
    assert!(copies);
    let (copies, watch) = watch.used(SURFACE, 3);
    assert!(copies && watch.trusts_seed());
    // Trusted: only a new seed or a write copies.
    let (copies, mut watch) = watch.used(SURFACE, 3);
    assert!(!copies);
    watch.written();
    let (copies, watch) = watch.used(SURFACE, 3);
    assert!(copies && watch.trusts_seed());
    let (copies, watch) = watch.used(SURFACE, 4);
    assert!(copies);
    // Another surface under the texture starts over.
    let (copies, watch) = watch.used(SURFACE + 1, 4);
    assert!(copies && !watch.trusts_seed());
}

/// A surface below a 1920x1080 frame keeps sampling its rows in
/// place: no copy at all.
#[test]
fn small_surfaces_stay_in_their_rows() {
    let mut device = device();
    let buffer = buffer(&mut device, 8 << 20);
    let planes = [
        PlaneLayout {
            offset: 0,
            bytes_per_row: 1280,
        },
        PlaneLayout {
            offset: 4 << 20,
            bytes_per_row: 1280,
        },
    ];
    let small = descriptor(
        PixelFormat::Ycbcr10_420_2p,
        640,
        360,
        TextureUsage::SHADER_READ,
    );
    let texture = device
        .new_two_plane_texture(buffer, &small, planes, 0)
        .unwrap();
    assert!(!device.has_aligned_copy(texture));
    device.destroy_texture(texture).unwrap();
}
