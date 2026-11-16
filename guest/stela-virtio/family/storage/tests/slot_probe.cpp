/*
 * A class deriving from IOBlockStorageDevice, so the compiler emits a
 * vtable laid out as IOStorageFamily.h declares it. xtask's
 * guest::family::storage tests compile this as the kext's sources are
 * compiled and check every slot against IOStorageFamily.slots. Never
 * linked into a kext.
 */
#include "storage/IOStorageFamily.h"

class StelaSlotProbeBlock : public IOBlockStorageDevice {
	OSDeclareDefaultStructors(StelaSlotProbeBlock);

public:
	IOReturn doEjectMedia() override { return kIOReturnUnsupported; }
	IOReturn doFormatMedia(UInt64) override { return kIOReturnUnsupported; }
	UInt32 doGetFormatCapacities(UInt64 *, UInt32) const override { return 0; }
	char *getVendorString() override { return nullptr; }
	char *getProductString() override { return nullptr; }
	char *getRevisionString() override { return nullptr; }
	char *getAdditionalDeviceInfoString() override { return nullptr; }
	IOReturn reportBlockSize(UInt64 *) override { return kIOReturnSuccess; }
	IOReturn reportEjectability(bool *) override { return kIOReturnSuccess; }
	IOReturn reportMaxValidBlock(UInt64 *) override { return kIOReturnSuccess; }
	IOReturn reportMediaState(bool *, bool *) override { return kIOReturnSuccess; }
	IOReturn reportRemovability(bool *) override { return kIOReturnSuccess; }
	IOReturn reportWriteProtection(bool *) override { return kIOReturnSuccess; }
	IOReturn doAsyncReadWrite(IOMemoryDescriptor *, UInt64, UInt64, IOStorageAttributes *,
	    IOStorageCompletion *) override { return kIOReturnUnsupported; }
};

OSDefineMetaClassAndStructors(StelaSlotProbeBlock, IOBlockStorageDevice);
