/*
 * IOSerialFamily's kernel classes, declared by hand: no header of the
 * family is in the tree (xnu's open source has none).
 *
 * Every virtual method is declared in the slot the boot collection has it:
 * IOSerialFamily.slots beside this file is that layout, generated from the
 * 26B123 kernelcache by `cargo xtask guest-family`, and xtask's
 * guest::family tests compile tests/slot_probe.cpp against this header and
 * check every slot of its vtables against the manifest. The sizes asserted
 * below are the manifest's, each from the class's MetaClass constructor.
 *
 * IOSerialDriverSync's twelve methods are pure virtual, so the kernel names
 * none of its slots 272-283. IOSerialStreamSync names the same slots in the
 * same order, and forwards each call to its driver; the driver's methods
 * take the stream's parameters plus the stream's refCon, a `void *` [mem:
 * the trailing refCon, as Apple's serial drivers declare them].
 *
 * The constants: [kc] marks a value IOSerialBSDClient's code in 26B123
 * passes to the stream or tests in what it answers (read from the family's
 * disassembly), its meaning from that use and the open-source headers as
 * remembered; [mem] marks a value from memory alone.
 */
#pragma once

#include <IOKit/IOService.h>

class IOSerialDriverSync;

/* The driver behind a serial stream: one port, its state, its queues. */
class IOSerialDriverSync : public IOService {
	OSDeclareAbstractStructors(IOSerialDriverSync);

public:
	virtual IOReturn acquirePort(bool sleep, void *refCon) = 0;
	virtual IOReturn releasePort(void *refCon) = 0;
	virtual IOReturn setState(UInt32 state, UInt32 mask, void *refCon) = 0;
	virtual UInt32 getState(void *refCon) = 0;
	virtual IOReturn watchState(UInt32 *state, UInt32 mask, void *refCon) = 0;
	virtual UInt32 nextEvent(void *refCon) = 0;
	virtual IOReturn executeEvent(UInt32 event, UInt32 data, void *refCon) = 0;
	virtual IOReturn requestEvent(UInt32 event, UInt32 *data, void *refCon) = 0;
	virtual IOReturn enqueueEvent(UInt32 event, UInt32 data, bool sleep, void *refCon) = 0;
	virtual IOReturn dequeueEvent(UInt32 *event, UInt32 *data, bool sleep, void *refCon) = 0;
	virtual IOReturn enqueueData(UInt8 *buffer, UInt32 size, UInt32 *count, bool sleep, void *refCon) = 0;
	virtual IOReturn dequeueData(UInt8 *buffer, UInt32 size, UInt32 *count, UInt32 min, void *refCon) = 0;

	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 0);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 1);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 2);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 3);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 4);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 5);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 6);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 7);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 8);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 9);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 10);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 11);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 12);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 13);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 14);
	OSMetaClassDeclareReservedUnused(IOSerialDriverSync, 15);
};

/*
 * The nub a serial driver publishes: IOSerialBSDClient matches it and
 * makes its /dev/tty.* and /dev/cu.* nodes, and calls the stream, which
 * calls its driver (the provider it is attached to) with its refCon.
 */
class IOSerialStreamSync : public IOService {
	OSDeclareDefaultStructors(IOSerialStreamSync);

protected:
	/* [mem] names; the size is the kernel's. */
	IOSerialDriverSync *fProvider;
	void *fRefCon;

public:
	bool attach(IOService *provider) override;

	virtual IOReturn acquirePort(bool sleep);
	virtual IOReturn releasePort();
	virtual IOReturn setState(UInt32 state, UInt32 mask);
	virtual UInt32 getState();
	virtual IOReturn watchState(UInt32 *state, UInt32 mask);
	virtual UInt32 nextEvent();
	virtual IOReturn executeEvent(UInt32 event, UInt32 data);
	virtual IOReturn requestEvent(UInt32 event, UInt32 *data);
	virtual IOReturn enqueueEvent(UInt32 event, UInt32 data, bool sleep);
	virtual IOReturn dequeueEvent(UInt32 *event, UInt32 *data, bool sleep);
	virtual IOReturn enqueueData(UInt8 *buffer, UInt32 size, UInt32 *count, bool sleep);
	virtual IOReturn dequeueData(UInt8 *buffer, UInt32 size, UInt32 *count, UInt32 min);

	using IOService::init;
	/* The stream's refCon, which each call hands the driver. */
	virtual bool init(OSDictionary *dictionary, void *refCon);

	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 0);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 1);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 2);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 3);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 4);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 5);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 6);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 7);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 8);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 9);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 10);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 11);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 12);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 13);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 14);
	OSMetaClassDeclareReservedUnused(IOSerialStreamSync, 15);
};

/* An RS-232 stream: IOSerialBSDClient makes it a plain tty. */
class IORS232SerialStreamSync : public IOSerialStreamSync {
	OSDeclareDefaultStructors(IORS232SerialStreamSync);
};

static_assert(sizeof(IOSerialDriverSync) == 0x88, "IOSerialDriverSync's size in IOSerialFamily.slots");
static_assert(sizeof(IOSerialStreamSync) == 0x98, "IOSerialStreamSync's size in IOSerialFamily.slots");
static_assert(sizeof(IORS232SerialStreamSync) == 0x98, "IORS232SerialStreamSync's size in IOSerialFamily.slots");

/*
 * The family's registry keys, which it also exports as OSSymbols
 * (gIOTTYBaseNameKey, gIOTTYSuffixKey, ...). [mem] strings.
 */
#define kIOTTYBaseNameKey "IOTTYBaseName"
#define kIOTTYSuffixKey "IOTTYSuffix"
#define kIOCalloutDeviceKey "IOCalloutDevice"
#define kIODialinDeviceKey "IODialinDevice"
extern const OSSymbol *gIOTTYBaseNameKey;
extern const OSSymbol *gIOTTYSuffixKey;

/*
 * Port state, a 32-bit word (getState, setState, watchState). The upper
 * half is the family's, the lower the RS-232 lines'. Each receive bit is
 * its transmit bit shifted right by PD_S_RX_OFFSET.
 */
#define PD_S_ACQUIRED 0x80000000U         /* [mem] */
#define PD_S_ACTIVE 0x40000000U           /* [mem] */
#define PD_S_TX_ENABLE 0x20000000U        /* [kc] setState mask */
#define PD_S_TX_BUSY 0x10000000U          /* [kc] watchState mask */
#define PD_S_TX_EVENT 0x08000000U         /* [kc] setState and watchState masks */
#define PD_S_TXQ_EMPTY 0x04000000U        /* [kc] watchState value */
#define PD_S_TXQ_LOW_WATER 0x02000000U    /* [kc] watchState value */
#define PD_S_TXQ_HIGH_WATER 0x01000000U   /* [mem] */
#define PD_S_TXQ_FULL 0x00800000U         /* [mem] */
#define PD_S_TXQ_MASK (PD_S_TXQ_EMPTY | PD_S_TXQ_LOW_WATER | PD_S_TXQ_HIGH_WATER | PD_S_TXQ_FULL)
#define PD_S_RX_OFFSET 7                  /* [mem] */
#define PD_S_RX_ENABLE (PD_S_TX_ENABLE >> PD_S_RX_OFFSET)          /* 0x00400000, [kc] */
#define PD_S_RX_BUSY (PD_S_TX_BUSY >> PD_S_RX_OFFSET)
#define PD_S_RX_EVENT (PD_S_TX_EVENT >> PD_S_RX_OFFSET)            /* 0x00100000, [kc] */
#define PD_S_RXQ_EMPTY (PD_S_TXQ_EMPTY >> PD_S_RX_OFFSET)
#define PD_S_RXQ_LOW_WATER (PD_S_TXQ_LOW_WATER >> PD_S_RX_OFFSET)
#define PD_S_RXQ_HIGH_WATER (PD_S_TXQ_HIGH_WATER >> PD_S_RX_OFFSET)
#define PD_S_RXQ_FULL (PD_S_TXQ_FULL >> PD_S_RX_OFFSET)
#define PD_S_RXQ_MASK (PD_S_TXQ_MASK >> PD_S_RX_OFFSET)

/* The RS-232 lines (TIOCM_* values). */
#define PD_RS232_S_DTR 0x00000002U        /* [kc] setState mask 0x6 */
#define PD_RS232_S_RTS 0x00000004U        /* [kc] */
#define PD_RS232_S_CTS 0x00000020U        /* [mem] */
#define PD_RS232_S_CAR 0x00000040U        /* [kc] a dial-in open waits for it */
#define PD_RS232_S_DSR 0x00000100U        /* [mem] */

/*
 * Events (executeEvent, requestEvent, enqueueEvent): the low two bits are
 * the data's size, the rest the event's number.
 */
#define PD_E_EOQ 0x00U                    /* [mem] no event */
#define PD_E_ACTIVE 0x05U                 /* [kc] 1 opens the port, 0 closes it */
#define PD_E_TXQ_SIZE 0x0bU               /* [mem] */
#define PD_E_RXQ_SIZE 0x0fU               /* [mem] */
#define PD_E_TXQ_LOW_WATER 0x13U          /* [mem] */
#define PD_E_RXQ_LOW_WATER 0x17U          /* [mem] */
#define PD_E_TXQ_HIGH_WATER 0x1bU         /* [mem] */
#define PD_E_RXQ_HIGH_WATER 0x1fU         /* [mem] */
#define PD_E_TXQ_AVAILABLE 0x23U          /* [kc] the room before enqueueData */
#define PD_E_RXQ_AVAILABLE 0x27U          /* [mem] */
#define PD_E_TXQ_FLUSH 0x28U              /* [kc] executed with no data */
#define PD_E_RXQ_FLUSH 0x2fU              /* [kc] executed at open */
#define PD_E_FLOW_CONTROL 0x53U           /* [kc] requested, PD_RS232_A_* bits */
