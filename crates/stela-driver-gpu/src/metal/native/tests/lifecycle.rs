//! Driver acknowledgements required before releasing possibly queued objects.
use super::super::*;
use crate::metal::nvidia::{Chip, fixture::nvk};
use alloc::sync::Arc;
use core::{
    cell::Cell,
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering},
};
use vk::Handle;

struct Calls {
    result: vk::Result,
    waits: AtomicUsize,
    frees: AtomicUsize,
    /// Submitted work never finishes, and waits for it time out.
    hung: bool,
    timeline: AtomicUsize,
}

impl Calls {
    fn new(result: vk::Result) -> Self {
        Self {
            result,
            waits: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            hung: false,
            timeline: AtomicUsize::new(0),
        }
    }
}

unsafe extern "system" fn wait_queue(queue: vk::Queue) -> vk::Result {
    // SAFETY: tests encode their kept Calls allocation in the queue handle.
    let calls = unsafe { &*(queue.as_raw() as *const Calls) };
    calls.waits.fetch_add(1, Ordering::Relaxed);
    calls.result
}

unsafe extern "system" fn wait_device(device: vk::Device) -> vk::Result {
    // SAFETY: tests encode their kept Calls allocation in the device handle.
    let calls = unsafe { &*(device.as_raw() as *const Calls) };
    calls.waits.fetch_add(1, Ordering::Relaxed);
    calls.result
}

unsafe extern "system" fn free_buffers(
    device: vk::Device,
    _: vk::CommandPool,
    count: u32,
    _: *const vk::CommandBuffer,
) {
    // SAFETY: as in wait_device; the callback never dereferences fake buffers.
    let calls = unsafe { &*(device.as_raw() as *const Calls) };
    assert_eq!(calls.waits.load(Ordering::Relaxed), 1);
    calls.frees.fetch_add(count as usize, Ordering::Relaxed);
}

fn device(calls: &Calls) -> ash::Device {
    // SAFETY: only the typed callbacks above are invoked; Calls outlives it.
    unsafe {
        ash::Device::load_with(
            |name| match name.to_bytes() {
                b"vkQueueWaitIdle" => wait_queue as *const c_void,
                b"vkDeviceWaitIdle" => wait_device as *const c_void,
                b"vkFreeCommandBuffers" => free_buffers as *const c_void,
                b"vkWaitSemaphores" => wait_timeline as *const c_void,
                _ => core::ptr::null(),
            },
            vk::Device::from_raw(calls as *const Calls as u64),
        )
    }
}

/// A wait for submitted work answers this: the work finished, or not in
/// time, as on a hung channel.
unsafe extern "system" fn wait_timeline(
    device: vk::Device,
    info: *const vk::SemaphoreWaitInfo<'_>,
    timeout: u64,
) -> vk::Result {
    // SAFETY: as in wait_device; the caller passes a valid wait.
    let (calls, info) = unsafe { (&*(device.as_raw() as *const Calls), &*info) };
    assert_eq!(info.semaphore_count, 1);
    // SAFETY: one semaphore and one value, as counted.
    let (semaphore, value) = unsafe { (*info.p_semaphores, *info.p_values) };
    assert_eq!(
        (semaphore.as_raw(), value),
        (7, 3),
        "each timeline's last value"
    );
    assert!(
        matches!(timeout, 0 | IDLE_STEP_NS | IDLE_WAIT_NS),
        "{timeout}"
    );
    calls.timeline.fetch_add(1, Ordering::Relaxed);
    if calls.hung {
        vk::Result::TIMEOUT
    } else {
        vk::Result::SUCCESS
    }
}

struct Exporter(Arc<AtomicUsize>);

impl Drop for Exporter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl MemoryExporter for Exporter {
    fn export(&self, _: &[MemoryRange]) -> Result<ExportedMemory> {
        unreachable!()
    }
    fn close(&self, _: ExportedMemory) {
        unreachable!()
    }
    fn unmap(&self, _: ExportedMemory) {
        unreachable!()
    }
}

unsafe extern "system" fn instance_proc(
    _: vk::Instance,
    _: *const core::ffi::c_char,
) -> vk::PFN_vkVoidFunction {
    None
}

unsafe extern "system" fn device_proc(
    _: vk::Device,
    _: *const core::ffi::c_char,
) -> vk::PFN_vkVoidFunction {
    None
}

fn backend(device: ash::Device, drops: &Arc<AtomicUsize>) -> NativeBackend {
    // SAFETY: these tables have no GPU; only wait_device runs during Drop.
    // Missing destruction callbacks fail the test if Drop tries to free state.
    let (entry, instance) = unsafe {
        (
            ash::Entry::from_static_fn(ash::StaticFn {
                get_instance_proc_addr: instance_proc,
            }),
            ash::Instance::load_with(
                |name| {
                    if name.to_bytes() == b"vkGetDeviceProcAddr" {
                        device_proc as *const c_void
                    } else {
                        core::ptr::null()
                    }
                },
                vk::Instance::null(),
            ),
        )
    };
    let conditional = ext::conditional_rendering::Device::new(&instance, &device);
    let exporter: Arc<dyn MemoryExporter> = Arc::new(Exporter(drops.clone()));
    let outside_device = device.clone();
    NativeBackend {
        _entry: ManuallyDrop::new(entry),
        instance,
        messenger: None,
        device,
        fns: ExtensionFns {
            host_image_copy: None,
            dynamic_state3: None,
            sample_locations: None,
            depth_bias: None,
            mesh: None,
            calibrated: None,
            external_memory_fd: None,
            external_semaphore_fd: None,
            conditional,
            generated: None,
            executables: None,
        },
        queue: vk::Queue::null(),
        snapshot: nvk(Chip::find(0x1b3).unwrap(), 16 << 30),
        heap: Heap {
            layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
        },
        null_sampler: vk::Sampler::null(),
        layout: vk::PipelineLayout::null(),
        command_pool: vk::CommandPool::null(),
        in_flight: VecDeque::new(),
        unsignaled: Vec::new(),
        mappings: BTreeMap::new(),
        memories: crate::metal::native::memory::Memories::new(1024),
        exporter: ManuallyDrop::new(exporter.clone()),
        outside: Arc::new(crate::metal::native::NativeOutside::new(
            outside_device,
            exporter,
            false,
            || {},
        )),
        imports: BTreeMap::new(),
        import_cache: crate::metal::native::ImportCache::default(),
        retired: VecDeque::new(),
        image_dedicated: alloc::collections::BTreeSet::new(),
        unbound: Vec::new(),
        refusals: None,
        outstanding: None,
        new_images: Vec::new(),
        zeroing: crate::metal::native::zero::Zeroing::new(1 << 32, |_| {}),
        bind_timeline: vk::Semaphore::null(),
        bind_value: 0,
        accumulate: vk::Pipeline::null(),
        select: vk::Pipeline::null(),
        resolve_draws: vk::Pipeline::null(),
        resolve_dispatches: vk::Pipeline::null(),
        store_texels: vk::Pipeline::null(),
        load_texels: vk::Pipeline::null(),
        average_samples: vk::Pipeline::null(),
        decode_etc: vk::Pipeline::null(),
        decode_pvrtc: vk::Pipeline::null(),
        reorder_blocks: vk::Pipeline::null(),
        decode_astc: vk::Pipeline::null(),
        store: crate::metal::native::pipeline_store::Persisted::none(|_| {}),
        deferral: Default::default(),
        premade: Default::default(),
        ticks: crate::metal::timing::no_clock,
        split: [0; 3],
        idle: Default::default(),
        idle_waited: 0,
    }
}

#[test]
fn failed_device_idle_keeps_the_export_owner() {
    for result in [vk::Result::ERROR_DEVICE_LOST, vk::Result::TIMEOUT] {
        let calls = Calls::new(result);
        let drops = Arc::new(AtomicUsize::new(0));
        let backend = backend(device(&calls), &drops);
        drop(backend);
        assert_eq!(calls.waits.load(Ordering::Relaxed), 1);
        assert_eq!(calls.frees.load(Ordering::Relaxed), 0);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn idle_acknowledgement_requires_native_success_and_keeps_roots_owned() {
    for (result, expected) in [
        (vk::Result::SUCCESS, Ok(())),
        (vk::Result::ERROR_DEVICE_LOST, Err(Error::DeviceLost)),
        // The host's bound on a fence wait ended Mesa's queue idle wait.
        (vk::Result::TIMEOUT, Err(Error::DeviceLost)),
        (
            vk::Result::ERROR_OUT_OF_HOST_MEMORY,
            Err(Error::OutOfMemory),
        ),
    ] {
        let calls = Calls::new(result);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut backend = ManuallyDrop::new(backend(device(&calls), &drops));
        assert_eq!(backend.acknowledge_idle(), expected);
        assert_eq!(calls.waits.load(Ordering::Relaxed), 1);
        assert_eq!(calls.frees.load(Ordering::Relaxed), 0);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        // SAFETY: this fixture has no native GPU resources. Its two owned
        // Rust roots are dropped once without invoking fake Vulkan teardown.
        unsafe {
            ManuallyDrop::drop(&mut backend.exporter);
            ManuallyDrop::drop(&mut backend._entry);
        }
    }
}

/// Work in flight on two submissions of one timeline.
fn in_flight(backend: &mut NativeBackend) {
    for (value, buffer) in [(2, 1), (3, 2)] {
        backend.in_flight.push_back(InFlight {
            buffers: CommandBuffers::One(vk::CommandBuffer::from_raw(buffer)),
            timeline: vk::Semaphore::from_raw(7),
            value,
        });
    }
}

/// A hung channel's fences signal only when the kernel driver's job timeout
/// kills it, 10 s on: the idle proof waits a bounded time for the queue's
/// own work, and work unfinished by then keeps everything, never waiting
/// in `vkDeviceWaitIdle` for the channel.
#[test]
fn idle_proofs_wait_a_bounded_time_for_submitted_work() {
    for hung in [false, true] {
        let calls = Calls {
            hung,
            ..Calls::new(vk::Result::SUCCESS)
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let mut backend = ManuallyDrop::new(backend(device(&calls), &drops));
        in_flight(&mut backend);
        let expected = if hung { Err(Error::DeviceLost) } else { Ok(()) };
        assert_eq!(backend.acknowledge_idle(), expected);
        assert_eq!(calls.timeline.load(Ordering::Relaxed), 1);
        assert_eq!(calls.waits.load(Ordering::Relaxed), usize::from(!hung));
        // SAFETY: as in the test above; the fake buffers own nothing.
        unsafe {
            ManuallyDrop::drop(&mut backend.exporter);
            ManuallyDrop::drop(&mut backend._entry);
        }
    }
    // Dropped with hung work, the backend keeps its export owner.
    let calls = Calls {
        hung: true,
        ..Calls::new(vk::Result::SUCCESS)
    };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut backend = backend(device(&calls), &drops);
    in_flight(&mut backend);
    drop(backend);
    assert_eq!(calls.timeline.load(Ordering::Relaxed), 1);
    assert_eq!(calls.waits.load(Ordering::Relaxed), 0);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
}

struct BufferCalls {
    status: Cell<vk::Result>,
    value: Cell<u64>,
    allocated: Cell<u64>,
    queries: Cell<usize>,
    freed: Cell<Option<(u32, u64)>>,
}

unsafe extern "system" fn allocate(
    device: vk::Device,
    info: *const vk::CommandBufferAllocateInfo<'_>,
    output: *mut vk::CommandBuffer,
) -> vk::Result {
    // SAFETY: the fixture owns the kept BufferCalls and Vulkan arguments.
    let (calls, info) = unsafe { (&*(device.as_raw() as *const BufferCalls), &*info) };
    assert!(info.level == vk::CommandBufferLevel::PRIMARY);
    assert_eq!(info.command_pool.as_raw(), 2);
    if calls.status.get() == vk::Result::SUCCESS {
        for index in 0..info.command_buffer_count {
            let next = calls.allocated.get() + 1;
            // SAFETY: the caller provides one writable handle per requested buffer.
            unsafe {
                output
                    .add(index as usize)
                    .write(vk::CommandBuffer::from_raw(next))
            };
            calls.allocated.set(next);
        }
    }
    calls.status.get()
}

unsafe extern "system" fn counter(
    device: vk::Device,
    _: vk::Semaphore,
    value: *mut u64,
) -> vk::Result {
    // SAFETY: the fixture owns the kept BufferCalls.
    let calls = unsafe { &*(device.as_raw() as *const BufferCalls) };
    calls.queries.set(calls.queries.get() + 1);
    // SAFETY: the caller provides a writable counter.
    unsafe { value.write(calls.value.get()) };
    calls.status.get()
}

unsafe extern "system" fn free_completed(
    device: vk::Device,
    pool: vk::CommandPool,
    count: u32,
    buffers: *const vk::CommandBuffer,
) {
    // SAFETY: the fixture owns the kept BufferCalls and submitted handle array.
    let (calls, buffers) = unsafe {
        (
            &*(device.as_raw() as *const BufferCalls),
            core::slice::from_raw_parts(buffers, count as usize),
        )
    };
    assert_eq!(pool.as_raw(), 2);
    let ordered = buffers
        .iter()
        .fold(0, |sum, buffer| sum * 10 + buffer.as_raw());
    assert!(calls.freed.replace(Some((count, ordered))).is_none());
}

#[test]
fn inline_and_owned_buffers_retire_once_in_queue_order() {
    let calls = BufferCalls {
        status: Cell::new(vk::Result::SUCCESS),
        value: Cell::new(0),
        allocated: Cell::new(0),
        queries: Cell::new(0),
        freed: Cell::new(None),
    };
    // SAFETY: only these typed test callbacks are invoked and Calls stays live.
    let device = unsafe {
        ash::Device::load_with(
            |name| match name.to_bytes() {
                b"vkAllocateCommandBuffers" => allocate as *const c_void,
                b"vkGetSemaphoreCounterValue" => counter as *const c_void,
                b"vkFreeCommandBuffers" => free_completed as *const c_void,
                _ => core::ptr::null(),
            },
            vk::Device::from_raw(&calls as *const BufferCalls as u64),
        )
    };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut backend = ManuallyDrop::new(backend(device, &drops));
    backend.command_pool = vk::CommandPool::from_raw(2);
    for (timeline, value, count) in [(7, 1, 1), (7, 2, 1), (7, 3, 2), (8, 4, 1)] {
        let buffers =
            CommandBuffers::allocate(&backend.device, backend.command_pool, count).unwrap();
        assert_eq!(buffers.as_slice().len(), count as usize);
        backend.in_flight.push_back(InFlight {
            buffers,
            timeline: vk::Semaphore::from_raw(timeline),
            value,
        });
    }
    calls.status.set(vk::Result::ERROR_DEVICE_LOST);
    for count in [1, 2] {
        assert!(matches!(
            CommandBuffers::allocate(&backend.device, backend.command_pool, count),
            Err(Error::DeviceLost),
        ));
    }
    assert_eq!(backend.reclaim(), Err(Error::DeviceLost));
    assert_eq!(
        backend.semaphore_value(Semaphore(7)),
        Err(Error::DeviceLost)
    );
    assert_eq!(backend.in_flight.len(), 4);
    assert_eq!(calls.allocated.get(), 5);
    assert_eq!(calls.freed.get(), None);

    calls.status.set(vk::Result::SUCCESS);
    backend.reclaim().unwrap();
    assert_eq!(calls.freed.get(), None);
    calls.value.set(4);
    assert_eq!(backend.semaphore_value(Semaphore(8)), Ok(4));
    assert_eq!(calls.freed.get(), None);
    assert_eq!(backend.in_flight.len(), 4);
    calls.value.set(1);
    backend.reclaim().unwrap();
    assert_eq!(calls.freed.replace(None), Some((1, 1)));
    assert_eq!(backend.in_flight.len(), 3);

    calls.value.set(4);
    assert_eq!(backend.semaphore_value(Semaphore(7)), Ok(4));
    assert_eq!(calls.freed.replace(None), Some((3, 234)));
    assert_eq!(backend.in_flight.len(), 1);
    assert_eq!(backend.semaphore_value(Semaphore(8)), Ok(4));
    assert_eq!(calls.freed.replace(None), Some((1, 5)));
    assert!(backend.in_flight.is_empty());
    let queries = calls.queries.get();
    backend.reclaim().unwrap();
    assert_eq!(calls.queries.get(), queries);
    // SAFETY: the fixture has no Vulkan resources and the list is empty.
    unsafe { ManuallyDrop::drop(&mut backend.exporter) };
}

#[test]
fn a_freed_import_is_taken_back_for_the_same_guest_ram_only() {
    use crate::metal::native::{ExportedMemory, ImportCache};
    use stela_contracts::gate::MemoryRange;
    let mut page = [0u8; 64];
    let pointer = core::ptr::NonNull::new(page.as_mut_ptr()).unwrap();
    let exported = |bytes| ExportedMemory {
        fd: -1,
        pointer,
        bytes,
    };
    let range = |address| MemoryRange {
        address,
        bytes: 4096,
    };
    let mut cache = ImportCache::default();
    assert!(
        cache
            .keep(
                alloc::vec![range(0x1000)],
                vk::DeviceMemory::from_raw(1),
                exported(4096)
            )
            .is_empty()
    );
    assert!(cache.take(&[range(0x2000)]).is_none(), "other guest RAM");
    assert!(
        cache.take(&[range(0x1000), range(0x3000)]).is_none(),
        "more guest RAM"
    );
    let kept = cache.take(&[range(0x1000)]).expect("the same guest RAM");
    assert_eq!(kept.memory, vk::DeviceMemory::from_raw(1));
    assert!(cache.take(&[range(0x1000)]).is_none(), "taken once");
    // Past the bounds the oldest go, for the caller to free.
    let mut evicted = 0;
    for index in 0..ImportCache::COUNT as u64 + 2 {
        evicted += cache
            .keep(
                alloc::vec![range(index << 12)],
                vk::DeviceMemory::from_raw(index + 2),
                exported(4096),
            )
            .len();
    }
    assert_eq!(evicted, 2);
    assert!(cache.take(&[range(0)]).is_none(), "the oldest went");
    let big = cache.keep(
        alloc::vec![range(1 << 40)],
        vk::DeviceMemory::from_raw(99),
        exported(ImportCache::BYTES),
    );
    assert_eq!(
        big.len(),
        ImportCache::COUNT,
        "every older one goes for the bytes"
    );
    assert_eq!(cache.drain().count(), 1);
}

/// A backend about to be dropped proves its queue idle in steps of at most
/// one short wait each, under the processor's half second, and the steps
/// together wait no longer than the idle bound. Once a step ends the proof,
/// dropping waits for nothing.
#[test]
fn idle_steps_wait_briefly_each_and_end_at_the_idle_bound() {
    let steps = (IDLE_WAIT_NS / IDLE_STEP_NS) as usize;
    const _: () = assert!(IDLE_STEP_NS < 250_000_000);
    // A hung channel: the timeline never reaches its last value.
    let calls = Calls {
        hung: true,
        ..Calls::new(vk::Result::SUCCESS)
    };
    let drops = Arc::new(AtomicUsize::new(0));
    let mut stepped = backend(device(&calls), &drops);
    in_flight(&mut stepped);
    for _ in 1..steps {
        assert_eq!(stepped.idle_step(), IdleStep::Waiting);
    }
    assert_eq!(stepped.idle_step(), IdleStep::Unproven);
    assert_eq!(stepped.idle_step(), IdleStep::Unproven, "sticky");
    // Two timeline waits a step (a poll, then the short wait) until the
    // bound; no queue idle wait.
    assert_eq!(calls.timeline.load(Ordering::Relaxed), 2 * steps);
    assert_eq!(calls.waits.load(Ordering::Relaxed), 0);
    drop(stepped);
    assert_eq!(calls.timeline.load(Ordering::Relaxed), 2 * steps);
    assert_eq!(drops.load(Ordering::Relaxed), 0, "the export owner stays");

    // The host ends each queue idle wait at its bound.
    let calls = Calls::new(vk::Result::TIMEOUT);
    let drops = Arc::new(AtomicUsize::new(0));
    let mut stepped = backend(device(&calls), &drops);
    in_flight(&mut stepped);
    for _ in 1..steps {
        assert_eq!(stepped.idle_step(), IdleStep::Waiting);
    }
    assert_eq!(stepped.idle_step(), IdleStep::Unproven);
    assert_eq!(calls.waits.load(Ordering::Relaxed), steps);
    drop(stepped);
    assert_eq!(calls.waits.load(Ordering::Relaxed), steps);
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    // A drained queue is idle at the first step.
    let calls = Calls::new(vk::Result::SUCCESS);
    let drops = Arc::new(AtomicUsize::new(0));
    let mut stepped = ManuallyDrop::new(backend(device(&calls), &drops));
    in_flight(&mut stepped);
    assert_eq!(stepped.idle_step(), IdleStep::Idle);
    assert_eq!(stepped.idle_step(), IdleStep::Idle);
    assert_eq!(calls.waits.load(Ordering::Relaxed), 1);
    // SAFETY: as in the tests above; the fake buffers own nothing.
    unsafe {
        ManuallyDrop::drop(&mut stepped.exporter);
        ManuallyDrop::drop(&mut stepped._entry);
    }
}
