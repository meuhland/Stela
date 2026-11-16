/*
 * StelaVirtIOSound: the virtio sound device (ID 25) as an IOAudioFamily
 * device with one output engine, 48 kHz stereo 16-bit.
 *
 *   StelaVirtIODevice (VirtIODeviceID 25)
 *    └─ StelaVirtIOSound : IOAudioDevice        the virtio side: queues, control
 *        └─ StelaVirtIOSoundEngine : IOAudioEngine
 *            ├─ IOAudioStream (output, the sample buffer)
 *            └─ volume and mute controls
 *
 * The sample buffer is kPeriods periods of kPeriodFrames frames in one wired
 * page. Every period is a TX transfer the device holds; the device reads a
 * period as its own clock reaches it, and completes it once its clock passed
 * the period's last frame. Each completion moves the engine's position to the
 * next period and sends the period back for the buffer's next loop; the last
 * period's completion is the buffer's wrap, which takes the engine's time
 * stamp. The engine's clock is so the host's: see virtio-k5.md.
 */
#pragma once

#include "StelaVirtIO.h"

#include <IOKit/IOLocks.h>
#include <audio/IOAudioFamily.h>

class StelaVirtIOSoundEngine;

class StelaVirtIOSound : public IOAudioDevice {
	OSDeclareDefaultStructors(StelaVirtIOSound);

public:
	/* The stream, as SET_PARAMS sets it. */
	static const uint32_t kRate = 48000;
	static const uint32_t kChannels = 2;
	static const uint32_t kFrameBytes = 4;
	static const uint32_t kPeriodFrames = 480;
	static const uint32_t kPeriods = 8;
	static const uint32_t kBufferFrames = kPeriodFrames * kPeriods;
	static const uint32_t kPeriodBytes = kPeriodFrames * kFrameBytes;
	static const uint32_t kBufferBytes = kBufferFrames * kFrameBytes;

	bool init(OSDictionary *properties) override;
	bool initHardware(IOService *provider) override;
	void stop(IOService *provider) override;
	void free() override;
	using IOAudioDevice::init;

	/* For the engine. */
	void *sampleBuffer() const;
	/* The frame the device's clock is at in the buffer: the next period's first. */
	uint32_t currentSampleFrame() const { return __atomic_load_n(&currentFrame, __ATOMIC_ACQUIRE); }
	/* Every period to the device, then START. */
	bool startStream(StelaVirtIOSoundEngine *engine);
	/* STOP, then RELEASE and every period back, then PREPARE for the next start. */
	void stopStream();

private:
	/* One control request and its answer's status; false when none came. */
	bool control(const void *request, uint32_t requestBytes, void *response, uint32_t responseBytes,
	    uint32_t *status);
	bool pcm(uint32_t code);
	bool describe();
	bool configure();
	/* With queueLock held. */
	bool post(unsigned period);
	/* The TX queue's completions; from the interrupt, or stopStream. */
	void harvest();
	void publishCounters();
	/* Takes the interrupt back and resets the device: it uses nothing after. */
	void abandon();
	static void interruptOccurred(OSObject *owner, uint32_t status);

	StelaVirtIODevice *nub = nullptr;
	uint64_t features = 0;
	StelaVirtIOQueue controlQueue = {};
	StelaVirtIOQueue txQueue = {};
	/* The control messages, each period's header and status. */
	IOBufferMemoryDescriptor *messages = nullptr;
	uint64_t messagesPhysical = 0;
	IOBufferMemoryDescriptor *samples = nullptr;
	uint64_t samplesPhysical = 0;
	/* One control request at a time; every queue operation. */
	IOLock *controlLock = nullptr;
	IOLock *queueLock = nullptr;
	StelaVirtIOSoundEngine *engine = nullptr;
	/* The device is set up and not reset; the stream runs. */
	bool live = false;
	bool running = false;
	/* The transport hands this driver its interrupts. */
	bool handling = false;
	/* The device has this driver's features, and may have its queues. */
	bool negotiated = false;
	uint32_t currentFrame = 0;
	uint8_t tokens[kPeriods] = {};
	bool held[kPeriods] = {};
	uint64_t completions = 0;
	uint64_t wraps = 0;
	uint64_t transferErrors = 0;
	uint64_t starts = 0;
	uint64_t controlFailures = 0;
	uint32_t lastLatencyBytes = 0;
};

class StelaVirtIOSoundEngine : public IOAudioEngine {
	OSDeclareDefaultStructors(StelaVirtIOSoundEngine);

public:
	bool initWithDevice(StelaVirtIOSound *device);
	bool initHardware(IOService *provider) override;
	UInt32 getCurrentSampleFrame() override;
	bool driverDesiresHiResSampleIntervals(void) override;
	IOReturn performAudioEngineStart() override;
	IOReturn performAudioEngineStop() override;
	IOReturn clipOutputSamples(const void *mixBuf, void *sampleBuf, UInt32 firstSampleFrame,
	    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *audioStream) override;
	using IOAudioEngine::init;
	using IOAudioEngine::start;

	/* The device's clock passed the buffer's end. */
	void wrapped();

private:
	static IOReturn volumeChanged(OSObject *target, IOAudioControl *control, SInt32 oldValue, SInt32 newValue);
	static IOReturn muteChanged(OSObject *target, IOAudioControl *control, SInt32 oldValue, SInt32 newValue);

	StelaVirtIOSound *device = nullptr;
	/* The output's gain, 16.16, and mute. */
	uint32_t gain = 0x10000;
	bool muted = false;
};
