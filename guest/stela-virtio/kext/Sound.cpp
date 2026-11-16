#include "Sound.h"

#include <IOKit/IOLib.h>
#include <stela/virtio.h>
#include <stela/virtio_snd.h>

/*
 * StelaVirtIOSound and its engine (Sound.h). The device's side is plain
 * virtio: the control queue (0) answers one request at a time, polled; the
 * TX queue (2) holds every period of the sample buffer, and its completions
 * come by interrupt on the transport's work loop. The event and RX queues
 * are not set up: the device has no input stream and sends no events.
 *
 * A failure anywhere leaves the device FAILED and the driver unstarted: it
 * never panics the guest, and `ioreg` shows how far it came
 * (StelaVirtIOSound's properties, IOAudioEngine's state).
 */
OSDefineMetaClassAndStructors(StelaVirtIOSound, IOAudioDevice);
OSDefineMetaClassAndStructors(StelaVirtIOSoundEngine, IOAudioEngine);

static const uint32_t kPage = 0x4000;
/* The messages page: a control request, its answer, each period's header and status. */
static const uint32_t kRequestOffset = 0x0;
static const uint32_t kResponseOffset = 0x100;
static const uint32_t kMessageBytes = 0x100;
static const uint32_t kHeaderOffset = 0x1000;
static const uint32_t kStatusOffset = 0x2000;
static const uint16_t kControlEntries = 16;
/* Three descriptors a period. */
static const uint16_t kTxEntries = 32;
/* How long a control request may take: polls of a millisecond. */
static const unsigned kControlPolls = 1000;
/* How long the device may take to give back every period on RELEASE. */
static const unsigned kReleasePolls = 200;
/*
 * How far ahead of the engine's position the HAL writes: the device reads a
 * period as its clock reaches it, and the position (the time stamps) is up
 * to an interrupt's latency late. Three periods: 30 ms.
 */
static const uint32_t kSampleOffsetFrames = 3 * StelaVirtIOSound::kPeriodFrames;
/* Counters are published every this many wraps (about a second). */
static const uint64_t kPublishWraps = 16;
/* The volume control: 0 to kVolumeSteps, a dB each, the top 0 dB. */
static const SInt32 kVolumeSteps = 64;
/* Each step's gain, 16.16: 65536 x 10^((step - 64) / 20). */
static const uint32_t kGain[kVolumeSteps + 1] = {
    41, 46, 52, 58, 66, 74, 83, 93,
    104, 117, 131, 147, 165, 185, 207, 233,
    261, 293, 328, 369, 414, 464, 521, 584,
    655, 735, 825, 926, 1039, 1165, 1308, 1467,
    1646, 1847, 2072, 2325, 2609, 2927, 3285, 3685,
    4135, 4640, 5206, 5841, 6554, 7353, 8250, 9257,
    10387, 11654, 13076, 14672, 16462, 18471, 20724, 23253,
    26090, 29274, 32846, 36854, 41350, 46396, 52057, 58409,
    65536,
};

static_assert(StelaVirtIOSound::kBufferBytes <= kPage, "the sample buffer is one page");
static_assert(StelaVirtIOSound::kPeriods * 3 <= kTxEntries, "every period's transfer fits the TX queue");
static_assert(kHeaderOffset + StelaVirtIOSound::kPeriods * STELA_VIRTIO_SND_PCM_XFER_BYTES <= kStatusOffset,
    "the headers fit before the statuses");
static_assert(kStatusOffset + StelaVirtIOSound::kPeriods * STELA_VIRTIO_SND_PCM_STATUS_BYTES <= kPage,
    "the statuses fit the page");
static_assert(kSampleOffsetFrames + 2 * StelaVirtIOSound::kPeriodFrames <= StelaVirtIOSound::kBufferFrames,
    "a period zeroed at its completion is not one the HAL writes");
static_assert(STELA_VIRTIO_SND_SET_PARAMS_BYTES == 24 && STELA_VIRTIO_SND_PCM_INFO_BYTES == 32,
    "the messages' sizes are virtio_snd.h's");

/* The messages, as stela/virtio_snd.h lays them out. */
struct SndQueryInfo {
	uint32_t code;
	uint32_t startId;
	uint32_t count;
	uint32_t size;
};
static_assert(sizeof(SndQueryInfo) == STELA_VIRTIO_SND_QUERY_INFO_BYTES, "virtio_snd_query_info");

struct SndPcmHeader {
	uint32_t code;
	uint32_t streamId;
};
static_assert(sizeof(SndPcmHeader) == STELA_VIRTIO_SND_PCM_HDR_BYTES, "virtio_snd_pcm_hdr");

struct SndSetParams {
	SndPcmHeader header;
	uint32_t bufferBytes;
	uint32_t periodBytes;
	uint32_t features;
	uint8_t channels;
	uint8_t format;
	uint8_t rate;
	uint8_t padding;
};
static_assert(sizeof(SndSetParams) == STELA_VIRTIO_SND_SET_PARAMS_BYTES, "virtio_snd_pcm_set_params");
static_assert(__builtin_offsetof(SndSetParams, channels) == STELA_VIRTIO_SND_SET_PARAMS_CHANNELS, "channels");
static_assert(__builtin_offsetof(SndSetParams, rate) == STELA_VIRTIO_SND_SET_PARAMS_RATE, "rate");

struct SndPcmInfo {
	uint32_t hdaFnNid;
	uint32_t features;
	uint64_t formats;
	uint64_t rates;
	uint8_t direction;
	uint8_t channelsMin;
	uint8_t channelsMax;
	uint8_t padding[5];
};
static_assert(sizeof(SndPcmInfo) == STELA_VIRTIO_SND_PCM_INFO_BYTES, "virtio_snd_pcm_info");
static_assert(__builtin_offsetof(SndPcmInfo, rates) == STELA_VIRTIO_SND_PCM_INFO_RATES, "rates");
static_assert(__builtin_offsetof(SndPcmInfo, channelsMax) == STELA_VIRTIO_SND_PCM_INFO_CHANNELS_MAX, "channels");

struct SndPcmStatus {
	uint32_t status;
	uint32_t latencyBytes;
};
static_assert(sizeof(SndPcmStatus) == STELA_VIRTIO_SND_PCM_STATUS_BYTES, "virtio_snd_pcm_status");

static const char *
requestName(uint32_t code)
{
	switch (code) {
	case STELA_VIRTIO_SND_R_PCM_INFO: return "PCM_INFO";
	case STELA_VIRTIO_SND_R_PCM_SET_PARAMS: return "PCM_SET_PARAMS";
	case STELA_VIRTIO_SND_R_PCM_PREPARE: return "PCM_PREPARE";
	case STELA_VIRTIO_SND_R_PCM_RELEASE: return "PCM_RELEASE";
	case STELA_VIRTIO_SND_R_PCM_START: return "PCM_START";
	case STELA_VIRTIO_SND_R_PCM_STOP: return "PCM_STOP";
	default: return "a request";
	}
}

/* An allocation of one wired, physically contiguous page. */
static IOBufferMemoryDescriptor *
wiredPage(uint64_t *physical)
{
	auto page = IOBufferMemoryDescriptor::inTaskWithOptions(kernel_task, kIODirectionInOut, kPage, kPage);
	if (page && page->prepare() != kIOReturnSuccess) {
		OSSafeReleaseNULL(page);
	}
	IOByteCount bytes = 0;
	*physical = page ? page->getPhysicalSegment(0, &bytes, kIOMemoryMapperNone) : 0;
	if (page && (*physical == 0 || bytes < kPage)) {
		page->complete();
		OSSafeReleaseNULL(page);
		*physical = 0;
	}
	if (page) {
		bzero(page->getBytesNoCopy(), kPage);
	}
	return page;
}

bool
StelaVirtIOSound::init(OSDictionary *properties)
{
	if (!IOAudioDevice::init(properties)) {
		return false;
	}
	// The nub is in no power tree. Managing power, IOAudioDevice::start
	// would PMinit and join the provider's tree [kc], which refuses a
	// parent that never PMinit'ed [kc: addPowerChild]: the device would sit
	// in PM with no parent. The family's own idle and active states do not
	// need it [mem].
	setFamilyManagePower(false);
	return true;
}

bool
StelaVirtIOSound::initHardware(IOService *provider)
{
	if (!IOAudioDevice::initHardware(provider)) {
		return false;
	}
	nub = OSDynamicCast(StelaVirtIODevice, provider);
	if (!nub) {
		IOLog("StelaVirtIOSound: the provider is no StelaVirtIODevice\n");
		return false;
	}
	controlLock = IOLockAlloc();
	queueLock = IOLockAlloc();
	messages = wiredPage(&messagesPhysical);
	samples = wiredPage(&samplesPhysical);
	if (!controlLock || !queueLock || !messages || !samples) {
		IOLog("StelaVirtIOSound: cannot allocate its pages\n");
		return false;
	}
	features = nub->negotiate(STELA_VIRTIO_F_VERSION_1 | STELA_VIRTIO_F_EVENT_IDX);
	if (features == 0) {
		return false;
	}
	negotiated = true;
	if (!nub->createQueue(STELA_VIRTIO_SND_VQ_CONTROL, kControlEntries, features, &controlQueue) ||
	    !nub->createQueue(STELA_VIRTIO_SND_VQ_TX, kTxEntries, features, &txQueue)) {
		nub->fail();
		abandon();
		return false;
	}
	for (unsigned period = 0; period < kPeriods; period++) {
		// Stream 0 in every period's header.
		*reinterpret_cast<uint32_t *>(static_cast<uint8_t *>(messages->getBytesNoCopy()) + kHeaderOffset +
		    period * STELA_VIRTIO_SND_PCM_XFER_BYTES) = 0;
	}
	handling = true;
	nub->setHandler(this, &StelaVirtIOSound::interruptOccurred);
	nub->driverOK();
	live = true;
	setProperty("Periods", kPeriods, 32);
	setProperty("PeriodFrames", kPeriodFrames, 32);
	setProperty("SampleRate", kRate, 32);
	publishCounters();
	if (!describe() || !configure()) {
		nub->fail();
		abandon();
		publishCounters();
		return false;
	}
	setDeviceName("Stela virtio sound");
	setDeviceShortName("StelaVirtIOSound");
	setManufacturerName("Stela");
	auto newEngine = OSTypeAlloc(StelaVirtIOSoundEngine);
	if (!newEngine || !newEngine->initWithDevice(this)) {
		IOLog("StelaVirtIOSound: cannot make its engine\n");
		OSSafeReleaseNULL(newEngine);
		abandon();
		return false;
	}
	IOReturn activated = activateAudioEngine(newEngine);
	newEngine->release();
	if (activated != kIOReturnSuccess) {
		IOLog("StelaVirtIOSound: the family refused its engine (%08x)\n", activated);
		setProperty("EngineActivated", false);
		abandon();
		return false;
	}
	setProperty("EngineActivated", true);
	IOLog("StelaVirtIOSound: engine activated: %u frames a period, %u periods, %u Hz stereo 16-bit\n",
	    kPeriodFrames, kPeriods, kRate);
	return true;
}

/* PCM_INFO: the device's stream must take 2 channels of S16 at 48 kHz. */
bool
StelaVirtIOSound::describe()
{
	SndQueryInfo query = {STELA_VIRTIO_SND_R_PCM_INFO, 0, 1, sizeof(SndPcmInfo)};
	uint8_t answer[4 + sizeof(SndPcmInfo)] = {};
	uint32_t status = 0;
	if (!control(&query, sizeof query, answer, sizeof answer, &status) || status != STELA_VIRTIO_SND_S_OK) {
		IOLog("StelaVirtIOSound: PCM_INFO failed (status %x)\n", status);
		return false;
	}
	SndPcmInfo info;
	bcopy(answer + 4, &info, sizeof info);
	IOLog("StelaVirtIOSound: stream 0: direction %u, %u-%u channels, formats %llx, rates %llx\n", info.direction,
	    info.channelsMin, info.channelsMax, info.formats, info.rates);
	if (info.direction != STELA_VIRTIO_SND_D_OUTPUT || info.channelsMin > kChannels ||
	    info.channelsMax < kChannels || !(info.formats & (1ULL << STELA_VIRTIO_SND_FMT_S16)) ||
	    !(info.rates & (1ULL << STELA_VIRTIO_SND_RATE_48000))) {
		IOLog("StelaVirtIOSound: stream 0 does not take stereo S16 at 48 kHz\n");
		return false;
	}
	return true;
}

/* SET_PARAMS and PREPARE. */
bool
StelaVirtIOSound::configure()
{
	SndSetParams params = {};
	params.header = {STELA_VIRTIO_SND_R_PCM_SET_PARAMS, 0};
	params.bufferBytes = kBufferBytes;
	params.periodBytes = kPeriodBytes;
	params.channels = kChannels;
	params.format = STELA_VIRTIO_SND_FMT_S16;
	params.rate = STELA_VIRTIO_SND_RATE_48000;
	uint32_t status = 0;
	if (!control(&params, sizeof params, &status, sizeof status, &status) || status != STELA_VIRTIO_SND_S_OK) {
		IOLog("StelaVirtIOSound: PCM_SET_PARAMS failed (status %x)\n", status);
		return false;
	}
	return pcm(STELA_VIRTIO_SND_R_PCM_PREPARE);
}

bool
StelaVirtIOSound::pcm(uint32_t code)
{
	SndPcmHeader request = {code, 0};
	uint32_t status = 0;
	if (!control(&request, sizeof request, &status, sizeof status, &status) || status != STELA_VIRTIO_SND_S_OK) {
		IOLog("StelaVirtIOSound: %s failed (status %x)\n", requestName(code), status);
		controlFailures++;
		return false;
	}
	return true;
}

bool
StelaVirtIOSound::control(const void *request, uint32_t requestBytes, void *response, uint32_t responseBytes,
    uint32_t *status)
{
	*status = 0;
	if (!live || requestBytes > kMessageBytes || responseBytes > kMessageBytes || responseBytes < 4) {
		return false;
	}
	IOLockLock(controlLock);
	auto page = static_cast<uint8_t *>(messages->getBytesNoCopy());
	bcopy(request, page + kRequestOffset, requestBytes);
	bzero(page + kResponseOffset, kMessageBytes);
	struct stela_vq_buf buffers[2] = {
	    {messagesPhysical + kRequestOffset, requestBytes},
	    {messagesPhysical + kResponseOffset, responseBytes},
	};
	IOLockLock(queueLock);
	bool added = stela_vq_add(&controlQueue.vq, buffers, 1, 1, page) >= 0;
	if (added) {
		nub->kick(&controlQueue);
	}
	IOLockUnlock(queueLock);
	bool answered = false;
	for (unsigned poll = 0; added && poll < kControlPolls && !answered; poll++) {
		void *token = nullptr;
		uint32_t written = 0;
		IOLockLock(queueLock);
		int found = stela_vq_harvest(&controlQueue.vq, &token, &written);
		IOLockUnlock(queueLock);
		if (found == STELA_VQ_BROKEN) {
			IOLog("StelaVirtIOSound: the device used a control buffer it was not given\n");
			break;
		}
		if (found == STELA_VQ_NONE) {
			IOSleep(1);
			continue;
		}
		answered = written >= 4;
	}
	if (answered) {
		bcopy(page + kResponseOffset, response, responseBytes);
		*status = *static_cast<uint32_t *>(response);
	} else {
		IOLog("StelaVirtIOSound: %s got no answer\n", requestName(*static_cast<const uint32_t *>(request)));
		// A request the device may still hold: no other can be made.
		live = false;
	}
	IOLockUnlock(controlLock);
	return answered;
}

void *
StelaVirtIOSound::sampleBuffer() const
{
	return samples ? samples->getBytesNoCopy() : nullptr;
}

/* With queueLock held: period `period` to the device, unpublished. */
bool
StelaVirtIOSound::post(unsigned period)
{
	struct stela_vq_buf buffers[3] = {
	    {messagesPhysical + kHeaderOffset + period * STELA_VIRTIO_SND_PCM_XFER_BYTES,
	        uint32_t(STELA_VIRTIO_SND_PCM_XFER_BYTES)},
	    {samplesPhysical + period * kPeriodBytes, kPeriodBytes},
	    {messagesPhysical + kStatusOffset + period * STELA_VIRTIO_SND_PCM_STATUS_BYTES,
	        uint32_t(STELA_VIRTIO_SND_PCM_STATUS_BYTES)},
	};
	if (stela_vq_add(&txQueue.vq, buffers, 2, 1, &tokens[period]) < 0) {
		return false;
	}
	held[period] = true;
	return true;
}

bool
StelaVirtIOSound::startStream(StelaVirtIOSoundEngine *owner)
{
	if (!live) {
		return false;
	}
	IOLockLock(queueLock);
	engine = owner;
	__atomic_store_n(&currentFrame, 0, __ATOMIC_RELEASE);
	bool posted = true;
	for (unsigned period = 0; period < kPeriods; period++) {
		if (!held[period]) {
			posted &= post(period);
		}
	}
	stela_vq_arm(&txQueue.vq);
	nub->kick(&txQueue);
	running = posted;
	IOLockUnlock(queueLock);
	if (!posted || !pcm(STELA_VIRTIO_SND_R_PCM_START)) {
		IOLockLock(queueLock);
		running = false;
		IOLockUnlock(queueLock);
		return false;
	}
	starts++;
	publishCounters();
	return true;
}

void
StelaVirtIOSound::stopStream()
{
	IOLockLock(queueLock);
	running = false;
	IOLockUnlock(queueLock);
	if (!live) {
		return;
	}
	pcm(STELA_VIRTIO_SND_R_PCM_STOP);
	pcm(STELA_VIRTIO_SND_R_PCM_RELEASE);
	// RELEASE gives back every period the device held.
	for (unsigned poll = 0; poll < kReleasePolls; poll++) {
		harvest();
		bool any = false;
		for (unsigned period = 0; period < kPeriods; period++) {
			any |= held[period];
		}
		if (!any) {
			break;
		}
		IOSleep(1);
	}
	pcm(STELA_VIRTIO_SND_R_PCM_PREPARE);
	publishCounters();
}

void
StelaVirtIOSound::harvest()
{
	bool wrappedNow = false;
	bool reposted = false;
	IOLockLock(queueLock);
	for (;;) {
		void *token = nullptr;
		uint32_t written = 0;
		int found = stela_vq_harvest(&txQueue.vq, &token, &written);
		if (found == STELA_VQ_NONE) {
			// Ask for the next completion, and look once more for one the
			// device made before it could see that.
			stela_vq_arm(&txQueue.vq);
			if (!stela_vq_pending(&txQueue.vq)) {
				break;
			}
			continue;
		}
		unsigned period = found == STELA_VQ_BROKEN ? kPeriods : unsigned(static_cast<uint8_t *>(token) - tokens);
		if (period >= kPeriods) {
			IOLog("StelaVirtIOSound: the device used a transfer it was not given\n");
			running = false;
			live = false;
			nub->fail();
			break;
		}
		held[period] = false;
		completions++;
		auto status = reinterpret_cast<volatile SndPcmStatus *>(
		    static_cast<uint8_t *>(messages->getBytesNoCopy()) + kStatusOffset +
		    period * STELA_VIRTIO_SND_PCM_STATUS_BYTES);
		if (status->status != STELA_VIRTIO_SND_S_OK) {
			transferErrors++;
		}
		lastLatencyBytes = status->latencyBytes;
		if (!running) {
			continue;
		}
		// The device read this period and played it out: it is silence
		// until the HAL writes the buffer's next loop there.
		bzero(static_cast<uint8_t *>(samples->getBytesNoCopy()) + period * kPeriodBytes, kPeriodBytes);
		__atomic_store_n(&currentFrame, ((period + 1) % kPeriods) * kPeriodFrames, __ATOMIC_RELEASE);
		if (period == kPeriods - 1) {
			wraps++;
			wrappedNow = true;
		}
		reposted |= post(period);
	}
	if (reposted) {
		nub->kick(&txQueue);
	}
	StelaVirtIOSoundEngine *owner = running ? engine : nullptr;
	IOLockUnlock(queueLock);
	if (wrappedNow && owner) {
		owner->wrapped();
		if (wraps % kPublishWraps == 0) {
			publishCounters();
		}
	}
}

void
StelaVirtIOSound::interruptOccurred(OSObject *owner, uint32_t status)
{
	if (status & STELA_VIRTIO_MMIO_INT_VRING) {
		static_cast<StelaVirtIOSound *>(owner)->harvest();
	}
}

void
StelaVirtIOSound::publishCounters()
{
	setProperty("Completions", completions, 64);
	setProperty("Wraps", wraps, 64);
	setProperty("TransferErrors", transferErrors, 64);
	setProperty("StreamStarts", starts, 64);
	setProperty("ControlFailures", controlFailures, 64);
	setProperty("LatencyBytes", lastLatencyBytes, 32);
	setProperty("DeviceLive", live);
}

void
StelaVirtIOSound::stop(IOService *provider)
{
	// The engines stop first, while the device still answers.
	IOAudioDevice::stop(provider);
	if (queueLock) {
		IOLockLock(queueLock);
		running = false;
		engine = nullptr;
		IOLockUnlock(queueLock);
	}
	abandon();
}

void
StelaVirtIOSound::abandon()
{
	if (nub && handling) {
		nub->setHandler(nullptr, nullptr);
		handling = false;
	}
	if (nub && negotiated) {
		// Once the reset reads back the device uses no buffer.
		nub->reset();
		negotiated = false;
	}
	live = false;
	running = false;
}

void
StelaVirtIOSound::free()
{
	// A start that failed after the device was set up never stopped.
	abandon();
	StelaVirtIOQueue *queues[] = {&controlQueue, &txQueue};
	for (StelaVirtIOQueue *queue : queues) {
		if (queue->memory) {
			queue->memory->complete();
			OSSafeReleaseNULL(queue->memory);
		}
	}
	if (messages) {
		messages->complete();
		OSSafeReleaseNULL(messages);
	}
	if (samples) {
		samples->complete();
		OSSafeReleaseNULL(samples);
	}
	if (controlLock) {
		IOLockFree(controlLock);
		controlLock = nullptr;
	}
	if (queueLock) {
		IOLockFree(queueLock);
		queueLock = nullptr;
	}
	IOAudioDevice::free();
}

bool
StelaVirtIOSoundEngine::initWithDevice(StelaVirtIOSound *owner)
{
	if (!IOAudioEngine::init(nullptr)) {
		return false;
	}
	device = owner;
	return device != nullptr;
}

bool
StelaVirtIOSoundEngine::initHardware(IOService *provider)
{
	if (!IOAudioEngine::initHardware(provider) || !device || !device->sampleBuffer()) {
		return false;
	}
	IOAudioSampleRate rate = {StelaVirtIOSound::kRate, 0};
	setSampleRate(&rate);
	setNumSampleFramesPerBuffer(StelaVirtIOSound::kBufferFrames);
	setDescription("Stela virtio sound");
	setSampleOffset(kSampleOffsetFrames);

	auto stream = OSTypeAlloc(IOAudioStream);
	if (!stream || !stream->initWithAudioEngine(this, kIOAudioStreamDirectionOutput, 1)) {
		IOLog("StelaVirtIOSound: cannot make its output stream\n");
		OSSafeReleaseNULL(stream);
		return false;
	}
	IOAudioStreamFormat format = {
	    StelaVirtIOSound::kChannels,
	    kIOAudioStreamSampleFormatLinearPCM,
	    kIOAudioStreamNumericRepresentationSignedInt,
	    16,
	    16,
	    kIOAudioStreamAlignmentHighByte,
	    kIOAudioStreamByteOrderLittleEndian,
	    true,
	    0,
	};
	stream->setSampleBuffer(device->sampleBuffer(), StelaVirtIOSound::kBufferBytes);
	// No IO functions, so the stream clips with clipOutputSamples below.
	// The one-function form with a null function would leave the stream a
	// list of one null entry, which it clips with by calling nothing [kc:
	// IOAudioStream::clipOutputSamples(UInt32, UInt32)]: K5's silence.
	stream->addAvailableFormat(&format, &rate, &rate, static_cast<const AudioIOFunction *>(nullptr), 0);
	IOReturn formatSet = stream->setFormat(&format);
	IOReturn added = addAudioStream(stream);
	stream->release();
	if (added != kIOReturnSuccess) {
		IOLog("StelaVirtIOSound: the engine refused its stream (%08x, format %08x)\n", added, formatSet);
		return false;
	}

	auto volume = IOAudioLevelControl::createVolumeControl(kVolumeSteps, 0, kVolumeSteps, -(kVolumeSteps << 16), 0,
	    kIOAudioControlChannelIDAll, kIOAudioControlChannelNameAll, 0, kIOAudioControlUsageOutput);
	if (volume) {
		volume->setValueChangeHandler(&StelaVirtIOSoundEngine::volumeChanged, this);
		addDefaultAudioControl(volume);
		volume->release();
	}
	auto mute = IOAudioToggleControl::createMuteControl(false, kIOAudioControlChannelIDAll,
	    kIOAudioControlChannelNameAll, 0, kIOAudioControlUsageOutput);
	if (mute) {
		mute->setValueChangeHandler(&StelaVirtIOSoundEngine::muteChanged, this);
		addDefaultAudioControl(mute);
		mute->release();
	}
	IOLog("StelaVirtIOSound: output stream added (format %08x), volume %s, mute %s\n", formatSet,
	    volume ? "yes" : "no", mute ? "yes" : "no");
	return true;
}

UInt32
StelaVirtIOSoundEngine::getCurrentSampleFrame()
{
	return device ? device->currentSampleFrame() : 0;
}

/* As Apple's AppleUSBAudioEngine answers [kc]. */
bool
StelaVirtIOSoundEngine::driverDesiresHiResSampleIntervals(void)
{
	return true;
}

IOReturn
StelaVirtIOSoundEngine::performAudioEngineStart()
{
	if (!device || !device->startStream(this)) {
		IOLog("StelaVirtIOSound: the stream did not start\n");
		return kIOReturnError;
	}
	// The device's clock started at START: the engine's does now.
	takeTimeStamp(false);
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOSoundEngine::performAudioEngineStop()
{
	if (device) {
		device->stopStream();
	}
	return kIOReturnSuccess;
}

void
StelaVirtIOSoundEngine::wrapped()
{
	takeTimeStamp();
}

/*
 * The family's mix buffer, float, to the sample buffer, 16-bit: the
 * family's own saturating conversion, then the volume as an integer gain,
 * or silence while muted.
 */
IOReturn
StelaVirtIOSoundEngine::clipOutputSamples(const void *mixBuf, void *sampleBuf, UInt32 firstSampleFrame,
    UInt32 numSampleFrames, const IOAudioStreamFormat *streamFormat, IOAudioStream *)
{
	if (!mixBuf || !sampleBuf || firstSampleFrame > StelaVirtIOSound::kBufferFrames ||
	    numSampleFrames > StelaVirtIOSound::kBufferFrames - firstSampleFrame ||
	    (streamFormat && streamFormat->fNumChannels != StelaVirtIOSound::kChannels)) {
		return kIOReturnBadArgument;
	}
	const UInt32 channels = StelaVirtIOSound::kChannels;
	const UInt32 count = numSampleFrames * channels;
	auto in = static_cast<const float *>(mixBuf) + firstSampleFrame * channels;
	auto out = static_cast<SInt16 *>(sampleBuf) + firstSampleFrame * channels;
	if (__atomic_load_n(&muted, __ATOMIC_RELAXED)) {
		bzero(out, count * sizeof(SInt16));
		return kIOReturnSuccess;
	}
	IOAF_Float32ToNativeInt16(in, out, count);
	uint32_t level = __atomic_load_n(&gain, __ATOMIC_RELAXED);
	if (level < 0x10000) {
		for (UInt32 index = 0; index < count; index++) {
			out[index] = SInt16((int32_t(out[index]) * int32_t(level)) >> 16);
		}
	}
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOSoundEngine::volumeChanged(OSObject *target, IOAudioControl *, SInt32, SInt32 newValue)
{
	auto engine = OSDynamicCast(StelaVirtIOSoundEngine, target);
	if (!engine) {
		return kIOReturnBadArgument;
	}
	SInt32 step = newValue < 0 ? 0 : newValue > kVolumeSteps ? kVolumeSteps : newValue;
	__atomic_store_n(&engine->gain, kGain[step], __ATOMIC_RELAXED);
	return kIOReturnSuccess;
}

IOReturn
StelaVirtIOSoundEngine::muteChanged(OSObject *target, IOAudioControl *, SInt32, SInt32 newValue)
{
	auto engine = OSDynamicCast(StelaVirtIOSoundEngine, target);
	if (!engine) {
		return kIOReturnBadArgument;
	}
	__atomic_store_n(&engine->muted, newValue != 0, __ATOMIC_RELAXED);
	return kIOReturnSuccess;
}
