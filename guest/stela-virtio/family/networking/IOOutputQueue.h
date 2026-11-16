/*
 * IONetworkingFamily's output queues: the abstract one, the basic one and
 * the one gated by a work loop, which hands each packet to its target's
 * output handler inside the work loop's gate. As IONetworkController.h says
 * how these headers are written and checked.
 */
#pragma once

#include "IONetworkController.h"

class IONetworkData;
class IOWorkLoop;

/*
 * xnu's kern/queue.h defines enqueue() and dequeue() as macros, which would
 * rename the methods below (and change their symbols); not here.
 */
#pragma push_macro("enqueue")
#pragma push_macro("dequeue")
#undef enqueue
#undef dequeue

/* service()'s option: from the queue's own thread, not this one. [mem] */
enum {
	kServiceAsync = 0x1,
};

class IOOutputQueue : public OSObject {
	OSDeclareAbstractStructors(IOOutputQueue);

public:
	/* The slots this class overrides. */
	bool init() override;

protected:
	void free() override;
	/* Slots 39 on. */
	virtual bool scheduleServiceThread(void *param = 0);
	virtual bool cancelServiceThread();
	virtual void serviceThread(void *param);

public:
	virtual bool start() = 0;
	virtual bool stop() = 0;
	virtual bool service(IOOptionBits options = 0) = 0;
	virtual UInt32 flush() = 0;
	virtual bool setCapacity(UInt32 capacity) = 0;
	virtual UInt32 getCapacity() const = 0;
	virtual UInt32 getSize() const = 0;
	virtual UInt32 enqueue(mbuf_t m, void *param) = 0;
	virtual IOOutputAction getOutputHandler() const;
	virtual IONetworkData *getStatisticsData() const;
	virtual UInt32 getMbufPriority(mbuf_t m);

	OSMetaClassDeclareReservedUnused(IOOutputQueue, 1);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 2);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 3);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 4);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 5);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 6);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 7);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 8);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 9);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 10);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 11);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 12);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 13);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 14);
	OSMetaClassDeclareReservedUnused(IOOutputQueue, 15);

private:
	uint64_t _members[(0x20 - 0x10) / 8];
};

class IOBasicOutputQueue : public IOOutputQueue {
	OSDeclareDefaultStructors(IOBasicOutputQueue);

public:
	static IOBasicOutputQueue *withTarget(IONetworkController *target, UInt32 capacity = 0);
	static IOBasicOutputQueue *withTarget(IONetworkController *target, UInt32 capacity, UInt32 priorities);

	/* The slots this class overrides. */
	bool start() override;
	bool stop() override;
	bool service(IOOptionBits options = 0) override;
	UInt32 flush() override;
	bool setCapacity(UInt32 capacity) override;
	UInt32 getCapacity() const override;
	UInt32 getSize() const override;
	UInt32 enqueue(mbuf_t m, void *param) override;
	IONetworkData *getStatisticsData() const override;

protected:
	void free() override;
	void serviceThread(void *param) override;
	/* Slots 68 on. */
	virtual void output(IOMbufQueue *queue, UInt32 *state);
	virtual IOReturn handleNetworkDataAccess(IONetworkData *data, UInt32 type, void *param);

public:
	virtual bool init(OSObject *target, IOOutputAction action, UInt32 capacity = 0, UInt32 priorities = 1);
	virtual UInt32 getDropCount();
	virtual UInt32 getOutputCount();
	virtual UInt32 getRetryCount();
	virtual UInt32 getStallCount();
	virtual UInt32 getState() const;

private:
	uint64_t _members[(0x80 - 0x20) / 8];
};

class IOGatedOutputQueue : public IOBasicOutputQueue {
	OSDeclareDefaultStructors(IOGatedOutputQueue);

public:
	static IOGatedOutputQueue *withTarget(IONetworkController *target, IOWorkLoop *workloop, UInt32 capacity = 0);
	static IOGatedOutputQueue *withTarget(IONetworkController *target, IOWorkLoop *workloop, UInt32 capacity,
	    UInt32 priorities);

protected:
	/* The slots this class overrides. */
	void free() override;
	bool scheduleServiceThread(void *param) override;
	void output(IOMbufQueue *queue, UInt32 *state) override;

public:
	/* Slot 76. */
	virtual bool init(OSObject *target, IOOutputAction action, IOWorkLoop *workloop, UInt32 capacity = 0,
	    UInt32 priorities = 1);

private:
	uint64_t _members[(0x90 - 0x80) / 8];
};

static_assert(sizeof(IOOutputQueue) == 0x20, "networking.slots: IOOutputQueue is 0x20 bytes");
static_assert(sizeof(IOBasicOutputQueue) == 0x80, "networking.slots: IOBasicOutputQueue is 0x80 bytes");
static_assert(sizeof(IOGatedOutputQueue) == 0x90, "networking.slots: IOGatedOutputQueue is 0x90 bytes");

#pragma pop_macro("dequeue")
#pragma pop_macro("enqueue")
