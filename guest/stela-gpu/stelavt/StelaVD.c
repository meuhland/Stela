/*
 * StelaVD: a VideoToolbox H.264 and HEVC decoder for macOS running under
 * Stela, registered as a hardware decoder by StelaGPU.bundle in each process
 * with VideoToolbox (bundle/VideoEncoder.m), beside StelaVT's encoder.
 *
 * This first StelaVD decodes nothing itself: each session runs an inner
 * VideoToolbox session on Apple's software decoder and emits its pictures as
 * its own. It proves the plugin interface, the registration in the decoder
 * service and the picture path, before NVDEC is put behind it. While it
 * creates its inner session, the factory refuses (kVTCouldNotFindVideoDecoderErr)
 * so that VideoToolbox falls back to its software decoder for that one.
 *
 * VideoToolbox's decoder plugin interface is private. Its layout here follows
 * Apple's own decoder plugins (AppleH264SW.bundle, macOS 27.2,
 * kH264VideoDecoder_BaseClass and kH264VideoDecoder_VideoDecoderClass):
 * CoreMedia's derived-object pattern, a base class (version 1) and a decoder
 * class (version 3: startSession, decodeFrame, copySupportedPropertyDictionary,
 * setProperties, copySerializableProperties, canAcceptFormatDescription,
 * finishDelayedFrames, startTileSession, decodeTile).
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
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* ---- The private interface ---------------------------------------------- */

/*
 * On arm64e CoreMedia authenticates every pointer of these tables
 * (CMDerivedObjectCreate: `autda` of the vtable's base class with the
 * field's address and 0xb911), so each field is signed as Apple's plugins
 * sign it: the class pointers with the DA key and the functions with the IA
 * key, both address-diversified, with the discriminators of AppleH264SW's
 * fixups (macOS 27.2). Unsigned tables kill an arm64e client with
 * EXC_ARM_PAC_FAIL (WallpaperAerialsExtension did). Fields whose
 * discriminator is unknown stay NULL. The arm64 slice signs nothing.
 */
#if __has_feature(ptrauth_qualifier)
#include <ptrauth.h>
#define SIGNED_CLASS(d) __ptrauth(ptrauth_key_asda, 1, d)
#define SIGNED_CODE(d) __ptrauth(ptrauth_key_asia, 1, d)
#else
#define SIGNED_CLASS(d)
#define SIGNED_CODE(d)
#endif

typedef CFTypeRef VDBaseObjectRef;
typedef VDBaseObjectRef VTVideoDecoderRef;
typedef struct OpaqueVTVideoDecoderSession *VTVideoDecoderSession;
typedef struct OpaqueVTVideoDecoderFrame *VTVideoDecoderFrame;
typedef const void *VDBaseClassID;

typedef struct {
	uintptr_t version;
	size_t derivedStorageSize;
	const void *equal; /* discriminator unknown: NULL */
	OSStatus (*SIGNED_CODE(0x3af8) invalidate)(VDBaseObjectRef);
	void (*SIGNED_CODE(0x5680) finalize)(VDBaseObjectRef);
	CFStringRef (*SIGNED_CODE(0xf18e) copyDebugDescription)(VDBaseObjectRef);
	OSStatus (*SIGNED_CODE(0x9725) copyProperty)(VDBaseObjectRef, CFStringRef, CFAllocatorRef, void *);
	OSStatus (*SIGNED_CODE(0x2724) setProperty)(VDBaseObjectRef, CFStringRef, CFTypeRef);
	/* Read from version 2 (0xd563); this class is version 1. */
	const void *notificationBarrier;
	const void *reserved[3];
} VDBaseClass;

typedef struct {
	uintptr_t version;
	OSStatus (*SIGNED_CODE(0x7be9) startSession)(VTVideoDecoderRef, VTVideoDecoderSession,
	    CMVideoFormatDescriptionRef);
	OSStatus (*SIGNED_CODE(0xaf83) decodeFrame)(VTVideoDecoderRef, VTVideoDecoderFrame, CMSampleBufferRef,
	    VTDecodeFrameFlags, VTDecodeInfoFlags *);
	OSStatus (*SIGNED_CODE(0x50eb) copySupportedPropertyDictionary)(VTVideoDecoderRef, CFDictionaryRef *);
	const void *setProperties;              /* unknown: NULL */
	const void *copySerializableProperties; /* unknown: NULL */
	Boolean (*SIGNED_CODE(0x3f3c) canAcceptFormatDescription)(VTVideoDecoderRef, CMFormatDescriptionRef);
	const void *finishDelayedFrames; /* unknown: NULL; StelaVD emits each frame before decodeFrame returns */
	const void *startTileSession;    /* 0x9605, not offered */
	const void *decodeTile;          /* 0xba86, not offered */
	const void *reserved[8];
} VTVideoDecoderClass;

typedef struct {
	const void *reserved;
	const VDBaseClass *SIGNED_CLASS(0xb911) baseClass;
	const VTVideoDecoderClass *SIGNED_CLASS(0x2356) derivedClass;
} VTVideoDecoderVTable;

static struct {
	OSStatus (*derivedCreate)(CFAllocatorRef, const void *, VDBaseClassID, VDBaseObjectRef *);
	void *(*derivedStorage)(VDBaseObjectRef);
	VDBaseClassID (*decoderClassID)(void);
	VDBaseObjectRef (*decoderBase)(VTVideoDecoderRef);
	OSStatus (*setPixelBufferAttributes)(VTVideoDecoderSession, CFDictionaryRef);
	OSStatus (*emit)(VTVideoDecoderSession, VTVideoDecoderFrame, OSStatus, VTDecodeInfoFlags, CVImageBufferRef);
	bool ok;
} vd;

static pthread_once_t vdOnce = PTHREAD_ONCE_INIT;

static void resolve(void)
{
	vd.derivedCreate = dlsym(RTLD_DEFAULT, "CMDerivedObjectCreate");
	vd.derivedStorage = dlsym(RTLD_DEFAULT, "CMBaseObjectGetDerivedStorage");
	vd.decoderClassID = dlsym(RTLD_DEFAULT, "VTVideoDecoderGetClassID");
	vd.decoderBase = dlsym(RTLD_DEFAULT, "VTVideoDecoderGetCMBaseObject");
	vd.setPixelBufferAttributes = dlsym(RTLD_DEFAULT, "VTDecoderSessionSetPixelBufferAttributes");
	vd.emit = dlsym(RTLD_DEFAULT, "VTDecoderSessionEmitDecodedFrame");
	vd.ok = vd.derivedCreate && vd.derivedStorage && vd.decoderClassID && vd.decoderBase
	    && vd.setPixelBufferAttributes && vd.emit;
}

static os_log_t logger(void)
{
	static os_log_t log;
	static dispatch_once_t once;
	dispatch_once(&once, ^{
		log = os_log_create("org.stela.StelaVT", "decoder");
	});
	return log;
}

/* ---- A session ---------------------------------------------------------- */

typedef struct {
	CMVideoCodecType codec;
	VTVideoDecoderSession session;
	CMVideoFormatDescriptionRef format;
	VTDecompressionSessionRef inner;
	/* The frame the inner callback emitted last, so a refused decode is
	 * emitted once. */
	VTVideoDecoderFrame lastEmitted;
	uint64_t frames, errors, decodeNs, maxNs;
} Decoder;

/* Set while this thread creates an inner session: the factory refuses. */
static __thread bool creatingInner;

static const char *codecName(CMVideoCodecType codec)
{
	return codec == kCMVideoCodecType_HEVC ? "HEVC" : "H.264";
}

static uint64_t nowNs(void)
{
	return clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
}

static Decoder *storage(VTVideoDecoderRef decoder)
{
	return vd.derivedStorage(vd.decoderBase(decoder));
}

static void innerOutput(void *refcon, void *frameRefcon, OSStatus status, VTDecodeInfoFlags flags,
    CVImageBufferRef image, CMTime pts, CMTime duration)
{
	(void)pts;
	(void)duration;
	Decoder *d = refcon;
	if (status != noErr) {
		d->errors++;
	}
	d->lastEmitted = frameRefcon;
	vd.emit(d->session, frameRefcon, status, flags & ~kVTDecodeInfo_Asynchronous, image);
}

static void endInner(Decoder *d)
{
	if (d->inner) {
		VTDecompressionSessionWaitForAsynchronousFrames(d->inner);
		VTDecompressionSessionInvalidate(d->inner);
		CFRelease(d->inner);
		d->inner = NULL;
	}
}

static void finalize(VDBaseObjectRef object)
{
	Decoder *d = vd.derivedStorage(object);
	if (d->format) {
		CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(d->format);
		os_log(logger(), "StelaVD: %{public}s %dx%d session in %{public}s ended: %llu frames, decode %.2f ms "
		    "average, %.2f ms longest, %llu errors (Apple's software decoder inside)",
		    codecName(d->codec), size.width, size.height, getprogname(), d->frames,
		    d->frames ? d->decodeNs / 1e6 / d->frames : 0.0, d->maxNs / 1e6, d->errors);
	}
	endInner(d);
	if (d->format) {
		CFRelease(d->format);
		d->format = NULL;
	}
}

static OSStatus invalidate(VDBaseObjectRef object)
{
	endInner(vd.derivedStorage(object));
	return noErr;
}

static CFStringRef copyDebugDescription(VDBaseObjectRef object)
{
	Decoder *d = vd.derivedStorage(object);
	return CFStringCreateWithFormat(NULL, NULL, CFSTR("<StelaVD %s, %llu frames>"), codecName(d->codec),
	    d->frames);
}

/* Properties go to the inner session, which has the software decoder's. */
static OSStatus copyProperty(VDBaseObjectRef object, CFStringRef key, CFAllocatorRef allocator, void *value)
{
	Decoder *d = vd.derivedStorage(object);
	if (!d->inner) {
		return kVTPropertyNotSupportedErr;
	}
	return VTSessionCopyProperty(d->inner, key, allocator, value);
}

static OSStatus setProperty(VDBaseObjectRef object, CFStringRef key, CFTypeRef value)
{
	Decoder *d = vd.derivedStorage(object);
	if (!d->inner) {
		return kVTPropertyNotSupportedErr;
	}
	return VTSessionSetProperty(d->inner, key, value);
}

/* The picture format of a stream: 4:2:0 video range in 8 or 10 bits, from
 * the HEVC configuration record (hvcC: chroma_format_idc in byte 16,
 * bit_depth_luma_minus8 in byte 17); H.264 is taken as 8-bit 4:2:0. Zero for
 * anything else, which gets the software decoder's own pictures. */
static OSType pictureFormat(CMVideoFormatDescriptionRef format)
{
	if (CMFormatDescriptionGetMediaSubType(format) != kCMVideoCodecType_HEVC) {
		return kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
	}
	CFDictionaryRef atoms = CMFormatDescriptionGetExtension(format,
	    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms);
	CFDataRef hvcC = atoms && CFGetTypeID(atoms) == CFDictionaryGetTypeID() ? CFDictionaryGetValue(atoms, CFSTR("hvcC"))
	                                                                         : NULL;
	if (!hvcC || CFGetTypeID(hvcC) != CFDataGetTypeID() || CFDataGetLength(hvcC) < 23) {
		return 0;
	}
	const UInt8 *record = CFDataGetBytePtr(hvcC);
	int chroma = record[16] & 3, depth = 8 + (record[17] & 7);
	if (chroma != 1) {
		return 0;
	}
	return depth == 8 ? kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
	    : depth == 10 ? kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
	                  : 0;
}

/* The pictures StelaVD hands VideoToolbox, which converts them for the
 * client as it would a hardware decoder's: 4:2:0 video range at the stream's
 * depth, IOSurface. NULL when the format is left to the software decoder. */
static CFDictionaryRef copyPictureAttributes(OSType pixelFormat)
{
	if (!pixelFormat) {
		return NULL;
	}
	int32_t format = (int32_t)pixelFormat;
	CFNumberRef number = CFNumberCreate(NULL, kCFNumberSInt32Type, &format);
	CFDictionaryRef surface = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	const void *keys[] = {kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey};
	const void *values[] = {number, surface};
	CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 2, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	CFRelease(number);
	CFRelease(surface);
	return attributes;
}

static OSStatus startSession(VTVideoDecoderRef decoder, VTVideoDecoderSession session,
    CMVideoFormatDescriptionRef format)
{
	Decoder *d = storage(decoder);
	d->session = session;
	if (d->format) {
		CFRelease(d->format);
	}
	d->format = (CMVideoFormatDescriptionRef)CFRetain(format);
	endInner(d);
	OSType pixelFormat = pictureFormat(format);
	CFDictionaryRef attributes = copyPictureAttributes(pixelFormat);
	OSStatus status = attributes ? vd.setPixelBufferAttributes(session, attributes) : noErr;
	if (status == noErr) {
		VTDecompressionOutputCallbackRecord callback = {innerOutput, d};
		creatingInner = true;
		status = VTDecompressionSessionCreate(NULL, format, NULL, attributes, &callback, &d->inner);
		creatingInner = false;
	}
	if (attributes) {
		CFRelease(attributes);
	}
	CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(format);
	if (status == noErr) {
		char name[5] = {(char)(pixelFormat >> 24), (char)(pixelFormat >> 16), (char)(pixelFormat >> 8),
		    (char)pixelFormat, 0};
		os_log(logger(), "StelaVD: %{public}s %dx%d session in %{public}s[%d], pictures %{public}s (Apple's software "
		    "decoder inside)",
		    codecName(d->codec), size.width, size.height, getprogname(), getpid(), pixelFormat ? name : "native");
	} else {
		os_log_error(logger(), "StelaVD: %{public}s %dx%d session in %{public}s refused: %d",
		    codecName(d->codec), size.width, size.height, getprogname(), (int)status);
	}
	return status;
}

static OSStatus decodeFrame(VTVideoDecoderRef decoder, VTVideoDecoderFrame frame, CMSampleBufferRef sample,
    VTDecodeFrameFlags flags, VTDecodeInfoFlags *info)
{
	(void)flags;
	(void)info;
	Decoder *d = storage(decoder);
	if (!d->inner) {
		vd.emit(d->session, frame, kVTInvalidSessionErr, 0, NULL);
		return noErr;
	}
	uint64_t start = nowNs();
	VTDecodeInfoFlags innerInfo = 0;
	/* Synchronous: the inner callback emits this frame before it returns. */
	OSStatus status = VTDecompressionSessionDecodeFrame(d->inner, sample, 0, frame, &innerInfo);
	uint64_t ns = nowNs() - start;
	d->frames++;
	d->decodeNs += ns;
	if (ns > d->maxNs) {
		d->maxNs = ns;
	}
	if (status != noErr && d->lastEmitted != frame) {
		d->errors++;
		vd.emit(d->session, frame, status, 0, NULL);
	}
	return noErr;
}

static OSStatus copySupportedPropertyDictionary(VTVideoDecoderRef decoder, CFDictionaryRef *out)
{
	(void)decoder;
	*out = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	return noErr;
}

static Boolean canAcceptFormatDescription(VTVideoDecoderRef decoder, CMFormatDescriptionRef format)
{
	return CMFormatDescriptionGetMediaSubType(format) == storage(decoder)->codec;
}

static const VDBaseClass kBaseClass = {
	.version = 1,
	.derivedStorageSize = sizeof(Decoder),
	.invalidate = invalidate,
	.finalize = finalize,
	.copyDebugDescription = copyDebugDescription,
	.copyProperty = copyProperty,
	.setProperty = setProperty,
};

static const VTVideoDecoderClass kDecoderClass = {
	.version = 3,
	.startSession = startSession,
	.decodeFrame = decodeFrame,
	.copySupportedPropertyDictionary = copySupportedPropertyDictionary,
	.canAcceptFormatDescription = canAcceptFormatDescription,
};

static const VTVideoDecoderVTable kVTable = {NULL, &kBaseClass, &kDecoderClass};

/* The factory StelaGPU.bundle registers for 'avc1' and 'hvc1'. */
__attribute__((visibility("default"))) OSStatus StelaVD_CreateInstance(FourCharCode codec, CFAllocatorRef allocator,
    VTVideoDecoderRef *out)
{
	pthread_once(&vdOnce, resolve);
	*out = NULL;
	if (!vd.ok) {
		os_log_error(logger(), "StelaVD: VideoToolbox lacks a decoder plugin entry point");
		return kVTCouldNotFindVideoDecoderErr;
	}
	if (creatingInner || (codec != kCMVideoCodecType_H264 && codec != kCMVideoCodecType_HEVC)) {
		return kVTCouldNotFindVideoDecoderErr;
	}
	VDBaseObjectRef object = NULL;
	OSStatus status = vd.derivedCreate(allocator, &kVTable, vd.decoderClassID(), &object);
	if (status != noErr) {
		return status;
	}
	Decoder *d = vd.derivedStorage(object);
	memset(d, 0, sizeof *d);
	d->codec = codec;
	*out = object;
	return noErr;
}
