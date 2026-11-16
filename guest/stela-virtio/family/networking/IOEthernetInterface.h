/*
 * IONetworkingFamily's Ethernet interface, which IOEthernetController's
 * createInterface makes, as IONetworkController.h says how these headers
 * are written and checked.
 */
#pragma once

#include "IONetworkInterface.h"

class IOEthernetInterface : public IONetworkInterface {
	OSDeclareDefaultStructors(IOEthernetInterface);

public:
	/* The slots this class overrides. */
	bool willTerminate(IOService *provider, IOOptionBits options) override;
	bool init(IONetworkController *controller) override;
	bool inputEvent(UInt32 type, void *data) override;

protected:
	void free() override;
	const char *getNamePrefix() const override;
	bool controllerDidOpen(IONetworkController *controller) override;
	void controllerWillClose(IONetworkController *controller) override;
	SInt32 performCommand(IONetworkController *controller, unsigned long cmd, void *arg0, void *arg1) override;
	IOReturn controllerWillChangePowerState(IONetworkController *controller, IOPMPowerFlags flags,
	    UInt32 stateNumber, IOService *policyMaker) override;
	IOReturn controllerDidChangePowerState(IONetworkController *controller, IOPMPowerFlags flags,
	    UInt32 stateNumber, IOService *policyMaker) override;
	IOReturn attachToDataLinkLayer(IOOptionBits options, void *parameter) override;
	void feedPacketInputTap(mbuf_t) override;
	void feedPacketOutputTap(mbuf_t) override;
	bool initIfnetParams(struct ifnet_init_params *params) override;

public:
	/* Slots 330 on: all reserved. */
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 0);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 1);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 2);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 3);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 4);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 5);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 6);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 7);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 8);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 9);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 10);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 11);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 12);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 13);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 14);
	OSMetaClassDeclareReservedUnused(IOEthernetInterface, 15);

private:
	uint64_t _members[(0x148 - 0x110) / 8];
};

static_assert(sizeof(IOEthernetInterface) == 0x148, "networking.slots: IOEthernetInterface is 0x148 bytes");
