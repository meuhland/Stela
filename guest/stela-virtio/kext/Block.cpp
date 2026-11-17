#include "Block.h"

#include <IOKit/IOLib.h>
#include <stela/virtio.h>
#include <stela/virtio_blk.h>

OSDefineMetaClassAndStructors(StelaVirtIOBlock, IOBlockStorageDevice);

/* The wired page: each slot's header and status, then GET_ID's answer. */
static const uint32_t kPage = 0x4000;
static const uint32_t kSlotStride = 64;
static const uint32_t kStatusOffset = STELA_VIRTIO_BLK_HEADER_BYTES;
static const uint32_t kIdOffset = 0x1000;
/* The request queue (virtio 1.2, 5.2.2). */
static const uint16_t kRequestQueue = 0;
/* What a status byte reads before the device writes it. */
static const uint8_t kStatusPending = 0xff;

/* The strings the layers above name the disk by [mem: IOBlockStorageDriver
 * names its IOMedia "<vendor> <product> Media"]. */
static char kVendor[] = "Stela";
static char kProduct[] = "VirtIO Block";
static char kRevision[] = "1.0";

struct BlockHeader {
	uint32_t type;
	uint32_t reserved;
	uint64_t sector;
};

static_assert(sizeof(BlockHeader) == STELA_VIRTIO_BLK_HEADER_BYTES, "the device's 16-byte header");
static_assert(StelaVirtIOBlock::kSlots * kSlotStride <= kIdOffset, "every slot's header and status before the ID");
static_assert(kIdOffset + STELA_VIRTIO_BLK_ID_BYTES <= kPage, "the ID in the page");
static_assert(StelaVirtIOBlock::kSlots * (StelaVirtIOBlock::kSlotSegments + 2) <= StelaVirtIOBlock::kQueueEntries,
    "every slot's chain fits the queue at once");
static_assert(StelaVirtIOBlock::kQueueEntries <= STELA_VQ_MAX, "the ring code takes the queue");

struct StelaVirtIOBlock::Request {
	Request *next;
	/* Retained and prepared; null for FLUSH and GET_ID. */
	IOMemoryDescriptor *buffer;
	IOStorageCompletion completion;
	/* STELA_VIRTIO_BLK_T_IN, _OUT, _FLUSH or _GET_ID. */
	uint32_t type;
	uint64_t block;
	/* The data's bytes; how many went to the device, and came back done. */
	uint64_t bytes;
	uint64_t issued;
	uint64_t done;
	/* Parts on the device, and whether a FLUSH or GET_ID went. */
	unsigned parts;
	bool sent;
	IOReturn status;
	/* A caller sleeps on it (doSynchronizeCache) rather than completing. */
	bool waiter;
	bool finished;
};

/* Whether `request` has nothing more to send and nothing on the device. */
static bool
settled(const StelaVirtIOBlock::Request *request)
{
	if (request->parts != 0) {
		return false;
	}
	if (request->status != kIOReturnSuccess) {
		return true;
	}
	return request->buffer ? request->issued == request->bytes : request->sent;
}

bool
StelaVirtIOBlock::start(IOService *provider)
{
	if (!IOBlockStorageDevice::start(provider)) {
		return false;
	}
	nub = OSDynamicCast(StelaVirtIODevice, provider);
	if (!nub) {
		IOBlockStorageDevice::stop(provider);
		return false;
	}
	features = nub->negotiate(STELA_VIRTIO_F_VERSION_1 | STELA_VIRTIO_F_EVENT_IDX | STELA_VIRTIO_BLK_F_SIZE_MAX |
	    STELA_VIRTIO_BLK_F_SEG_MAX | STELA_VIRTIO_BLK_F_BLK_SIZE | STELA_VIRTIO_BLK_F_FLUSH | STELA_VIRTIO_BLK_F_RO);
	if (features == 0) {
		IOBlockStorageDevice::stop(provider);
		return false;
	}
	page = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage);
	if (page && page->prepare() != kIOReturnSuccess) {
		OSSafeReleaseNULL(page);
	}
	IOByteCount bytes = 0;
	pagePhysical = page ? page->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone) : 0;
	IOWorkLoop *workLoop = getWorkLoop();
	gate = workLoop ? IOCommandGate::commandGate(this) : nullptr;
	if (pagePhysical == 0 || bytes < kPage || !gate || workLoop->addEventSource(gate) != kIOReturnSuccess) {
		IOLog("StelaVirtIOBlock: cannot allocate its headers or its gate\n");
		OSSafeReleaseNULL(gate);
		nub->fail();
		stop(provider);
		return false;
	}
	bzero(page->getBytesNoCopy(), kPage);
	if (!readConfiguration() || !nub->createQueue(kRequestQueue, kQueueEntries, features, &queue)) {
		nub->fail();
		stop(provider);
		return false;
	}
	// Every slot's chain fits at once in the queue the device took.
	unsigned perSlot = queue.vq.size / kSlots;
	if (perSlot < 3) {
		IOLog("StelaVirtIOBlock: a queue of %u entries is too small\n", unsigned(queue.vq.size));
		nub->fail();
		stop(provider);
		return false;
	}
	if (segmentsMost > perSlot - 2) {
		segmentsMost = perSlot - 2;
	}
	nub->setHandler(this, &StelaVirtIOBlock::interruptOccurred);
	nub->driverOK();
	workLoop->runAction(&StelaVirtIOBlock::begin, this);
	publishProperties();
	publishCounters();
	IOLog("StelaVirtIOBlock: %llu blocks of %u bytes (%llu MiB)%s, %u slots of %u segments, features %llx\n", blocks,
	    blockSize, (blocks * blockSize) >> 20, readOnly ? ", read-only" : "", kSlots, segmentsMost, features);
	registerService();
	return true;
}

/* The device's configuration: capacity, block size and segment limits. */
bool
StelaVirtIOBlock::readConfiguration()
{
	uint64_t sectors = nub->readConfig(STELA_VIRTIO_BLK_CONFIG_CAPACITY, 8);
	blockSize = 512;
	if (features & STELA_VIRTIO_BLK_F_BLK_SIZE) {
		blockSize = uint32_t(nub->readConfig(STELA_VIRTIO_BLK_CONFIG_BLK_SIZE, 4));
	}
	if (blockSize < 512 || blockSize > 0x10000 || (blockSize & (blockSize - 1)) != 0) {
		IOLog("StelaVirtIOBlock: a block size of %u bytes\n", blockSize);
		return false;
	}
	blocks = sectors / (blockSize / STELA_VIRTIO_BLK_SECTOR_BYTES);
	if (blocks == 0) {
		IOLog("StelaVirtIOBlock: the device has no blocks (%llu sectors)\n", sectors);
		return false;
	}
	segmentsMost = kSlotSegments;
	if (features & STELA_VIRTIO_BLK_F_SEG_MAX) {
		uint64_t most = nub->readConfig(STELA_VIRTIO_BLK_CONFIG_SEG_MAX, 4);
		if (most != 0 && most < segmentsMost) {
			segmentsMost = uint32_t(most);
		}
	}
	segmentBytesMost = kPartBytes;
	if (features & STELA_VIRTIO_BLK_F_SIZE_MAX) {
		uint64_t most = nub->readConfig(STELA_VIRTIO_BLK_CONFIG_SIZE_MAX, 4);
		if (most != 0 && most < segmentBytesMost) {
			segmentBytesMost = most;
		}
	}
	readOnly = (features & STELA_VIRTIO_BLK_F_RO) != 0;
	flushes = (features & STELA_VIRTIO_BLK_F_FLUSH) != 0;
	return true;
}

void
StelaVirtIOBlock::publishProperties()
{
	OSDictionary *protocol = OSDictionary::withCapacity(2);
	OSString *interconnect = OSString::withCString(kIOPropertyInterconnectVirtualKey);
	OSString *location = OSString::withCString(kIOPropertyExternalKey);
	if (protocol && interconnect && location) {
		protocol->setObject(kIOPropertyPhysicalInterconnectTypeKey, interconnect);
		protocol->setObject(kIOPropertyPhysicalInterconnectLocationKey, location);
		setProperty(kIOPropertyProtocolCharacteristicsKey, protocol);
	}
	OSSafeReleaseNULL(protocol);
	OSSafeReleaseNULL(interconnect);
	OSSafeReleaseNULL(location);
	OSDictionary *device = OSDictionary::withCapacity(4);
	if (device) {
		const char *values[][2] = {
		    {kIOPropertyVendorNameKey, kVendor},
		    {kIOPropertyProductNameKey, kProduct},
		    {kIOPropertyProductRevisionLevelKey, kRevision},
		    {kIOPropertyProductSerialNumberKey, serial},
		};
		for (auto &value : values) {
			OSString *string = OSString::withCString(value[1]);
			if (string) {
				device->setObject(value[0], string);
			}
			OSSafeReleaseNULL(string);
		}
		setProperty(kIOPropertyDeviceCharacteristicsKey, device);
	}
	OSSafeReleaseNULL(device);
	setProperty("VirtIOBlockSize", blockSize, 32);
	setProperty("VirtIOBlocks", blocks, 64);
	setProperty("VirtIOReadOnly", readOnly);
	setProperty("VirtIOFeatures", features, 64);
}

void
StelaVirtIOBlock::publishCounters()
{
	setProperty("Reads", reads, 64);
	setProperty("Writes", writes, 64);
	setProperty("Flushes", flushCount, 64);
	setProperty("BytesRead", bytesRead, 64);
	setProperty("BytesWritten", bytesWritten, 64);
	setProperty("Errors", errors, 64);
}

uint8_t *
StelaVirtIOBlock::slotHeader(unsigned index) const
{
	return static_cast<uint8_t *>(page->getBytesNoCopy()) + index * kSlotStride;
}

uint64_t
StelaVirtIOBlock::slotPhysical(unsigned index) const
{
	return pagePhysical + uint64_t(index) * kSlotStride;
}

/* On the work loop: the queue armed, then the serial asked for. */
IOReturn
StelaVirtIOBlock::begin(OSObject *target, void *, void *, void *, void *)
{
	auto block = static_cast<StelaVirtIOBlock *>(target);
	block->running = true;
	stela_vq_arm(&block->queue.vq);
	auto request = static_cast<Request *>(IOMallocZero(sizeof(Request)));
	if (request) {
		request->type = STELA_VIRTIO_BLK_T_GET_ID;
		request->status = kIOReturnSuccess;
		block->append(request);
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::halt(OSObject *target, void *, void *, void *, void *)
{
	auto block = static_cast<StelaVirtIOBlock *>(target);
	block->running = false;
	block->abortAll(kIOReturnNoDevice);
	return kIOReturnSuccess;
}

void
StelaVirtIOBlock::stop(IOService *provider)
{
	IOWorkLoop *workLoop = getWorkLoop();
	if (nub) {
		nub->setHandler(nullptr, nullptr);
		// Once the reset reads back the device uses no slot and no buffer.
		nub->reset();
	}
	if (workLoop && gate) {
		workLoop->runAction(&StelaVirtIOBlock::halt, this);
	}
	if (gate) {
		workLoop->removeEventSource(gate);
		OSSafeReleaseNULL(gate);
	}
	IOBlockStorageDevice::stop(provider);
}

void
StelaVirtIOBlock::free()
{
	// The device was reset in stop: it uses neither the queue nor the page.
	if (queue.memory) {
		queue.memory->complete();
		OSSafeReleaseNULL(queue.memory);
	}
	if (page) {
		page->complete();
		OSSafeReleaseNULL(page);
	}
	IOBlockStorageDevice::free();
}

/* The device's interrupt, on the work loop with the gate held. */
void
StelaVirtIOBlock::interruptOccurred(OSObject *owner, uint32_t status)
{
	auto block = static_cast<StelaVirtIOBlock *>(owner);
	if (!block->running) {
		return;
	}
	if (status & STELA_VIRTIO_MMIO_INT_VRING) {
		block->harvest();
	}
	if (status & STELA_VIRTIO_MMIO_INT_CONFIG) {
		// The device's configuration never changes; this is its stop on an
		// error (DEVICE_NEEDS_RESET), which the transport logged. Nothing
		// on it completes now.
		IOLog("StelaVirtIOBlock: the device stopped; failing its requests\n");
		block->running = false;
		block->nub->fail();
		block->abortAll(kIOReturnIOError);
	}
	block->publishCounters();
}

void
StelaVirtIOBlock::append(Request *request)
{
	request->next = nullptr;
	if (tail) {
		tail->next = request;
	} else {
		head = request;
	}
	tail = request;
	issue();
}

void
StelaVirtIOBlock::issue()
{
	bool added = false;
	// Nothing goes to a device that stopped; what waits fails below.
	bool full = !running;
	for (Request *request = head; request && !full; request = request->next) {
		if (request->type == STELA_VIRTIO_BLK_T_FLUSH && !request->sent) {
			// A barrier: it goes once every request before it is done, and
			// none after it goes before it.
			bool busy = request != head;
			for (const Slot &slot : slots) {
				busy = busy || slot.busy;
			}
			if (busy) {
				break;
			}
		}
		while (request->status == kIOReturnSuccess &&
		    (request->buffer ? request->issued < request->bytes : !request->sent)) {
			unsigned index = 0;
			while (index < kSlots && slots[index].busy) {
				index++;
			}
			if (index == kSlots || !post(index, request)) {
				full = request->status == kIOReturnSuccess;
				break;
			}
			added = true;
		}
		if (request->type == STELA_VIRTIO_BLK_T_FLUSH && request->parts != 0) {
			// Nothing after a flush goes before it completes.
			break;
		}
	}
	if (added) {
		nub->kick(&queue);
	}
	// A request that failed before any part went is done now; finishing one
	// may start others (its completion may queue more), so look again.
	for (;;) {
		Request *done = head;
		while (done && !(done->status != kIOReturnSuccess && settled(done))) {
			done = done->next;
		}
		if (!done) {
			break;
		}
		finish(done);
	}
}

bool
StelaVirtIOBlock::post(unsigned index, Request *request)
{
	struct stela_vq_buf chain[kSlotSegments + 2];
	unsigned count = 0;
	uint64_t offset = request->issued;
	uint64_t bytes = 0;
	bool write = request->type == STELA_VIRTIO_BLK_T_OUT;
	chain[count++] = {slotPhysical(index), uint32_t(STELA_VIRTIO_BLK_HEADER_BYTES)};
	if (request->buffer) {
		uint64_t want = request->bytes - offset;
		if (want > kPartBytes) {
			want = kPartBytes;
		}
		while (bytes < want && count - 1 < segmentsMost) {
			IOByteCount length = 0;
			addr64_t physical = request->buffer->getPhysicalSegment(offset + bytes, &length, kIOMemoryMapperNone);
			if (physical == 0 || length == 0) {
				IOLog("StelaVirtIOBlock: no physical segment at %llu of a request\n", offset + bytes);
				request->status = kIOReturnDMAError;
				return false;
			}
			if (length > want - bytes) {
				length = want - bytes;
			}
			if (length > segmentBytesMost) {
				length = segmentBytesMost;
			}
			chain[count++] = {physical, uint32_t(length)};
			bytes += length;
		}
		// Whole blocks only: the segments' limit may have cut one.
		uint64_t whole = bytes - bytes % blockSize;
		if (whole == 0) {
			IOLog("StelaVirtIOBlock: a block spans more than %u segments\n", segmentsMost);
			request->status = kIOReturnDMAError;
			return false;
		}
		while (bytes > whole) {
			uint64_t extra = bytes - whole;
			if (chain[count - 1].len > extra) {
				chain[count - 1].len -= uint32_t(extra);
				bytes = whole;
			} else {
				bytes -= chain[count - 1].len;
				count--;
			}
		}
	} else if (request->type == STELA_VIRTIO_BLK_T_GET_ID) {
		chain[count++] = {pagePhysical + kIdOffset, uint32_t(STELA_VIRTIO_BLK_ID_BYTES)};
	}
	chain[count++] = {slotPhysical(index) + kStatusOffset, 1};
	uint8_t *place = slotHeader(index);
	BlockHeader header = {request->type, 0,
	    request->buffer ? (request->block * blockSize + offset) / STELA_VIRTIO_BLK_SECTOR_BYTES : 0};
	bcopy(&header, place, sizeof header);
	place[kStatusOffset] = kStatusPending;
	// The device reads the header and a write's data, and writes a read's
	// data, GET_ID's answer and the status.
	unsigned readable = write ? count - 1 : 1;
	if (stela_vq_add(&queue.vq, chain, readable, count - readable, &tokens[index]) < 0) {
		return false;
	}
	slots[index] = {request, offset, bytes, true};
	request->issued += bytes;
	request->parts++;
	request->sent = true;
	return true;
}

void
StelaVirtIOBlock::harvest()
{
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&queue.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			stela_vq_arm(&queue.vq);
			if (!stela_vq_pending(&queue.vq)) {
				break;
			}
			continue;
		}
		unsigned index = unsigned(static_cast<uint8_t *>(token) - tokens);
		if (found == STELA_VQ_BROKEN || index >= kSlots || !slots[index].busy) {
			IOLog("StelaVirtIOBlock: the device used a request it was not given\n");
			running = false;
			nub->fail();
			abortAll(kIOReturnIOError);
			return;
		}
		Slot &slot = slots[index];
		Request *request = slot.request;
		uint8_t status = slotHeader(index)[kStatusOffset];
		slot.busy = false;
		request->parts--;
		if (status == STELA_VIRTIO_BLK_S_OK) {
			request->done += slot.bytes;
		} else if (request->status == kIOReturnSuccess) {
			request->status = status == STELA_VIRTIO_BLK_S_UNSUPP ? kIOReturnUnsupported : kIOReturnIOError;
		}
	}
	issue();
	// Each settled request completes, oldest first. A completion may queue
	// more requests (the layers above continue a request they cut up), so
	// look from the head again after each.
	for (;;) {
		Request *done = head;
		while (done && !settled(done)) {
			done = done->next;
		}
		if (!done) {
			break;
		}
		finish(done);
	}
}

void
StelaVirtIOBlock::finish(Request *request)
{
	Request **link = &head;
	Request *previous = nullptr;
	while (*link && *link != request) {
		previous = *link;
		link = &(*link)->next;
	}
	if (!*link) {
		return;
	}
	*link = request->next;
	if (tail == request) {
		tail = previous;
	}
	request->next = nullptr;
	bool ok = request->status == kIOReturnSuccess;
	switch (request->type) {
	case STELA_VIRTIO_BLK_T_IN:
		reads += ok;
		bytesRead += request->done;
		break;
	case STELA_VIRTIO_BLK_T_OUT:
		writes += ok;
		bytesWritten += request->done;
		break;
	case STELA_VIRTIO_BLK_T_FLUSH:
		flushCount += ok;
		break;
	default:
		break;
	}
	errors += !ok;
	if (request->waiter) {
		request->finished = true;
		gate->commandWakeup(request, false);
		return;
	}
	if (request->type == STELA_VIRTIO_BLK_T_GET_ID) {
		if (ok) {
			const char *id = reinterpret_cast<const char *>(slotHeader(0)) + kIdOffset;
			unsigned length = 0;
			while (length < STELA_VIRTIO_BLK_ID_BYTES && id[length] != 0 && length + 1 < sizeof serial) {
				serial[length] = id[length];
				length++;
			}
			serial[length] = 0;
			publishProperties();
			IOLog("StelaVirtIOBlock: serial %s\n", serial);
		}
		IOFree(request, sizeof(Request));
		return;
	}
	IOStorageCompletion completion = request->completion;
	IOReturn status = request->status;
	uint64_t done = ok ? request->done : 0;
	if (request->buffer) {
		request->buffer->complete();
		request->buffer->release();
	}
	IOFree(request, sizeof(Request));
	IOStorage::complete(&completion, status, done);
}

void
StelaVirtIOBlock::abortAll(IOReturn status)
{
	for (Slot &slot : slots) {
		if (slot.busy) {
			slot.request->parts--;
			slot.busy = false;
		}
	}
	while (head) {
		if (head->status == kIOReturnSuccess) {
			head->status = status;
		}
		head->parts = 0;
		finish(head);
	}
}

IOReturn
StelaVirtIOBlock::submitGated(OSObject *owner, void *argument, void *, void *, void *)
{
	auto block = static_cast<StelaVirtIOBlock *>(owner);
	auto request = static_cast<Request *>(argument);
	if (!block->running) {
		request->status = kIOReturnOffline;
	}
	block->append(request);
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::waitGated(OSObject *owner, void *argument, void *, void *, void *)
{
	auto block = static_cast<StelaVirtIOBlock *>(owner);
	auto request = static_cast<Request *>(argument);
	if (!block->running) {
		return kIOReturnOffline;
	}
	block->append(request);
	while (!request->finished) {
		block->gate->commandSleep(request, THREAD_UNINT);
	}
	return request->status;
}

IOReturn
StelaVirtIOBlock::doAsyncReadWrite(IOMemoryDescriptor *buffer, UInt64 block, UInt64 nblks, IOStorageAttributes *,
    IOStorageCompletion *completion)
{
	if (!buffer || !completion) {
		return kIOReturnBadArgument;
	}
	bool write = buffer->getDirection() == kIODirectionOut;
	IOReturn refused = kIOReturnSuccess;
	if (!gate) {
		refused = kIOReturnOffline;
	} else if (nblks == 0 || block >= blocks || nblks > blocks - block ||
	    buffer->getLength() < nblks * blockSize) {
		refused = kIOReturnBadArgument;
	} else if (write && readOnly) {
		refused = kIOReturnNotWritable;
	} else if (buffer->prepare() != kIOReturnSuccess) {
		refused = kIOReturnVMError;
	}
	auto request = refused == kIOReturnSuccess ? static_cast<Request *>(IOMallocZero(sizeof(Request))) : nullptr;
	if (refused == kIOReturnSuccess && !request) {
		buffer->complete();
		refused = kIOReturnNoMemory;
	}
	if (refused != kIOReturnSuccess) {
		IOStorage::complete(completion, refused, 0);
		return kIOReturnSuccess;
	}
	buffer->retain();
	request->buffer = buffer;
	request->completion = *completion;
	request->type = write ? STELA_VIRTIO_BLK_T_OUT : STELA_VIRTIO_BLK_T_IN;
	request->block = block;
	request->bytes = nblks * blockSize;
	request->status = kIOReturnSuccess;
	gate->runAction(&StelaVirtIOBlock::submitGated, request);
	return kIOReturnSuccess;
}

/* FLUSH, the caller waiting: never on the work loop, which completes it. */
IOReturn
StelaVirtIOBlock::command(uint32_t type)
{
	IOWorkLoop *workLoop = getWorkLoop();
	if (!gate || !workLoop) {
		return kIOReturnOffline;
	}
	if (workLoop->onThread()) {
		return kIOReturnNotPermitted;
	}
	Request request = {};
	request.type = type;
	request.status = kIOReturnSuccess;
	request.waiter = true;
	return gate->runAction(&StelaVirtIOBlock::waitGated, &request);
}

IOReturn
StelaVirtIOBlock::doSynchronizeCache(void)
{
	// Without FLUSH the device's writes are durable once completed.
	return flushes ? command(STELA_VIRTIO_BLK_T_FLUSH) : kIOReturnSuccess;
}

/* The whole cache: virtio-blk's FLUSH has no range [mem: IOBlockStorageDriver
 * calls this, which some drivers implement instead of doSynchronizeCache]. */
IOReturn
StelaVirtIOBlock::doSynchronize(UInt64, UInt64, IOStorageSynchronizeOptions)
{
	return doSynchronizeCache();
}

IOReturn
StelaVirtIOBlock::doEjectMedia(void)
{
	return kIOReturnUnsupported;
}

IOReturn
StelaVirtIOBlock::doFormatMedia(UInt64)
{
	return kIOReturnUnsupported;
}

UInt32
StelaVirtIOBlock::doGetFormatCapacities(UInt64 *capacities, UInt32 capacitiesMaxCount) const
{
	if (!capacities || capacitiesMaxCount == 0) {
		return 0;
	}
	capacities[0] = blocks * blockSize;
	return 1;
}

IOReturn
StelaVirtIOBlock::doLockUnlockMedia(bool)
{
	return kIOReturnUnsupported;
}

char *
StelaVirtIOBlock::getVendorString(void)
{
	return kVendor;
}

char *
StelaVirtIOBlock::getProductString(void)
{
	return kProduct;
}

char *
StelaVirtIOBlock::getRevisionString(void)
{
	return kRevision;
}

/* The device's serial from GET_ID, empty until it answers. */
char *
StelaVirtIOBlock::getAdditionalDeviceInfoString(void)
{
	return serial;
}

IOReturn
StelaVirtIOBlock::reportBlockSize(UInt64 *blockSizeOut)
{
	if (!blockSizeOut) {
		return kIOReturnBadArgument;
	}
	*blockSizeOut = blockSize;
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportEjectability(bool *isEjectable)
{
	if (!isEjectable) {
		return kIOReturnBadArgument;
	}
	*isEjectable = false;
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportLockability(bool *isLockable)
{
	if (!isLockable) {
		return kIOReturnBadArgument;
	}
	*isLockable = false;
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportMaxValidBlock(UInt64 *maxBlock)
{
	if (!maxBlock) {
		return kIOReturnBadArgument;
	}
	*maxBlock = blocks - 1;
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportMediaState(bool *mediaPresent, bool *changedState)
{
	if (!mediaPresent) {
		return kIOReturnBadArgument;
	}
	*mediaPresent = blocks != 0;
	if (changedState) {
		*changedState = false;
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportPollRequirements(bool *pollRequired, bool *pollIsExpensive)
{
	if (pollRequired) {
		*pollRequired = false;
	}
	if (pollIsExpensive) {
		*pollIsExpensive = false;
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportRemovability(bool *isRemovable)
{
	if (!isRemovable) {
		return kIOReturnBadArgument;
	}
	*isRemovable = false;
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOBlock::reportWriteProtection(bool *isWriteProtected)
{
	if (!isWriteProtected) {
		return kIOReturnBadArgument;
	}
	*isWriteProtected = readOnly;
	return kIOReturnSuccess;
}

/* With FLUSH the device caches writes until flushed (virtio 1.2, 5.2.5). */
IOReturn
StelaVirtIOBlock::getWriteCacheState(bool *enabled)
{
	if (!enabled) {
		return kIOReturnBadArgument;
	}
	*enabled = flushes;
	return kIOReturnSuccess;
}

/* The mode is the device's: without CONFIG_WCE it cannot change. */
IOReturn
StelaVirtIOBlock::setWriteCacheState(bool enabled)
{
	return enabled == flushes ? kIOReturnSuccess : kIOReturnUnsupported;
}
