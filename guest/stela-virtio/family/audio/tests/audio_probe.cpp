/*
 * One class deriving from each IOAudioFamily class StelaVirtIO uses, so the
 * compiler emits a vtable laid out as IOAudioFamily.h declares it. xtask's
 * guest::family::audio tests compile this as the kext's sources are
 * compiled and check every slot against IOAudioFamily.slots. Never linked
 * into a kext.
 */
#include <audio/IOAudioFamily.h>

class StelaAudioProbeDevice : public IOAudioDevice {
	OSDeclareDefaultStructors(StelaAudioProbeDevice);
};

OSDefineMetaClassAndStructors(StelaAudioProbeDevice, IOAudioDevice);

class StelaAudioProbeEngine : public IOAudioEngine {
	OSDeclareDefaultStructors(StelaAudioProbeEngine);

public:
	UInt32 getCurrentSampleFrame() override { return 0; }
	bool driverDesiresHiResSampleIntervals(void) override { return true; }
};

OSDefineMetaClassAndStructors(StelaAudioProbeEngine, IOAudioEngine);

class StelaAudioProbeStream : public IOAudioStream {
	OSDeclareDefaultStructors(StelaAudioProbeStream);
};

OSDefineMetaClassAndStructors(StelaAudioProbeStream, IOAudioStream);

class StelaAudioProbeControl : public IOAudioControl {
	OSDeclareDefaultStructors(StelaAudioProbeControl);
};

OSDefineMetaClassAndStructors(StelaAudioProbeControl, IOAudioControl);

class StelaAudioProbeLevel : public IOAudioLevelControl {
	OSDeclareDefaultStructors(StelaAudioProbeLevel);
};

OSDefineMetaClassAndStructors(StelaAudioProbeLevel, IOAudioLevelControl);

class StelaAudioProbeToggle : public IOAudioToggleControl {
	OSDeclareDefaultStructors(StelaAudioProbeToggle);
};

OSDefineMetaClassAndStructors(StelaAudioProbeToggle, IOAudioToggleControl);
