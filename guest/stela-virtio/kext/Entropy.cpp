#include "StelaVirtIO.h"

#include <IOKit/IOLib.h>
#include <stela/virtio.h>

extern "C" {
#include <sys/random.h>
}

/*
 * StelaVirtIOEntropy: the virtio entropy device's driver (virtio 1.2, 5.4).
 * One request queue; the driver posts writable buffers and the device fills
 * them from the host's random source. Every completed buffer goes to the
 * kernel's pool through write_random. The first kInitialBytes come as fast
 * as the device answers; after that one buffer a second is posted, about
 * 64 bytes a second, as the pool reseeds from it and needs no stream.
 * BytesHarvested, Completions and Refused count what came, for the boot's
 * checks.
 */
OSDefineMetaClassAndStructors(StelaVirtIOEntropy, IOService);

/* Buffers the driver keeps, and their bytes. */
static const unsigned kSlots = 4;
static const uint32_t kSlotBytes = 64;
static const uint32_t kPage = 0x4000;
/* Bytes taken as fast as the device gives them, then one buffer per kPaceMS. */
static const uint64_t kInitialBytes = 512;
static const uint32_t kPaceMS = 1000;
/* Entries of the request queue: room for every slot. */
static const uint16_t kQueueEntries = 8;

static_assert(kSlots * kSlotBytes <= kPage, "every buffer is in the one page");
static_assert(kSlots <= kQueueEntries, "every buffer can be posted at once");

bool
StelaVirtIOEntropy::start(IOService *provider)
{
	if (!IOService::start(provider)) {
		return false;
	}
	nub = OSDynamicCast(StelaVirtIODevice, provider);
	if (!nub) {
		stop(provider);
		return false;
	}
	features = nub->negotiate(STELA_VIRTIO_F_VERSION_1 | STELA_VIRTIO_F_EVENT_IDX);
	if (features == 0) {
		stop(provider);
		return false;
	}
	buffers = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage);
	if (buffers && buffers->prepare() != kIOReturnSuccess) {
		OSSafeReleaseNULL(buffers);
	}
	IOByteCount bytes = 0;
	buffersPhysical = buffers ? buffers->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone) : 0;
	if (buffersPhysical == 0 || bytes < kPage) {
		IOLog("StelaVirtIOEntropy: cannot allocate its buffers\n");
		nub->fail();
		stop(provider);
		return false;
	}
	bzero(buffers->getBytesNoCopy(), kPage);
	if (!nub->createQueue(0, kQueueEntries, features, &queue)) {
		nub->fail();
		stop(provider);
		return false;
	}
	IOWorkLoop *workLoop = getWorkLoop();
	timer = workLoop ? IOTimerEventSource::timerEventSource(this, &StelaVirtIOEntropy::timerFired) : nullptr;
	if (!timer || workLoop->addEventSource(timer) != kIOReturnSuccess) {
		IOLog("StelaVirtIOEntropy: cannot make its timer\n");
		OSSafeReleaseNULL(timer);
		nub->fail();
		stop(provider);
		return false;
	}
	nub->setHandler(this, &StelaVirtIOEntropy::interruptOccurred);
	nub->driverOK();
	// Every buffer goes to the device at once, on the work loop as every
	// later post does.
	workLoop->runAction(&StelaVirtIOEntropy::postAll, this);
	publishCounters();
	IOLog("StelaVirtIOEntropy: %u buffers of %u bytes on a %u-entry queue, features %llx\n", kSlots,
	    kSlotBytes, unsigned(queue.vq.size), features);
	registerService();
	return true;
}

bool
StelaVirtIOEntropy::post(unsigned slot)
{
	struct stela_vq_buf buffer = {buffersPhysical + uint64_t(slot) * kSlotBytes, kSlotBytes};
	if (stela_vq_add(&queue.vq, &buffer, 0, 1, &slots[slot]) < 0) {
		return false;
	}
	posted[slot] = true;
	return true;
}

/* On the work loop. */
void
StelaVirtIOEntropy::harvest()
{
	if (!running) {
		return;
	}
	bool reposted = false;
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&queue.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			// Ask for the next completion, and look once more for one
			// the device made before it could see that.
			stela_vq_arm(&queue.vq);
			if (!stela_vq_pending(&queue.vq)) {
				break;
			}
			continue;
		}
		if (found == STELA_VQ_BROKEN) {
			IOLog("StelaVirtIOEntropy: the device used a buffer it was not given\n");
			running = false;
			nub->fail();
			break;
		}
		unsigned slot = unsigned(static_cast<uint8_t *>(token) - slots);
		if (slot >= kSlots) {
			IOLog("StelaVirtIOEntropy: a used buffer is no slot of the driver's\n");
			running = false;
			nub->fail();
			break;
		}
		posted[slot] = false;
		completions++;
		if (written > kSlotBytes) {
			written = kSlotBytes;
		}
		if (written > 0) {
			auto bytes = static_cast<uint8_t *>(buffers->getBytesNoCopy()) + slot * kSlotBytes;
			if (write_random(bytes, written) == 0) {
				bytesHarvested += written;
			} else {
				refused++;
			}
			bzero(bytes, kSlotBytes);
		}
		// Until the pool has its first bytes every buffer goes back at once.
		if (bytesHarvested < kInitialBytes) {
			reposted |= post(slot);
		}
	}
	if (reposted) {
		nub->kick(&queue);
	}
	if (bytesHarvested >= kInitialBytes && timer) {
		timer->setTimeoutMS(kPaceMS);
	}
	publishCounters();
}

/* On the work loop: one free buffer goes back to the device. */
void
StelaVirtIOEntropy::tick()
{
	if (!running) {
		return;
	}
	for (unsigned slot = 0; slot < kSlots; slot++) {
		if (!posted[slot]) {
			if (post(slot)) {
				nub->kick(&queue);
			}
			break;
		}
	}
}

void
StelaVirtIOEntropy::publishCounters()
{
	setProperty("BytesHarvested", bytesHarvested, 64);
	setProperty("Completions", completions, 64);
	setProperty("Refused", refused, 64);
}

void
StelaVirtIOEntropy::interruptOccurred(OSObject *owner, uint32_t status)
{
	if (status & STELA_VIRTIO_MMIO_INT_VRING) {
		static_cast<StelaVirtIOEntropy *>(owner)->harvest();
	}
}

void
StelaVirtIOEntropy::timerFired(OSObject *owner, IOTimerEventSource *)
{
	static_cast<StelaVirtIOEntropy *>(owner)->tick();
}

IOReturn
StelaVirtIOEntropy::postAll(OSObject *target, void *, void *, void *, void *)
{
	auto entropy = static_cast<StelaVirtIOEntropy *>(target);
	entropy->running = true;
	stela_vq_arm(&entropy->queue.vq);
	for (unsigned slot = 0; slot < kSlots; slot++) {
		entropy->post(slot);
	}
	entropy->nub->kick(&entropy->queue);
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOEntropy::halt(OSObject *target, void *, void *, void *, void *)
{
	static_cast<StelaVirtIOEntropy *>(target)->running = false;
	return kIOReturnSuccess;
}

void
StelaVirtIOEntropy::stop(IOService *provider)
{
	IOWorkLoop *workLoop = getWorkLoop();
	if (workLoop) {
		workLoop->runAction(&StelaVirtIOEntropy::halt, this);
	}
	if (nub) {
		nub->setHandler(nullptr, nullptr);
		// Once the reset reads back the device uses no buffer.
		nub->reset();
	}
	if (timer) {
		timer->cancelTimeout();
		workLoop->removeEventSource(timer);
		OSSafeReleaseNULL(timer);
	}
	IOService::stop(provider);
}

void
StelaVirtIOEntropy::free()
{
	// The device was reset in stop: it uses neither the queue nor a buffer.
	if (queue.memory) {
		queue.memory->complete();
		OSSafeReleaseNULL(queue.memory);
	}
	if (buffers) {
		buffers->complete();
		OSSafeReleaseNULL(buffers);
	}
	IOService::free();
}
