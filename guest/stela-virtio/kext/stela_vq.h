/*
 * The driver side of one split virtqueue (virtio 1.2, 2.7): plain C99, with
 * no IOKit, so the StelaVirtIO kext compiles it unchanged and the host's
 * tests run it against the device's queue (tests/virtqueue.c, and
 * stela_soc::virtio::queue through xtask).
 *
 * The queue's three areas are the caller's memory: the descriptor table,
 * the available ring and the used ring, each at the address the device is
 * given. Descriptors are handed out from a free list; a chain is a run of
 * readable buffers then writable ones, as the device requires, and is named
 * by its head, whose token the caller gets back when the device uses it.
 * Indirect descriptors are never made. With VIRTIO_F_EVENT_IDX the driver
 * asks for interrupts by `used_event` and the device for notifications by
 * `avail_event`; without it, by the rings' flags.
 *
 * The indices are free-running 16-bit counters, so they wrap at 2^16 as the
 * specification's do. Nothing here locks: the caller serializes each queue.
 */
#pragma once

#include <stdint.h>

#include <stela/virtio.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The most entries a queue of this code has. */
#define STELA_VQ_MAX 256u

struct stela_vq_desc {
	uint64_t addr;
	uint32_t len;
	uint16_t flags;
	uint16_t next;
};

struct stela_vq_used_elem {
	uint32_t id;
	uint32_t len;
};

/* One buffer of a chain the driver adds. */
struct stela_vq_buf {
	uint64_t addr;
	uint32_t len;
};

struct stela_vq {
	uint16_t size;
	/* The first free descriptor, and how many are free. */
	uint16_t free_head;
	uint16_t free_count;
	/* The available index the driver has filled up to, published or not. */
	uint16_t avail_idx;
	/* The available index the device last heard of (a kick, or none wanted). */
	uint16_t kicked;
	/* The next used index the driver harvests. */
	uint16_t last_used;
	int event_idx;
	volatile struct stela_vq_desc *desc;
	/* flags, idx, ring[size], used_event */
	volatile uint16_t *avail;
	/* flags, idx, ring[size] of stela_vq_used_elem, avail_event */
	volatile uint8_t *used;
	/* Each head's token while its chain is the device's, else null. */
	void *tokens[STELA_VQ_MAX];
	/* Each head's chain length. */
	uint16_t lengths[STELA_VQ_MAX];
};

/* What a harvest found. */
enum stela_vq_harvested {
	STELA_VQ_NONE = 0,
	STELA_VQ_USED = 1,
	/* The used entry names no chain of the driver's: the device is broken. */
	STELA_VQ_BROKEN = -1,
};

/* Whether an index moving from `old` to `now` passes `event` (vring_need_event). */
static inline int
stela_vq_need_event(uint16_t event, uint16_t now, uint16_t old)
{
	return (uint16_t)(now - event - 1) < (uint16_t)(now - old);
}

static inline volatile uint16_t *
stela_vq_used_idx(struct stela_vq *vq)
{
	return (volatile uint16_t *)(vq->used + 2);
}

static inline volatile uint16_t *
stela_vq_used_flags(struct stela_vq *vq)
{
	return (volatile uint16_t *)vq->used;
}

static inline volatile uint16_t *
stela_vq_avail_event(struct stela_vq *vq)
{
	return (volatile uint16_t *)(vq->used + 4 + 8 * (uint32_t)vq->size);
}

static inline volatile struct stela_vq_used_elem *
stela_vq_used_elem(struct stela_vq *vq, uint16_t index)
{
	return (volatile struct stela_vq_used_elem *)(vq->used + 4 + 8 * (uint32_t)(index % vq->size));
}

/*
 * Sets up a queue of `size` entries, a power of two up to STELA_VQ_MAX, on
 * zeroed areas: `desc` 16-byte aligned, `avail` 2-byte, `used` 4-byte.
 * Answers 0, or -1 for a size or area this code cannot use.
 */
static inline int
stela_vq_init(struct stela_vq *vq, uint16_t size, void *desc, void *avail, void *used, int event_idx)
{
	if (size == 0 || (size & (size - 1)) != 0 || size > STELA_VQ_MAX) {
		return -1;
	}
	if (((uintptr_t)desc % STELA_VIRTIO_DESC_ALIGN) != 0 || ((uintptr_t)avail % STELA_VIRTIO_AVAIL_ALIGN) != 0 ||
	    ((uintptr_t)used % STELA_VIRTIO_USED_ALIGN) != 0) {
		return -1;
	}
	vq->size = size;
	vq->desc = (volatile struct stela_vq_desc *)desc;
	vq->avail = (volatile uint16_t *)avail;
	vq->used = (volatile uint8_t *)used;
	vq->event_idx = event_idx;
	vq->free_head = 0;
	vq->free_count = size;
	vq->avail_idx = 0;
	vq->kicked = 0;
	vq->last_used = 0;
	for (uint16_t index = 0; index < size; index++) {
		vq->desc[index].next = (uint16_t)(index + 1);
		vq->tokens[index] = 0;
		vq->lengths[index] = 0;
	}
	return 0;
}

/*
 * Makes `readable` buffers the device reads, then `writable` ones it writes,
 * one chain the device may take once published, and answers its head; -1
 * when the queue has too few free descriptors, the chain is empty, or the
 * token is null (a used entry with no token is how a broken device shows).
 */
static inline int
stela_vq_add(struct stela_vq *vq, const struct stela_vq_buf *buffers, unsigned readable, unsigned writable,
    void *token)
{
	unsigned count = readable + writable;
	if (count == 0 || count > vq->free_count || token == 0) {
		return -1;
	}
	uint16_t head = vq->free_head;
	uint16_t index = head;
	for (unsigned at = 0; at < count; at++) {
		volatile struct stela_vq_desc *desc = &vq->desc[index];
		uint16_t next = desc->next;
		desc->addr = buffers[at].addr;
		desc->len = buffers[at].len;
		desc->flags = (uint16_t)((at < readable ? 0 : STELA_VIRTIO_DESC_F_WRITE) |
		    (at + 1 < count ? STELA_VIRTIO_DESC_F_NEXT : 0));
		if (at + 1 == count) {
			vq->free_head = next;
		} else {
			index = next;
		}
	}
	vq->free_count = (uint16_t)(vq->free_count - count);
	vq->tokens[head] = token;
	vq->lengths[head] = (uint16_t)count;
	vq->avail[2 + vq->avail_idx % vq->size] = head;
	vq->avail_idx++;
	return head;
}

/* Publishes the chains added since: the device may take them from now on. */
static inline void
stela_vq_publish(struct stela_vq *vq)
{
	__atomic_store_n(&vq->avail[1], vq->avail_idx, __ATOMIC_RELEASE);
}

/*
 * Whether the device asked to hear of the chains published since the last
 * call: the caller then writes the queue's notify register.
 */
static inline int
stela_vq_kick_needed(struct stela_vq *vq)
{
	/* The index's store before the event's load, as the device fences too. */
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	uint16_t old = vq->kicked;
	uint16_t now = vq->avail_idx;
	vq->kicked = now;
	if (now == old) {
		return 0;
	}
	if (vq->event_idx) {
		return stela_vq_need_event(__atomic_load_n(stela_vq_avail_event(vq), __ATOMIC_ACQUIRE), now, old);
	}
	return !(__atomic_load_n(stela_vq_used_flags(vq), __ATOMIC_ACQUIRE) & STELA_VIRTIO_USED_F_NO_NOTIFY);
}

/* Whether the device has used chains the driver has not harvested. */
static inline int
stela_vq_pending(struct stela_vq *vq)
{
	return __atomic_load_n(stela_vq_used_idx(vq), __ATOMIC_ACQUIRE) != vq->last_used;
}

/*
 * Takes the next chain the device used: its token and the bytes the device
 * wrote, its descriptors going back to the free list.
 */
static inline int
stela_vq_harvest(struct stela_vq *vq, void **token, uint32_t *written)
{
	if (!stela_vq_pending(vq)) {
		return STELA_VQ_NONE;
	}
	volatile struct stela_vq_used_elem *elem = stela_vq_used_elem(vq, vq->last_used);
	uint32_t id = elem->id;
	uint32_t length = elem->len;
	if (id >= vq->size || vq->tokens[id] == 0) {
		return STELA_VQ_BROKEN;
	}
	uint16_t count = vq->lengths[id];
	uint16_t tail = (uint16_t)id;
	for (uint16_t at = 1; at < count; at++) {
		tail = vq->desc[tail].next;
	}
	vq->desc[tail].next = vq->free_head;
	vq->free_head = (uint16_t)id;
	vq->free_count = (uint16_t)(vq->free_count + count);
	*token = vq->tokens[id];
	*written = length;
	vq->tokens[id] = 0;
	vq->lengths[id] = 0;
	vq->last_used++;
	return STELA_VQ_USED;
}

/*
 * Asks for an interrupt at the next used chain. The device may have used
 * one before it could see this: the caller looks again (stela_vq_pending)
 * after arming, and harvests on.
 */
static inline void
stela_vq_arm(struct stela_vq *vq)
{
	if (vq->event_idx) {
		vq->avail[2 + vq->size] = vq->last_used;
	} else {
		vq->avail[0] = 0;
	}
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
}

/* Asks for no interrupt until the next stela_vq_arm. */
static inline void
stela_vq_disarm(struct stela_vq *vq)
{
	if (vq->event_idx) {
		vq->avail[2 + vq->size] = (uint16_t)(vq->last_used + 0x8000);
	} else {
		vq->avail[0] = STELA_VIRTIO_AVAIL_F_NO_INTERRUPT;
	}
}

#ifdef __cplusplus
}
#endif
