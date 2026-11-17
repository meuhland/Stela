#include "StelaVirtIO.h"

#include <IOKit/IOLib.h>
#include <stela/virtio.h>

OSDefineMetaClassAndStructors(StelaVirtIOMMIOTransport, IOService);
OSDefineMetaClassAndStructors(StelaVirtIODevice, IOService);

/* The guest's page: each queue's three areas share one. */
static const uint32_t kPage = 0x4000;
/* Where a queue's areas lie in its page. */
static const uint32_t kAvailOffset = 0x1000;
static const uint32_t kUsedOffset = 0x2000;
/* How long a reset may take to read back as done. */
static const unsigned kResetPolls = 1000;
/* How often a configuration read is tried while the configuration changes. */
static const unsigned kConfigReads = 8;

static_assert(STELA_VIRTIO_DESC_BYTES(STELA_VQ_MAX) <= kAvailOffset, "the descriptor table fits before the available ring");
static_assert(STELA_VIRTIO_AVAIL_BYTES(STELA_VQ_MAX) <= kUsedOffset - kAvailOffset, "the available ring fits before the used ring");
static_assert(STELA_VIRTIO_USED_BYTES(STELA_VQ_MAX) <= kPage - kUsedOffset, "the used ring fits the page");
static_assert(kAvailOffset % STELA_VIRTIO_AVAIL_ALIGN == 0 && kUsedOffset % STELA_VIRTIO_USED_ALIGN == 0,
    "each area is aligned as the device reads it");
static_assert(sizeof(struct stela_vq_desc) == STELA_VIRTIO_DESCRIPTOR_BYTES, "a descriptor is the device's 16 bytes");
static_assert(sizeof(struct stela_vq_used_elem) == 8, "a used element is the device's 8 bytes");

bool
StelaVirtIOMMIOTransport::start(IOService *provider)
{
	if (!IOService::start(provider)) {
		return false;
	}
	if (!mapRegisters(provider)) {
		stop(provider);
		return false;
	}
	device = read(STELA_VIRTIO_MMIO_DEVICE_ID);
	if (device == 0) {
		IOLog("StelaVirtIO: %s has no device\n", provider->getName());
		stop(provider);
		return false;
	}
	if (!reset()) {
		IOLog("StelaVirtIO: device %u did not reset\n", device);
		stop(provider);
		return false;
	}
	write(STELA_VIRTIO_MMIO_STATUS, STELA_VIRTIO_STATUS_ACKNOWLEDGE);
	workLoop = IOWorkLoop::workLoop();
	interrupt = workLoop ? IOInterruptEventSource::interruptEventSource(this,
	                           &StelaVirtIOMMIOTransport::interruptOccurred, provider, 0)
	                     : nullptr;
	if (!interrupt || workLoop->addEventSource(interrupt) != kIOReturnSuccess) {
		IOLog("StelaVirtIO: cannot take device %u's interrupt\n", device);
		OSSafeReleaseNULL(interrupt);
		stop(provider);
		return false;
	}
	interrupt->enable();
	setProperty("VirtIODeviceID", device, 32);
	setProperty("VirtIOVendorID", read(STELA_VIRTIO_MMIO_VENDOR_ID), 32);
	nub = StelaVirtIODevice::withTransport(this, device);
	if (!nub || !nub->attach(this)) {
		IOLog("StelaVirtIO: cannot publish device %u\n", device);
		OSSafeReleaseNULL(nub);
		stop(provider);
		return false;
	}
	IOLog("StelaVirtIO: %s: device %u, vendor %08x, version %u\n", provider->getName(), device,
	    read(STELA_VIRTIO_MMIO_VENDOR_ID), read(STELA_VIRTIO_MMIO_VERSION));
	registerService();
	nub->registerService();
	return true;
}

bool
StelaVirtIOMMIOTransport::mapRegisters(IOService *provider)
{
	IODeviceMemory *page = provider->getDeviceMemoryWithIndex(0);
	if (!page || page->getLength() != STELA_VIRTIO_MMIO_BYTES) {
		IOLog("StelaVirtIO: the register page is not %llu bytes\n", STELA_VIRTIO_MMIO_BYTES);
		return false;
	}
	registers = page->map(kIOMapInhibitCache);
	if (!registers) {
		IOLog("StelaVirtIO: cannot map the register page\n");
		return false;
	}
	uint32_t magic = read(STELA_VIRTIO_MMIO_MAGIC_VALUE);
	uint32_t version = read(STELA_VIRTIO_MMIO_VERSION);
	if (magic != STELA_VIRTIO_MMIO_MAGIC || version != STELA_VIRTIO_MMIO_VERSION_2) {
		IOLog("StelaVirtIO: not a virtio-mmio version 2 page (magic %08x, version %u)\n", magic, version);
		return false;
	}
	return true;
}

bool
StelaVirtIOMMIOTransport::reset()
{
	write(STELA_VIRTIO_MMIO_STATUS, 0);
	for (unsigned poll = 0; poll < kResetPolls; poll++) {
		if (read(STELA_VIRTIO_MMIO_STATUS) == 0) {
			return true;
		}
		IODelay(10);
	}
	return false;
}

void
StelaVirtIOMMIOTransport::stop(IOService *provider)
{
	if (nub) {
		nub->terminate();
		OSSafeReleaseNULL(nub);
	}
	if (registers) {
		// Once the reset reads back the device reaches no guest memory.
		reset();
	}
	if (interrupt) {
		interrupt->disable();
		workLoop->removeEventSource(interrupt);
		OSSafeReleaseNULL(interrupt);
	}
	IOService::stop(provider);
}

void
StelaVirtIOMMIOTransport::free()
{
	OSSafeReleaseNULL(workLoop);
	OSSafeReleaseNULL(registers);
	IOService::free();
}

IOWorkLoop *
StelaVirtIOMMIOTransport::getWorkLoop() const
{
	return workLoop;
}

namespace {
struct HandlerChange {
	OSObject *owner;
	StelaVirtIOHandler handler;
};
} // namespace

void
StelaVirtIOMMIOTransport::setHandler(OSObject *owner, StelaVirtIOHandler newHandler)
{
	if (!workLoop) {
		return;
	}
	// On the work loop, so no interrupt handler runs half-changed.
	HandlerChange change = {owner, newHandler};
	workLoop->runAction(&StelaVirtIOMMIOTransport::changeHandler, this, &change);
}

IOReturn
StelaVirtIOMMIOTransport::changeHandler(OSObject *target, void *change, void *, void *, void *)
{
	auto transport = static_cast<StelaVirtIOMMIOTransport *>(target);
	auto wanted = static_cast<HandlerChange *>(change);
	transport->handlerOwner = wanted->owner;
	transport->handler = wanted->handler;
	return kIOReturnSuccess;
}

/*
 * The device holds its level line while INTERRUPT_STATUS has a bit set:
 * acknowledging every bit first lowers it, and a cause arriving while the
 * driver works raises it again.
 */
void
StelaVirtIOMMIOTransport::interruptOccurred(OSObject *owner, IOInterruptEventSource *, int)
{
	auto transport = static_cast<StelaVirtIOMMIOTransport *>(owner);
	uint32_t status = transport->read(STELA_VIRTIO_MMIO_INTERRUPT_STATUS);
	if (status == 0) {
		return;
	}
	transport->write(STELA_VIRTIO_MMIO_INTERRUPT_ACK, status);
	transport->interrupts++;
	if (status & STELA_VIRTIO_MMIO_INT_CONFIG) {
		uint32_t device = transport->read(STELA_VIRTIO_MMIO_STATUS);
		if (device & STELA_VIRTIO_STATUS_DEVICE_NEEDS_RESET) {
			IOLog("StelaVirtIO: device %u stopped on an error (status %02x)\n", transport->device, device);
		}
	}
	if (transport->handler) {
		transport->handler(transport->handlerOwner, status);
	}
}

uint32_t
StelaVirtIOMMIOTransport::read(uint64_t reg) const
{
	return *reinterpret_cast<volatile uint32_t *>(registers->getVirtualAddress() + reg);
}

void
StelaVirtIOMMIOTransport::write(uint64_t reg, uint32_t value)
{
	*reinterpret_cast<volatile uint32_t *>(registers->getVirtualAddress() + reg) = value;
}

StelaVirtIODevice *
StelaVirtIODevice::withTransport(StelaVirtIOMMIOTransport *transport, uint32_t device)
{
	auto nub = new StelaVirtIODevice;
	if (!nub || !nub->init()) {
		OSSafeReleaseNULL(nub);
		return nullptr;
	}
	nub->transport = transport;
	nub->device = device;
	nub->setProperty("VirtIODeviceID", device, 32);
	char name[32];
	snprintf(name, sizeof name, "virtio-device%u", device);
	nub->setName(name);
	return nub;
}

/* A personality names its device by VirtIODeviceID, whatever the number's size. */
bool
StelaVirtIODevice::matchPropertyTable(OSDictionary *table, SInt32 *score)
{
	if (!IOService::matchPropertyTable(table, score)) {
		return false;
	}
	auto wanted = OSDynamicCast(OSNumber, table->getObject("VirtIODeviceID"));
	return wanted && wanted->unsigned32BitValue() == device;
}

uint64_t
StelaVirtIODevice::negotiate(uint64_t wanted)
{
	uint32_t status = transport->read(STELA_VIRTIO_MMIO_STATUS) | STELA_VIRTIO_STATUS_DRIVER;
	transport->write(STELA_VIRTIO_MMIO_STATUS, status);
	uint64_t offered = 0;
	for (uint32_t select = 0; select < 2; select++) {
		transport->write(STELA_VIRTIO_MMIO_DEVICE_FEATURES_SEL, select);
		offered |= uint64_t(transport->read(STELA_VIRTIO_MMIO_DEVICE_FEATURES)) << (32 * select);
	}
	uint64_t accepted = offered & wanted & ~STELA_VIRTIO_F_INDIRECT_DESC;
	if (!(accepted & STELA_VIRTIO_F_VERSION_1)) {
		IOLog("StelaVirtIO: device %u offers no VERSION_1 (%llx)\n", device, offered);
		fail();
		return 0;
	}
	for (uint32_t select = 0; select < 2; select++) {
		transport->write(STELA_VIRTIO_MMIO_DRIVER_FEATURES_SEL, select);
		transport->write(STELA_VIRTIO_MMIO_DRIVER_FEATURES, uint32_t(accepted >> (32 * select)));
	}
	status |= STELA_VIRTIO_STATUS_FEATURES_OK;
	transport->write(STELA_VIRTIO_MMIO_STATUS, status);
	if (!(transport->read(STELA_VIRTIO_MMIO_STATUS) & STELA_VIRTIO_STATUS_FEATURES_OK)) {
		IOLog("StelaVirtIO: device %u refused features %llx of %llx\n", device, accepted, offered);
		fail();
		return 0;
	}
	IOLog("StelaVirtIO: device %u features %llx of %llx\n", device, accepted, offered);
	return accepted;
}

bool
StelaVirtIODevice::createQueue(uint16_t index, uint16_t entries, uint64_t features, StelaVirtIOQueue *queue)
{
	transport->write(STELA_VIRTIO_MMIO_QUEUE_SEL, index);
	if (transport->read(STELA_VIRTIO_MMIO_QUEUE_READY) != 0) {
		IOLog("StelaVirtIO: device %u queue %u is already in use\n", device, index);
		return false;
	}
	uint32_t most = transport->read(STELA_VIRTIO_MMIO_QUEUE_NUM_MAX);
	uint16_t size = entries;
	while (size > most || size > STELA_VQ_MAX) {
		size /= 2;
	}
	if (size == 0) {
		IOLog("StelaVirtIO: device %u has no queue %u\n", device, index);
		return false;
	}
	queue->index = index;
	queue->memory = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage);
	if (queue->memory && queue->memory->prepare() != kIOReturnSuccess) {
		OSSafeReleaseNULL(queue->memory);
	}
	if (!queue->memory) {
		IOLog("StelaVirtIO: cannot allocate device %u queue %u\n", device, index);
		return false;
	}
	auto base = static_cast<uint8_t *>(queue->memory->getBytesNoCopy());
	bzero(base, kPage);
	IOByteCount bytes = 0;
	queue->physical = queue->memory->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone);
	if (queue->physical == 0 || bytes < kPage ||
	    stela_vq_init(&queue->vq, size, base, base + kAvailOffset, base + kUsedOffset,
	        (features & STELA_VIRTIO_F_EVENT_IDX) != 0) != 0) {
		IOLog("StelaVirtIO: device %u queue %u is not in one contiguous page\n", device, index);
		freeQueue(queue);
		return false;
	}
	const uint64_t areas[3][2] = {
	    {STELA_VIRTIO_MMIO_QUEUE_DESC_LOW, queue->physical},
	    {STELA_VIRTIO_MMIO_QUEUE_AVAIL_LOW, queue->physical + kAvailOffset},
	    {STELA_VIRTIO_MMIO_QUEUE_USED_LOW, queue->physical + kUsedOffset},
	};
	transport->write(STELA_VIRTIO_MMIO_QUEUE_NUM, size);
	for (auto &area : areas) {
		transport->write(area[0], uint32_t(area[1]));
		transport->write(area[0] + 4, uint32_t(area[1] >> 32));
	}
	__atomic_thread_fence(__ATOMIC_SEQ_CST);
	transport->write(STELA_VIRTIO_MMIO_QUEUE_READY, 1);
	if (transport->read(STELA_VIRTIO_MMIO_QUEUE_READY) != 1) {
		IOLog("StelaVirtIO: device %u refused queue %u at %llx\n", device, index, queue->physical);
		freeQueue(queue);
		return false;
	}
	IOLog("StelaVirtIO: device %u queue %u: %u entries at %llx\n", device, index, unsigned(size), queue->physical);
	return true;
}

/* Only after the device stopped using the queue (a reset, or QUEUE_READY 0). */
void
StelaVirtIODevice::freeQueue(StelaVirtIOQueue *queue)
{
	if (queue->memory) {
		queue->memory->complete();
		OSSafeReleaseNULL(queue->memory);
	}
	queue->physical = 0;
}

void
StelaVirtIODevice::driverOK()
{
	transport->write(STELA_VIRTIO_MMIO_STATUS,
	    transport->read(STELA_VIRTIO_MMIO_STATUS) | STELA_VIRTIO_STATUS_DRIVER_OK);
}

uint64_t
StelaVirtIODevice::readConfig(uint32_t offset, unsigned bytes)
{
	uint64_t value = 0;
	for (unsigned attempt = 0; attempt < kConfigReads; attempt++) {
		uint32_t generation = transport->read(STELA_VIRTIO_MMIO_CONFIG_GENERATION);
		value = transport->read(STELA_VIRTIO_MMIO_CONFIG + offset);
		if (bytes == 8) {
			value |= uint64_t(transport->read(STELA_VIRTIO_MMIO_CONFIG + offset + 4)) << 32;
		}
		if (transport->read(STELA_VIRTIO_MMIO_CONFIG_GENERATION) == generation) {
			break;
		}
	}
	return value;
}

void
StelaVirtIODevice::fail()
{
	transport->write(STELA_VIRTIO_MMIO_STATUS, transport->read(STELA_VIRTIO_MMIO_STATUS) | STELA_VIRTIO_STATUS_FAILED);
}

void
StelaVirtIODevice::notify(const StelaVirtIOQueue *queue)
{
	transport->write(STELA_VIRTIO_MMIO_QUEUE_NOTIFY, queue->index);
}

void
StelaVirtIODevice::kick(StelaVirtIOQueue *queue)
{
	stela_vq_publish(&queue->vq);
	if (stela_vq_kick_needed(&queue->vq)) {
		notify(queue);
	}
}

bool
StelaVirtIODevice::reset()
{
	return transport->reset();
}

void
StelaVirtIODevice::setHandler(OSObject *owner, StelaVirtIOHandler handler)
{
	transport->setHandler(owner, handler);
}

bool
StelaVirtIODevice::readConfig(uint32_t offset, uint32_t *words, unsigned count)
{
	for (unsigned attempt = 0; attempt < kResetPolls; attempt++) {
		uint32_t generation = transport->read(STELA_VIRTIO_MMIO_CONFIG_GENERATION);
		for (unsigned word = 0; word < count; word++) {
			words[word] = transport->read(STELA_VIRTIO_MMIO_CONFIG + offset + 4 * word);
		}
		if (transport->read(STELA_VIRTIO_MMIO_CONFIG_GENERATION) == generation) {
			return true;
		}
	}
	return false;
}
