/*
 * StelaVT: a VideoToolbox H.264 and HEVC encoder plugin for macOS running
 * under Stela, which has no Apple video encoder. It registers as a hardware
 * encoder so real-time clients (Parsec, screen sharing) that ask for one get
 * a fast encoder instead of Apple's software HEVC.
 *
 * Each session encodes on the host's NVENC when the host serves it, through
 * StelaGPU.bundle's gate (StelaEncoder.h), and otherwise with x264:
 *
 * - H.264: NVENC, or x264 when the host refuses the session; a session that
 *   NVENC loses mid-stream goes on with x264, starting with an IDR.
 * - HEVC: NVENC only. A session is refused at creation
 *   (kVTCouldNotFindVideoEncoderErr) when the host has no HEVC encoder, so
 *   VideoToolbox's client falls back to H.264.
 *
 * NVENC's output is Annex B, which becomes length-prefixed samples (AVCC,
 * HVCC) under a format description made of the session's parameter sets.
 *
 * Pixel formats: a session NVENC serves asks for 'BGRA' first, then '420v':
 * the host converts BGRA to NV12 on its own CPU while it stores the picture
 * for the engine, so a client capturing BGRA (Parsec) hands its buffers
 * through, and VideoToolbox does no conversion of its own in the guest
 * (VTPixelTransferSession, through Metal). The host's parameter sets signal
 * video range, so it asks for no '420f'. x264 takes NV12 only: a BGRA frame
 * that reaches it (NVENC lost mid-stream) is converted here first, as the
 * host converts it.
 *
 * VideoToolbox's plugin interface is private. Its layout here follows
 * Apple's own encoder plugins (JPEGVideoEncoder.bundle, macOS 27.2):
 * CoreMedia's derived-object pattern, a base class and an encoder class.
 */
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <dlfcn.h>
#include <os/log.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <x264.h>

#include "StelaEncoder.h"

/* ---- The private interface ---------------------------------------------- */

typedef CFTypeRef CMBaseObjectRef;
typedef CMBaseObjectRef VTVideoEncoderRef;
typedef struct OpaqueVTEncoderSession *VTEncoderSessionRef;
typedef void *VTEncoderFrameToken;
typedef const void *CMBaseClassID;

typedef struct {
	uintptr_t version;
	size_t derivedStorageSize;
	Boolean (*equal)(CMBaseObjectRef, CMBaseObjectRef);
	OSStatus (*invalidate)(CMBaseObjectRef);
	void (*finalize)(CMBaseObjectRef);
	CFStringRef (*copyDebugDescription)(CMBaseObjectRef);
	OSStatus (*copyProperty)(CMBaseObjectRef, CFStringRef, CFAllocatorRef, void *);
	OSStatus (*setProperty)(CMBaseObjectRef, CFStringRef, CFTypeRef);
	OSStatus (*notificationBarrier)(CMBaseObjectRef);
	const void *reserved[3];
} CMBaseClass;

typedef struct {
	uintptr_t version;
	OSStatus (*startSession)(VTVideoEncoderRef, VTEncoderSessionRef, CMVideoDimensions);
	OSStatus (*encodeFrame)(VTVideoEncoderRef, VTEncoderFrameToken, CVImageBufferRef, CMTime, CMTime,
	    CFDictionaryRef, VTEncodeInfoFlags *);
	OSStatus (*completeFrames)(VTVideoEncoderRef, CMTime);
	OSStatus (*copySupportedPropertyDictionary)(VTVideoEncoderRef, CFDictionaryRef *);
	const void *reserved[16];
} VTVideoEncoderClass;

typedef struct {
	const void *reserved;
	const CMBaseClass *baseClass;
	const VTVideoEncoderClass *derivedClass;
} VTVideoEncoderVTable;

static struct {
	OSStatus (*derivedCreate)(CFAllocatorRef, const void *, CMBaseClassID, CMBaseObjectRef *);
	void *(*derivedStorage)(CMBaseObjectRef);
	CMBaseClassID (*encoderClassID)(void);
	CMBaseObjectRef (*encoderBase)(VTVideoEncoderRef);
	OSStatus (*setPixelBufferAttributes)(VTEncoderSessionRef, CFDictionaryRef);
	OSStatus (*emit)(VTEncoderSessionRef, VTEncoderFrameToken, OSStatus, VTEncodeInfoFlags, CMSampleBufferRef);
	bool ok;
} vt;

static pthread_once_t vtOnce = PTHREAD_ONCE_INIT;

static void resolve(void)
{
	vt.derivedCreate = dlsym(RTLD_DEFAULT, "CMDerivedObjectCreate");
	vt.derivedStorage = dlsym(RTLD_DEFAULT, "CMBaseObjectGetDerivedStorage");
	vt.encoderClassID = dlsym(RTLD_DEFAULT, "VTVideoEncoderGetClassID");
	vt.encoderBase = dlsym(RTLD_DEFAULT, "VTVideoEncoderGetCMBaseObject");
	vt.setPixelBufferAttributes = dlsym(RTLD_DEFAULT, "VTEncoderSessionSetPixelBufferAttributes");
	vt.emit = dlsym(RTLD_DEFAULT, "VTEncoderSessionEmitEncodedFrame");
	vt.ok = vt.derivedCreate && vt.derivedStorage && vt.encoderClassID && vt.encoderBase
	    && vt.setPixelBufferAttributes && vt.emit;
}

static os_log_t logger(void)
{
	static os_log_t log;
	static dispatch_once_t once;
	dispatch_once(&once, ^{
		log = os_log_create("org.stela.StelaVT", "encoder");
	});
	return log;
}

/* ---- The host's encoder ----------------------------------------------- */

/* The bundle's table, which its constructor hands over. */
static _Atomic(const StelaEncoderAPI *) handed;

/* StelaGPU.bundle calls this before it registers StelaVT. */
__attribute__((visibility("default"))) void StelaVT_UseEncoders(const StelaEncoderAPI *api)
{
	if (api && api->version >= STELA_ENCODER_API_VERSION) {
		atomic_store(&handed, api);
	}
}

/* The host's encoder calls: the handed table, or the bundle's by class when
 * this image is not the one the bundle loaded; NULL without the bundle. */
static const StelaEncoderAPI *host(void)
{
	const StelaEncoderAPI *api = atomic_load(&handed);
	if (api) {
		return api;
	}
	void *(*getClass)(const char *) = dlsym(RTLD_DEFAULT, "objc_getClass");
	void *(*registerName)(const char *) = dlsym(RTLD_DEFAULT, "sel_registerName");
	void *send = dlsym(RTLD_DEFAULT, "objc_msgSend");
	void *class = getClass ? getClass(STELA_ENCODER_CLASS) : NULL;
	if (!class || !registerName || !send) {
		return NULL;
	}
	const StelaEncoderAPI *(*message)(void *, void *) = (const StelaEncoderAPI *(*)(void *, void *))send;
	api = message(class, registerName("api"));
	if (!api || api->version < STELA_ENCODER_API_VERSION) {
		return NULL;
	}
	atomic_store(&handed, api);
	return api;
}

/* Whether the host's encoder codes `codec` now, and whether it takes BGRA
 * (`bgra`, optional); why not in `reason`. */
static bool hostCodesInput(uint32_t codec, bool *bgra, char *reason, size_t bytes)
{
	const StelaEncoderAPI *api = host();
	if (!api) {
		snprintf(reason, bytes, "StelaGPU.bundle gave no host encoder in this process");
		return false;
	}
	StelaEncoderCapabilities capabilities;
	StelaEncoderStatus status = api->capabilities(&capabilities, reason, bytes);
	if (status != StelaEncoderOk) {
		return false;
	}
	if (!(capabilities.codecs & (1u << codec))) {
		snprintf(reason, bytes, "the host's encoder does not code this codec");
		return false;
	}
	if (bgra) {
		*bgra = (capabilities.inputs & (1u << StelaEncoderBGRA)) != 0;
	}
	return true;
}

static bool hostCodes(uint32_t codec, char *reason, size_t bytes)
{
	return hostCodesInput(codec, NULL, reason, bytes);
}

/* ---- The encoder ---------------------------------------------------------- */

enum { kPending = 64 };
/* Frames NVENC may refuse in a row before the session goes to x264. */
enum { kMostDrops = 3 };
/* The parameter sets' room: H.264 36 bytes and HEVC 95 at 1080p. */
enum { kSetsBytes = 512 };

typedef enum {
	BackendNone,
	BackendNVENC,
	BackendX264,
} Backend;

typedef struct {
	pthread_mutex_t lock;
	VTEncoderSessionRef session;
	CMVideoCodecType codec;
	int32_t width, height;
	Backend backend;
	x264_t *x264;
	CMFormatDescriptionRef format;
	/* NVENC's session, its output buffer and the frame number it counts. */
	StelaEncoderSession *nvenc;
	uint8_t *out;
	size_t outBytes;
	uint64_t frame;
	/* Frames since the last IDR, which the key frame interval counts. */
	int sinceKey;
	int drops;
	/* A bitrate set since the last frame, which the next one carries. */
	int newBitrate;
	/* Settings, applied when the encoder opens at the first frame. */
	int bitrate; /* bits per second, 0: default */
	int keyInterval;
	double keyDuration;
	double frameRate;
	/* x264's frames, by its pts. */
	struct {
		bool used;
		int64_t pts;
		VTEncoderFrameToken token;
		CMTime time, duration;
	} pending[kPending];
	int64_t nextPts;
	uint64_t frames, bytes, idrs;
	uint64_t gpuMicros, convertMicros;
	/* Frames that came as BGRA, and each encode's time in this process. */
	uint64_t bgraFrames;
	uint64_t encodeMicros, encodeMost;
	/* x264's NV12 of a BGRA frame. */
	uint8_t *scratch;
	size_t scratchBytes;
} Encoder;

static const char *codecName(CMVideoCodecType codec)
{
	return codec == kCMVideoCodecType_HEVC ? "HEVC" : "H.264";
}

static const char *backendName(Backend backend)
{
	return backend == BackendNVENC ? "NVENC" : backend == BackendX264 ? "x264" : "none";
}

static Encoder *storage(VTVideoEncoderRef encoder)
{
	return vt.derivedStorage(vt.encoderBase(encoder));
}

static void closeNVENC(Encoder *e)
{
	if (e->nvenc) {
		host()->close(e->nvenc);
		e->nvenc = NULL;
	}
	free(e->out);
	e->out = NULL;
	e->outBytes = 0;
}

static void finalize(CMBaseObjectRef object)
{
	Encoder *e = vt.derivedStorage(object);
	closeNVENC(e);
	if (e->x264) {
		x264_encoder_close(e->x264);
	}
	if (e->format) {
		CFRelease(e->format);
	}
	free(e->scratch);
	uint64_t frames = e->frames ? e->frames : 1;
	os_log(logger(), "StelaVT: %{public}s session closed after %llu frames (%llu BGRA), %llu bytes, %llu IDRs, on"
	    " %{public}s (NVENC gpu %llu us, host convert %llu us, encode call %llu us, longest %llu us, a frame)",
	    codecName(e->codec), e->frames, e->bgraFrames, e->bytes, e->idrs, backendName(e->backend),
	    e->gpuMicros / frames, e->convertMicros / frames, e->encodeMicros / frames, e->encodeMost);
	pthread_mutex_destroy(&e->lock);
}

static CFStringRef copyDebugDescription(CMBaseObjectRef object)
{
	Encoder *e = vt.derivedStorage(object);
	return CFStringCreateWithFormat(NULL, NULL, CFSTR("<StelaVT %s %s %dx%d, %llu frames>"),
	    backendName(e->backend), codecName(e->codec), e->width, e->height, e->frames);
}

static bool number(CFTypeRef value, double *out)
{
	return value && CFGetTypeID(value) == CFNumberGetTypeID()
	    && CFNumberGetValue(value, kCFNumberDoubleType, out);
}

static bool truth(CFTypeRef value)
{
	if (value && CFGetTypeID(value) == CFBooleanGetTypeID()) {
		return CFBooleanGetValue(value);
	}
	double n;
	return number(value, &n) && n != 0;
}

static OSStatus setProperty(CMBaseObjectRef object, CFStringRef key, CFTypeRef value)
{
	Encoder *e = vt.derivedStorage(object);
	double n = 0;
	OSStatus status = noErr;
	pthread_mutex_lock(&e->lock);
	if (CFEqual(key, kVTCompressionPropertyKey_AverageBitRate)) {
		if (number(value, &n) && n > 0) {
			e->bitrate = (int)n;
			if (e->x264) {
				x264_param_t p;
				x264_encoder_parameters(e->x264, &p);
				p.rc.i_bitrate = e->bitrate / 1000;
				p.rc.i_vbv_max_bitrate = p.rc.i_bitrate;
				x264_encoder_reconfig(e->x264, &p);
			}
			/* NVENC takes it with the next frame. */
			e->newBitrate = e->bitrate;
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_ExpectedFrameRate)) {
		if (number(value, &n) && n > 0) {
			e->frameRate = n;
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_MaxKeyFrameInterval)) {
		if (number(value, &n) && n > 0) {
			e->keyInterval = (int)n;
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration)) {
		if (number(value, &n) && n > 0) {
			e->keyDuration = n;
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_RealTime)
	    || CFEqual(key, kVTCompressionPropertyKey_AllowFrameReordering)
	    || CFEqual(key, kVTCompressionPropertyKey_ProfileLevel)
	    || CFEqual(key, kVTCompressionPropertyKey_DataRateLimits)
	    || CFEqual(key, kVTCompressionPropertyKey_Quality)
	    || CFEqual(key, kVTCompressionPropertyKey_H264EntropyMode)
	    || CFEqual(key, kVTCompressionPropertyKey_ColorPrimaries)
	    || CFEqual(key, kVTCompressionPropertyKey_TransferFunction)
	    || CFEqual(key, kVTCompressionPropertyKey_YCbCrMatrix)
	    || CFEqual(key, kVTVideoEncoderSpecification_EnableLowLatencyRateControl)) {
		/* Accepted: NVENC codes one frame at a time with no B-frames, and
		 * x264's zerolatency tuning never reorders either. */
	} else {
		status = kVTPropertyNotSupportedErr;
	}
	pthread_mutex_unlock(&e->lock);
	os_log_debug(logger(), "StelaVT: setProperty %{public}@ -> %d", key, (int)status);
	return status;
}

static OSStatus copyProperty(CMBaseObjectRef object, CFStringRef key, CFAllocatorRef allocator, void *out)
{
	Encoder *e = vt.derivedStorage(object);
	CFTypeRef *value = out;
	if (CFEqual(key, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder)) {
		*value = CFRetain(kCFBooleanTrue);
		return noErr;
	}
	if (CFEqual(key, kVTCompressionPropertyKey_AverageBitRate)) {
		int bitrate = e->bitrate;
		*value = CFNumberCreate(allocator, kCFNumberIntType, &bitrate);
		return noErr;
	}
	if (CFEqual(key, kVTCompressionPropertyKey_RealTime) || CFEqual(key, kVTVideoEncoderSpecification_EnableLowLatencyRateControl)) {
		*value = CFRetain(kCFBooleanTrue);
		return noErr;
	}
	if (CFEqual(key, kVTCompressionPropertyKey_AllowFrameReordering)) {
		*value = CFRetain(kCFBooleanFalse);
		return noErr;
	}
	os_log_debug(logger(), "StelaVT: copyProperty %{public}@ unsupported", key);
	return kVTPropertyNotSupportedErr;
}

static void addProperty(CFMutableDictionaryRef supported, CFStringRef key, CFStringRef type)
{
	const void *keys[] = {kVTPropertyTypeKey, kVTPropertyReadWriteStatusKey};
	const void *values[] = {type, kVTPropertyReadWriteStatus_ReadWrite};
	CFDictionaryRef info = CFDictionaryCreate(NULL, keys, values, 2, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	CFDictionarySetValue(supported, key, info);
	CFRelease(info);
}

static OSStatus copySupportedPropertyDictionary(VTVideoEncoderRef encoder, CFDictionaryRef *out)
{
	CFMutableDictionaryRef d = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	addProperty(d, kVTCompressionPropertyKey_AverageBitRate, kVTPropertyType_Number);
	addProperty(d, kVTCompressionPropertyKey_ExpectedFrameRate, kVTPropertyType_Number);
	addProperty(d, kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTPropertyType_Number);
	addProperty(d, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTPropertyType_Number);
	addProperty(d, kVTCompressionPropertyKey_RealTime, kVTPropertyType_Boolean);
	addProperty(d, kVTCompressionPropertyKey_AllowFrameReordering, kVTPropertyType_Boolean);
	addProperty(d, kVTCompressionPropertyKey_ProfileLevel, kVTPropertyType_Enumeration);
	addProperty(d, kVTCompressionPropertyKey_DataRateLimits, kVTPropertyType_Enumeration);
	addProperty(d, kVTCompressionPropertyKey_Quality, kVTPropertyType_Number);
	addProperty(d, kVTCompressionPropertyKey_H264EntropyMode, kVTPropertyType_Enumeration);
	addProperty(d, kVTCompressionPropertyKey_ColorPrimaries, kVTPropertyType_Enumeration);
	addProperty(d, kVTCompressionPropertyKey_TransferFunction, kVTPropertyType_Enumeration);
	addProperty(d, kVTCompressionPropertyKey_YCbCrMatrix, kVTPropertyType_Enumeration);
	addProperty(d, kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kVTPropertyType_Boolean);
	os_log_debug(logger(), "StelaVT: copySupportedPropertyDictionary");
	*out = d;
	(void)encoder;
	return noErr;
}

static OSStatus startSession(VTVideoEncoderRef encoder, VTEncoderSessionRef session, CMVideoDimensions size)
{
	Encoder *e = storage(encoder);
	pthread_mutex_lock(&e->lock);
	e->session = session;
	e->width = size.width;
	e->height = size.height;
	pthread_mutex_unlock(&e->lock);
	/* NVENC: BGRA first, which the host converts, then NV12 in video range,
	 * which its parameter sets signal. x264: NV12 in either range. */
	char reason[160];
	uint32_t codec = e->codec == kCMVideoCodecType_HEVC ? StelaEncoderHEVC : StelaEncoderH264;
	bool bgra = false;
	bool nvenc = hostCodesInput(codec, &bgra, reason, sizeof reason);
	int withBGRA[] = {kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange};
	int withNV12[] = {kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange};
	const int *formats = nvenc && bgra ? withBGRA : withNV12;
	CFIndex count = nvenc && !bgra ? 1 : 2;
	CFNumberRef numbers[2];
	for (CFIndex i = 0; i < count; i++) {
		numbers[i] = CFNumberCreate(NULL, kCFNumberIntType, &formats[i]);
	}
	CFArrayRef list = CFArrayCreate(NULL, (const void **)numbers, count, &kCFTypeArrayCallBacks);
	CFDictionaryRef surface = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	const void *keys[] = {kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey};
	const void *values[] = {list, surface};
	CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 2, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	OSStatus status = vt.setPixelBufferAttributes(session, attributes);
	CFRelease(attributes);
	CFRelease(surface);
	CFRelease(list);
	for (CFIndex i = 0; i < count; i++) {
		CFRelease(numbers[i]);
	}
	os_log(logger(), "StelaVT: %{public}s session %dx%d, %{public}s, pixel buffer attributes %d",
	    codecName(e->codec), size.width, size.height,
	    nvenc ? (bgra ? "BGRA for NVENC, then 420v" : "420v for NVENC") : "420v and 420f", (int)status);
	return status;
}

/* x264 opened for the first frame's size and range, with the settings so far. */
static OSStatus openX264(Encoder *e, int width, int height, bool fullRange)
{
	x264_param_t p;
	if (x264_param_default_preset(&p, "ultrafast", "zerolatency") < 0) {
		return kVTVideoEncoderMalfunctionErr;
	}
	p.i_width = width;
	p.i_height = height;
	p.i_csp = X264_CSP_NV12;
	double fps = e->frameRate > 0 ? e->frameRate : 60;
	p.i_fps_num = (uint32_t)(fps * 1000);
	p.i_fps_den = 1000;
	p.i_timebase_num = p.i_fps_den;
	p.i_timebase_den = p.i_fps_num;
	int kbps = e->bitrate > 0 ? e->bitrate / 1000 : 20000;
	p.rc.i_rc_method = X264_RC_ABR;
	p.rc.i_bitrate = kbps;
	p.rc.i_vbv_max_bitrate = kbps;
	p.rc.i_vbv_buffer_size = (int)(kbps / fps * 2) + 1; /* two frames: low latency */
	p.i_keyint_max = e->keyInterval > 0 ? e->keyInterval : X264_KEYINT_MAX_INFINITE;
	p.b_annexb = 0; /* 4-byte lengths, as AVCC wants */
	p.b_repeat_headers = 0;
	p.vui.b_fullrange = fullRange;
	p.i_log_level = X264_LOG_NONE;
	p.i_threads = 0; /* zerolatency: sliced threads, one frame in flight */
	x264_param_apply_profile(&p, "high");
	e->x264 = x264_encoder_open(&p);
	if (!e->x264) {
		return kVTVideoEncoderMalfunctionErr;
	}
	x264_nal_t *nals;
	int count;
	if (x264_encoder_headers(e->x264, &nals, &count) < 0) {
		return kVTVideoEncoderMalfunctionErr;
	}
	const uint8_t *sets[2] = {NULL, NULL};
	size_t sizes[2] = {0, 0};
	for (int i = 0; i < count; i++) {
		int slot = nals[i].i_type == NAL_SPS ? 0 : nals[i].i_type == NAL_PPS ? 1 : -1;
		if (slot >= 0) {
			sets[slot] = nals[i].p_payload + 4;
			sizes[slot] = (size_t)nals[i].i_payload - 4;
		}
	}
	if (!sets[0] || !sets[1]) {
		return kVTVideoEncoderMalfunctionErr;
	}
	if (e->format) {
		CFRelease(e->format);
		e->format = NULL;
	}
	OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, 2, sets, sizes, 4, &e->format);
	os_log(logger(), "StelaVT: x264 open %dx%d %.1f fps %d kbps keyint %d range %s: %d", width, height, fps, kbps,
	    p.i_keyint_max, fullRange ? "full" : "video", (int)status);
	return status;
}

/* ---- Annex B ---------------------------------------------------------------- */

typedef struct {
	const uint8_t *data;
	size_t size;
} Nal;

enum { kMostNals = 256 };

/* The NAL units of Annex B `bytes`, without their start codes. */
static size_t splitAnnexB(const uint8_t *bytes, size_t length, Nal *nals, size_t most)
{
	size_t count = 0;
	size_t start = SIZE_MAX;
	size_t i = 0;
	while (i + 3 <= length) {
		if (bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1) {
			if (start != SIZE_MAX && count < most) {
				/* The zero_byte of a 4-byte start code is not the NAL's. */
				size_t end = i;
				while (end > start && bytes[end - 1] == 0) {
					end--;
				}
				nals[count++] = (Nal){bytes + start, end - start};
			}
			start = i + 3;
			i += 3;
		} else {
			i++;
		}
	}
	if (start != SIZE_MAX && start < length && count < most) {
		nals[count++] = (Nal){bytes + start, length - start};
	}
	return count;
}

static int nalType(CMVideoCodecType codec, Nal nal)
{
	if (nal.size == 0) {
		return -1;
	}
	return codec == kCMVideoCodecType_HEVC ? (nal.data[0] >> 1) & 0x3f : nal.data[0] & 0x1f;
}

/* Parameter sets and access unit delimiters, which the format description
 * or nothing carries in a length-prefixed stream. */
static bool outOfBand(CMVideoCodecType codec, int type)
{
	return codec == kCMVideoCodecType_HEVC ? (type >= 32 && type <= 35) : (type == 7 || type == 8 || type == 9);
}

/* The format description of NVENC's parameter sets. */
static OSStatus formatOfSets(Encoder *e, const uint8_t *sets, size_t length)
{
	Nal nals[8];
	size_t count = splitAnnexB(sets, length, nals, 8);
	bool hevc = e->codec == kCMVideoCodecType_HEVC;
	/* H.264: SPS, PPS; HEVC: VPS, SPS, PPS. */
	const int wanted[3] = {hevc ? 32 : 7, hevc ? 33 : 8, hevc ? 34 : -1};
	size_t needed = hevc ? 3 : 2;
	const uint8_t *pointers[3] = {NULL, NULL, NULL};
	size_t sizes[3] = {0, 0, 0};
	for (size_t i = 0; i < count; i++) {
		int type = nalType(e->codec, nals[i]);
		for (size_t slot = 0; slot < needed; slot++) {
			if (type == wanted[slot] && !pointers[slot]) {
				pointers[slot] = nals[i].data;
				sizes[slot] = nals[i].size;
			}
		}
	}
	for (size_t slot = 0; slot < needed; slot++) {
		if (!pointers[slot]) {
			return kVTVideoEncoderMalfunctionErr;
		}
	}
	if (e->format) {
		CFRelease(e->format);
		e->format = NULL;
	}
	if (hevc) {
		return CMVideoFormatDescriptionCreateFromHEVCParameterSets(NULL, needed, pointers, sizes, 4, NULL,
		    &e->format);
	}
	return CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, needed, pointers, sizes, 4, &e->format);
}

/* Opens NVENC for the first frame's size with the settings so far; why not
 * in `reason`. */
static OSStatus openNVENC(Encoder *e, int width, int height, char *reason, size_t bytes)
{
	const StelaEncoderAPI *api = host();
	if (!api) {
		snprintf(reason, bytes, "StelaGPU.bundle gave no host encoder in this process");
		return kVTCouldNotFindVideoEncoderErr;
	}
	double fps = e->frameRate > 0 ? e->frameRate : 60;
	StelaEncoderConfig config = {
		.codec = e->codec == kCMVideoCodecType_HEVC ? StelaEncoderHEVC : StelaEncoderH264,
		.width = (uint32_t)width,
		.height = (uint32_t)height,
		.fps_milli = (uint32_t)(fps * 1000),
		.rate = StelaEncoderCBR,
		.bitrate = e->bitrate > 0 ? (uint32_t)e->bitrate : 0,
		.qp = 0,
	};
	uint8_t sets[kSetsBytes];
	size_t setsLength = 0;
	uint64_t most = 0;
	StelaEncoderStatus status = api->open(&config, sets, sizeof sets, &setsLength, &most, &e->nvenc, reason, bytes);
	if (status != StelaEncoderOk) {
		e->nvenc = NULL;
		return kVTCouldNotFindVideoEncoderErr;
	}
	OSStatus made = setsLength <= sizeof sets ? formatOfSets(e, sets, setsLength) : kVTVideoEncoderMalfunctionErr;
	e->outBytes = (size_t)most;
	e->out = made == noErr ? malloc(e->outBytes) : NULL;
	if (made != noErr || !e->out) {
		snprintf(reason, bytes, "the host's parameter sets (%zu bytes) make no format description: %d", setsLength,
		    (int)made);
		closeNVENC(e);
		return kVTVideoEncoderMalfunctionErr;
	}
	e->newBitrate = 0;
	e->sinceKey = 0;
	return noErr;
}

/* Picks the session's encoder at its first frame and says which, once. */
static OSStatus openBackend(Encoder *e, int width, int height, bool fullRange, bool bgra)
{
	char reason[160] = "";
	bool hevc = e->codec == kCMVideoCodecType_HEVC;
	if (!fullRange || hevc) {
		if (openNVENC(e, width, height, reason, sizeof reason) == noErr) {
			e->backend = BackendNVENC;
			os_log(logger(), "StelaVT: %{public}s session %dx%d %.0f fps %d kb/s encodes on NVENC (the host's) from %{public}s%{public}s",
			    codecName(e->codec), width, height, e->frameRate > 0 ? e->frameRate : 60, e->bitrate / 1000,
			    bgra ? "BGRA, which the host converts" : "420v",
			    fullRange ? ", full-range input coded as video range" : "");
			return noErr;
		}
	} else {
		snprintf(reason, sizeof reason, "full-range input, which NVENC's parameter sets do not signal");
	}
	if (hevc) {
		os_log_error(logger(), "StelaVT: HEVC session %dx%d has no encoder: NVENC refused: %{public}s", width, height,
		    reason);
		return kVTVideoEncoderNotAvailableNowErr;
	}
	OSStatus status = openX264(e, width, height, fullRange);
	if (status == noErr) {
		e->backend = BackendX264;
		os_log(logger(), "StelaVT: H.264 session %dx%d encodes on x264: NVENC refused: %{public}s", width, height,
		    reason);
	}
	return status;
}

/* ---- Emitting ------------------------------------------------------------ */

static void markSync(CMSampleBufferRef sample, bool key)
{
	CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, true);
	CFMutableDictionaryRef first = (CFMutableDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
	CFDictionarySetValue(first, kCMSampleAttachmentKey_NotSync, key ? kCFBooleanFalse : kCFBooleanTrue);
	CFDictionarySetValue(first, kCMSampleAttachmentKey_DependsOnOthers, key ? kCFBooleanFalse : kCFBooleanTrue);
}

/* Emits `length` bytes of length-prefixed sample data at `data`, which the
 * sample takes (malloc's); frees it on failure. */
static OSStatus emitSample(Encoder *e, VTEncoderFrameToken token, uint8_t *data, size_t length, bool key, CMTime time,
    CMTime duration)
{
	CMSampleTimingInfo timing = {
		.duration = duration,
		.presentationTimeStamp = time,
		.decodeTimeStamp = time,
	};
	CMBlockBufferRef block = NULL;
	CMSampleBufferRef sample = NULL;
	OSStatus status = CMBlockBufferCreateWithMemoryBlock(NULL, data, length, kCFAllocatorMalloc, NULL, 0, length, 0,
	    &block);
	if (status != noErr) {
		free(data);
	}
	if (status == noErr) {
		status = CMSampleBufferCreateReady(NULL, block, e->format, 1, 1, &timing, 1, &length, &sample);
	}
	if (status == noErr) {
		markSync(sample, key);
	}
	e->frames++;
	e->bytes += length;
	e->idrs += key;
	OSStatus emitted = vt.emit(e->session, token, status, 0, status == noErr ? sample : NULL);
	if (sample) {
		CFRelease(sample);
	}
	if (block) {
		CFRelease(block);
	}
	return status == noErr ? emitted : status;
}

static OSStatus emitX264(Encoder *e, x264_nal_t *nals, int count, x264_picture_t *out)
{
	size_t total = 0;
	for (int i = 0; i < count; i++) {
		total += (size_t)nals[i].i_payload;
	}
	int slot = -1;
	for (int i = 0; i < kPending; i++) {
		if (e->pending[i].used && e->pending[i].pts == out->i_pts) {
			slot = i;
			break;
		}
	}
	if (slot < 0) {
		return kVTVideoEncoderMalfunctionErr;
	}
	e->pending[slot].used = false;
	uint8_t *data = malloc(total ? total : 1);
	if (!data) {
		return kVTVideoEncoderMalfunctionErr;
	}
	size_t offset = 0;
	for (int i = 0; i < count; i++) {
		memcpy(data + offset, nals[i].p_payload, (size_t)nals[i].i_payload);
		offset += (size_t)nals[i].i_payload;
	}
	return emitSample(e, e->pending[slot].token, data, total, out->b_keyframe, e->pending[slot].time,
	    e->pending[slot].duration);
}

/* NVENC's Annex B frame as one length-prefixed sample, its parameter sets
 * left to the format description. */
static OSStatus emitNVENC(Encoder *e, VTEncoderFrameToken token, const uint8_t *bytes, size_t length, bool key,
    CMTime time, CMTime duration)
{
	Nal nals[kMostNals];
	size_t count = splitAnnexB(bytes, length, nals, kMostNals);
	size_t total = 0;
	for (size_t i = 0; i < count; i++) {
		if (!outOfBand(e->codec, nalType(e->codec, nals[i]))) {
			total += 4 + nals[i].size;
		}
	}
	uint8_t *data = total ? malloc(total) : NULL;
	if (!data) {
		return kVTVideoEncoderMalfunctionErr;
	}
	size_t at = 0;
	for (size_t i = 0; i < count; i++) {
		if (outOfBand(e->codec, nalType(e->codec, nals[i]))) {
			continue;
		}
		uint32_t size = (uint32_t)nals[i].size;
		data[at] = (uint8_t)(size >> 24);
		data[at + 1] = (uint8_t)(size >> 16);
		data[at + 2] = (uint8_t)(size >> 8);
		data[at + 3] = (uint8_t)size;
		memcpy(data + at + 4, nals[i].data, nals[i].size);
		at += 4 + nals[i].size;
	}
	return emitSample(e, token, data, total, key, time, duration);
}

/* ---- Encoding ---------------------------------------------------------- */

/* BT.709 limited range, 8-bit fixed point: the host's own conversion
 * (crates/stela-driver-gpu/src/nvenc/surface.rs), so x264 and NVENC code the
 * same picture. */
static inline uint8_t lumaOf(int r, int g, int b)
{
	return (uint8_t)(16 + ((47 * r + 157 * g + 16 * b + 128) >> 8));
}

static inline uint8_t clampChroma(int v)
{
	return (uint8_t)(v < 16 ? 16 : v > 240 ? 240 : v);
}

/* BGRA `image` (locked) as NV12 in `e->scratch`: luma `width` bytes a row,
 * then interleaved CbCr, each the mean of a 2x2 block. */
static bool bgraToNV12(Encoder *e, CVImageBufferRef image, int width, int height)
{
	size_t need = (size_t)width * (size_t)height * 3 / 2;
	if (e->scratchBytes < need) {
		uint8_t *grown = realloc(e->scratch, need);
		if (!grown) {
			return false;
		}
		e->scratch = grown;
		e->scratchBytes = need;
	}
	const uint8_t *pixels = CVPixelBufferGetBaseAddress(image);
	size_t pitch = CVPixelBufferGetBytesPerRow(image);
	uint8_t *luma = e->scratch, *chroma = e->scratch + (size_t)width * (size_t)height;
	for (int y = 0; y + 1 < height; y += 2) {
		const uint8_t *above = pixels + (size_t)y * pitch, *below = above + pitch;
		uint8_t *top = luma + (size_t)y * (size_t)width, *bottom = top + width;
		uint8_t *uv = chroma + (size_t)(y / 2) * (size_t)width;
		for (int x = 0; x + 1 < width; x += 2) {
			const uint8_t *p[4] = {above + 4 * x, above + 4 * x + 4, below + 4 * x, below + 4 * x + 4};
			int r = 0, g = 0, b = 0;
			for (int i = 0; i < 4; i++) {
				b += p[i][0];
				g += p[i][1];
				r += p[i][2];
			}
			top[x] = lumaOf(p[0][2], p[0][1], p[0][0]);
			top[x + 1] = lumaOf(p[1][2], p[1][1], p[1][0]);
			bottom[x] = lumaOf(p[2][2], p[2][1], p[2][0]);
			bottom[x + 1] = lumaOf(p[3][2], p[3][1], p[3][0]);
			r = (r + 2) / 4;
			g = (g + 2) / 4;
			b = (b + 2) / 4;
			uv[x] = clampChroma(128 + ((-26 * r - 87 * g + 113 * b + 128) >> 8));
			uv[x + 1] = clampChroma(128 + ((112 * r - 102 * g - 10 * b + 128) >> 8));
		}
	}
	return true;
}

static OSStatus encodeX264(Encoder *e, VTEncoderFrameToken token, CVImageBufferRef image, CMTime time,
    CMTime duration, bool forceKey)
{
	int slot = -1;
	for (int i = 0; i < kPending; i++) {
		if (!e->pending[i].used) {
			slot = i;
			break;
		}
	}
	if (slot < 0) {
		return kVTVideoEncoderMalfunctionErr;
	}
	CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
	x264_picture_t in, out;
	x264_picture_init(&in);
	in.img.i_csp = X264_CSP_NV12;
	in.img.i_plane = 2;
	if (CVPixelBufferGetPixelFormatType(image) == kCVPixelFormatType_32BGRA) {
		/* x264 takes no BGRA for 4:2:0: converted here, as the host would. */
		int width = (int)CVPixelBufferGetWidth(image), height = (int)CVPixelBufferGetHeight(image);
		if (!bgraToNV12(e, image, width, height)) {
			CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
			return kVTVideoEncoderMalfunctionErr;
		}
		in.img.plane[0] = e->scratch;
		in.img.plane[1] = e->scratch + (size_t)width * (size_t)height;
		in.img.i_stride[0] = in.img.i_stride[1] = width;
	} else {
		for (int plane = 0; plane < 2; plane++) {
			in.img.plane[plane] = CVPixelBufferGetBaseAddressOfPlane(image, (size_t)plane);
			in.img.i_stride[plane] = (int)CVPixelBufferGetBytesPerRowOfPlane(image, (size_t)plane);
		}
	}
	in.i_pts = e->nextPts++;
	if (forceKey) {
		in.i_type = X264_TYPE_IDR;
	}
	e->pending[slot].used = true;
	e->pending[slot].pts = in.i_pts;
	e->pending[slot].token = token;
	e->pending[slot].time = time;
	e->pending[slot].duration = duration;
	x264_nal_t *nals;
	int count;
	int size = x264_encoder_encode(e->x264, &nals, &count, &in, &out);
	CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
	if (size < 0) {
		e->pending[slot].used = false;
		return kVTVideoEncoderMalfunctionErr;
	}
	return size > 0 ? emitX264(e, nals, count, &out) : noErr;
}

/* NVENC lost the session, or refused too many frames: H.264 goes on with
 * x264 from an IDR, under x264's format description; HEVC has no other
 * encoder. */
static OSStatus leaveNVENC(Encoder *e, int width, int height, bool fullRange, const char *why)
{
	closeNVENC(e);
	if (e->codec == kCMVideoCodecType_HEVC) {
		e->backend = BackendNone;
		os_log_error(logger(), "StelaVT: HEVC session %dx%d lost NVENC at frame %llu: %{public}s; no other encoder",
		    width, height, e->frame, why);
		return kVTVideoEncoderMalfunctionErr;
	}
	OSStatus status = openX264(e, width, height, fullRange);
	e->backend = status == noErr ? BackendX264 : BackendNone;
	os_log_error(logger(), "StelaVT: H.264 session %dx%d lost NVENC at frame %llu: %{public}s; x264 goes on from an IDR: %d",
	    width, height, e->frame, why, (int)status);
	return status;
}

/* One frame on NVENC: noErr with the frame emitted or dropped; an error when
 * the session must leave NVENC. */
static OSStatus encodeNVENC(Encoder *e, VTEncoderFrameToken token, CVImageBufferRef image, CMTime time,
    CMTime duration, bool forceKey, char *reason, size_t bytes)
{
	double fps = e->frameRate > 0 ? e->frameRate : 60;
	int keyFrames = e->keyInterval;
	if (e->keyDuration > 0) {
		int byDuration = (int)(e->keyDuration * fps);
		keyFrames = keyFrames > 0 && keyFrames < byDuration ? keyFrames : byDuration;
	}
	bool key = forceKey || (keyFrames > 0 && e->sinceKey >= keyFrames);
	CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
	bool bgra = CVPixelBufferGetPixelFormatType(image) == kCVPixelFormatType_32BGRA;
	StelaEncoderFrame frame = {
		.frame = ++e->frame,
		.force_idr = key,
		.bitrate = e->newBitrate > 0 ? (uint32_t)e->newBitrate : 0,
		.out = e->out,
		.out_bytes = e->outBytes,
	};
	if (bgra) {
		/* One plane, which the host converts while it stores it. */
		size_t pitch = CVPixelBufferGetBytesPerRow(image);
		frame.luma = CVPixelBufferGetBaseAddress(image);
		frame.luma_bytes = pitch * CVPixelBufferGetHeight(image);
		frame.luma_pitch = (uint32_t)pitch;
		frame.input = StelaEncoderBGRA;
		e->bgraFrames++;
	} else {
		size_t lumaPitch = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
		size_t chromaPitch = CVPixelBufferGetBytesPerRowOfPlane(image, 1);
		frame.luma = CVPixelBufferGetBaseAddressOfPlane(image, 0);
		frame.luma_bytes = lumaPitch * CVPixelBufferGetHeightOfPlane(image, 0);
		frame.chroma = CVPixelBufferGetBaseAddressOfPlane(image, 1);
		frame.chroma_bytes = chromaPitch * CVPixelBufferGetHeightOfPlane(image, 1);
		frame.luma_pitch = (uint32_t)lumaPitch;
		frame.chroma_pitch = (uint32_t)chromaPitch;
		frame.input = StelaEncoderNV12;
	}
	StelaEncoderResult result = {0};
	uint64_t began = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
	StelaEncoderStatus status = host()->encode(e->nvenc, &frame, &result, reason, bytes);
	if (status == StelaEncoderOk && result.bytes > e->outBytes) {
		/* Longer than the host said a frame takes: the same frame again. */
		uint8_t *grown = realloc(e->out, (size_t)result.bytes);
		if (grown) {
			e->out = grown;
			e->outBytes = (size_t)result.bytes;
			frame.out = e->out;
			frame.out_bytes = e->outBytes;
			status = host()->encode(e->nvenc, &frame, &result, reason, bytes);
		}
	}
	CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
	uint64_t micros = (clock_gettime_nsec_np(CLOCK_UPTIME_RAW) - began) / 1000;
	e->encodeMicros += micros;
	e->encodeMost = micros > e->encodeMost ? micros : e->encodeMost;
	if (status == StelaEncoderOk && result.bytes <= e->outBytes) {
		e->newBitrate = 0;
		e->drops = 0;
		e->sinceKey = result.idr ? 1 : e->sinceKey + 1;
		e->gpuMicros += result.gpu_us;
		e->convertMicros += result.convert_us;
		return emitNVENC(e, token, e->out, (size_t)result.bytes, result.idr, time, duration);
	}
	if (status == StelaEncoderDropped && ++e->drops < kMostDrops) {
		/* Nothing was coded; the session's next frame is an IDR. */
		os_log(logger(), "StelaVT: NVENC dropped frame %llu: %{public}s", e->frame, reason);
		e->sinceKey = 0;
		return vt.emit(e->session, token, noErr, kVTEncodeInfo_FrameDropped, NULL);
	}
	if (status == StelaEncoderOk) {
		snprintf(reason, bytes, "a frame of %llu bytes past its %zu", result.bytes, e->outBytes);
	}
	return kVTVideoEncoderMalfunctionErr;
}

static OSStatus encodeFrame(VTVideoEncoderRef encoder, VTEncoderFrameToken token, CVImageBufferRef image, CMTime time,
    CMTime duration, CFDictionaryRef options, VTEncodeInfoFlags *flags)
{
	Encoder *e = storage(encoder);
	OSType format = CVPixelBufferGetPixelFormatType(image);
	if (format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
	    && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange && format != kCVPixelFormatType_32BGRA) {
		return kVTPixelTransferNotSupportedErr;
	}
	/* BGRA becomes video-range NV12, on the host or here for x264. */
	bool fullRange = format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
	int width = (int)CVPixelBufferGetWidth(image), height = (int)CVPixelBufferGetHeight(image);
	bool forceKey = options && truth(CFDictionaryGetValue(options, kVTEncodeFrameOptionKey_ForceKeyFrame));
	pthread_mutex_lock(&e->lock);
	OSStatus status = noErr;
	if (e->backend == BackendNone) {
		status = openBackend(e, width, height, fullRange, format == kCVPixelFormatType_32BGRA);
	}
	if (status == noErr && e->backend == BackendNVENC) {
		char reason[160] = "";
		if (fullRange && e->codec == kCMVideoCodecType_H264) {
			/* x264 signals full range; NVENC does not. */
			status = leaveNVENC(e, width, height, fullRange, "full-range input");
			forceKey = true;
		} else {
			status = encodeNVENC(e, token, image, time, duration, forceKey, reason, sizeof reason);
			if (status != noErr) {
				status = leaveNVENC(e, width, height, fullRange, reason);
				forceKey = true;
			}
		}
	}
	if (status == noErr && e->backend == BackendX264) {
		status = encodeX264(e, token, image, time, duration, forceKey);
	}
	pthread_mutex_unlock(&e->lock);
	(void)flags;
	return status;
}

static OSStatus completeFrames(VTVideoEncoderRef encoder, CMTime until)
{
	Encoder *e = storage(encoder);
	OSStatus status = noErr;
	pthread_mutex_lock(&e->lock);
	/* NVENC holds no frame: each was emitted as it was encoded. */
	while (status == noErr && e->x264 && x264_encoder_delayed_frames(e->x264) > 0) {
		x264_nal_t *nals;
		int count;
		x264_picture_t out;
		int size = x264_encoder_encode(e->x264, &nals, &count, NULL, &out);
		if (size < 0) {
			status = kVTVideoEncoderMalfunctionErr;
		} else if (size > 0) {
			status = emitX264(e, nals, count, &out);
		}
	}
	pthread_mutex_unlock(&e->lock);
	(void)until;
	return status;
}

static const CMBaseClass kBaseClass = {
	.version = 1,
	.derivedStorageSize = sizeof(Encoder),
	.finalize = finalize,
	.copyDebugDescription = copyDebugDescription,
	.copyProperty = copyProperty,
	.setProperty = setProperty,
};

static const VTVideoEncoderClass kEncoderClass = {
	.version = 1,
	.startSession = startSession,
	.encodeFrame = encodeFrame,
	.completeFrames = completeFrames,
	.copySupportedPropertyDictionary = copySupportedPropertyDictionary,
};

static const VTVideoEncoderVTable kVTable = {NULL, &kBaseClass, &kEncoderClass};

/* The factory the bundle's Info.plist names, and the one StelaGPU.bundle
 * registers for 'avc1' and, once its host codes HEVC, 'hvc1'. */
__attribute__((visibility("default"))) OSStatus StelaVT_CreateInstance(FourCharCode codec, CFAllocatorRef allocator,
    VTVideoEncoderRef *out)
{
	pthread_once(&vtOnce, resolve);
	if (!vt.ok) {
		os_log_error(logger(), "StelaVT: VideoToolbox lacks a plugin entry point");
		return kVTVideoEncoderNotAvailableNowErr;
	}
	if (codec != kCMVideoCodecType_H264 && codec != kCMVideoCodecType_HEVC) {
		return kVTCouldNotFindVideoEncoderErr;
	}
	if (codec == kCMVideoCodecType_HEVC) {
		/* x264 has no HEVC: without NVENC the client takes another codec. */
		char reason[160] = "";
		if (!hostCodes(StelaEncoderHEVC, reason, sizeof reason)) {
			os_log(logger(), "StelaVT: HEVC refused: %{public}s", reason);
			return kVTCouldNotFindVideoEncoderErr;
		}
	}
	CMBaseObjectRef object = NULL;
	OSStatus status = vt.derivedCreate(allocator, &kVTable, vt.encoderClassID(), &object);
	if (status != noErr) {
		return status;
	}
	Encoder *e = vt.derivedStorage(object);
	memset(e, 0, sizeof *e);
	pthread_mutex_init(&e->lock, NULL);
	e->codec = codec;
	os_log_debug(logger(), "StelaVT: CreateInstance %{public}s", codecName(codec));
	*out = object;
	return noErr;
}
