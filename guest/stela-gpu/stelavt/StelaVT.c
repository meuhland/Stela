/*
 * StelaVT: a VideoToolbox H.264 encoder plugin backed by x264, for macOS
 * running under Stela, which has no Apple video encoder. It registers as a
 * hardware encoder so real-time clients (Parsec, screen sharing) that ask
 * for one get a fast software encoder instead of Apple's software HEVC.
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
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <x264.h>

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

/* ---- The encoder ---------------------------------------------------------- */

enum { kPending = 64 };

typedef struct {
	pthread_mutex_t lock;
	VTEncoderSessionRef session;
	int32_t width, height;
	x264_t *x264;
	CMFormatDescriptionRef format;
	/* Settings, applied when x264 opens at the first frame. */
	int bitrate; /* bits per second, 0: default */
	int keyInterval;
	double frameRate;
	bool fullRange;
	/* Frames x264 holds, by its pts. */
	struct {
		bool used;
		int64_t pts;
		VTEncoderFrameToken token;
		CMTime time, duration;
	} pending[kPending];
	int64_t nextPts;
	uint64_t frames, bytes;
} Encoder;

static Encoder *storage(VTVideoEncoderRef encoder)
{
	return vt.derivedStorage(vt.encoderBase(encoder));
}

static void finalize(CMBaseObjectRef object)
{
	Encoder *e = vt.derivedStorage(object);
	if (e->x264) {
		x264_encoder_close(e->x264);
	}
	if (e->format) {
		CFRelease(e->format);
	}
	os_log(logger(), "StelaVT: closed after %llu frames, %llu bytes", e->frames, e->bytes);
	pthread_mutex_destroy(&e->lock);
}

static CFStringRef copyDebugDescription(CMBaseObjectRef object)
{
	Encoder *e = vt.derivedStorage(object);
	return CFStringCreateWithFormat(NULL, NULL, CFSTR("<StelaVT x264 H.264 %dx%d, %llu frames>"), e->width,
	    e->height, e->frames);
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
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_ExpectedFrameRate)) {
		if (number(value, &n) && n > 0) {
			e->frameRate = n;
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_MaxKeyFrameInterval)) {
		if (number(value, &n) && n > 0) {
			e->keyInterval = (int)n;
		}
	} else if (CFEqual(key, kVTCompressionPropertyKey_RealTime)
	    || CFEqual(key, kVTCompressionPropertyKey_AllowFrameReordering)
	    || CFEqual(key, kVTCompressionPropertyKey_ProfileLevel)
	    || CFEqual(key, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration)
	    || CFEqual(key, kVTCompressionPropertyKey_DataRateLimits)
	    || CFEqual(key, kVTCompressionPropertyKey_Quality)
	    || CFEqual(key, kVTCompressionPropertyKey_H264EntropyMode)
	    || CFEqual(key, kVTCompressionPropertyKey_ColorPrimaries)
	    || CFEqual(key, kVTCompressionPropertyKey_TransferFunction)
	    || CFEqual(key, kVTCompressionPropertyKey_YCbCrMatrix)
	    || CFEqual(key, kVTVideoEncoderSpecification_EnableLowLatencyRateControl)) {
		/* Accepted: x264's zerolatency tuning already never reorders. */
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
	/* NV12, video or full range: what x264 reads without conversion. */
	int formats[] = {kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange};
	CFNumberRef numbers[2];
	for (int i = 0; i < 2; i++) {
		numbers[i] = CFNumberCreate(NULL, kCFNumberIntType, &formats[i]);
	}
	CFArrayRef list = CFArrayCreate(NULL, (const void **)numbers, 2, &kCFTypeArrayCallBacks);
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
	CFRelease(numbers[0]);
	CFRelease(numbers[1]);
	os_log(logger(), "StelaVT: session %dx%d, pixel buffer attributes %d", size.width, size.height, (int)status);
	return status;
}

/* x264 opened for the first frame's size and range, with the settings so far. */
static OSStatus openEncoder(Encoder *e, int width, int height, bool fullRange)
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
	OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, 2, sets, sizes, 4, &e->format);
	os_log(logger(), "StelaVT: x264 open %dx%d %.1f fps %d kbps keyint %d range %s: %d", width, height, fps, kbps,
	    p.i_keyint_max, fullRange ? "full" : "video", (int)status);
	return status;
}

static OSStatus emit(Encoder *e, x264_nal_t *nals, int count, x264_picture_t *out)
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
	VTEncoderFrameToken token = e->pending[slot].token;
	CMSampleTimingInfo timing = {
		.duration = e->pending[slot].duration,
		.presentationTimeStamp = e->pending[slot].time,
		.decodeTimeStamp = e->pending[slot].time,
	};
	e->pending[slot].used = false;
	CMBlockBufferRef block = NULL;
	CMSampleBufferRef sample = NULL;
	OSStatus status = CMBlockBufferCreateWithMemoryBlock(NULL, NULL, total, NULL, NULL, 0, total,
	    kCMBlockBufferAssureMemoryNowFlag, &block);
	size_t offset = 0;
	for (int i = 0; status == noErr && i < count; i++) {
		status = CMBlockBufferReplaceDataBytes(nals[i].p_payload, block, offset, (size_t)nals[i].i_payload);
		offset += (size_t)nals[i].i_payload;
	}
	if (status == noErr) {
		status = CMSampleBufferCreateReady(NULL, block, e->format, 1, 1, &timing, 1, &total, &sample);
	}
	if (status == noErr) {
		CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, true);
		CFMutableDictionaryRef first = (CFMutableDictionaryRef)CFArrayGetValueAtIndex(attachments, 0);
		bool key = out->b_keyframe;
		CFDictionarySetValue(first, kCMSampleAttachmentKey_NotSync, key ? kCFBooleanFalse : kCFBooleanTrue);
		CFDictionarySetValue(first, kCMSampleAttachmentKey_DependsOnOthers, key ? kCFBooleanFalse : kCFBooleanTrue);
	}
	e->frames++;
	e->bytes += total;
	OSStatus emitted = vt.emit(e->session, token, status, 0, status == noErr ? sample : NULL);
	if (sample) {
		CFRelease(sample);
	}
	if (block) {
		CFRelease(block);
	}
	return status == noErr ? emitted : status;
}

static OSStatus encodeFrame(VTVideoEncoderRef encoder, VTEncoderFrameToken token, CVImageBufferRef image, CMTime time,
    CMTime duration, CFDictionaryRef options, VTEncodeInfoFlags *flags)
{
	Encoder *e = storage(encoder);
	OSType format = CVPixelBufferGetPixelFormatType(image);
	if (format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
	    && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) {
		return kVTPixelTransferNotSupportedErr;
	}
	pthread_mutex_lock(&e->lock);
	OSStatus status = noErr;
	if (!e->x264) {
		status = openEncoder(e, (int)CVPixelBufferGetWidth(image), (int)CVPixelBufferGetHeight(image),
		    format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
	}
	int slot = -1;
	for (int i = 0; status == noErr && i < kPending; i++) {
		if (!e->pending[i].used) {
			slot = i;
			break;
		}
	}
	if (status == noErr && slot < 0) {
		status = kVTVideoEncoderMalfunctionErr;
	}
	if (status == noErr) {
		CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
		x264_picture_t in, out;
		x264_picture_init(&in);
		in.img.i_csp = X264_CSP_NV12;
		in.img.i_plane = 2;
		for (int plane = 0; plane < 2; plane++) {
			in.img.plane[plane] = CVPixelBufferGetBaseAddressOfPlane(image, (size_t)plane);
			in.img.i_stride[plane] = (int)CVPixelBufferGetBytesPerRowOfPlane(image, (size_t)plane);
		}
		in.i_pts = e->nextPts++;
		if (options && truth(CFDictionaryGetValue(options, kVTEncodeFrameOptionKey_ForceKeyFrame))) {
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
			status = kVTVideoEncoderMalfunctionErr;
		} else if (size > 0) {
			status = emit(e, nals, count, &out);
		}
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
	while (status == noErr && e->x264 && x264_encoder_delayed_frames(e->x264) > 0) {
		x264_nal_t *nals;
		int count;
		x264_picture_t out;
		int size = x264_encoder_encode(e->x264, &nals, &count, NULL, &out);
		if (size < 0) {
			status = kVTVideoEncoderMalfunctionErr;
		} else if (size > 0) {
			status = emit(e, nals, count, &out);
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

/* The factory the bundle's Info.plist names. */
__attribute__((visibility("default"))) OSStatus StelaVT_CreateInstance(FourCharCode codec, CFAllocatorRef allocator,
    VTVideoEncoderRef *out)
{
	pthread_once(&vtOnce, resolve);
	if (!vt.ok) {
		os_log_error(logger(), "StelaVT: VideoToolbox lacks a plugin entry point");
		return kVTVideoEncoderNotAvailableNowErr;
	}
	if (codec != kCMVideoCodecType_H264) {
		return kVTCouldNotFindVideoEncoderErr;
	}
	CMBaseObjectRef object = NULL;
	OSStatus status = vt.derivedCreate(allocator, &kVTable, vt.encoderClassID(), &object);
	if (status != noErr) {
		return status;
	}
	Encoder *e = vt.derivedStorage(object);
	memset(e, 0, sizeof *e);
	pthread_mutex_init(&e->lock, NULL);
	os_log_debug(logger(), "StelaVT: CreateInstance");
	*out = object;
	return noErr;
}
