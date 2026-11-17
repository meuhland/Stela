/*
 * IONetworkingFamily (com.apple.iokit.IONetworkingFamily), written by hand:
 * no header of the family is on disk. Each class declares its virtual
 * methods in the order of its vtable in the 26B123 boot collection, and
 * declares an override exactly where that class overrides a slot, so the
 * vtable clang builds for a class names, slot by slot, the symbols the
 * collection's does: networking.slots records them, and xtask's family test
 * checks every slot of every class here against it. Each class's size is
 * the one its MetaClass constructor registers (IONetworkingFamily.slots), as the
 * static_asserts say; the members are opaque.
 *
 * Only the names, the parameter types and the order are checked. Return
 * types, default arguments and every constant marked [mem] come from memory
 * of Apple's open-source IONetworkingFamily headers; [inf] marks one
 * inferred from xnu's headers. StelaVirtIONet uses as few of them as it can.
 */
#pragma once

#include <IOKit/IOService.h>
#include <net/if_media.h>
#include <sys/kpi_mbuf.h>

class IOCommandGate;
class IOKernelDebugger;
class IONetworkInterface;
class IONetworkMedium;
class IOOutputQueue;
class OSData;
class OSString;
class OSSymbol;
struct IOMbufQueue;
struct IOPacketBufferConstraints;

/* An ifmedia word (net/if_media.h): type, subtype and options. [inf] */
typedef UInt32 IOMediumType;

/* The output handler a queue calls with each packet. */
typedef UInt32 (OSObject::*IOOutputAction)(mbuf_t packet, void *param);

/* What an output handler answers. [mem] */
enum {
	kIOReturnOutputSuccess = 0x00,
	kIOReturnOutputStall = 0x01,
	kIOReturnOutputDropped = 0x02,
};

/* setLinkStatus's status: the ifmedia status bits. [inf] */
enum {
	kIONetworkLinkValid = IFM_AVALID,
	kIONetworkLinkActive = IFM_ACTIVE,
};

/*
 * getChecksumSupport and getChecksumDemand's family and bits: the mbuf
 * checksum bits of sys/kpi_mbuf.h. [inf]
 */
enum {
	kChecksumFamilyTCPIP = 0x00000001,
};
enum {
	kChecksumIP = MBUF_CSUM_REQ_IP,
	kChecksumTCP = MBUF_CSUM_REQ_TCP,
	kChecksumUDP = MBUF_CSUM_REQ_UDP,
	kChecksumTCPIPv6 = MBUF_CSUM_REQ_TCPIPV6,
	kChecksumUDPIPv6 = MBUF_CSUM_REQ_UDPIPV6,
};

class IONetworkController : public IOService {
	OSDeclareAbstractStructors(IONetworkController);

public:
	typedef IOReturn (*Action)(void *target, void *param0, void *param1, void *param2, void *param3);

	/* OSObject's and IOService's slots this class overrides. */
	bool init(OSDictionary *properties) override;
	IOReturn setAggressiveness(unsigned long type, unsigned long newLevel) override;
	void systemWillShutdown(IOOptionBits specifier) override;
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	IOReturn message(UInt32 type, IOService *provider, void *argument = NULL) override;

protected:
	void free() override;
	bool handleOpen(IOService *client, IOOptionBits options, void *argument) override;
	void handleClose(IOService *client, IOOptionBits options) override;
	bool handleIsOpen(const IOService *client) const override;

public:
	/* Slots 272 on. */
	virtual IOReturn executeCommand(OSObject *client, Action action, void *target, void *param0 = 0,
	    void *param1 = 0, void *param2 = 0, void *param3 = 0);
	virtual UInt32 outputPacket(mbuf_t packet, void *param);
	virtual UInt32 getFeatures() const;
	virtual const OSString *newVendorString() const;
	virtual const OSString *newModelString() const;
	virtual const OSString *newRevisionString() const;
	virtual const IONetworkMedium *getSelectedMedium() const;
	virtual const OSDictionary *getMediumDictionary() const;
	virtual OSDictionary *copyMediumDictionary() const;
	virtual IOOutputAction getOutputHandler() const;
	virtual IOReturn doEnable(IOService *client);
	virtual IOReturn doDisable(IOService *client);
	virtual IOCommandGate *getCommandGate() const;
	virtual IOReturn getHardwareAddress(void *addr, UInt32 *inOutAddrBytes) = 0;
	virtual IOReturn setHardwareAddress(const void *addr, UInt32 addrBytes) = 0;
	virtual IOReturn enable(IOService *client);
	virtual IOReturn disable(IOService *client);
	virtual IOReturn setMaxPacketSize(UInt32 maxSize);
	virtual IOReturn getMaxPacketSize(UInt32 *maxSize) const = 0;
	virtual IOReturn getMinPacketSize(UInt32 *minSize) const = 0;
	virtual IOReturn selectMedium(const IONetworkMedium *medium);
	virtual IOReturn selectMediumWithName(const OSSymbol *mediumName);
	virtual IOReturn getPacketFilters(const OSSymbol *group, UInt32 *filters) const = 0;
	virtual IOReturn enablePacketFilter(const OSSymbol *group, UInt32 aFilter, UInt32 enabledFilters,
	    IOOptionBits options = 0) = 0;
	virtual IOReturn disablePacketFilter(const OSSymbol *group, UInt32 aFilter, UInt32 enabledFilters,
	    IOOptionBits options = 0) = 0;
	virtual IOOutputQueue *getOutputQueue() const;
	virtual void getPacketBufferConstraints(IOPacketBufferConstraints *constraints) const;
	virtual mbuf_t allocatePacket(UInt32 size);
	virtual mbuf_t copyPacket(const mbuf_t m, UInt32 size = 0);
	virtual mbuf_t replacePacket(mbuf_t *mp, UInt32 size = 0);
	virtual mbuf_t replaceOrCopyPacket(mbuf_t *mp, UInt32 length, bool *replaced);
	virtual void freePacket(mbuf_t, IOOptionBits options = 0);
	virtual UInt32 releaseFreePackets();
	virtual IOReturn getChecksumSupport(UInt32 *checksumMask, UInt32 checksumFamily, bool isOutput);
	virtual bool setChecksumResult(mbuf_t packet, UInt32 checksumFamily, UInt32 resultMask, UInt32 validMask,
	    UInt32 param0 = 0, UInt32 param1 = 0);
	virtual void getChecksumDemand(const mbuf_t packet, UInt32 checksumFamily, UInt32 *demandMask,
	    void *param0 = 0, void *param1 = 0);
	virtual bool publishMediumDictionary(const OSDictionary *mediumDict);
	virtual bool setSelectedMedium(const IONetworkMedium *medium);
	virtual bool setLinkStatus(UInt32 status, const IONetworkMedium *activeMedium = 0, UInt64 speed = 0,
	    OSData *data = 0);
	virtual IOReturn registerWithPolicyMaker(IOService *policyMaker);
	virtual bool createWorkLoop();
	virtual IOReturn prepare();
	virtual bool publishProperties();
	virtual OSObject *getCommandClient() const;
	virtual IOReturn enable(IONetworkInterface *interface);
	virtual IOReturn disable(IONetworkInterface *interface);
	virtual bool attachInterface(IONetworkInterface **interface, bool doRegister = true);
	virtual void detachInterface(IONetworkInterface *interface, bool sync = false);
	virtual IONetworkInterface *createInterface() = 0;
	virtual bool configureInterface(IONetworkInterface *interface);
	virtual IOOutputQueue *createOutputQueue();
	virtual IOReturn enable(IOKernelDebugger *debugger);
	virtual IOReturn disable(IOKernelDebugger *debugger);
	virtual bool attachDebuggerClient(IOKernelDebugger **debuggerP);
	virtual void detachDebuggerClient(IOKernelDebugger *debugger);
	virtual void receivePacket(void *pkt, UInt32 *pktSize, UInt32 timeout);
	virtual void sendPacket(void *pkt, UInt32 pktSize);
	virtual UInt32 getDebuggerLinkStatus();
	virtual bool setDebuggerMode(bool active);
	virtual IOReturn outputStart(IONetworkInterface *interface, IOOptionBits options);
	virtual IOReturn setInputPacketPollingEnable(IONetworkInterface *interface, bool enabled);
	virtual void pollInputPackets(IONetworkInterface *interface, uint32_t maxCount, IOMbufQueue *pollQueue,
	    void *context);
	virtual void networkInterfaceNotification(IONetworkInterface *interface, uint32_t type, void *argument);
	virtual mbuf_t allocatePacketNoWait(UInt32 size);
	virtual IOReturn setHardwareAssists(UInt32 assists, UInt32 mask);

	OSMetaClassDeclareReservedUnused(IONetworkController, 8);
	OSMetaClassDeclareReservedUnused(IONetworkController, 9);
	OSMetaClassDeclareReservedUnused(IONetworkController, 10);
	OSMetaClassDeclareReservedUnused(IONetworkController, 11);
	OSMetaClassDeclareReservedUnused(IONetworkController, 12);
	OSMetaClassDeclareReservedUnused(IONetworkController, 13);
	OSMetaClassDeclareReservedUnused(IONetworkController, 14);
	OSMetaClassDeclareReservedUnused(IONetworkController, 15);
	OSMetaClassDeclareReservedUnused(IONetworkController, 16);
	OSMetaClassDeclareReservedUnused(IONetworkController, 17);
	OSMetaClassDeclareReservedUnused(IONetworkController, 18);
	OSMetaClassDeclareReservedUnused(IONetworkController, 19);
	OSMetaClassDeclareReservedUnused(IONetworkController, 20);
	OSMetaClassDeclareReservedUnused(IONetworkController, 21);
	OSMetaClassDeclareReservedUnused(IONetworkController, 22);
	OSMetaClassDeclareReservedUnused(IONetworkController, 23);
	OSMetaClassDeclareReservedUnused(IONetworkController, 24);
	OSMetaClassDeclareReservedUnused(IONetworkController, 25);
	OSMetaClassDeclareReservedUnused(IONetworkController, 26);
	OSMetaClassDeclareReservedUnused(IONetworkController, 27);
	OSMetaClassDeclareReservedUnused(IONetworkController, 28);
	OSMetaClassDeclareReservedUnused(IONetworkController, 29);
	OSMetaClassDeclareReservedUnused(IONetworkController, 30);
	OSMetaClassDeclareReservedUnused(IONetworkController, 31);

private:
	uint64_t _members[(0x110 - 0x88) / 8];
};

static_assert(sizeof(IOService) == 0x88, "IONetworkingFamily.slots: IOService is 0x88 bytes");
static_assert(sizeof(IONetworkController) == 0x110, "IONetworkingFamily.slots: IONetworkController is 0x110 bytes");

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

static_assert(sizeof(IOEthernetController) == 0x118, "IONetworkingFamily.slots: IOEthernetController is 0x118 bytes");

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

static_assert(sizeof(IONetworkInterface) == 0x110, "IONetworkingFamily.slots: IONetworkInterface is 0x110 bytes");

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

static_assert(sizeof(IOEthernetInterface) == 0x148, "IONetworkingFamily.slots: IOEthernetInterface is 0x148 bytes");

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

static_assert(sizeof(IOOutputQueue) == 0x20, "IONetworkingFamily.slots: IOOutputQueue is 0x20 bytes");
static_assert(sizeof(IOBasicOutputQueue) == 0x80, "IONetworkingFamily.slots: IOBasicOutputQueue is 0x80 bytes");
static_assert(sizeof(IOGatedOutputQueue) == 0x90, "IONetworkingFamily.slots: IOGatedOutputQueue is 0x90 bytes");

#pragma pop_macro("dequeue")
#pragma pop_macro("enqueue")

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

static_assert(sizeof(IOPacketQueue) == 0x28, "IONetworkingFamily.slots: IOPacketQueue is 0x28 bytes");

#pragma pop_macro("dequeue")
#pragma pop_macro("enqueue")

/* The Ethernet media StelaVirtIONet publishes: ifmedia words. [inf] */
enum {
	kIOMediumEthernet = IFM_ETHER,
	kIOMediumEthernetAuto = IFM_ETHER | IFM_AUTO,
	kIOMediumEthernet1000BaseT = IFM_ETHER | IFM_1000_T,
	kIOMediumOptionFullDuplex = IFM_FDX,
};

class IONetworkMedium : public OSObject {
	OSDeclareDefaultStructors(IONetworkMedium);

public:
	static IONetworkMedium *medium(IOMediumType type, UInt64 speed, UInt32 flags = 0, UInt32 index = 0,
	    const char *name = 0);
	static bool addMedium(OSDictionary *dict, const IONetworkMedium *medium);

	/* The slots this class overrides. */
	bool serialize(OSSerialize *s) const override;
	bool isEqualTo(const OSMetaClassBase *obj) const override;

protected:
	void free() override;

public:
	/* Slots 39 on. */
	virtual bool init(IOMediumType type, UInt64 speed, UInt32 flags = 0, UInt32 index = 0, const char *name = 0);
	virtual IOMediumType getType() const;
	virtual UInt64 getSpeed() const;
	virtual UInt32 getFlags() const;
	virtual UInt32 getIndex() const;
	virtual const OSSymbol *getName() const;
	/* Slot 45: no export names it. [mem] */
	virtual const OSSymbol *getKey() const;
	virtual bool isEqualTo(const IONetworkMedium *medium) const;

	OSMetaClassDeclareReservedUnused(IONetworkMedium, 0);
	OSMetaClassDeclareReservedUnused(IONetworkMedium, 1);
	OSMetaClassDeclareReservedUnused(IONetworkMedium, 2);
	OSMetaClassDeclareReservedUnused(IONetworkMedium, 3);

private:
	uint64_t _members[(0x38 - 0x10) / 8];
};

class IONetworkData : public OSObject {
	OSDeclareDefaultStructors(IONetworkData);

public:
	typedef IOReturn (*Action)(void *target, void *param, IONetworkData *data, UInt32 type, void *buffer,
	    UInt32 *bufferSize, UInt32 bufferOffset);

	/* The slots this class overrides. */
	bool serialize(OSSerialize *s) const override;

protected:
	void free() override;
	/* Slots 39 on. */
	virtual bool writeBytes(const void *srcBuffer, UInt32 srcBufferSize, UInt32 writeOffset = 0);
	virtual bool readBytes(void *dstBuffer, UInt32 *dstBufferSize, UInt32 readOffset = 0) const;
	virtual bool clearBuffer();

public:
	virtual bool init(const char *name, UInt32 bufferType, UInt32 bufferSize, void *extBuffer = 0,
	    UInt32 accessTypes = 0, void *target = 0, Action action = 0, void *param = 0);
	virtual void setAccessTypes(UInt32 types);
	virtual void setNotificationTarget(void *target, Action action, void *param = 0);
	virtual const void *getBuffer() const;
	virtual UInt32 getBufferType() const;
	virtual UInt32 getAccessTypes() const;
	virtual void *getNotificationTarget() const;
	virtual Action getNotificationAction() const;
	virtual void *getNotificationParameter() const;
	virtual const OSSymbol *getKey() const;
	virtual UInt32 getSize() const;
	virtual IOReturn reset();
	virtual IOReturn read(void *dstBuffer, UInt32 *dstBufferSize, UInt32 readOffset = 0);
	virtual IOReturn write(void *srcBuffer, UInt32 srcBufferSize, UInt32 writeOffset = 0);

	OSMetaClassDeclareReservedUnused(IONetworkData, 0);
	OSMetaClassDeclareReservedUnused(IONetworkData, 1);
	OSMetaClassDeclareReservedUnused(IONetworkData, 2);
	OSMetaClassDeclareReservedUnused(IONetworkData, 3);

private:
	uint64_t _members[(0x50 - 0x10) / 8];
};

static_assert(sizeof(OSObject) == 0x10, "IONetworkingFamily.slots: OSObject is 0x10 bytes");
static_assert(sizeof(IONetworkMedium) == 0x38, "IONetworkingFamily.slots: IONetworkMedium is 0x38 bytes");
static_assert(sizeof(IONetworkData) == 0x50, "IONetworkingFamily.slots: IONetworkData is 0x50 bytes");
