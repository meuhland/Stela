/*
 * StelaVirtIO: Stela's own guest drivers for its virtio devices, each a
 * virtio-mmio version 2 register page at /arm-io/stela-virtio<n>
 * (stela_contracts::virtio, generated into stela/virtio.h).
 *
 *   AppleARMIODevice (compatible "stela,virtio-mmio")
 *    └─ StelaVirtIOMMIOTransport   maps the page, takes the interrupt
 *        └─ StelaVirtIODevice      the nub, VirtIODeviceID = n
 *            ├─ StelaVirtIOEntropy (n = 4)
 *            ├─ StelaVirtIOConsole (n = 3, Console.h)
 *            └─ StelaVirtIOBlock   (n = 2, Block.h)
 *
 * The transport resets the device and acknowledges it, then publishes one
 * nub; a driver matches the nub by its personality's VirtIODeviceID
 * (StelaVirtIODevice::matchPropertyTable) and drives the device through the
 * nub's plain methods: feature negotiation, split virtqueues (stela_vq.h)
 * in physically contiguous guest pages, notifications, and the interrupt,
 * which the transport acknowledges and hands to the driver on its work loop.
 *
 * Only `--virtio` boots place the nodes, so without it nothing here runs.
 */
#pragma once

#include <IOKit/IOBufferMemoryDescriptor.h>
#include <IOKit/IOInterruptEventSource.h>
#include <IOKit/IOService.h>
#include <IOKit/IOTimerEventSource.h>
#include <IOKit/IOWorkLoop.h>

#include "stela_vq.h"

class StelaVirtIODevice;

/* One split virtqueue: its areas in one wired guest page, and its ring state. */
struct StelaVirtIOQueue {
	IOBufferMemoryDescriptor *memory;
	uint64_t physical;
	uint16_t index;
	struct stela_vq vq;
};

/* A driver's interrupt handler: the INTERRUPT_STATUS bits the transport acknowledged. */
typedef void (*StelaVirtIOHandler)(OSObject *owner, uint32_t status);

class StelaVirtIOMMIOTransport : public IOService {
	OSDeclareDefaultStructors(StelaVirtIOMMIOTransport);

public:
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	void free() override;
	IOWorkLoop *getWorkLoop() const override;

	uint32_t read(uint64_t reg) const;
	void write(uint64_t reg, uint32_t value);
	uint32_t deviceID() const { return device; }
	/* Writes 0 to STATUS and waits for the device to read 0 back. */
	bool reset();
	/* Hands interrupts to `handler` on the work loop; null takes them back. */
	void setHandler(OSObject *owner, StelaVirtIOHandler handler);

private:
	bool mapRegisters(IOService *provider);
	static void interruptOccurred(OSObject *owner, IOInterruptEventSource *source, int count);
	static IOReturn changeHandler(OSObject *target, void *change, void *, void *, void *);

	IOMemoryMap *registers = nullptr;
	IOWorkLoop *workLoop = nullptr;
	IOInterruptEventSource *interrupt = nullptr;
	StelaVirtIODevice *nub = nullptr;
	uint32_t device = 0;
	/* With the work loop's gate held: the driver's handler, and its owner. */
	OSObject *handlerOwner = nullptr;
	StelaVirtIOHandler handler = nullptr;
	uint64_t interrupts = 0;
};

class StelaVirtIODevice : public IOService {
	OSDeclareDefaultStructors(StelaVirtIODevice);

public:
	/* The nub of `transport`'s device, named by its ID for matching. */
	static StelaVirtIODevice *withTransport(StelaVirtIOMMIOTransport *transport, uint32_t device);
	using IOService::matchPropertyTable;
	bool matchPropertyTable(OSDictionary *table, SInt32 *score) override;

	uint32_t deviceID() const { return device; }
	/*
	 * DRIVER, then the features both offer of `wanted`: VERSION_1 is
	 * required and INDIRECT_DESC never taken. FEATURES_OK, read back.
	 * Answers the features accepted, 0 when the device refused them (and
	 * then the device is FAILED).
	 */
	uint64_t negotiate(uint64_t wanted);
	/* Queue `index` of up to `entries` entries, ready on the device. */
	bool createQueue(uint16_t index, uint16_t entries, uint64_t features, StelaVirtIOQueue *queue);
	void freeQueue(StelaVirtIOQueue *queue);
	void driverOK();
	/*
	 * 4 or 8 bytes of the device's configuration space at `offset`, read
	 * as 32-bit words and again while CONFIG_GENERATION changes under them.
	 */
	uint64_t readConfig(uint32_t offset, unsigned bytes);
	/* Marks the device FAILED: it uses no queue after this. */
	void fail();
	void notify(const StelaVirtIOQueue *queue);
	/* Publishes the queue's chains and notifies the device if it asked. */
	void kick(StelaVirtIOQueue *queue);
	bool reset();
	void setHandler(OSObject *owner, StelaVirtIOHandler handler);

private:
	StelaVirtIOMMIOTransport *transport = nullptr;
	uint32_t device = 0;
};

class StelaVirtIOEntropy : public IOService {
	OSDeclareDefaultStructors(StelaVirtIOEntropy);

public:
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	void free() override;

private:
	/* Posts slot `slot`'s buffer, unpublished. */
	bool post(unsigned slot);
	/* Feeds every completed buffer to the kernel's random pool. */
	void harvest();
	/* Posts a free buffer at the paced rate. */
	void tick();
	void publishCounters();
	static void interruptOccurred(OSObject *owner, uint32_t status);
	static void timerFired(OSObject *owner, IOTimerEventSource *timer);
	/* Work-loop actions: every buffer to the device, and the driver's halt. */
	static IOReturn postAll(OSObject *target, void *, void *, void *, void *);
	static IOReturn halt(OSObject *target, void *, void *, void *, void *);

	StelaVirtIODevice *nub = nullptr;
	StelaVirtIOQueue queue = {};
	IOBufferMemoryDescriptor *buffers = nullptr;
	uint64_t buffersPhysical = 0;
	IOTimerEventSource *timer = nullptr;
	/* Each slot's token (its address), and whether it is the device's. */
	uint8_t slots[4] = {};
	bool posted[4] = {};
	bool running = false;
	uint64_t features = 0;
	uint64_t bytesHarvested = 0;
	uint64_t completions = 0;
	uint64_t refused = 0;
};
