/*
 * StelaVirtIOConsole: the virtio console's driver (virtio 1.2, 5.3), one
 * port, as a serial driver of IOSerialFamily (family/IOSerialFamily.h):
 *
 *   StelaVirtIODevice (VirtIODeviceID = 3)
 *    └─ StelaVirtIOConsole : IOSerialDriverSync
 *        └─ IORS232SerialStreamSync, IOTTYBaseName "stela-virtio",
 *           IOTTYSuffix "-console"
 *            └─ IOSerialBSDClient: /dev/tty.stela-virtio-console and
 *               /dev/cu.stela-virtio-console [mem: the name is base then
 *               suffix]
 *
 * It follows the shape of Apple's serial drivers (AppleUSBCDCACMData and
 * the older sample drivers, as remembered [mem]): a port state word whose
 * waiters sleep on it through the work loop's command gate, a transmit and
 * a receive queue, and the stream's twelve calls each run under the gate.
 *
 * Transmit: enqueueData copies the caller's bytes into free slots of a
 * wired page, each slot one readable chain on transmitq (queue 1).
 * Receive: writable slots on receiveq (queue 0); each completed one is
 * copied into a ring that dequeueData empties, and a slot goes back to the
 * device only while the ring has room for every slot the device holds, so
 * a reader that stops leaves the host's bytes waiting on the host.
 *
 * virtio-console has no line settings: baud rate, data size, parity and
 * flow control are taken and ignored, and read back as set.
 */
#pragma once

#include <IOKit/IOCommandGate.h>

#include "IOSerialFamily.h"
#include "StelaVirtIO.h"

class StelaVirtIOConsole : public IOSerialDriverSync {
	OSDeclareDefaultStructors(StelaVirtIOConsole);

public:
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	void free() override;

	IOReturn acquirePort(bool sleep, void *refCon) override;
	IOReturn releasePort(void *refCon) override;
	IOReturn setState(UInt32 state, UInt32 mask, void *refCon) override;
	UInt32 getState(void *refCon) override;
	IOReturn watchState(UInt32 *state, UInt32 mask, void *refCon) override;
	UInt32 nextEvent(void *refCon) override;
	IOReturn executeEvent(UInt32 event, UInt32 data, void *refCon) override;
	IOReturn requestEvent(UInt32 event, UInt32 *data, void *refCon) override;
	IOReturn enqueueEvent(UInt32 event, UInt32 data, bool sleep, void *refCon) override;
	IOReturn dequeueEvent(UInt32 *event, UInt32 *data, bool sleep, void *refCon) override;
	IOReturn enqueueData(UInt8 *buffer, UInt32 size, UInt32 *count, bool sleep, void *refCon) override;
	IOReturn dequeueData(UInt8 *buffer, UInt32 size, UInt32 *count, UInt32 min, void *refCon) override;

	/* The slots and the receive ring. */
	static const unsigned kTxSlots = 16;
	static const unsigned kRxSlots = 8;
	static const uint32_t kSlotBytes = 256;
	static const uint32_t kRxRingBytes = 4096;
	/* Events whose data requestEvent reads back as executeEvent set it. */
	static const unsigned kEventsKept = 16;

	/* One call of the stream's, run under the gate (Console.cpp). */
	struct Call;

private:
	static IOReturn gated(OSObject *owner, void *call, void *, void *, void *);
	IOReturn run(Call *call);

	/* With the gate held. */
	IOReturn acquireGated(bool sleep);
	IOReturn releaseGated();
	void setStateGated(UInt32 state, UInt32 mask);
	IOReturn watchStateGated(UInt32 *state, UInt32 mask);
	IOReturn executeGated(UInt32 event, UInt32 data);
	IOReturn requestGated(UInt32 event, UInt32 *data);
	IOReturn enqueueGated(UInt8 *buffer, UInt32 size, UInt32 *count, bool sleep);
	IOReturn dequeueGated(UInt8 *buffer, UInt32 size, UInt32 *count, UInt32 min);
	/* Recomputes the queue bits of the state from the queues. */
	void updateQueues();
	/* Copies up to `size` received bytes out of the ring. */
	UInt32 take(UInt8 *buffer, UInt32 size);
	void flushReceived();
	/* Puts free receive slots back on the device while the ring has room. */
	void postReceive();
	void harvestTransmit();
	void harvestReceive();
	void publishCounters();
	uint8_t *slot(unsigned index) const;
	uint64_t slotPhysical(unsigned index) const;
	static void interruptOccurred(OSObject *owner, uint32_t status);
	static IOReturn begin(OSObject *target, void *, void *, void *, void *);
	static IOReturn halt(OSObject *target, void *, void *, void *, void *);
	bool publishStream();

	StelaVirtIODevice *nub = nullptr;
	IOCommandGate *gate = nullptr;
	IORS232SerialStreamSync *stream = nullptr;
	StelaVirtIOQueue receiveq = {};
	StelaVirtIOQueue transmitq = {};
	/* The slots: kTxSlots transmit ones, then kRxSlots receive ones. */
	IOBufferMemoryDescriptor *buffers = nullptr;
	uint64_t buffersPhysical = 0;
	uint64_t features = 0;
	bool running = false;
	/* The port's state, which watchState sleeps on. */
	UInt32 portState = 0;
	/* Each slot's token (its address), and whether the device has it. */
	uint8_t txTokens[kTxSlots] = {};
	bool txBusy[kTxSlots] = {};
	uint8_t rxTokens[kRxSlots] = {};
	bool rxPosted[kRxSlots] = {};
	/* The receive ring. */
	uint8_t received[kRxRingBytes] = {};
	uint32_t receivedHead = 0;
	uint32_t receivedCount = 0;
	/* What executeEvent set, for requestEvent. */
	UInt32 eventCodes[kEventsKept] = {};
	UInt32 eventData[kEventsKept] = {};
	unsigned eventsKept = 0;
	uint64_t bytesTransmitted = 0;
	uint64_t bytesReceived = 0;
	uint64_t receiveDropped = 0;
};
