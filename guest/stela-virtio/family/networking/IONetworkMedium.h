/*
 * IONetworkingFamily's medium and its statistics buffers, as
 * IONetworkController.h says how these headers are written and checked.
 */
#pragma once

#include "IONetworkController.h"

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

static_assert(sizeof(OSObject) == 0x10, "networking.slots: OSObject is 0x10 bytes");
static_assert(sizeof(IONetworkMedium) == 0x38, "networking.slots: IONetworkMedium is 0x38 bytes");
static_assert(sizeof(IONetworkData) == 0x50, "networking.slots: IONetworkData is 0x50 bytes");
