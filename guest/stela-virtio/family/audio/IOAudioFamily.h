/*
 * IOAudioFamily's kernel classes, declared by hand: no header of the family
 * is in the tree (xnu's open source has none).
 *
 * Every virtual method is declared in the slot the boot collection has it:
 * IOAudioFamily.slots beside this file is that layout, generated from the
 * 26B123 kernelcache by `cargo xtask guest-family`, and xtask's
 * guest::family::audio tests compile tests/audio_probe.cpp against this
 * header and check every slot of its vtables against the manifest, by
 * name, `const` and parameters. The sizes asserted below are the
 * manifest's, each from the class's MetaClass constructor. Each class
 * declares its reserved methods first, as the collection lays them out:
 * the ones a release used, then the ones still reserved, then the rest.
 *
 * IOAudioEngine's two pure virtual slots, 333 and 389, have no name in the
 * collection; Apple's AppleUSBAudioEngine, which derives from IOAudioEngine
 * in the same collection, names them getCurrentSampleFrame() and
 * driverDesiresHiResSampleIntervals() (the manifest's `= name`).
 *
 * Markings:
 * - [kc]: read from 26B123's code (the disassembly of IOAudioFamily and
 *   AppleUSBAudio): the layouts of IOAudioStreamFormat and its extension
 *   (IOAudioStream::createDictionaryFromFormat reads each field at these
 *   offsets and widths), the four-character codes AppleUSBAudio and the
 *   family pass ('lpcm', 'sint', 'outp', 'vlme', 'mute'), that
 *   IOAudioDevice::start calls initHardware (and, managing power, joins its
 *   provider's power tree), that output is direction 0, and
 *   IOAF_Float32ToNativeInt16's saturating conversion.
 * - [mem]: from memory of the open-source IOAudioFamily headers alone:
 *   every return type (a slot's mangled name carries none), the enums'
 *   values, the other structures' fields and the default arguments. The
 *   slot check sees parameters only. StelaVirtIOSound uses few of them.
 */
#pragma once

#include <IOKit/IOBufferMemoryDescriptor.h>
#include <IOKit/IOCommandGate.h>
#include <IOKit/IOService.h>
#include <IOKit/IOUserClient.h>
#include <IOKit/IOWorkLoop.h>
#include <libkern/OSTypes.h>

class IOAudioControl;
class IOAudioControlUserClient;
class IOAudioDevice;
class IOAudioEngine;
class IOAudioEngineUserClient;
class IOAudioPort;
class IOAudioStream;
class IOTimerEventSource;
struct IOAudioClientBuffer;
struct IOAudioStreamFormatDesc;
struct IOAudioEngineStatus;

/* IOAudioTypes.h */

typedef struct _IOAudioSampleRate {
	UInt32 whole;
	UInt32 fraction;
} IOAudioSampleRate; /* [mem] */

/* [kc] UInt32 x3 at 0, 4, 8; UInt8 x5 at 0xc-0x10; UInt32 at 0x14. */
typedef struct _IOAudioStreamFormat {
	UInt32 fNumChannels;
	UInt32 fSampleFormat;
	UInt32 fNumericRepresentation;
	UInt8 fBitDepth;
	UInt8 fBitWidth;
	UInt8 fAlignment;
	UInt8 fByteOrder;
	UInt8 fIsMixable;
	UInt32 fDriverTag;
} IOAudioStreamFormat;
static_assert(sizeof(IOAudioStreamFormat) == 24, "as createDictionaryFromFormat reads it");

/* [kc] UInt32 x4 at 0, 4, 8, 0xc. */
typedef struct _IOAudioStreamFormatExtension {
	UInt32 fVersion;
	UInt32 fFlags;
	UInt32 fFramesPerPacket;
	UInt32 fBytesPerPacket;
} IOAudioStreamFormatExtension;
static_assert(sizeof(IOAudioStreamFormatExtension) == 16, "as createDictionaryFromFormat reads it");

typedef struct _IOAudioTimeStamp {
	UInt64 sampleTime; /* [mem] */
	UInt64 hostTime;
	UInt64 rateScalar;
	UInt32 flags;
} IOAudioTimeStamp;

typedef struct IOAudioEnginePosition {
	UInt32 fLoopCount; /* [mem] */
	UInt32 fSampleFrame;
} IOAudioEnginePosition;

typedef enum _IOAudioStreamDirection {
	/* [kc] addAudioStream files direction 0 in the set performErase erases. */
	kIOAudioStreamDirectionOutput = 0,
	kIOAudioStreamDirectionInput = 1
} IOAudioStreamDirection;

typedef enum _IOAudioEngineState {
	kIOAudioEngineStopped = 0, /* [mem] */
	kIOAudioEngineRunning = 1,
	kIOAudioEnginePaused = 2,
	kIOAudioEngineResumed = 3
} IOAudioEngineState;

typedef enum _IOAudioDevicePowerState {
	kIOAudioDeviceSleep = 0, /* [mem] */
	kIOAudioDeviceIdle = 1,
	kIOAudioDeviceActive = 2
} IOAudioDevicePowerState;

/* [kc] the codes AppleUSBAudio passes; their names [mem]. */
enum : UInt32 {
	kIOAudioStreamSampleFormatLinearPCM = 'lpcm',
	kIOAudioStreamNumericRepresentationSignedInt = 'sint',
	kIOAudioControlUsageOutput = 'outp',
	kIOAudioLevelControlSubTypeVolume = 'vlme',
	kIOAudioToggleControlSubTypeMute = 'mute',
};

/* [mem] */
enum : UInt8 {
	kIOAudioStreamAlignmentLowByte = 0,
	kIOAudioStreamAlignmentHighByte = 1,
	kIOAudioStreamByteOrderBigEndian = 0,
	kIOAudioStreamByteOrderLittleEndian = 1,
};

/* [mem] */
#define kIOAudioControlChannelIDAll 0
#define kIOAudioControlChannelNameAll "All Channels"
#define kIOAudioNewClockDomain 0xFFFFFFFF

/* IOAudioStream.h: what a stream calls to clip or convert its samples. */
typedef IOReturn (*AudioIOFunction)(const void *mixBuf, void *sampleBuf, UInt32 firstSampleFrame,
    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);

/*
 * IOAudioFamily's own float to 16-bit conversion, with saturation: what a
 * clipOutputSamples calls rather than float code of its own [kc: NEON in
 * IOAudioFamily, `__Z25IOAF_Float32ToNativeInt16PKfPsj`]. Its return type
 * [mem].
 */
void IOAF_Float32ToNativeInt16(const float *src, SInt16 *dst, UInt32 count);

class IOAudioDevice : public IOService {
	OSDeclareDefaultStructors(IOAudioDevice);

public:
	typedef void (*TimerEvent)(OSObject *target, IOAudioDevice *audioDevice);

protected:
	/* Reserved slots 0-5, used. */
	virtual void setDeviceTransportType(const UInt32 transportType);
	virtual IOReturn setIdleAudioSleepTime(unsigned long long sleepDelay);
	virtual void scheduleIdleAudioSleep(void);
	virtual void setConfigurationApplicationBundle(const char *bundleID);
	virtual void setDeviceCanBeDefault(UInt32 defaultsFlags);
	virtual void setDeviceModelName(const char *modelName);

private:
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 6);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 7);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 8);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 9);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 10);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 11);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 12);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 13);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 14);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 15);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 16);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 17);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 18);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 19);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 20);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 21);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 22);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 23);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 24);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 25);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 26);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 27);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 28);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 29);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 30);
	OSMetaClassDeclareReservedUnused(IOAudioDevice, 31);

public:
	/* IOService's, overridden. */
	bool init(OSDictionary *properties) override;
	void free() override;
	bool willTerminate(IOService *provider, IOOptionBits options) override;
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	IOWorkLoop *getWorkLoop() const override;
	IOReturn setAggressiveness(unsigned long type, unsigned long newLevel) override;
	IOReturn setPowerState(unsigned long powerStateOrdinal, IOService *device) override;
	using IOService::init;

	virtual bool initHardware(IOService *provider);
	virtual void setDeviceName(const char *deviceName);
	virtual void setDeviceShortName(const char *shortName);
	virtual void setManufacturerName(const char *manufacturerName);
	virtual IOCommandGate *getCommandGate() const;
	virtual IOReturn activateAudioEngine(IOAudioEngine *audioEngine);
	virtual IOReturn activateAudioEngine(IOAudioEngine *audioEngine, bool shouldStartAudioEngine);
	virtual void deactivateAllAudioEngines();
	virtual void setFamilyManagePower(bool manage);
	virtual IOReturn protectedSetPowerState(unsigned long powerStateOrdinal, IOService *device);
	virtual IOReturn performPowerStateChange(IOAudioDevicePowerState oldPowerState,
	    IOAudioDevicePowerState newPowerState, UInt32 *microsecondsUntilComplete);
	virtual IOReturn completePowerStateChange();
	virtual IOReturn protectedCompletePowerStateChange();
	virtual IOAudioDevicePowerState getPowerState();
	virtual IOAudioDevicePowerState getPendingPowerState();
	virtual void waitForPendingPowerStateChange();
	virtual IOReturn initiatePowerStateChange(UInt32 *microsecondsUntilComplete = NULL);
	virtual IOReturn flushAudioControls();
	virtual IOReturn addTimerEvent(OSObject *target, TimerEvent event, AbsoluteTime interval);
	virtual void removeTimerEvent(OSObject *target);
	virtual void removeAllTimerEvents();
	virtual IOReturn attachAudioPort(IOAudioPort *port, IORegistryEntry *parent, IORegistryEntry *child);
	virtual void detachAllAudioPorts();
	virtual void dispatchTimerEvents(bool force);
	virtual void audioEngineStarting();
	virtual void audioEngineStopped();

protected:
	uint8_t stelaIOAudioDeviceFields[0xe0 - 0x88];
};
static_assert(sizeof(IOAudioDevice) == 0xe0, "IOAudioDevice's size in 26B123");

class IOAudioEngine : public IOService {
	OSDeclareAbstractStructors(IOAudioEngine);

public:
	/* Reserved slots 0-16, used. */
	virtual IOReturn performFormatChange(IOAudioStream *audioStream, const IOAudioStreamFormat *newFormat,
	    const IOAudioStreamFormatExtension *formatExtension, const IOAudioSampleRate *newSampleRate);
	virtual IOBufferMemoryDescriptor *getStatusDescriptor();
	virtual IOReturn getNearestStartTime(IOAudioStream *audioStream, IOAudioTimeStamp *ioTimeStamp, bool isInput);
	virtual IOBufferMemoryDescriptor *getBytesInInputBufferArrayDescriptor();
	virtual IOBufferMemoryDescriptor *getBytesInOutputBufferArrayDescriptor();
	virtual IOReturn eraseOutputSamples(const void *mixBuf, void *sampleBuf, UInt32 firstSampleFrame,
	    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);
	virtual void setClockIsStable(bool clockIsStable);
	virtual void setMixClipOverhead(UInt32 newMixClipOverhead);
	virtual void setClockDomain(UInt32 clockDomain = kIOAudioNewClockDomain);
	virtual IOReturn convertInputSamplesVBR(const void *sampleBuf, void *destBuf, UInt32 firstSampleFrame,
	    UInt32 &numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);
	virtual void setInputSampleOffset(UInt32 numSamples);
	virtual void setOutputSampleOffset(UInt32 numSamples);
	virtual IOReturn createUserClient(task_t task, void *securityID, UInt32 type,
	    IOAudioEngineUserClient **newUserClient, OSDictionary *properties);
	virtual IOReturn setAttributeForConnection(SInt32 connectIndex, UInt32 attribute, uintptr_t value);
	virtual IOReturn getAttributeForConnection(SInt32 connectIndex, UInt32 attribute, uintptr_t *value);
	virtual IOReturn calculateSampleTimeoutHiRes(UInt64 sampleIntervalHi, UInt32 numSampleFrames,
	    IOAudioEnginePosition *startingPosition, AbsoluteTime *wakeupTime);
	virtual bool useHiResSampleInterval(void);

private:
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 17);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 18);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 19);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 20);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 21);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 22);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 23);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 24);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 25);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 26);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 27);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 28);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 29);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 30);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 31);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 32);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 33);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 34);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 35);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 36);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 37);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 38);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 39);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 40);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 41);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 42);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 43);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 44);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 45);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 46);
	OSMetaClassDeclareReservedUnused(IOAudioEngine, 47);

public:
	/* IOService's, overridden. */
	bool init(OSDictionary *properties) override;
	void free() override;
	void registerService(IOOptionBits options = 0) override;
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	IOWorkLoop *getWorkLoop() const override;
	IOReturn newUserClient(task_t task, void *securityID, UInt32 type, OSDictionary *properties,
	    IOUserClient **handler) override;
	IOReturn newUserClient(task_t task, void *securityID, UInt32 type, IOUserClient **handler) override;
	using IOService::init;

	virtual IOCommandGate *getCommandGate() const;
	virtual bool start(IOService *provider, IOAudioDevice *device);
	virtual bool initHardware(IOService *provider);
	virtual void setAudioDevice(IOAudioDevice *device);
	virtual void setIndex(UInt32 index);
	virtual void setDescription(const char *description);
	virtual IOReturn addAudioStream(IOAudioStream *stream);
	virtual IOAudioStream *getAudioStream(IOAudioStreamDirection direction, UInt32 channelID);
	virtual void lockAllStreams();
	virtual void unlockAllStreams();
	virtual void updateChannelNumbers();
	virtual void resetStatusBuffer();
	virtual void clearAllSampleBuffers();
	/* Slot 333, pure: the sample frame the hardware is at in the buffer. */
	virtual UInt32 getCurrentSampleFrame() = 0;
	virtual IOReturn startAudioEngine();
	virtual IOReturn stopAudioEngine();
	virtual IOReturn pauseAudioEngine();
	virtual IOReturn resumeAudioEngine();
	virtual IOReturn performAudioEngineStart();
	virtual IOReturn performAudioEngineStop();
	virtual IOAudioEngineState getState();
	virtual const IOAudioSampleRate *getSampleRate();
	virtual IOReturn hardwareSampleRateChanged(const IOAudioSampleRate *sampleRate);
	virtual bool getRunEraseHead();
	virtual const IOAudioEngineStatus *getStatus();
	virtual void timerFired();
	virtual AbsoluteTime getTimerInterval();
	virtual void performErase();
	virtual void performFlush();
	virtual void stopEngineAtPosition(IOAudioEnginePosition *endingPosition);
	virtual IOReturn mixOutputSamples(const void *sourceBuf, void *mixBuf, UInt32 firstSampleFrame,
	    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);
	virtual IOReturn clipOutputSamples(const void *mixBuf, void *sampleBuf, UInt32 firstSampleFrame,
	    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);
	virtual void resetClipPosition(IOAudioStream *audioStream, UInt32 clipSampleFrame);
	virtual IOReturn convertInputSamples(const void *sampleBuf, void *destBuf, UInt32 firstSampleFrame,
	    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);
	virtual void takeTimeStamp(bool incrementLoopCount = true, AbsoluteTime *timestamp = NULL);
	virtual IOReturn getLoopCountAndTimeStamp(UInt32 *loopCount, AbsoluteTime *timestamp);
	virtual IOReturn calculateSampleTimeout(AbsoluteTime *sampleInterval, UInt32 numSampleFrames,
	    IOAudioEnginePosition *startingPosition, AbsoluteTime *wakeupTime);
	virtual IOReturn performFormatChange(IOAudioStream *audioStream, const IOAudioStreamFormat *newFormat,
	    const IOAudioSampleRate *newSampleRate);
	virtual void beginConfigurationChange();
	virtual void completeConfigurationChange();
	virtual void cancelConfigurationChange();
	virtual IOReturn addDefaultAudioControl(IOAudioControl *defaultAudioControl);
	virtual IOReturn removeDefaultAudioControl(IOAudioControl *defaultAudioControl);
	virtual void removeAllDefaultAudioControls();
	virtual OSString *getGlobalUniqueID();
	virtual OSString *getLocalUniqueID();
	virtual void setNumSampleFramesPerBuffer(UInt32 numSampleFrames);
	virtual UInt32 getNumSampleFramesPerBuffer();
	virtual IOAudioEngineState setState(IOAudioEngineState newState);
	virtual void setSampleRate(const IOAudioSampleRate *newSampleRate);
	virtual void setSampleLatency(UInt32 numSamples);
	virtual void setOutputSampleLatency(UInt32 numSamples);
	virtual void setInputSampleLatency(UInt32 numSamples);
	virtual void setSampleOffset(UInt32 numSamples);
	virtual void setRunEraseHead(bool runEraseHead);
	virtual void clientClosed(IOAudioEngineUserClient *client);
	virtual void addTimer();
	virtual void removeTimer();
	virtual void sendFormatChangeNotification(IOAudioStream *audioStream);
	virtual void sendNotification(UInt32 notificationType);
	virtual IOReturn createUserClient(task_t task, void *securityID, UInt32 type,
	    IOAudioEngineUserClient **newUserClient);
	virtual IOReturn addUserClient(IOAudioEngineUserClient *newUserClient);
	virtual IOReturn removeUserClient(IOAudioEngineUserClient *userClient);
	virtual IOReturn detachUserClients();
	virtual IOReturn startClient(IOAudioEngineUserClient *userClient);
	virtual IOReturn stopClient(IOAudioEngineUserClient *userClient);
	virtual IOReturn incrementActiveUserClients();
	virtual IOReturn decrementActiveUserClients();
	virtual void detachAudioStreams();
	/*
	 * Slot 389, pure: whether the engine's sample intervals are computed in
	 * high resolution. AppleUSBAudioEngine answers true [kc].
	 */
	virtual bool driverDesiresHiResSampleIntervals(void) = 0;

protected:
	uint8_t stelaIOAudioEngineFields[0x118 - 0x88];
};
static_assert(sizeof(IOAudioEngine) == 0x118, "IOAudioEngine's size in 26B123");

class IOAudioStream : public IOService {
	OSDeclareDefaultStructors(IOAudioStream);

public:
	/* Reserved slots 0-11, used. */
	virtual const IOAudioStreamFormatExtension *getFormatExtension();
	virtual IOReturn setFormat(const IOAudioStreamFormat *streamFormat,
	    const IOAudioStreamFormatExtension *formatExtension, bool callDriver = true);
	virtual IOReturn setFormat(const IOAudioStreamFormat *streamFormat,
	    const IOAudioStreamFormatExtension *formatExtension, OSDictionary *formatDict, bool callDriver = true);
	virtual void addAvailableFormat(const IOAudioStreamFormat *streamFormat,
	    const IOAudioStreamFormatExtension *formatExtension, const IOAudioSampleRate *minRate,
	    const IOAudioSampleRate *maxRate, const AudioIOFunction *ioFunctionList, UInt32 numFunctions);
	virtual void addAvailableFormat(const IOAudioStreamFormat *streamFormat,
	    const IOAudioStreamFormatExtension *formatExtension, const IOAudioSampleRate *minRate,
	    const IOAudioSampleRate *maxRate, AudioIOFunction ioFunction = NULL);
	virtual bool validateFormat(IOAudioStreamFormat *streamFormat, IOAudioStreamFormatExtension *formatExtension,
	    IOAudioStreamFormatDesc *formatDesc);
	virtual void setTerminalType(const UInt32 terminalType);
	virtual IOReturn mixOutputSamples(const void *sourceBuf, void *mixBuf, UInt32 firstSampleFrame,
	    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream);
	virtual void setSampleLatency(UInt32 numSamples);
	virtual bool validateFormat(IOAudioStreamFormat *streamFormat, IOAudioStreamFormatExtension *formatExtension,
	    IOAudioStreamFormatDesc *formatDesc, const IOAudioSampleRate *sampleRate);
	virtual UInt32 getNumSampleFramesRead();
	virtual void setDefaultNumSampleFramesRead(UInt32 requestedNumSampleFrames);

private:
	OSMetaClassDeclareReservedUnused(IOAudioStream, 12);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 13);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 14);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 15);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 16);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 17);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 18);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 19);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 20);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 21);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 22);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 23);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 24);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 25);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 26);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 27);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 28);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 29);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 30);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 31);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 32);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 33);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 34);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 35);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 36);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 37);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 38);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 39);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 40);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 41);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 42);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 43);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 44);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 45);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 46);
	OSMetaClassDeclareReservedUnused(IOAudioStream, 47);

public:
	/* IOService's, overridden. */
	void free() override;
	IOReturn setProperties(OSObject *properties) override;
	void stop(IOService *provider) override;
	IOWorkLoop *getWorkLoop() const override;

	virtual bool initWithAudioEngine(IOAudioEngine *engine, IOAudioStreamDirection dir, UInt32 startChannelID,
	    const char *streamDescription = NULL, OSDictionary *properties = 0);
	virtual IOAudioStreamDirection getDirection();
	virtual void setSampleBuffer(void *buffer, UInt32 size);
	virtual void *getSampleBuffer();
	virtual UInt32 getSampleBufferSize();
	virtual void setMixBuffer(void *mixBuf, UInt32 size);
	virtual void *getMixBuffer();
	virtual UInt32 getMixBufferSize();
	virtual void numSampleFramesPerBufferChanged();
	virtual void clearSampleBuffer();
	virtual void setIOFunction(AudioIOFunction ioFunction);
	virtual void setIOFunctionList(const AudioIOFunction *ioFunctionList, UInt32 numFunctions);
	virtual const IOAudioStreamFormat *getFormat();
	virtual IOReturn setFormat(const IOAudioStreamFormat *streamFormat, bool callDriver = true);
	virtual IOReturn setFormat(OSDictionary *formatDict);
	virtual IOReturn setFormat(const IOAudioStreamFormat *streamFormat, OSDictionary *formatDict,
	    bool callDriver = true);
	virtual IOReturn hardwareFormatChanged(const IOAudioStreamFormat *streamFormat);
	virtual void addAvailableFormat(const IOAudioStreamFormat *streamFormat, const IOAudioSampleRate *minRate,
	    const IOAudioSampleRate *maxRate, const AudioIOFunction *ioFunctionList, UInt32 numFunctions);
	virtual void addAvailableFormat(const IOAudioStreamFormat *streamFormat, const IOAudioSampleRate *minRate,
	    const IOAudioSampleRate *maxRate, AudioIOFunction ioFunction = NULL);
	virtual void clearAvailableFormats();
	virtual bool validateFormat(IOAudioStreamFormat *streamFormat, IOAudioStreamFormatDesc *formatDesc);
	virtual UInt32 getStartingChannelID();
	virtual UInt32 getMaxNumChannels();
	virtual void setStreamAvailable(bool available);
	virtual bool getStreamAvailable();
	virtual IOReturn addDefaultAudioControl(IOAudioControl *defaultAudioControl);
	virtual void removeDefaultAudioControls();
	virtual void lockStreamForIO();
	virtual void unlockStreamForIO();
	virtual void updateNumClients();
	virtual IOReturn addClient(IOAudioClientBuffer *clientBuffer);
	virtual void removeClient(IOAudioClientBuffer *clientBuffer);
	virtual UInt32 getNumClients();
	virtual IOReturn processOutputSamples(IOAudioClientBuffer *clientBuffer, UInt32 firstSampleFrame,
	    UInt32 loopCount, bool samplesAvailable);
	virtual IOReturn readInputSamples(IOAudioClientBuffer *clientBuffer, UInt32 firstSampleFrame);
	virtual void resetClipInfo();
	virtual void clipIfNecessary();
	virtual void clipOutputSamples(UInt32 numSamples, UInt32 numFrames);
	virtual void setStartingChannelNumber(UInt32 channelNumber);
	virtual void setDirection(IOAudioStreamDirection dir);

protected:
	uint8_t stelaIOAudioStreamFields[0x150 - 0x88];
};
static_assert(sizeof(IOAudioStream) == 0x150, "IOAudioStream's size in 26B123");

class IOAudioControl : public IOService {
	OSDeclareDefaultStructors(IOAudioControl);

public:
	typedef IOReturn (*IntValueChangeHandler)(OSObject *target, IOAudioControl *audioControl, SInt32 oldValue,
	    SInt32 newValue);
	typedef IOReturn (*DataValueChangeHandler)(OSObject *target, IOAudioControl *audioControl,
	    const void *oldData, UInt32 oldDataSize, const void *newData, UInt32 newDataSize);
	typedef IOReturn (*ObjectValueChangeHandler)(OSObject *target, IOAudioControl *audioControl,
	    OSObject *oldValue, OSObject *newValue);

	/* Reserved slots 0-3, used. */
	virtual void sendChangeNotification(UInt32 notificationType);
	virtual void setReadOnlyFlag();
	virtual void sendQueuedNotifications(void);
	virtual IOReturn createUserClient(task_t task, void *securityID, UInt32 taggedType,
	    IOAudioControlUserClient **newUserClient, OSDictionary *properties);

private:
	OSMetaClassDeclareReservedUnused(IOAudioControl, 4);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 5);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 6);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 7);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 8);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 9);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 10);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 11);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 12);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 13);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 14);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 15);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 16);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 17);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 18);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 19);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 20);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 21);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 22);
	OSMetaClassDeclareReservedUnused(IOAudioControl, 23);

public:
	/* IOService's, overridden. */
	void free() override;
	IOReturn setProperties(OSObject *properties) override;
	bool start(IOService *provider) override;
	void stop(IOService *provider) override;
	IOReturn newUserClient(task_t task, void *securityID, UInt32 type, OSDictionary *properties,
	    IOUserClient **handler) override;
	IOReturn newUserClient(task_t task, void *securityID, UInt32 type, IOUserClient **handler) override;
	using IOService::getWorkLoop;
	using IOService::init;

	virtual bool init(UInt32 type, OSObject *initialValue, UInt32 channelID, const char *channelName = 0,
	    UInt32 cntrlID = 0, UInt32 subType = 0, UInt32 usage = 0, OSDictionary *properties = 0);
	virtual bool attachAndStart(IOService *provider);
	virtual bool getIsStarted();
	/* Not const, unlike IOService's: a slot of its own. */
	virtual IOWorkLoop *getWorkLoop();
	virtual IOCommandGate *getCommandGate();
	virtual IOReturn createUserClient(task_t task, void *securityID, UInt32 type,
	    IOAudioControlUserClient **newUserClient);
	virtual void clientClosed(IOAudioControlUserClient *client);
	virtual void setValueChangeHandler(IntValueChangeHandler intValueChangeHandler, OSObject *target);
	virtual void setValueChangeHandler(DataValueChangeHandler dataValueChangeHandler, OSObject *target);
	virtual void setValueChangeHandler(ObjectValueChangeHandler objectValueChangeHandler, OSObject *target);
	virtual void setValueChangeTarget(OSObject *target);
	virtual IOReturn flushValue();
	virtual IOReturn setValue(OSObject *newValue);
	virtual IOReturn setValue(SInt32 intValue);
	virtual IOReturn hardwareValueChanged(OSObject *newValue);
	virtual OSObject *getValue();
	virtual SInt32 getIntValue();
	virtual const void *getDataBytes();
	virtual UInt32 getDataLength();
	virtual UInt32 getControlID();
	virtual UInt32 getChannelID();
	virtual UInt32 getType();
	virtual UInt32 getSubType();
	virtual UInt32 getUsage();
	virtual void setCoreAudioPropertyID(UInt32 propertyID);
	virtual void sendValueChangeNotification();
	virtual void setChannelName(const char *channelName);
	virtual void setChannelID(UInt32 newChannelID);
	virtual void setChannelNumber(SInt32 channelNumber);
	virtual void setType(UInt32 type);
	virtual void setSubType(UInt32 subType);
	virtual void setUsage(UInt32 usage);
	virtual void setControlID(UInt32 cntrlID);
	virtual IOReturn validateValue(OSObject *value);
	virtual IOReturn updateValue(OSObject *newValue);
	virtual IOReturn _setValue(OSObject *newValue);
	virtual IOReturn performValueChange(OSObject *newValue);
	virtual IOReturn addUserClient(IOAudioControlUserClient *newUserClient);
	virtual IOReturn removeUserClient(IOAudioControlUserClient *userClient);
	virtual IOReturn detachUserClients();

protected:
	uint8_t stelaIOAudioControlFields[0xe0 - 0x88];
};
static_assert(sizeof(IOAudioControl) == 0xe0, "IOAudioControl's size in 26B123");

class IOAudioLevelControl : public IOAudioControl {
	OSDeclareDefaultStructors(IOAudioLevelControl);

public:
	/* A volume control from minValue to maxValue, minDB to maxDB (16.16). */
	static IOAudioLevelControl *createVolumeControl(SInt32 initialValue, SInt32 minValue, SInt32 maxValue,
	    IOFixed minDB, IOFixed maxDB, UInt32 channelID, const char *channelName = 0, UInt32 cntrlID = 0,
	    UInt32 usage = 0);

	/* Reserved slot 0, used. */
	virtual void setLinearScale(bool useLinearScale);

private:
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 1);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 2);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 3);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 4);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 5);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 6);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 7);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 8);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 9);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 10);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 11);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 12);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 13);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 14);
	OSMetaClassDeclareReservedUnused(IOAudioLevelControl, 15);

public:
	/* IOService's and IOAudioControl's, overridden. */
	void free() override;
	IOReturn validateValue(OSObject *value) override;
	using IOAudioControl::init;

	virtual bool init(SInt32 initialValue, SInt32 minValue, SInt32 maxValue, IOFixed minDB, IOFixed maxDB,
	    UInt32 channelID, const char *channelName = 0, UInt32 cntrlID = 0, UInt32 subType = 0, UInt32 usage = 0,
	    OSDictionary *properties = 0);
	virtual void setMinValue(SInt32 minValue);
	virtual SInt32 getMinValue();
	virtual void setMaxValue(SInt32 maxValue);
	virtual SInt32 getMaxValue();
	virtual void setMinDB(IOFixed minDB);
	virtual IOFixed getMinDB();
	virtual void setMaxDB(IOFixed maxDB);
	virtual IOFixed getMaxDB();
	virtual IOReturn addRange(SInt32 minValue, SInt32 maxValue, IOFixed minDB, IOFixed maxDB);
	virtual IOReturn addNegativeInfinity(SInt32 negativeInfinityValue);

protected:
	uint8_t stelaIOAudioLevelControlFields[0x100 - 0xe0];
};
static_assert(sizeof(IOAudioLevelControl) == 0x100, "IOAudioLevelControl's size in 26B123");

class IOAudioToggleControl : public IOAudioControl {
	OSDeclareDefaultStructors(IOAudioToggleControl);

public:
	/* A mute control, its value 1 while muted. */
	static IOAudioToggleControl *createMuteControl(bool initialValue, UInt32 channelID,
	    const char *channelName = 0, UInt32 cntrlID = 0, UInt32 usage = 0);

private:
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 0);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 1);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 2);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 3);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 4);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 5);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 6);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 7);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 8);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 9);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 10);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 11);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 12);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 13);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 14);
	OSMetaClassDeclareReservedUnused(IOAudioToggleControl, 15);

public:
	using IOAudioControl::init;

	virtual bool init(bool initialValue, UInt32 channelID, const char *channelName = 0, UInt32 cntrlID = 0,
	    UInt32 subType = 0, UInt32 usage = 0, OSDictionary *properties = 0);

protected:
	uint8_t stelaIOAudioToggleControlFields[0xe8 - 0xe0];
};
static_assert(sizeof(IOAudioToggleControl) == 0xe8, "IOAudioToggleControl's size in 26B123");
