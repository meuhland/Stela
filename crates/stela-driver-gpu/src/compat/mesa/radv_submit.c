/* SPDX-License-Identifier: GPL-2.0-or-later */
/* RADV's counterparts of `submit_thread.c`'s NVK hooks.
 *
 * Puts a queue's submissions on Mesa's submit thread, a Linux task of the
 * host's environment: vkQueueSubmit2 then queues the work and returns, and
 * the thread makes the driver's submission (DRM_IOCTL_AMDGPU_CS) where Linux
 * runs, so the processor that committed does not wait for that environment.
 * A submission amdgpu refuses short of losing the context still makes its
 * signals (radv_queue_refuse, Mesa patch 0038), so the queue goes on; the
 * caller asks for the failure by a signal once it shows
 * (stela_radv_submit_failure). The same holds without the thread.
 *
 * vk_device_enable_threaded_submit() wants no queue yet, for drivers whose
 * queues start deferred; RADV's general queue starts immediate, so this
 * checks what it checks and lets the device take threads on demand itself.
 * RADV's sync type is DRM's syncobj, which waits for pending signals with a
 * timeline syncobj as NVK's does; without one RADV emulates timelines, which
 * cannot, and the queue keeps its submissions. */
#include "radv_queue.h"
#include "vk_device.h"
#include "vk_physical_device.h"
#include "vk_queue.h"
#include "vk_semaphore.h"
#include "vk_sync.h"

VkResult stela_radv_submit_thread(VkQueue handle);
VkResult stela_radv_submit_failure(VkQueue queue_handle, VkSemaphore semaphore_handle, uint64_t value);
uint32_t stela_radv_forget_failures(VkQueue queue_handle);
void stela_radv_count_failures(VkQueue queue_handle, uint32_t *outstanding);

VkResult stela_radv_submit_thread(VkQueue handle)
{
	VK_FROM_HANDLE(vk_queue, queue, handle);
	struct vk_device *device = queue->base.device;
	/* A wait the thread reorders must be able to wait for its signal's
	 * submission. */
	for (const struct vk_sync_type *const *type = device->physical->supported_sync_types; *type;
	     type++) {
		if (((*type)->features & VK_SYNC_FEATURE_GPU_WAIT) &&
		    !((*type)->features & VK_SYNC_FEATURE_WAIT_PENDING))
			return VK_ERROR_FEATURE_NOT_PRESENT;
	}
	if (!vk_device_supports_threaded_submit(device))
		device->submit_mode = VK_QUEUE_SUBMIT_MODE_THREADED_ON_DEMAND;
	return vk_queue_enable_submit_thread(queue);
}

/* The failure of the refused submission that signaled `value` on
 * `semaphore_handle`, taken once; VK_SUCCESS for every other. */
VkResult stela_radv_submit_failure(VkQueue queue_handle, VkSemaphore semaphore_handle, uint64_t value)
{
	VK_FROM_HANDLE(vk_queue, queue, queue_handle);
	VK_FROM_HANDLE(vk_semaphore, semaphore, semaphore_handle);
	struct radv_queue *radv = container_of(queue, struct radv_queue, vk);
	return radv_queue_take_failure(radv, vk_semaphore_get_active_sync(semaphore), value);
}

/* Drops the kept failures nobody will take; answers how many. */
uint32_t stela_radv_forget_failures(VkQueue queue_handle)
{
	VK_FROM_HANDLE(vk_queue, queue, queue_handle);
	return radv_queue_forget_failures(container_of(queue, struct radv_queue, vk));
}

/* Has the queue raise `*outstanding` by each failure it keeps, before the
 * refused submission's signals are made, and lower it by each it drops
 * unasked; the caller lowers it by those it takes and settles. Before the
 * queue's first submission. */
void stela_radv_count_failures(VkQueue queue_handle, uint32_t *outstanding)
{
	VK_FROM_HANDLE(vk_queue, queue, queue_handle);
	container_of(queue, struct radv_queue, vk)->failures_outstanding = outstanding;
}
