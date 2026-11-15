/*
 * StelaNet: the guest's front end of Stela's paravirtual network adapter at
 * /arm-io/stela-net0 (stela_contracts::net, generated into stela/net.h).
 *
 * Prototype: a plain ifnet (the BSD network KPI, no IONetworkingFamily
 * vtables) named stn0. It reports its link (KEV_DL_LINK_ON/OFF, SIOCGIFMEDIA)
 * as an Ethernet driver does, so IPConfiguration configures it once asked
 * (`ipconfig set stn0 DHCP`, which the org.stela.StelaNet job does); not being
 * an IONetworkInterface, it is no hardware port in System Settings. Frames move
 * through two virtio split virtqueues the kext allocates: receive buffers of
 * 2 KiB each posted once and reposted as they are harvested, and transmit
 * slots of 2 KiB each that every packet is copied into, with a backlog of
 * packets waiting for slots while the ring is full. When the device
 * offers them, TCP and UDP checksums are offloaded both ways; the guest
 * still segments every frame itself.
 *
 * The personality matches only the node `--guest-network-stelanet` places,
 * so without it nothing of this class runs.
 */
#pragma once

#include <IOKit/IOBufferMemoryDescriptor.h>
#include <IOKit/IOInterruptEventSource.h>
#include <IOKit/IOLocks.h>
#include <IOKit/IOService.h>
#include <IOKit/IOWorkLoop.h>

extern "C" {
#include <net/kpi_interface.h>
#include <sys/kpi_mbuf.h>
}

class StelaNet : public IOService {
	OSDeclareDefaultStructors(StelaNet);

public:
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	void free() override;

	/* One split virtqueue and the 2 KiB buffers its descriptors name. */
	struct Queue {
		IOBufferMemoryDescriptor *ring;
		IOBufferMemoryDescriptor *buffers;
		uint8_t *base;
		uint64_t physical;
		uint16_t size;
		/* The driver's next available index, and the next used one it reads. */
		uint16_t avail;
		uint16_t used;
	};

private:
	bool mapRegisters(IOService *provider);
	bool allocateQueue(Queue *queue);
	void freeQueue(Queue *queue);
	bool attachInterface();
	/* Moves every received frame up the stack and reposts its buffer. */
	void harvestReceive();
	/* With txLock held: takes back the transmit slots the device completed. */
	void reclaimTransmit();
	errno_t transmit(mbuf_t packet);
	/* Moves `packet`, if any, and the backlog into free transmit slots. */
	errno_t pump(mbuf_t packet);
	/* With txLock held: copies a packet into the next slot, which is free. */
	bool fillSlot(mbuf_t packet);
	/* Posts the link's state to the stack. */
	void reportLink(bool up);
	/* SIOCGIFMEDIA. */
	errno_t mediaStatus(void *data);
	/* Marks the slot's header NEEDS_CSUM when the stack left a checksum partial. */
	void markPartialChecksum(mbuf_t packet, uint8_t *data, size_t length);
	/* Writes KICK if the device asked to hear of `queue` moving from `old` to `now`. */
	void kick(Queue *queue, uint16_t old, uint16_t now, uint64_t which);

	static errno_t ifOutput(ifnet_t interface, mbuf_t packet);
	static errno_t ifIoctl(ifnet_t interface, unsigned long command, void *data);
	static void ifDetached(ifnet_t interface);
	static void interruptOccurred(OSObject *owner, IOInterruptEventSource *source, int count);

	uint64_t read(uint64_t reg) const;
	void write(uint64_t reg, uint64_t value);

	IOMemoryMap *registers = nullptr;
	IOWorkLoop *workLoop = nullptr;
	IOInterruptEventSource *interrupt = nullptr;
	/* Serializes transmit; output runs on any thread. */
	IOLock *txLock = nullptr;
	Queue rx = {};
	Queue tx = {};
	/* Transmit slots the device has not completed. */
	uint16_t txInFlight = 0;
	/* With txLock held: packets waiting for a transmit slot, oldest first. */
	mbuf_t backlogHead = nullptr;
	mbuf_t backlogTail = nullptr;
	uint32_t backlogCount = 0;
	/* LINK bit 0, as last read. */
	bool linkUp = false;
	uint64_t accepted = 0;
	uint8_t mac[6] = {};
	ifnet_t ifp = nullptr;
	/*
	 * Set once the interface is attached and cleared before it detaches:
	 * received frames go up the stack only meanwhile. Before, they are
	 * dropped, but their buffers are still reposted and the interrupt
	 * re-armed, or the first frame to arrive early would stall receive.
	 */
	bool attached = false;
	/* With txLock held: the device runs and output may queue. */
	bool running = false;
	uint64_t txDropped = 0;
	uint64_t rxDropped = 0;
};
