/*
 * Host tests of stela_vq.h, which the kext compiles unchanged, against a
 * device written here from the specification. Built with -I to a directory
 * holding the generated stela/virtio.h.
 *
 * With `image FILE` it lays out the loopback queue (below) with its chains
 * published and writes its three areas to FILE; with `harvest FILE` it lays
 * out the same queue, takes FILE as the areas after a device used the
 * chains, and harvests them. xtask's loopback test runs the device queue
 * of stela_soc::virtio between the two
 * (`guest::virtio_tests::the_kext_ring_and_the_device_queue_agree`).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../stela_vq.h"

static int failures;

#define CHECK(condition)                                                       \
	do {                                                                   \
		if (!(condition)) {                                            \
			printf("%s:%d: %s\n", __FILE__, __LINE__, #condition); \
			failures++;                                            \
		}                                                              \
	} while (0)

/* The areas of a queue of up to STELA_VQ_MAX entries, apart as a device sees them. */
struct areas {
	_Alignas(16) uint8_t desc[16 * STELA_VQ_MAX];
	_Alignas(16) uint8_t avail[6 + 2 * STELA_VQ_MAX];
	_Alignas(16) uint8_t used[6 + 8 * STELA_VQ_MAX];
};

/* The device's side, from virtio 1.2 2.7: what it last took and used. */
struct device {
	uint16_t last_avail;
	uint16_t used;
};

static uint16_t
load16(const void *at)
{
	uint16_t value;
	memcpy(&value, at, 2);
	return value;
}

static void
store16(void *at, uint16_t value)
{
	memcpy(at, &value, 2);
}

/* Takes the next available chain and uses it, `written` bytes; its head, or -1. */
static int
device_use(struct areas *areas, uint16_t size, struct device *device, uint32_t written, unsigned *buffers)
{
	uint16_t index = load16(areas->avail + 2);
	if (index == device->last_avail) {
		return -1;
	}
	uint16_t head = load16(areas->avail + 4 + 2 * (device->last_avail % size));
	device->last_avail++;
	unsigned count = 0;
	uint16_t at = head;
	for (;;) {
		struct stela_vq_desc desc;
		memcpy(&desc, areas->desc + 16 * at, sizeof desc);
		count++;
		if (!(desc.flags & STELA_VIRTIO_DESC_F_NEXT)) {
			break;
		}
		at = desc.next;
	}
	*buffers = count;
	uint32_t elem[2] = {head, written};
	memcpy(areas->used + 4 + 8 * (device->used % size), elem, 8);
	device->used++;
	store16(areas->used + 2, device->used);
	return head;
}

static void
lifecycle(void)
{
	static struct areas areas;
	struct stela_vq vq;
	CHECK(stela_vq_init(&vq, 0, areas.desc, areas.avail, areas.used, 0) == -1);
	CHECK(stela_vq_init(&vq, 6, areas.desc, areas.avail, areas.used, 0) == -1);
	CHECK(stela_vq_init(&vq, 512, areas.desc, areas.avail, areas.used, 0) == -1);
	CHECK(stela_vq_init(&vq, 8, areas.desc + 8, areas.avail, areas.used, 0) == -1);
	CHECK(stela_vq_init(&vq, 8, areas.desc, areas.avail + 1, areas.used, 0) == -1);
	CHECK(stela_vq_init(&vq, 8, areas.desc, areas.avail, areas.used + 2, 0) == -1);
	CHECK(stela_vq_init(&vq, 8, areas.desc, areas.avail, areas.used, 0) == 0);
	CHECK(vq.free_count == 8);

	int tokens[8];
	struct stela_vq_buf chain[3] = {{0x1000, 16}, {0x2000, 64}, {0x3000, 8}};
	/* Nothing to add, no token, or more than the queue holds. */
	CHECK(stela_vq_add(&vq, chain, 0, 0, &tokens[0]) == -1);
	CHECK(stela_vq_add(&vq, chain, 1, 0, 0) == -1);
	CHECK(stela_vq_add(&vq, chain, 5, 4, &tokens[0]) == -1);
	/* A readable buffer then two writable ones. */
	int head = stela_vq_add(&vq, chain, 1, 2, &tokens[0]);
	CHECK(head == 0);
	CHECK(vq.free_count == 5);
	struct stela_vq_desc desc[3];
	memcpy(desc, areas.desc, sizeof desc);
	CHECK(desc[0].addr == 0x1000 && desc[0].len == 16 && desc[0].flags == STELA_VIRTIO_DESC_F_NEXT);
	CHECK(desc[1].flags == (STELA_VIRTIO_DESC_F_NEXT | STELA_VIRTIO_DESC_F_WRITE) && desc[0].next == 1);
	CHECK(desc[2].flags == STELA_VIRTIO_DESC_F_WRITE && desc[2].len == 8);
	CHECK(load16(areas.avail + 4) == 0);
	/* Unpublished: the device sees nothing yet. */
	CHECK(load16(areas.avail + 2) == 0);
	stela_vq_publish(&vq);
	CHECK(load16(areas.avail + 2) == 1);
	/* Without EVENT_IDX the used ring's flags decide. */
	CHECK(stela_vq_kick_needed(&vq) == 1);
	CHECK(stela_vq_kick_needed(&vq) == 0);
	CHECK(stela_vq_add(&vq, chain, 1, 0, &tokens[1]) == 3);
	stela_vq_publish(&vq);
	store16(areas.used, STELA_VIRTIO_USED_F_NO_NOTIFY);
	CHECK(stela_vq_kick_needed(&vq) == 0);
	store16(areas.used, 0);

	/* The device uses them in order; the driver harvests both. */
	struct device device = {0, 0};
	void *token = 0;
	uint32_t written = 0;
	unsigned buffers = 0;
	CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_NONE);
	CHECK(device_use(&areas, 8, &device, 40, &buffers) == 0 && buffers == 3);
	CHECK(device_use(&areas, 8, &device, 0, &buffers) == 3 && buffers == 1);
	CHECK(device_use(&areas, 8, &device, 0, &buffers) == -1);
	CHECK(stela_vq_pending(&vq));
	CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_USED);
	CHECK(token == &tokens[0] && written == 40);
	CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_USED);
	CHECK(token == &tokens[1] && written == 0);
	CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_NONE);
	CHECK(vq.free_count == 8);
	/* The whole queue is free again: eight one-buffer chains fit, a ninth not. */
	for (int at = 0; at < 8; at++) {
		CHECK(stela_vq_add(&vq, chain, 0, 1, &tokens[at]) >= 0);
	}
	CHECK(stela_vq_add(&vq, chain, 0, 1, &tokens[0]) == -1);
	CHECK(vq.free_count == 0);

	/* A used entry naming no chain the device has is a broken device. */
	uint32_t bogus[2] = {9, 0};
	memcpy(areas.used + 4 + 8 * (device.used % 8), bogus, 8);
	store16(areas.used + 2, (uint16_t)(device.used + 1));
	CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_BROKEN);
}

static void
interrupts_and_kicks(void)
{
	static struct areas areas;
	struct stela_vq vq;
	int token;
	struct stela_vq_buf buffer = {0x1000, 64};
	memset(&areas, 0, sizeof areas);
	CHECK(stela_vq_init(&vq, 4, areas.desc, areas.avail, areas.used, 1) == 0);
	/* EVENT_IDX: arming names the next used index, disarming one far away. */
	stela_vq_arm(&vq);
	CHECK(load16(areas.avail + 4 + 2 * 4) == 0);
	stela_vq_disarm(&vq);
	CHECK(load16(areas.avail + 4 + 2 * 4) == 0x8000);
	/* The device asks for a kick once the index passes avail_event. */
	store16(areas.used + 4 + 8 * 4, 1);
	stela_vq_add(&vq, &buffer, 0, 1, &token);
	stela_vq_publish(&vq);
	CHECK(stela_vq_kick_needed(&vq) == 0); /* 0 -> 1 does not pass 1 */
	stela_vq_add(&vq, &buffer, 0, 1, &token);
	stela_vq_publish(&vq);
	CHECK(stela_vq_kick_needed(&vq) == 1); /* 1 -> 2 passes it */
	/* Without EVENT_IDX the available ring's flags carry the driver's wish. */
	memset(&areas, 0, sizeof areas);
	CHECK(stela_vq_init(&vq, 4, areas.desc, areas.avail, areas.used, 0) == 0);
	stela_vq_disarm(&vq);
	CHECK(load16(areas.avail) == STELA_VIRTIO_AVAIL_F_NO_INTERRUPT);
	stela_vq_arm(&vq);
	CHECK(load16(areas.avail) == 0);
}

static void
wraparound(void)
{
	/* More than 2^16 chains through a 4-entry queue, with the device's
	 * avail_event asking for every kick. */
	static struct areas areas;
	struct stela_vq vq;
	struct device device = {0, 0};
	int tokens[4];
	memset(&areas, 0, sizeof areas);
	CHECK(stela_vq_init(&vq, 4, areas.desc, areas.avail, areas.used, 1) == 0);
	struct stela_vq_buf buffer = {0x4000, 32};
	unsigned kicks = 0;
	for (unsigned round = 0; round < 70000; round++) {
		CHECK(stela_vq_add(&vq, &buffer, 0, 1, &tokens[round % 4]) >= 0);
		stela_vq_publish(&vq);
		/* The device wants to hear of the next chain. */
		store16(areas.used + 4 + 8 * 4, device.last_avail);
		kicks += stela_vq_kick_needed(&vq);
		unsigned buffers;
		CHECK(device_use(&areas, 4, &device, round & 0xff, &buffers) >= 0);
		void *token;
		uint32_t written;
		CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_USED);
		CHECK(token == &tokens[round % 4] && written == (round & 0xff));
	}
	CHECK(kicks == 70000);
	CHECK(vq.avail_idx == (uint16_t)70000 && vq.last_used == (uint16_t)70000);
	CHECK(vq.free_count == 4);
}

/* The loopback queue: 8 entries, its areas at these guest physical addresses. */
#define LOOP_SIZE 8
#define LOOP_DESC 0x80000000ull
#define LOOP_AVAIL 0x80001000ull
#define LOOP_USED 0x80002000ull
static int loop_tokens[3];

/* The loopback's chains: a request and its reply, a large reply, three requests. */
static int
loopback(struct stela_vq *vq, struct areas *areas)
{
	memset(areas, 0, sizeof *areas);
	if (stela_vq_init(vq, LOOP_SIZE, areas->desc, areas->avail, areas->used, 1) != 0) {
		return -1;
	}
	struct stela_vq_buf first[2] = {{0x80010000ull, 16}, {0x80010100ull, 64}};
	struct stela_vq_buf second[1] = {{0x80020000ull, 4096}};
	struct stela_vq_buf third[3] = {{0x80030000ull, 8}, {0x80030010ull, 8}, {0x80030020ull, 8}};
	if (stela_vq_add(vq, first, 1, 1, &loop_tokens[0]) != 0 || stela_vq_add(vq, second, 0, 1, &loop_tokens[1]) != 2 ||
	    stela_vq_add(vq, third, 3, 0, &loop_tokens[2]) != 3) {
		return -1;
	}
	stela_vq_disarm(vq);
	stela_vq_arm(vq);
	stela_vq_publish(vq);
	return 0;
}

/* The three areas as one image: each at its offset from LOOP_DESC. */
static int
image(const char *path)
{
	static struct areas areas;
	struct stela_vq vq;
	if (loopback(&vq, &areas) != 0) {
		return 1;
	}
	static uint8_t bytes[0x3000];
	memcpy(bytes, areas.desc, 16 * LOOP_SIZE);
	memcpy(bytes + (LOOP_AVAIL - LOOP_DESC), areas.avail, 6 + 2 * LOOP_SIZE);
	memcpy(bytes + (LOOP_USED - LOOP_DESC), areas.used, 6 + 8 * LOOP_SIZE);
	FILE *file = fopen(path, "wb");
	if (!file || fwrite(bytes, 1, sizeof bytes, file) != sizeof bytes) {
		return 1;
	}
	fclose(file);
	printf("ok\n");
	return 0;
}

/* Harvests what the device used: the chains in its order, with its lengths. */
static int
harvest(const char *path)
{
	static struct areas areas;
	struct stela_vq vq;
	if (loopback(&vq, &areas) != 0) {
		return 1;
	}
	static uint8_t bytes[0x3000];
	FILE *file = fopen(path, "rb");
	if (!file || fread(bytes, 1, sizeof bytes, file) != sizeof bytes) {
		return 1;
	}
	fclose(file);
	/* The device writes only the used ring; the rest must be as the driver left it. */
	CHECK(memcmp(bytes, areas.desc, 16 * LOOP_SIZE) == 0);
	CHECK(memcmp(bytes + (LOOP_AVAIL - LOOP_DESC), areas.avail, 4 + 2 * LOOP_SIZE) == 0);
	memcpy(areas.used, bytes + (LOOP_USED - LOOP_DESC), 6 + 8 * LOOP_SIZE);
	void *token;
	uint32_t written;
	/* The device used the second, first and third, as the Rust side does. */
	static const struct {
		int token;
		uint32_t written;
	} expected[3] = {{1, 4096}, {0, 64}, {2, 0}};
	for (int at = 0; at < 3; at++) {
		CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_USED);
		CHECK(token == &loop_tokens[expected[at].token] && written == expected[at].written);
	}
	CHECK(stela_vq_harvest(&vq, &token, &written) == STELA_VQ_NONE);
	CHECK(vq.free_count == LOOP_SIZE);
	/* The device asked for a kick at the next chain. */
	CHECK(load16(areas.used + 4 + 8 * LOOP_SIZE) == 3);
	printf("%s\n", failures ? "FAILED" : "ok");
	return failures != 0;
}

int
main(int argc, char **argv)
{
	if (argc == 3 && strcmp(argv[1], "image") == 0) {
		return image(argv[2]);
	}
	if (argc == 3 && strcmp(argv[1], "harvest") == 0) {
		return harvest(argv[2]);
	}
	lifecycle();
	interrupts_and_kicks();
	wraparound();
	printf("%s\n", failures ? "FAILED" : "ok");
	return failures != 0;
}
