/*
 * IONetworkingFamily's packet queue, as IONetworkController.h says how these
 * headers are written and checked.
 */
#pragma once

#include "IONetworkController.h"

/*
 * xnu's kern/queue.h defines enqueue() and dequeue() as macros, which would
 * rename the methods below (and change their symbols); not here.
 */
#pragma push_macro("enqueue")
#pragma push_macro("dequeue")
#undef enqueue
#undef dequeue

class IOPacketQueue : public OSObject {
	OSDeclareDefaultStructors(IOPacketQueue);

public:
	static IOPacketQueue *withCapacity(UInt32 capacity);

protected:
	/* The slot this class overrides. */
	void free() override;

public:
	/* Slots 39 on. */
	virtual bool initWithCapacity(UInt32 capacity);
	virtual UInt32 getSize() const;
	virtual bool setCapacity(UInt32 capacity);
	virtual UInt32 getCapacity() const;
	virtual mbuf_t peek() const;
	virtual void prepend(mbuf_t m);
	virtual void prepend(IOPacketQueue *queue);
	virtual void lockPrepend(mbuf_t m);
	virtual bool enqueue(mbuf_t m);
	virtual bool enqueue(IOPacketQueue *queue);
	virtual UInt32 enqueueWithDrop(mbuf_t m);
	virtual bool lockEnqueue(mbuf_t m);
	virtual UInt32 lockEnqueueWithDrop(mbuf_t m);
	virtual mbuf_t dequeue();
	virtual mbuf_t lockDequeue();
	virtual mbuf_t dequeueAll();
	virtual mbuf_t lockDequeueAll();
	virtual UInt32 flush();
	virtual UInt32 lockFlush();

	OSMetaClassDeclareReservedUnused(IOPacketQueue, 0);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 1);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 2);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 3);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 4);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 5);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 6);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 7);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 8);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 9);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 10);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 11);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 12);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 13);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 14);
	OSMetaClassDeclareReservedUnused(IOPacketQueue, 15);

private:
	uint64_t _members[(0x28 - 0x10) / 8];
};

static_assert(sizeof(IOPacketQueue) == 0x28, "networking.slots: IOPacketQueue is 0x28 bytes");

#pragma pop_macro("dequeue")
#pragma pop_macro("enqueue")
