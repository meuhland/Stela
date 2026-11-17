/*
 * IOStorageFamily's IOBlockStorageDevice, declared by hand: no header of
 * the family is in the tree (xnu's open source has none).
 *
 * Every virtual method is declared in the slot the boot collection has it:
 * IOStorageFamily.slots beside this file is that layout, generated from the
 * 26B123 kernelcache by `cargo xtask guest-family`, and xtask's
 * guest::family::storage tests compile tests/slot_probe.cpp against this
 * header and check every slot of its vtable against the manifest. The size
 * asserted below is the manifest's, from the class's MetaClass constructor.
 *
 * IOBlockStorageDevice's fourteen pure virtual slots have no name in the
 * kernel. Four block storage devices of other families name them, each the
 * same way (IOBlockStorageServices, IOAHCIBlockStorageDevice,
 * IONVMeBlockStorageDevice and AppleSDXCBlockStorageDevice): the manifest
 * records the method all four put in each, and the generator refuses a
 * kernel where they disagree. So every slot, its parameters and `const`
 * included, is [kc]. Return types are not in a mangled name and are [mem],
 * as are the extents' layouts and every name below that is not a method's.
 * IOStorageCompletion's layout is [kc] (IOStorage::complete's code).
 * Nothing here reads an IOStorageAttributes.
 */
#pragma once

#include <IOKit/IOMemoryDescriptor.h>
#include <IOKit/IOService.h>

/* The option and priority types, as their methods' manglings take them. */
typedef UInt32 IOStorageOptions;                    /* [mem] */
typedef UInt8 IOStoragePriority;                    /* [kc] doSetPriority's `h` */
typedef UInt32 IOStorageUnmapOptions;               /* [kc] doUnmap's last `j` */
typedef UInt32 IOStorageSynchronizeOptions;         /* [kc] doSynchronize's `j` */
typedef UInt64 IOStorageGetProvisionStatusOptions;  /* [kc] doGetProvisionStatus's last `y` */

/* A request's attributes: options, priority and more [mem]; only passed on. */
struct IOStorageAttributes;

/*
 * How a request completes: its action called with the status and the bytes
 * moved. [kc] 26B123's IOStorage::complete reads `action` at +8 and calls
 * it, unless null, with `target` (+0) and `parameter` (+0x10); the action
 * is signed with a type discriminator (0xe90c) and no address diversity,
 * so the 24 bytes may be copied whole. This kext never calls `action`
 * itself: IOStorage::complete does, as the kernel signed it.
 */
typedef void (*IOStorageCompletionAction)(void *target, void *parameter, IOReturn status, UInt64 actualByteCount);

struct IOStorageCompletion {
	void *target;
	IOStorageCompletionAction action;
	void *parameter;
};

static_assert(sizeof(IOStorageCompletion) == 24, "IOStorage::complete's 24 bytes");

/* IOStorage's one method this kext calls, exported by IOStorageFamily
 * (__ZN9IOStorage8completeEP19IOStorageCompletioniy). Only the static
 * method is declared here, not the class. */
class IOStorage {
public:
	static void complete(IOStorageCompletion *completion, IOReturn status, UInt64 actualByteCount);
};

/* A run of blocks, for doUnmap and doSetPriority [mem]. */
struct IOBlockStorageDeviceExtent {
	UInt64 blockStart;
	UInt64 blockCount;
};

/* A run of blocks and how it is provisioned, for doGetProvisionStatus [mem]. */
struct IOBlockStorageProvisionDeviceExtent {
	UInt64 blockStart;
	UInt64 blockCount;
	UInt8 provisionType;
};

/*
 * A block storage device: IOStorageFamily's IOBlockStorageDriver matches
 * it once registered, and publishes its IOMedia, through which partition
 * schemes and file systems reach the blocks.
 */
class IOBlockStorageDevice : public IOService {
	OSDeclareAbstractStructors(IOBlockStorageDevice);

protected:
	/* [mem] name; the size is the kernel's. */
	struct ExpansionData;
	ExpansionData *_expansionData;

public:
	using IOService::getProperty;
	using IOService::init;

	bool init(OSDictionary *properties) override;
	IOReturn setProperties(OSObject *properties) override;
	OSObject *getProperty(const OSSymbol *aKey) const override;

	virtual IOReturn doEjectMedia(void) = 0;
	virtual IOReturn doFormatMedia(UInt64 byteCapacity) = 0;
	virtual UInt32 doGetFormatCapacities(UInt64 *capacities, UInt32 capacitiesMaxCount) const = 0;
	virtual IOReturn doLockUnlockMedia(bool doLock);
	virtual IOReturn doSynchronizeCache(void);
	virtual char *getVendorString(void) = 0;
	virtual char *getProductString(void) = 0;
	virtual char *getRevisionString(void) = 0;
	virtual char *getAdditionalDeviceInfoString(void) = 0;
	virtual IOReturn reportBlockSize(UInt64 *blockSize) = 0;
	virtual IOReturn reportEjectability(bool *isEjectable) = 0;
	virtual IOReturn reportLockability(bool *isLockable);
	virtual IOReturn reportMaxValidBlock(UInt64 *maxBlock) = 0;
	virtual IOReturn reportMediaState(bool *mediaPresent, bool *changedState) = 0;
	virtual IOReturn reportPollRequirements(bool *pollRequired, bool *pollIsExpensive);
	virtual IOReturn reportRemovability(bool *isRemovable) = 0;
	virtual IOReturn reportWriteProtection(bool *isWriteProtected) = 0;
	virtual IOReturn getWriteCacheState(bool *enabled);
	virtual IOReturn setWriteCacheState(bool enabled);
	virtual IOReturn doAsyncReadWrite(IOMemoryDescriptor *buffer, UInt64 block, UInt64 nblks,
	    IOStorageAttributes *attributes, IOStorageCompletion *completion) = 0;
	virtual void requestIdle(void);
	virtual IOReturn doDiscard(UInt64 block, UInt64 nblks);
	virtual IOReturn doUnmap(IOBlockStorageDeviceExtent *extents, UInt32 extentsCount,
	    IOStorageUnmapOptions options = 0);
	virtual IOReturn doSetPriority(IOBlockStorageDeviceExtent *extents, UInt32 extentsCount,
	    IOStoragePriority priority);
	virtual IOReturn doSynchronize(UInt64 block, UInt64 nblks, IOStorageSynchronizeOptions options = 0);
	virtual IOReturn doGetProvisionStatus(UInt64 block, UInt64 nblks, UInt32 *extentsCount,
	    IOBlockStorageProvisionDeviceExtent *extents, IOStorageGetProvisionStatusOptions options = 0);

	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 4);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 5);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 6);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 7);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 8);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 9);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 10);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 11);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 12);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 13);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 14);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 15);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 16);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 17);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 18);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 19);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 20);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 21);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 22);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 23);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 24);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 25);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 26);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 27);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 28);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 29);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 30);
	OSMetaClassDeclareReservedUnused(IOBlockStorageDevice, 31);
};

static_assert(sizeof(IOBlockStorageDevice) == 0x90, "IOBlockStorageDevice's size in IOStorageFamily.slots");

/*
 * The registry keys a block storage device publishes for the layers above
 * [mem: IOStorageProtocolCharacteristics.h and IOStorageDeviceCharacteristics.h].
 * "External" makes Disk Arbitration call the disk external.
 */
#define kIOPropertyProtocolCharacteristicsKey "Protocol Characteristics"
#define kIOPropertyPhysicalInterconnectTypeKey "Physical Interconnect"
#define kIOPropertyPhysicalInterconnectLocationKey "Physical Interconnect Location"
#define kIOPropertyInterconnectVirtualKey "Virtual Interface"
#define kIOPropertyExternalKey "External"
#define kIOPropertyDeviceCharacteristicsKey "Device Characteristics"
#define kIOPropertyVendorNameKey "Vendor Name"
#define kIOPropertyProductNameKey "Product Name"
#define kIOPropertyProductRevisionLevelKey "Product Revision Level"
#define kIOPropertyProductSerialNumberKey "Serial Number"
