/*
 * StelaVirtIONet: the virtio network device's driver (virtio 1.2, 5.1), an
 * IOEthernetController on the IONetworkingFamily declarations of
 * family/IONetworkingFamily.h (checked slot by slot against the boot
 * collection).
 *
 * The receive queue (0) holds kRxEntries writable 2 KiB buffers, the
 * transmit queue (1) up to kTxEntries readable ones, each a slot of a wired,
 * physically addressed page (kIOMemoryMapperNone) whose index is the
 * descriptor's: every chain is one buffer, a virtio_net_hdr_v1 then the
 * frame. Frames are copied both ways. The driver accepts VERSION_1,
 * EVENT_IDX, MAC, STATUS and the checksum offloads (CSUM, GUEST_CSUM), but
 * neither merged buffers nor coalesced segments, so every frame fits one
 * buffer.
 *
 * The output queue is IONetworkingFamily's gated one: outputPacket runs in
 * the work loop's gate, which the transport's interrupt handler shares, so
 * nothing here locks. A full transmit ring stalls the queue until the
 * device completes a chain. The link follows the device's STATUS, a change
 * arriving as a configuration interrupt; the medium is 1000BASE-T full
 * duplex. The device filters nothing a driver could set: promiscuous and
 * multicast requests are accepted as they are.
 */
#pragma once

#include "StelaVirtIO.h"

#include "IONetworkingFamily.h"

class StelaVirtIONet : public IOEthernetController {
	OSDeclareDefaultStructors(StelaVirtIONet);

public:
	/* Receive buffers and transmit slots, each 2 KiB, eight to a page. */
	static const uint16_t kRxEntries = 128;
	static const uint16_t kTxEntries = 128;
	static const uint32_t kBufferBytes = 2048;
	static const uint32_t kPage = 0x4000;
	static const unsigned kPerPage = kPage / kBufferBytes;

	bool start(IOService *provider) override;
	void stop(IOService *provider) override;

	using IOEthernetController::getHardwareAddress;
	IOReturn getHardwareAddress(IOEthernetAddress *address) override;
	using IOEthernetController::enable;
	using IOEthernetController::disable;
	IOReturn enable(IONetworkInterface *interface) override;
	IOReturn disable(IONetworkInterface *interface) override;
	UInt32 outputPacket(mbuf_t packet, void *param) override;
	IOOutputQueue *createOutputQueue() override;
	bool configureInterface(IONetworkInterface *interface) override;
	IOReturn getChecksumSupport(UInt32 *checksumMask, UInt32 checksumFamily, bool isOutput) override;
	IOReturn setMulticastMode(bool active) override;
	IOReturn setMulticastList(IOEthernetAddress *addresses, UInt32 count) override;
	IOReturn setPromiscuousMode(bool active) override;
	const OSString *newVendorString() const override;
	const OSString *newModelString() const override;

protected:
	void free() override;

private:
	/* A queue's buffers: page `slot / kPerPage`, at `slot % kPerPage`. */
	struct Buffers {
		IOBufferMemoryDescriptor *pages[kRxEntries / kPerPage];
		uint64_t physical[kRxEntries / kPerPage];
		uint8_t *bytes[kRxEntries / kPerPage];
	};
	static_assert(kTxEntries == kRxEntries, "one Buffers shape for both queues");

	bool allocate(Buffers *buffers);
	void release(Buffers *buffers);
	static uint8_t *at(const Buffers *buffers, uint16_t slot);
	static uint64_t physicalAt(const Buffers *buffers, uint16_t slot);

	/* Reads the address and the link from the configuration space. */
	bool readConfig();
	/* Tells the family the link, as the device's STATUS has it. */
	void publishLink();
	/* On the work loop: posts free receive buffers, unpublished. */
	unsigned postReceive();
	/* On the work loop: takes every frame the device wrote. */
	void receive();
	mbuf_t receivePacket(const uint8_t *buffer, uint32_t written);
	/* Takes back the transmit slots the device used. */
	void reclaimTransmit();
	/* NEEDS_CSUM for a partial TCP or UDP checksum; an IPv4 header's here. */
	void markChecksums(mbuf_t packet, uint8_t *buffer, size_t length);
	/* Stops the driver's use of the device, after it failed. */
	void failed(const char *why);
	void publishCounters();
	static void interruptOccurred(OSObject *owner, uint32_t status);
	static void timerFired(OSObject *owner, IOTimerEventSource *timer);
	static IOReturn begin(OSObject *target, void *, void *, void *, void *);
	static IOReturn halt(OSObject *target, void *, void *, void *, void *);

	StelaVirtIODevice *nub = nullptr;
	IONetworkInterface *netif = nullptr;
	IONetworkStats *netStats = nullptr;
	IONetworkMedium *medium = nullptr;
	IOOutputQueue *outputQueue = nullptr;
	IOTimerEventSource *timer = nullptr;
	StelaVirtIOQueue rxQueue = {};
	StelaVirtIOQueue txQueue = {};
	Buffers rx = {};
	Buffers tx = {};
	/* Each slot's token (its address), as the ring core needs one. */
	uint8_t rxTokens[kRxEntries] = {};
	uint8_t txTokens[kTxEntries] = {};
	uint64_t features = 0;
	uint8_t mac[6] = {};
	bool linkUp = true;
	/* The device runs its queues (DRIVER_OK, no error since). */
	bool running = false;
	/* The interface is enabled: frames go up and come down. */
	bool enabled = false;
	/* The output queue waits for a transmit completion. */
	bool stalled = false;
	uint64_t rxFrames = 0;
	uint64_t rxBytes = 0;
	uint64_t rxDropped = 0;
	uint64_t txFrames = 0;
	uint64_t txBytes = 0;
	uint64_t txDropped = 0;
	uint64_t txStalls = 0;
	uint64_t linkChanges = 0;
	uint64_t interrupts = 0;
};
