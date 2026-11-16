/*
 * StelaVirtIOBlock: the virtio block device's driver (virtio 1.2, 5.2), a
 * block storage device of IOStorageFamily (family/storage/IOStorageFamily.h):
 *
 *   StelaVirtIODevice (VirtIODeviceID = 2)
 *    └─ StelaVirtIOBlock : IOBlockStorageDevice
 *        └─ IOBlockStorageDriver
 *            └─ IOMedia, "Stela VirtIO Block Media" [mem: vendor, product,
 *               "Media"], then partition schemes and file systems
 *
 * One request queue. Each request of the layers above (doAsyncReadWrite)
 * is cut into parts of at most kPartBytes and the device's SEG_MAX and
 * SIZE_MAX, each part one chain: a header from the slot's place in a wired
 * page, the request's buffer as its physical segments
 * (getPhysicalSegment, kIOMemoryMapperNone: the device's DMA is guest
 * physical, as the transport's rings are), and the slot's status byte. At
 * most kSlots parts are on the device at once; the rest wait in order. A
 * request completes (IOStorage's completion) once all its parts have, with
 * the bytes moved, or kIOReturnIOError at the first part the device
 * answered IOERR.
 *
 * Every request runs under a command gate on the transport's work loop,
 * which the device's interrupt also runs on. doSynchronizeCache and
 * doSynchronize send FLUSH and sleep until it completes; the serial comes
 * from GET_ID once, at start.
 *
 * The device is a fixed disk: not removable, not ejectable, not lockable,
 * and write-protected when the device offers RO. Its interconnect is
 * "Virtual Interface", at an "External" location, so Disk Arbitration
 * lists it as an external disk.
 */
#pragma once

#include <IOKit/IOCommandGate.h>

#include "StelaVirtIO.h"
#include "storage/IOStorageFamily.h"

class StelaVirtIOBlock : public IOBlockStorageDevice {
	OSDeclareDefaultStructors(StelaVirtIOBlock);

public:
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	void free() override;

	IOReturn doEjectMedia(void) override;
	IOReturn doFormatMedia(UInt64 byteCapacity) override;
	UInt32 doGetFormatCapacities(UInt64 *capacities, UInt32 capacitiesMaxCount) const override;
	IOReturn doLockUnlockMedia(bool doLock) override;
	IOReturn doSynchronizeCache(void) override;
	char *getVendorString(void) override;
	char *getProductString(void) override;
	char *getRevisionString(void) override;
	char *getAdditionalDeviceInfoString(void) override;
	IOReturn reportBlockSize(UInt64 *blockSize) override;
	IOReturn reportEjectability(bool *isEjectable) override;
	IOReturn reportLockability(bool *isLockable) override;
	IOReturn reportMaxValidBlock(UInt64 *maxBlock) override;
	IOReturn reportMediaState(bool *mediaPresent, bool *changedState) override;
	IOReturn reportPollRequirements(bool *pollRequired, bool *pollIsExpensive) override;
	IOReturn reportRemovability(bool *isRemovable) override;
	IOReturn reportWriteProtection(bool *isWriteProtected) override;
	IOReturn getWriteCacheState(bool *enabled) override;
	IOReturn setWriteCacheState(bool enabled) override;
	IOReturn doAsyncReadWrite(IOMemoryDescriptor *buffer, UInt64 block, UInt64 nblks,
	    IOStorageAttributes *attributes, IOStorageCompletion *completion) override;
	IOReturn doSynchronize(UInt64 block, UInt64 nblks, IOStorageSynchronizeOptions options) override;

	/* Parts on the device at once, and the request queue's entries: room
	 * for every slot's header, data buffers and status. */
	static const unsigned kSlots = 8;
	static const unsigned kSlotSegments = 30;
	static const uint16_t kQueueEntries = 256;
	/* The most bytes one part moves. */
	static const uint64_t kPartBytes = 1024 * 1024;

	/* One request of the layers above, or of this driver (Block.cpp). */
	struct Request;

private:
	/* A part on the device: its request, where in it, and how much. */
	struct Slot {
		Request *request;
		uint64_t offset;
		uint64_t bytes;
		bool busy;
	};

	static IOReturn submitGated(OSObject *owner, void *request, void *, void *, void *);
	static IOReturn waitGated(OSObject *owner, void *request, void *, void *, void *);
	static IOReturn begin(OSObject *target, void *, void *, void *, void *);
	static IOReturn halt(OSObject *target, void *, void *, void *, void *);
	static void interruptOccurred(OSObject *owner, uint32_t status);

	/* With the gate held. */
	void append(Request *request);
	/* Hands waiting parts to free slots, and tells the device. */
	void issue();
	/* One part of `request` into slot `index`; false when the request
	 * cannot be cut (its error is then set). */
	bool post(unsigned index, Request *request);
	void harvest();
	/* Takes a finished request off the queue and completes it. */
	void finish(Request *request);
	/* Fails every request not completed: the device is gone. */
	void abortAll(IOReturn status);
	/* Sends `type` (FLUSH or GET_ID) and waits for it, off the work loop. */
	IOReturn command(uint32_t type);
	bool readConfiguration();
	void publishProperties();
	void publishCounters();
	uint8_t *slotHeader(unsigned index) const;
	uint64_t slotPhysical(unsigned index) const;

	StelaVirtIODevice *nub = nullptr;
	IOCommandGate *gate = nullptr;
	StelaVirtIOQueue queue = {};
	/* Each slot's header and status, and GET_ID's answer, in one wired page. */
	IOBufferMemoryDescriptor *page = nullptr;
	uint64_t pagePhysical = 0;
	uint64_t features = 0;
	bool running = false;
	/* The disk: blocks of blockSize bytes, and what one part may take. */
	uint64_t blocks = 0;
	uint32_t blockSize = 0;
	uint32_t segmentsMost = 0;
	uint64_t segmentBytesMost = 0;
	bool readOnly = false;
	bool flushes = false;
	Slot slots[kSlots] = {};
	uint8_t tokens[kSlots] = {};
	/* Requests not completed, in arrival order. */
	Request *head = nullptr;
	Request *tail = nullptr;
	char serial[24] = {};
	uint64_t reads = 0;
	uint64_t writes = 0;
	uint64_t flushCount = 0;
	uint64_t bytesRead = 0;
	uint64_t bytesWritten = 0;
	uint64_t errors = 0;
};
