/*
 * One class deriving from each IOSerialFamily class StelaVirtIO uses, so
 * the compiler emits a vtable laid out as IOSerialFamily.h declares it.
 * xtask's guest::family tests compile this as the kext's sources are
 * compiled and check every slot against IOSerialFamily.slots. Never linked
 * into a kext.
 */
#include "IOSerialFamily.h"

class StelaSlotProbeDriver : public IOSerialDriverSync {
	OSDeclareDefaultStructors(StelaSlotProbeDriver);

public:
	IOReturn acquirePort(bool, void *) override { return kIOReturnSuccess; }
	IOReturn releasePort(void *) override { return kIOReturnSuccess; }
	IOReturn setState(UInt32, UInt32, void *) override { return kIOReturnSuccess; }
	UInt32 getState(void *) override { return 0; }
	IOReturn watchState(UInt32 *, UInt32, void *) override { return kIOReturnSuccess; }
	UInt32 nextEvent(void *) override { return PD_E_EOQ; }
	IOReturn executeEvent(UInt32, UInt32, void *) override { return kIOReturnSuccess; }
	IOReturn requestEvent(UInt32, UInt32 *, void *) override { return kIOReturnSuccess; }
	IOReturn enqueueEvent(UInt32, UInt32, bool, void *) override { return kIOReturnSuccess; }
	IOReturn dequeueEvent(UInt32 *, UInt32 *, bool, void *) override { return kIOReturnSuccess; }
	IOReturn enqueueData(UInt8 *, UInt32, UInt32 *, bool, void *) override { return kIOReturnSuccess; }
	IOReturn dequeueData(UInt8 *, UInt32, UInt32 *, UInt32, void *) override { return kIOReturnSuccess; }
};

OSDefineMetaClassAndStructors(StelaSlotProbeDriver, IOSerialDriverSync);

class StelaSlotProbeRS232 : public IORS232SerialStreamSync {
	OSDeclareDefaultStructors(StelaSlotProbeRS232);
};

OSDefineMetaClassAndStructors(StelaSlotProbeRS232, IORS232SerialStreamSync);
