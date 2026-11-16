/*
 * IONetworkingFamily (com.apple.iokit.IONetworkingFamily), written by hand:
 * no header of the family is on disk. Each class declares its virtual
 * methods in the order of its vtable in the 26B123 boot collection, and
 * declares an override exactly where that class overrides a slot, so the
 * vtable clang builds for a class names, slot by slot, the symbols the
 * collection's does: networking.slots records them, and xtask's family test
 * checks every slot of every class here against it. Each class's size is
 * the one its MetaClass constructor registers (networking.slots), as the
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

static_assert(sizeof(IOService) == 0x88, "networking.slots: IOService is 0x88 bytes");
static_assert(sizeof(IONetworkController) == 0x110, "networking.slots: IONetworkController is 0x110 bytes");
