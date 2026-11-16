/*
 * IONetworkingFamily's network interfaces, as IONetworkController.h says
 * how these headers are written and checked.
 */
#pragma once

#include "IONetworkController.h"

class IONetworkData;
class IOUserClient;
struct ifnet;
struct ifnet_init_params;
enum IOMbufServiceClass : int;

/* The interface's statistics, kept in its IONetworkData of this key. [mem] */
#define kIONetworkStatsKey "IONetworkStatsKey"
struct IONetworkStats {
	UInt32 inputPackets;
	UInt32 inputErrors;
	UInt32 outputPackets;
	UInt32 outputErrors;
	UInt32 collisions;
};

class IONetworkInterface : public IOService {
	OSDeclareAbstractStructors(IONetworkInterface);

public:
	/* The slots this class overrides. */
	bool serializeProperties(OSSerialize *s) const override;
	bool requestTerminate(IOService *provider, IOOptionBits options) override;
	bool willTerminate(IOService *provider, IOOptionBits options) override;
	IOReturn message(UInt32 type, IOService *provider, void *argument = NULL) override;
	IOReturn newUserClient(task_t owningTask, void *security_id, UInt32 type, IOUserClient **handler) override;
	IOReturn powerStateWillChangeTo(IOPMPowerFlags flags, unsigned long stateNumber, IOService *policyMaker) override;
	IOReturn powerStateDidChangeTo(IOPMPowerFlags flags, unsigned long stateNumber, IOService *policyMaker) override;

protected:
	void free() override;
	bool handleOpen(IOService *client, IOOptionBits options, void *argument) override;
	void handleClose(IOService *client, IOOptionBits options) override;
	bool handleIsOpen(const IOService *client) const override;

public:
	/* Slots 272 on. */
	virtual bool isPrimaryInterface() const;
	virtual bool init(IONetworkController *controller);
	virtual bool isRegistered() const;
	virtual UInt32 getInterfaceState() const;
	virtual IONetworkController *getController() const;
	virtual UInt32 inputPacket(mbuf_t packet, UInt32 length = 0, IOOptionBits options = 0, void *param = 0);
	virtual UInt32 flushInputQueue();
	virtual UInt32 clearInputQueue();
	virtual bool inputEvent(UInt32 type, void *data);
	virtual bool registerOutputHandler(OSObject *target, IOOutputAction action);
	virtual const char *getNamePrefix() const = 0;
	virtual UInt8 getInterfaceType() const;
	virtual UInt32 getMaxTransferUnit() const;
	virtual UInt16 getFlags() const;
	virtual UInt32 getExtraFlags() const;
	virtual UInt8 getMediaAddressLength() const;
	virtual UInt8 getMediaHeaderLength() const;
	virtual UInt16 getUnitNumber() const;
	virtual bool addNetworkData(IONetworkData *aNetData);
	virtual bool removeNetworkData(const OSSymbol *aKey);
	virtual bool removeNetworkData(const char *aKey);
	virtual IONetworkData *getNetworkData(const char *aKey) const;
	virtual IONetworkData *getNetworkData(const OSSymbol *key) const;
	virtual bool setInterfaceType(UInt8 type);
	virtual bool setMaxTransferUnit(UInt32 mtu);
	virtual bool setFlags(UInt16 flags, UInt16 clear = 0);
	virtual bool setExtraFlags(UInt32 flags, UInt32 clear = 0);
	virtual bool setMediaAddressLength(UInt8 length);
	virtual bool setMediaHeaderLength(UInt8 length);
	virtual bool setUnitNumber(UInt16 unit);
	virtual void lock();
	virtual void unlock();
	virtual bool controllerDidOpen(IONetworkController *controller);
	virtual void controllerWillClose(IONetworkController *controller);
	virtual SInt32 performCommand(IONetworkController *controller, unsigned long cmd, void *arg0, void *arg1);
	virtual ifnet_t getIfnet() const;
	virtual bool initIfnet(struct ifnet *ifp);
	virtual bool handleClientOpen(IOService *client, IOOptionBits options, void *argument);
	virtual void handleClientClose(IOService *client, IOOptionBits options);
	virtual UInt32 setInterfaceState(UInt32 set, UInt32 clear = 0);
	virtual IOReturn controllerWillChangePowerState(IONetworkController *controller, IOPMPowerFlags flags,
	    UInt32 stateNumber, IOService *policyMaker);
	virtual IOReturn controllerDidChangePowerState(IONetworkController *controller, IOPMPowerFlags flags,
	    UInt32 stateNumber, IOService *policyMaker);
	virtual IOReturn attachToDataLinkLayer(IOOptionBits options, void *parameter);
	virtual void detachFromDataLinkLayer(IOOptionBits options, void *parameter);
	virtual void feedPacketInputTap(mbuf_t);
	virtual void feedPacketOutputTap(mbuf_t);
	virtual bool initIfnetParams(struct ifnet_init_params *params);
	virtual IOReturn configureOutputPullModel(uint32_t driverQueueSize, uint32_t outputModelOptions,
	    uint32_t reserved0, uint32_t reserved1, uint32_t reserved2);
	virtual IOReturn configureInputPacketPolling(uint32_t pollingModelOptions, uint32_t reserved);
	virtual void reportDataTransferRates(uint64_t outputRateMax, uint64_t inputRateMax,
	    uint64_t outputRateEffective, uint64_t inputRateEffective);
	virtual IOReturn dequeueOutputPackets(uint32_t maxCount, mbuf_t *packetHead, mbuf_t *packetTail,
	    uint32_t *packetCount, uint64_t *packetBytes);
	virtual IOReturn dequeueOutputPacketsWithServiceClass(uint32_t maxCount, IOMbufServiceClass serviceClass,
	    mbuf_t *packetHead, mbuf_t *packetTail, uint32_t *packetCount, uint64_t *packetBytes);
	virtual IOReturn enqueueInputPacket(mbuf_t packet, IOMbufQueue *queue, IOOptionBits options);

	OSMetaClassDeclareReservedUnused(IONetworkInterface, 11);
	OSMetaClassDeclareReservedUnused(IONetworkInterface, 12);
	OSMetaClassDeclareReservedUnused(IONetworkInterface, 13);
	OSMetaClassDeclareReservedUnused(IONetworkInterface, 14);
	OSMetaClassDeclareReservedUnused(IONetworkInterface, 15);

private:
	uint64_t _members[(0x110 - 0x88) / 8];
};

static_assert(sizeof(IONetworkInterface) == 0x110, "networking.slots: IONetworkInterface is 0x110 bytes");
