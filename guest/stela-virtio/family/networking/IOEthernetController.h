/*
 * IONetworkingFamily's Ethernet controller, as IONetworkController.h says
 * how these headers are written and checked.
 */
#pragma once

#include "IONetworkController.h"

/* A station address. */
struct IOEthernetAddress {
	UInt8 bytes[6];
};

/* The AVB state setAVBControllerState takes; StelaVirtIONet has no AVB. */
enum IOEthernetControllerAVBState {
	kIOEthernetControllerAVBStateDisabled = 0,
};

class IOEthernetController : public IONetworkController {
	OSDeclareAbstractStructors(IOEthernetController);

public:
	/* The AVB packet and filter element: only their names matter here. */
	struct IOEthernetAVBPacket;
	struct IOEthernetAVBIngressFilterElement;
	typedef void (*avbPacketHandler)(void *context, IOEthernetAVBPacket *packet);

	static void initialize();

	/* The slots this class overrides. */
	bool init(OSDictionary *properties) override;
	IOReturn getHardwareAddress(void *addr, UInt32 *inOutAddrBytes) override;
	IOReturn setHardwareAddress(const void *addr, UInt32 addrBytes) override;
	IOReturn getMaxPacketSize(UInt32 *maxSize) const override;
	IOReturn getMinPacketSize(UInt32 *minSize) const override;
	IOReturn getPacketFilters(const OSSymbol *group, UInt32 *filters) const override;
	IOReturn enablePacketFilter(const OSSymbol *group, UInt32 aFilter, UInt32 enabledFilters,
	    IOOptionBits options = 0) override;
	IOReturn disablePacketFilter(const OSSymbol *group, UInt32 aFilter, UInt32 enabledFilters,
	    IOOptionBits options = 0) override;

protected:
	void free() override;
	bool publishProperties() override;
	IONetworkInterface *createInterface() override;

public:
	/* Slots 361 on. */
	virtual IOReturn getPacketFilters(UInt32 *filters) const;
	virtual IOReturn getHardwareAddress(IOEthernetAddress *addrP) = 0;
	virtual IOReturn setHardwareAddress(const IOEthernetAddress *addrP);
	virtual IOReturn setMulticastMode(bool active);
	virtual IOReturn setMulticastList(IOEthernetAddress *addrs, UInt32 count);
	virtual IOReturn setPromiscuousMode(bool active);
	virtual IOReturn setWakeOnMagicPacket(bool active);
	virtual bool getVlanTagDemand(mbuf_t m, UInt32 *vlanTag);
	virtual void setVlanTag(mbuf_t m, UInt32 vlanTag);
	virtual IOReturn setAVBControllerState(IOEthernetControllerAVBState state);
	virtual IOReturn transmitRealtimePackets(UInt32 queueIndex, IOEthernetAVBPacket **packets, UInt32 packetCount,
	    bool commonTimestamp, UInt32 *successfulPacketCount);
	virtual IOReturn cleanupTransmitQueue(UInt32 queueIndex);
	virtual IOReturn setRealtimeReceiveQueueFilter(UInt32 queueIndex, IOEthernetAVBIngressFilterElement *filters,
	    UInt32 filterCount);
	virtual IOReturn getRealtimeReceiveQueueFilter(UInt32 queueIndex, IOEthernetAVBIngressFilterElement *filters,
	    UInt32 *filterCount);
	virtual IOReturn setRealtimeReceiveQueuePacketHandler(UInt32 queueIndex, avbPacketHandler handler,
	    void *context);
	virtual IOReturn setRealtimeReceiveDestinationMACList(UInt32 queueIndex, IOEthernetAddress *addresses,
	    int addressCount);
	virtual IOReturn transmitTimeSyncPacket(IOEthernetAVBPacket *packet, uint64_t expiryTime);

	OSMetaClassDeclareReservedUnused(IOEthernetController, 10);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 11);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 12);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 13);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 14);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 15);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 16);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 17);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 18);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 19);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 20);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 21);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 22);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 23);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 24);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 25);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 26);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 27);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 28);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 29);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 30);
	OSMetaClassDeclareReservedUnused(IOEthernetController, 31);

private:
	uint64_t _members[(0x118 - 0x110) / 8];
};

static_assert(sizeof(IOEthernetController) == 0x118, "networking.slots: IOEthernetController is 0x118 bytes");
