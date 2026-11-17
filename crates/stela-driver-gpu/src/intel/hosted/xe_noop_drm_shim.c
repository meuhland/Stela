/* SPDX-License-Identifier: GPL-2.0-or-later */
/* An xe render node with no GPU behind it, for host tests of ANV over xe: the
 * xe counterpart of Mesa's i915 no-op shim (src/intel/tools), on Mesa's DRM
 * shim (src/drm-shim), which intercepts the process's open, ioctl and mmap of
 * /dev/dri/renderD128 and its sysfs entries. Queries answer what xe answers
 * for the device INTEL_STUB_GPU_DEVICE_ID names (hex), from Mesa's own device
 * table; buffers are host memory; binds, submissions and syncobjs succeed at
 * once and nothing runs. Only the ioctls Stela's xe registers
 * (xe_device.c's xe_ioctls) are answered; any other driver ioctl, or a query
 * this file does not know, fails with the reason on stderr, so a test that
 * passes used only those.
 *
 * Each ioctl answered is counted; at exit one line names them:
 * "xe shim: ioctls DEVICE_QUERY=n ..." for the test to compare. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/mman.h>

#include "dev/intel_device_info.h"
#include "drm-shim/drm_shim.h"
#include "drm-uapi/xe_drm.h"
#include "util/macros.h"

static struct intel_device_info devinfo;
static uint32_t device_id;
static uint32_t next_vm = 1, next_queue = 1;

/* The driver ioctls by number, with how often each was answered. */
static const char *const names[] = {
	[DRM_XE_DEVICE_QUERY] = "DEVICE_QUERY",
	[DRM_XE_GEM_CREATE] = "GEM_CREATE",
	[DRM_XE_GEM_MMAP_OFFSET] = "GEM_MMAP_OFFSET",
	[DRM_XE_VM_CREATE] = "VM_CREATE",
	[DRM_XE_VM_DESTROY] = "VM_DESTROY",
	[DRM_XE_VM_BIND] = "VM_BIND",
	[DRM_XE_EXEC_QUEUE_CREATE] = "EXEC_QUEUE_CREATE",
	[DRM_XE_EXEC_QUEUE_DESTROY] = "EXEC_QUEUE_DESTROY",
	[DRM_XE_EXEC_QUEUE_GET_PROPERTY] = "EXEC_QUEUE_GET_PROPERTY",
	[DRM_XE_EXEC] = "EXEC",
	[DRM_XE_VM_GET_PROPERTY] = "VM_GET_PROPERTY",
};
static unsigned counts[ARRAY_SIZE(names)];

static int fail(int error)
{
	errno = error;
	return -1;
}

static void report(void)
{
	fprintf(stderr, "xe shim: ioctls");
	for (unsigned i = 0; i < ARRAY_SIZE(names); i++)
		if (counts[i])
			fprintf(stderr, " %s=%u", names[i], counts[i]);
	fprintf(stderr, "\n");
}

/* Answers a query of `bytes`: its size when asked with none, else the data. */
static int answer(struct drm_xe_device_query *query, const void *data, uint32_t bytes)
{
	if (query->size == 0) {
		query->size = bytes;
		return 0;
	}
	if (query->size != bytes)
		return fail(EINVAL);
	memcpy((void *)(uintptr_t)query->data, data, bytes);
	return 0;
}

static bool discrete(void)
{
	return devinfo.has_local_mem;
}

static int query_config(struct drm_xe_device_query *query)
{
	uint8_t storage[sizeof(struct drm_xe_query_config) + 5 * sizeof(uint64_t)] = {0};
	struct drm_xe_query_config *config = (void *)storage;
	config->num_params = 5;
	config->info[DRM_XE_QUERY_CONFIG_REV_AND_DEVICE_ID] =
		device_id | (uint64_t)devinfo.revision << 16;
	config->info[DRM_XE_QUERY_CONFIG_FLAGS] = discrete() ? DRM_XE_QUERY_CONFIG_FLAG_HAS_VRAM : 0;
	/* xe's XE_PAGE_SIZE, or 64 KiB where VRAM needs it (DG2). */
	config->info[DRM_XE_QUERY_CONFIG_MIN_ALIGNMENT] = discrete() ? 64 << 10 : 4 << 10;
	config->info[DRM_XE_QUERY_CONFIG_VA_BITS] = 48;
	/* DRM_SCHED_PRIORITY_NORMAL: higher needs CAP_SYS_NICE. */
	config->info[DRM_XE_QUERY_CONFIG_MAX_EXEC_QUEUE_PRIORITY] = 1;
	return answer(query, storage, sizeof(storage));
}

static int query_regions(struct drm_xe_device_query *query)
{
	uint8_t storage[sizeof(struct drm_xe_query_mem_regions) +
			2 * sizeof(struct drm_xe_mem_region)] = {0};
	struct drm_xe_query_mem_regions *regions = (void *)storage;
	regions->num_mem_regions = discrete() ? 2 : 1;
	struct drm_xe_mem_region *system = &regions->mem_regions[0];
	system->mem_class = DRM_XE_MEM_REGION_CLASS_SYSMEM;
	system->min_page_size = 4 << 10;
	system->total_size = 16ull << 30;
	if (discrete()) {
		struct drm_xe_mem_region *vram = &regions->mem_regions[1];
		vram->mem_class = DRM_XE_MEM_REGION_CLASS_VRAM;
		vram->instance = 1;
		vram->min_page_size = 64 << 10;
		vram->total_size = 8ull << 30;
		vram->cpu_visible_size = 8ull << 30;
	}
	uint32_t bytes = sizeof(*regions) + regions->num_mem_regions * sizeof(*system);
	return answer(query, storage, bytes);
}

static int query_gts(struct drm_xe_device_query *query)
{
	uint8_t storage[sizeof(struct drm_xe_query_gt_list) + sizeof(struct drm_xe_gt)] = {0};
	struct drm_xe_query_gt_list *list = (void *)storage;
	list->num_gt = 1;
	struct drm_xe_gt *gt = &list->gt_list[0];
	gt->type = DRM_XE_QUERY_GT_TYPE_MAIN;
	gt->reference_clock = devinfo.timestamp_frequency ? (uint32_t)devinfo.timestamp_frequency
							  : 19200000;
	gt->near_mem_regions = discrete() ? 1 << 1 : 1 << 0;
	gt->far_mem_regions = discrete() ? 1 << 0 : 0;
	if (devinfo.gfx_ip_ver) {
		gt->ip_ver_major = (uint16_t)(devinfo.gfx_ip_ver >> 16);
		gt->ip_ver_minor = (uint16_t)(devinfo.gfx_ip_ver & 0xffff);
	} else {
		gt->ip_ver_major = (uint16_t)(devinfo.verx10 / 10);
		gt->ip_ver_minor = (uint16_t)(devinfo.verx10 % 10);
	}
	gt->ip_ver_rev = (uint16_t)devinfo.revision;
	return answer(query, storage, sizeof(storage));
}

static int query_engines(struct drm_xe_device_query *query)
{
	uint8_t storage[sizeof(struct drm_xe_query_engines) + 3 * sizeof(struct drm_xe_engine)] = {0};
	struct drm_xe_query_engines *engines = (void *)storage;
	const uint16_t classes[] = {DRM_XE_ENGINE_CLASS_RENDER, DRM_XE_ENGINE_CLASS_COPY,
				    DRM_XE_ENGINE_CLASS_COMPUTE};
	/* Compute engines from Xe-HP on. */
	engines->num_engines = devinfo.verx10 >= 125 ? 3 : 2;
	for (unsigned i = 0; i < engines->num_engines; i++)
		engines->engines[i].instance.engine_class = classes[i];
	uint32_t bytes = sizeof(*engines) + engines->num_engines * sizeof(engines->engines[0]);
	return answer(query, storage, bytes);
}

/* The DSS geometry and EU masks of Mesa's table for the device, laid out as
 * xe reports them: DSS bits per slice of 6 before Xe-HP, of 4 from it. */
static int query_topology(struct drm_xe_device_query *query)
{
	uint8_t storage[2 * (sizeof(struct drm_xe_query_topology_mask) + 8)] = {0};
	unsigned per_slice = devinfo.verx10 >= 125 ? 4 : 6;
	uint64_t dss = 0;
	for (unsigned s = 0; s < (unsigned)devinfo.num_slices; s++)
		dss |= BITFIELD64_MASK(devinfo.num_subslices[s]) << (s * per_slice);
	uint64_t eus = BITFIELD64_MASK(devinfo.max_eus_per_subslice);
	struct drm_xe_query_topology_mask *mask = (void *)storage;
	mask->type = DRM_XE_TOPO_DSS_GEOMETRY;
	mask->num_bytes = 8;
	memcpy(mask->mask, &dss, 8);
	mask = (void *)&mask->mask[8];
	mask->type = DRM_XE_TOPO_EU_PER_DSS;
	mask->num_bytes = 8;
	memcpy(mask->mask, &eus, 8);
	return answer(query, storage, sizeof(storage));
}

static int query_cycles(struct drm_xe_device_query *query)
{
	if (query->size == 0) {
		query->size = sizeof(struct drm_xe_query_engine_cycles);
		return 0;
	}
	struct drm_xe_query_engine_cycles *cycles = (void *)(uintptr_t)query->data;
	struct timespec now;
	if (query->size != sizeof(*cycles) || clock_gettime(cycles->clockid, &now))
		return fail(EINVAL);
	uint64_t ns = (uint64_t)now.tv_sec * 1000000000ull + (uint64_t)now.tv_nsec;
	cycles->cpu_timestamp = ns;
	cycles->cpu_delta = 0;
	cycles->width = 36;
	cycles->engine_cycles = ns / 52;
	return 0;
}

static int xe_device_query(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	struct drm_xe_device_query *query = argument;
	if (query->extensions || query->reserved[0] || query->reserved[1])
		return fail(EINVAL);
	switch (query->query) {
	case DRM_XE_DEVICE_QUERY_ENGINES:
		return query_engines(query);
	case DRM_XE_DEVICE_QUERY_MEM_REGIONS:
		return query_regions(query);
	case DRM_XE_DEVICE_QUERY_CONFIG:
		return query_config(query);
	case DRM_XE_DEVICE_QUERY_GT_LIST:
		return query_gts(query);
	case DRM_XE_DEVICE_QUERY_HWCONFIG:
		/* GuC's table is empty before GuC loads it, as on platforms
		 * without one. */
		return answer(query, NULL, 0);
	case DRM_XE_DEVICE_QUERY_GT_TOPOLOGY:
		return query_topology(query);
	case DRM_XE_DEVICE_QUERY_ENGINE_CYCLES:
		return query_cycles(query);
	case DRM_XE_DEVICE_QUERY_UC_FW_VERSION:
	case DRM_XE_DEVICE_QUERY_OA_UNITS:
	case DRM_XE_DEVICE_QUERY_PXP_STATUS:
	case DRM_XE_DEVICE_QUERY_EU_STALL:
		/* No firmware loaded, no observation, no PXP: xe's answer
		 * where those are absent. */
		return fail(ENODEV);
	default:
		fprintf(stderr, "xe shim: unknown device query %u\n", query->query);
		return fail(EINVAL);
	}
}

static int xe_gem_create(int fd, unsigned long request, void *argument)
{
	(void)request;
	struct drm_xe_gem_create *create = argument;
	struct shim_fd *shim_fd = drm_shim_fd_lookup(fd);
	if (create->extensions || !create->size || create->size > UINT32_MAX)
		return fail(EINVAL);
	struct shim_bo *bo = calloc(1, sizeof(*bo));
	if (!bo || drm_shim_bo_init(bo, create->size)) {
		free(bo);
		return fail(ENOMEM);
	}
	create->handle = (uint32_t)drm_shim_bo_get_handle(shim_fd, bo);
	drm_shim_bo_put(bo);
	return 0;
}

static int xe_gem_mmap_offset(int fd, unsigned long request, void *argument)
{
	(void)request;
	struct drm_xe_gem_mmap_offset *mmap = argument;
	struct shim_fd *shim_fd = drm_shim_fd_lookup(fd);
	struct shim_bo *bo = drm_shim_bo_lookup(shim_fd, (int)mmap->handle);
	if (!bo)
		return fail(ENOENT);
	mmap->offset = drm_shim_bo_get_mmap_offset(shim_fd, bo);
	drm_shim_bo_put(bo);
	return 0;
}

static int xe_vm_create(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	struct drm_xe_vm_create *create = argument;
	create->vm_id = __atomic_fetch_add(&next_vm, 1, __ATOMIC_RELAXED);
	return 0;
}

static int xe_exec_queue_create(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	struct drm_xe_exec_queue_create *create = argument;
	if (!create->width || !create->num_placements || !create->vm_id)
		return fail(EINVAL);
	create->exec_queue_id = __atomic_fetch_add(&next_queue, 1, __ATOMIC_RELAXED);
	return 0;
}

static int xe_exec_queue_get_property(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	struct drm_xe_exec_queue_get_property *property = argument;
	if (property->property != DRM_XE_EXEC_QUEUE_GET_PROPERTY_BAN)
		return fail(EINVAL);
	property->value = 0;
	return 0;
}

static int xe_vm_get_property(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	struct drm_xe_vm_get_property *property = argument;
	/* No such VM, as xe answers an ID it never gave. */
	if (property->vm_id == 0 || property->vm_id >= next_vm)
		return fail(ENOENT);
	if (property->property != DRM_XE_VM_GET_PROPERTY_FAULTS)
		return fail(EINVAL);
	property->size = 0;
	return 0;
}

/* Binds, destruction and submissions: accepted, with nothing to run. */
static int xe_accept(int fd, unsigned long request, void *argument)
{
	(void)fd;
	(void)request;
	(void)argument;
	return 0;
}

static int counted(int fd, unsigned long request, void *argument);

static ioctl_fn_t handlers[ARRAY_SIZE(names)] = {
	[DRM_XE_DEVICE_QUERY] = xe_device_query,
	[DRM_XE_GEM_CREATE] = xe_gem_create,
	[DRM_XE_GEM_MMAP_OFFSET] = xe_gem_mmap_offset,
	[DRM_XE_VM_CREATE] = xe_vm_create,
	[DRM_XE_VM_DESTROY] = xe_accept,
	[DRM_XE_VM_BIND] = xe_accept,
	[DRM_XE_EXEC_QUEUE_CREATE] = xe_exec_queue_create,
	[DRM_XE_EXEC_QUEUE_DESTROY] = xe_accept,
	[DRM_XE_EXEC_QUEUE_GET_PROPERTY] = xe_exec_queue_get_property,
	[DRM_XE_EXEC] = xe_accept,
	[DRM_XE_VM_GET_PROPERTY] = xe_vm_get_property,
};
static ioctl_fn_t driver_ioctls[ARRAY_SIZE(names)];

static int counted(int fd, unsigned long request, void *argument)
{
	unsigned number = _IOC_NR(request) - DRM_COMMAND_BASE;
	__atomic_fetch_add(&counts[number], 1, __ATOMIC_RELAXED);
	return handlers[number](fd, request, argument);
}

/* ANV stats its device's primary node, whose path is NULL when only a render
 * node is listed, as Stela's libdrm stand-in lists it. Linux answers a NULL
 * path with EFAULT; Mesa's DRM shim reads every path it intercepts, so the
 * host test renames its stat and stat64 (objcopy) and these come first. The
 * names are assembler labels so no C library prototype marks the path as
 * never NULL, which would let the compiler drop the check. */
int stela_drm_shim_stat(const char *path, void *buffer);
int stela_drm_shim_stat64(const char *path, void *buffer);
int stela_xe_shim_stat(const char *path, void *buffer) __asm__("stat");
int stela_xe_shim_stat64(const char *path, void *buffer) __asm__("stat64");

__attribute__((visibility("default"))) int stela_xe_shim_stat(const char *path, void *buffer)
{
	return path ? stela_drm_shim_stat(path, buffer) : fail(EFAULT);
}

__attribute__((visibility("default"))) int stela_xe_shim_stat64(const char *path, void *buffer)
{
	return path ? stela_drm_shim_stat64(path, buffer) : fail(EFAULT);
}

void drm_shim_driver_init(void)
{
	const char *named = getenv("INTEL_STUB_GPU_DEVICE_ID");
	device_id = named ? (uint32_t)strtoul(named, NULL, 16) : 0;
	if (!device_id || !intel_get_device_info_from_pci_id((int)device_id, &devinfo)) {
		fprintf(stderr, "xe shim: INTEL_STUB_GPU_DEVICE_ID names no device Mesa knows\n");
		return;
	}
	for (unsigned i = 0; i < ARRAY_SIZE(names); i++)
		driver_ioctls[i] = handlers[i] ? counted : NULL;
	shim_device.driver_name = "xe";
	shim_device.driver_ioctls = driver_ioctls;
	shim_device.driver_ioctl_count = ARRAY_SIZE(driver_ioctls);
	atexit(report);
	drm_shim_pci_device_setup(0x8086, (uint16_t)device_id, "0000:00:02.0", "xe");
}
