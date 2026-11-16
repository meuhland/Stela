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

#include "stela_pcm.h"

class StelaVirtIOSoundEngine;

class StelaVirtIOSound : public IOAudioDevice {
	OSDeclareDefaultStructors(StelaVirtIOSound);

public:
	/* The stream, as SET_PARAMS sets it (stela_pcm.h). */
	static const uint32_t kRate = STELA_PCM_RATE;
	static const uint32_t kChannels = STELA_PCM_CHANNELS;
	static const uint32_t kFrameBytes = STELA_PCM_FRAME_BYTES;
	static const uint32_t kPeriodFrames = STELA_PCM_PERIOD_FRAMES;
	static const uint32_t kPeriods = STELA_PCM_PERIODS;
	static const uint32_t kBufferFrames = STELA_PCM_BUFFER_FRAMES;
	static const uint32_t kPeriodBytes = STELA_PCM_PERIOD_BYTES;
	static const uint32_t kBufferBytes = STELA_PCM_BUFFER_BYTES;

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
	/* The engine's clipOutputSamples: stela_pcm_clip at the output's volume and mute, metered. */
	IOReturn clip(const void *mixBuf, void *sampleBuf, UInt32 firstSampleFrame, UInt32 numSampleFrames);
	/* The volume control's step (0 to STELA_PCM_VOLUME_STEPS) and the mute control's value. */
	void setVolumeStep(SInt32 step);
	void setMuted(bool muted);

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
	/*
	 * The output: its gain (16.16), volume step and mute, set by the
	 * controls and read by clip. The sample path's meters, for the boot's
	 * checks: clip's calls, the mix's peak (|x| x 32768) clip saw and the
	 * sample buffer's peak in the periods the device read, both since the
	 * last publishCounters. Atomics: clip runs on the family's IO threads,
	 * the completions on the transport's work loop.
	 */
	uint32_t outputGain = STELA_PCM_UNITY;
	int32_t volumeStep = STELA_PCM_VOLUME_STEPS;
	bool outputMuted = false;
	uint64_t clipCalls = 0;
	uint64_t clipErrors = 0;
	uint32_t mixPeak = 0;
	uint32_t samplePeak = 0;
	/* The same over the stream, for its line at stop. */
	uint64_t streamClipCalls = 0;
	uint32_t streamMixPeak = 0;
	uint32_t streamSamplePeak = 0;
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
};
