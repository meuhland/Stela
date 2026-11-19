/*
 * StelaVD: a VideoToolbox H.264 and HEVC decoder for macOS running under
 * Stela, registered as a hardware decoder by StelaGPU.bundle in each process
 * with VideoToolbox (bundle/VideoEncoder.m), beside StelaVT's encoder.
 *
 * Two backends, one per session:
 *
 * - NVDEC, the host's decoder (StelaDecoder.h), which the bundle hands over
 *   through StelaVD_UseDecoders before it registers StelaVD. startSession
 *   keeps the stream's avcC/hvcC and sets the pixel buffers' format (420v,
 *   or x420 at 10 bits); the first decodeFrame opens the host session, in
 *   display order when VideoToolbox asks for temporal processing
 *   (kVTDecodeFrame_EnableTemporalProcessing) and in decode order when not.
 *   Each sample goes to the host with pixel buffers from the session's pool
 *   as destinations; each picture the host shows in one is emitted with the
 *   frame token of the sample that decoded it, which StelaVD keeps by the
 *   number it gave that sample. A sample with no picture is emitted dropped
 *   at once. finishDelayedFrames flushes what the host still holds.
 * - Apple's software decoder, in an inner VideoToolbox session whose
 *   pictures are emitted as StelaVD's own: for every stream the host
 *   refuses, when StelaGPU gave no host decoder, and after the host fails.
 *   While StelaVD creates its inner session the factory refuses
 *   (kVTCouldNotFindVideoDecoderErr), so VideoToolbox falls back to its
 *   software decoder for that one.
 *
 * Fallback: a host that refuses the open hands the session to Apple's
 * decoder at once. A sample the host's parser or engine refuses is emitted
 * as an error; three in a row, or a session the host lost, or any other
 * failure, close the host session: every frame it still held is emitted as
 * an error, and so is each next frame until a sync sample, which starts
 * Apple's decoder for the rest of the session. A client is never sent a
 * frame twice nor left without one.
 *
 * VideoToolbox's decoder plugin interface is private. Its layout here follows
 * Apple's own decoder plugins (AppleH264SW.bundle and AV1DecoderSW.bundle,
 * macOS 27.2): CoreMedia's derived-object pattern, a base class (version 1)
 * and a decoder class (version 3: startSession, decodeFrame,
 * copySupportedPropertyDictionary, setProperties, copySerializableProperties,
 * canAcceptFormatDescription, finishDelayedFrames, startTileSession,
 * decodeTile).
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
#include <unistd.h>

#include "StelaDecoder.h"

/* ---- The private interface ---------------------------------------------- */

/*
 * On arm64e CoreMedia authenticates every pointer of these tables
 * (CMDerivedObjectCreate: `autda` of the vtable's base class with the
 * field's address and 0xb911), so each field is signed as Apple's plugins
 * sign it: the class pointers with the DA key and the functions with the IA
 * key, both address-diversified, with the discriminators of AppleH264SW's
 * and AV1DecoderSW's fixups (macOS 27.2). Unsigned tables kill an arm64e
 * client with EXC_ARM_PAC_FAIL (WallpaperAerialsExtension did). Fields whose
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
	const void *setProperties;              /* +0x20, empty in Apple's too: NULL */
	const void *copySerializableProperties; /* +0x28, empty in Apple's too: NULL */
	Boolean (*SIGNED_CODE(0x3f3c) canAcceptFormatDescription)(VTVideoDecoderRef, CMFormatDescriptionRef);
	/* +0x38: AV1DecoderSW's fixup (auth-rebase div=0xBFC3 ad=1 key=IA). */
	OSStatus (*SIGNED_CODE(0xbfc3) finishDelayedFrames)(VTVideoDecoderRef);
	const void *startTileSession; /* 0x9605, not offered */
	const void *decodeTile;       /* 0xba86, not offered */
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
	/* The session's pixel buffers, as Apple's decoders make theirs: its
	 * pool, else one at a time. Either may be absent. */
	CVPixelBufferPoolRef (*pixelBufferPool)(VTVideoDecoderSession);
	OSStatus (*createPixelBuffer)(VTVideoDecoderSession, CVPixelBufferRef *);
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
	vd.pixelBufferPool = dlsym(RTLD_DEFAULT, "VTDecoderSessionGetPixelBufferPool");
	vd.createPixelBuffer = dlsym(RTLD_DEFAULT, "VTDecoderSessionCreatePixelBuffer");
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

/* ---- The host's decoder ------------------------------------------------ */

/* The bundle's table, which its constructor hands over. */
static _Atomic(const StelaDecoderAPI *) handed;

/* StelaGPU.bundle calls this before it registers StelaVD. */
__attribute__((visibility("default"))) void StelaVD_UseDecoders(const StelaDecoderAPI *api)
{
	if (api && api->version >= STELA_DECODER_API_VERSION) {
		atomic_store(&handed, api);
	}
}

/* The host's decoder calls: the handed table, or the bundle's by class when
 * this image is not the one the bundle loaded; NULL without the bundle. */
static const StelaDecoderAPI *host(void)
{
	const StelaDecoderAPI *api = atomic_load(&handed);
	if (api) {
		return api;
	}
	void *(*getClass)(const char *) = dlsym(RTLD_DEFAULT, "objc_getClass");
	void *(*registerName)(const char *) = dlsym(RTLD_DEFAULT, "sel_registerName");
	void *send = dlsym(RTLD_DEFAULT, "objc_msgSend");
	void *class = getClass ? getClass(STELA_DECODER_CLASS) : NULL;
	if (!class || !registerName || !send) {
		return NULL;
	}
	const StelaDecoderAPI *(*message)(void *, void *) = (const StelaDecoderAPI *(*)(void *, void *))send;
	api = message(class, registerName("api"));
	if (!api || api->version < STELA_DECODER_API_VERSION) {
		return NULL;
	}
	atomic_store(&handed, api);
	return api;
}

/* ---- A session ---------------------------------------------------------- */

typedef enum {
	/* No decoder yet (before startSession), or the host failed and the
	 * next sync sample starts Apple's. */
	BackendNone,
	/* The host's NVDEC; `host` is NULL until the first frame opens it. */
	BackendHost,
	BackendApple,
} Backend;

/* The frame token of each sample given to the host, by the number StelaVD
 * gave it, until the host shows its picture. */
typedef struct {
	uint64_t number;
	VTVideoDecoderFrame frame;
} Token;

#define TOKENS 64
/* Destinations a host decode gives, and spare pixel buffers kept. */
#define DESTINATIONS 3
#define SPARES 4
/* Samples refused in a row before the host session is given up. */
#define MOST_DROPS 3

typedef struct {
	CMVideoCodecType codec;
	VTVideoDecoderSession session;
	CMVideoFormatDescriptionRef format;
	/* The stream's avcC or hvcC, and the pictures' format. */
	CFDataRef record;
	OSType pixelFormat;
	Backend backend;

	/* Apple's software decoder. */
	VTDecompressionSessionRef inner;
	/* The frame the inner callback emitted last, so a refused decode is
	 * emitted once. */
	VTVideoDecoderFrame lastEmitted;

	/* The host's decoder. */
	StelaDecoderSession *host;
	StelaDecoderStream stream;
	bool decodeOrder;
	uint64_t nextNumber;
	Token tokens[TOKENS];
	unsigned tokenCount;
	unsigned drops;
	CVPixelBufferRef spares[SPARES];
	unsigned spareCount;
	char why[160];

	/* Statistics. */
	uint64_t frames, errors, decodeNs, maxNs;
	uint64_t shown, dropped, hostCalls, gpuUs, ceUs, copyUs, hostPictures;
} Decoder;

/* Set while this thread creates an inner session: the factory refuses. */
static __thread bool creatingInner;

static const char *codecName(CMVideoCodecType codec)
{
	return codec == kCMVideoCodecType_HEVC ? "HEVC" : "H.264";
}

static const char *backendName(Backend backend)
{
	switch (backend) {
	case BackendHost:
		return "NVDEC";
	case BackendApple:
		return "Apple software";
	default:
		return "none";
	}
}

static uint64_t nowNs(void)
{
	return clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
}

static Decoder *storage(VTVideoDecoderRef decoder)
{
	return vd.derivedStorage(vd.decoderBase(decoder));
}

/* ---- Apple's software decoder inside ------------------------------------ */

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

/* The pictures StelaVD hands VideoToolbox, which converts them for the
 * client as it would a hardware decoder's: 4:2:0 video range at the stream's
 * depth, IOSurface, `width` by `height` when given. NULL when the format is
 * left to the software decoder. */
static CFDictionaryRef copyPictureAttributes(OSType pixelFormat, int32_t width, int32_t height)
{
	if (!pixelFormat) {
		return NULL;
	}
	int32_t format = (int32_t)pixelFormat;
	CFNumberRef number = CFNumberCreate(NULL, kCFNumberSInt32Type, &format);
	CFNumberRef wide = CFNumberCreate(NULL, kCFNumberSInt32Type, &width);
	CFNumberRef high = CFNumberCreate(NULL, kCFNumberSInt32Type, &height);
	CFDictionaryRef surface = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	const void *keys[] = {kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey,
	    kCVPixelBufferWidthKey, kCVPixelBufferHeightKey};
	const void *values[] = {number, surface, wide, high};
	CFIndex count = width > 0 && height > 0 ? 4 : 2;
	CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, count, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	CFRelease(number);
	CFRelease(wide);
	CFRelease(high);
	CFRelease(surface);
	return attributes;
}

/* Starts Apple's decoder for the rest of the session. */
static OSStatus startApple(Decoder *d)
{
	endInner(d);
	CFDictionaryRef attributes = copyPictureAttributes(d->pixelFormat, 0, 0);
	OSStatus status = attributes ? vd.setPixelBufferAttributes(d->session, attributes) : noErr;
	if (status == noErr) {
		VTDecompressionOutputCallbackRecord callback = {innerOutput, d};
		creatingInner = true;
		status = VTDecompressionSessionCreate(NULL, d->format, NULL, attributes, &callback, &d->inner);
		creatingInner = false;
	}
	if (attributes) {
		CFRelease(attributes);
	}
	d->backend = status == noErr ? BackendApple : BackendNone;
	return status;
}

/* Decodes `sample` with Apple's decoder, which emits it before returning. */
static void decodeApple(Decoder *d, VTVideoDecoderFrame frame, CMSampleBufferRef sample)
{
	VTDecodeInfoFlags innerInfo = 0;
	/* Synchronous: the inner callback emits this frame before it returns. */
	OSStatus status = VTDecompressionSessionDecodeFrame(d->inner, sample, 0, frame, &innerInfo);
	if (status != noErr && d->lastEmitted != frame) {
		d->errors++;
		vd.emit(d->session, frame, status, 0, NULL);
	}
}

/* ---- The host's NVDEC --------------------------------------------------- */

static void addToken(Decoder *d, uint64_t number, VTVideoDecoderFrame frame)
{
	if (d->tokenCount == TOKENS) {
		/* A picture the stream never shows: the oldest is given up. */
		d->dropped++;
		vd.emit(d->session, d->tokens[0].frame, noErr, kVTDecodeInfo_FrameDropped, NULL);
		memmove(&d->tokens[0], &d->tokens[1], (TOKENS - 1) * sizeof(Token));
		d->tokenCount--;
	}
	d->tokens[d->tokenCount++] = (Token){.number = number, .frame = frame};
}

/* The frame token of sample `number`, which leaves the table; NULL if none. */
static VTVideoDecoderFrame takeToken(Decoder *d, uint64_t number)
{
	for (unsigned i = 0; i < d->tokenCount; i++) {
		if (d->tokens[i].number == number) {
			VTVideoDecoderFrame frame = d->tokens[i].frame;
			memmove(&d->tokens[i], &d->tokens[i + 1], (d->tokenCount - i - 1) * sizeof(Token));
			d->tokenCount--;
			return frame;
		}
	}
	return NULL;
}

/* Emits every frame the host still holds with `status`: they will never
 * come. */
static void failTokens(Decoder *d, OSStatus status)
{
	for (unsigned i = 0; i < d->tokenCount; i++) {
		d->errors++;
		vd.emit(d->session, d->tokens[i].frame, status, 0, NULL);
	}
	d->tokenCount = 0;
}

static void releaseSpares(Decoder *d)
{
	for (unsigned i = 0; i < d->spareCount; i++) {
		CVPixelBufferRelease(d->spares[i]);
	}
	d->spareCount = 0;
}

/* A pixel buffer of the session's: a spare, else from its pool, else made
 * by VideoToolbox; NULL when none can be had. */
static CVPixelBufferRef takeBuffer(Decoder *d)
{
	if (d->spareCount) {
		return d->spares[--d->spareCount];
	}
	CVPixelBufferRef buffer = NULL;
	CVPixelBufferPoolRef pool = vd.pixelBufferPool ? vd.pixelBufferPool(d->session) : NULL;
	if (pool && CVPixelBufferPoolCreatePixelBuffer(NULL, pool, &buffer) == kCVReturnSuccess) {
		return buffer;
	}
	if (vd.createPixelBuffer && vd.createPixelBuffer(d->session, &buffer) == noErr) {
		return buffer;
	}
	return NULL;
}

static void giveBuffer(Decoder *d, CVPixelBufferRef buffer)
{
	if (d->spareCount < SPARES) {
		d->spares[d->spareCount++] = buffer;
	} else {
		CVPixelBufferRelease(buffer);
	}
}

/* Locks `buffer` and describes its planes as a destination; false when it
 * cannot hold the stream's pictures (it is left unlocked). */
static bool describe(Decoder *d, CVPixelBufferRef buffer, StelaDecoderDestination *destination)
{
	if (CVPixelBufferGetPixelFormatType(buffer) != d->pixelFormat || CVPixelBufferGetPlaneCount(buffer) != 2 ||
	    CVPixelBufferGetWidth(buffer) < d->stream.display_width ||
	    CVPixelBufferGetHeight(buffer) < d->stream.display_height) {
		return false;
	}
	if (CVPixelBufferLockBaseAddress(buffer, 0) != kCVReturnSuccess) {
		return false;
	}
	*destination = (StelaDecoderDestination){
		.luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0),
		.luma_bytes = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0) * CVPixelBufferGetHeightOfPlane(buffer, 0),
		.chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1),
		.chroma_bytes = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1) * CVPixelBufferGetHeightOfPlane(buffer, 1),
		.luma_pitch = (uint32_t)CVPixelBufferGetBytesPerRowOfPlane(buffer, 0),
		.chroma_pitch = (uint32_t)CVPixelBufferGetBytesPerRowOfPlane(buffer, 1),
	};
	if (!destination->luma || !destination->chroma) {
		CVPixelBufferUnlockBaseAddress(buffer, 0);
		return false;
	}
	return true;
}

/* The color of the stream, as Apple's decoders tag their pictures. */
static void tagColor(Decoder *d, CVPixelBufferRef buffer)
{
	const CFStringRef keys[] = {kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunctionKey,
	    kCVImageBufferYCbCrMatrixKey, kCVImageBufferChromaLocationTopFieldKey};
	for (size_t i = 0; i < sizeof(keys) / sizeof(keys[0]); i++) {
		CFTypeRef value = CMFormatDescriptionGetExtension(d->format, keys[i]);
		if (value) {
			CVBufferSetAttachment(buffer, keys[i], value, kCVAttachmentMode_ShouldPropagate);
		}
	}
}

/*
 * One sample (`bytes`, possibly none) through the host, frame `frame` (NULL
 * for a flush) with `flags`: the pictures it shows are emitted with their
 * frames' tokens, as many calls as the destinations need. noErr, or the
 * host's refusal as a StelaDecoderStatus in `*host` with the reason in
 * d->why.
 */
static OSStatus decodeHost(Decoder *d, VTVideoDecoderFrame frame, const void *bytes, size_t length, uint32_t flags,
    StelaDecoderStatus *refused, bool *decoded)
{
	const StelaDecoderAPI *api = host();
	uint64_t number = ++d->nextNumber;
	if (frame) {
		addToken(d, number, frame);
	}
	*decoded = false;
	for (unsigned calls = 0; calls < 64; calls++) {
		CVPixelBufferRef buffers[DESTINATIONS] = {0};
		StelaDecoderDestination destinations[DESTINATIONS];
		uint32_t count = 0;
		while (count < DESTINATIONS) {
			CVPixelBufferRef buffer = takeBuffer(d);
			if (!buffer) {
				break;
			}
			if (!describe(d, buffer, &destinations[count])) {
				CVPixelBufferRelease(buffer);
				break;
			}
			buffers[count++] = buffer;
		}
		if (count == 0) {
			snprintf(d->why, sizeof(d->why), "the session gave no pixel buffer for the pictures");
			*refused = StelaDecoderNoMemory;
			return kVTAllocationFailedErr;
		}
		StelaDecoderSample sample = {
			.frame = number,
			.sample = bytes,
			.sample_bytes = length,
			.flags = flags,
			.destinations = destinations,
			.count = count,
		};
		StelaDecoderResult result = {0};
		StelaDecoderStatus status = api->decode(d->host, &sample, &result, d->why, sizeof(d->why));
		for (uint32_t i = 0; i < count; i++) {
			CVPixelBufferUnlockBaseAddress(buffers[i], 0);
		}
		if (status != StelaDecoderOk) {
			for (uint32_t i = 0; i < count; i++) {
				giveBuffer(d, buffers[i]);
			}
			*refused = status;
			return status == StelaDecoderDropped ? kVTVideoDecoderBadDataErr : kVTVideoDecoderMalfunctionErr;
		}
		d->hostCalls += result.calls;
		if (result.taken && result.decoded) {
			*decoded = true;
			d->hostPictures++;
			d->gpuUs += result.gpu_us;
		}
		d->ceUs += result.ce_us;
		d->copyUs += result.copy_us;
		for (uint32_t i = 0; i < count; i++) {
			if (i >= result.outputs || !(destinations[i].flags & StelaDecodedWritten)) {
				giveBuffer(d, buffers[i]);
				continue;
			}
			VTVideoDecoderFrame shown = takeToken(d, destinations[i].frame);
			if (!shown) {
				/* A picture of a sample whose frame was already given up. */
				giveBuffer(d, buffers[i]);
				continue;
			}
			tagColor(d, buffers[i]);
			d->shown++;
			if (destinations[i].flags & StelaDecodedErrors) {
				d->errors++;
			}
			vd.emit(d->session, shown, noErr, 0, buffers[i]);
			CVPixelBufferRelease(buffers[i]);
		}
		/* More pictures wait: the same sample (not yet taken), or the rest
		 * of a flush, with fresh destinations. */
		if (!result.taken || ((flags & StelaDecodeFlush) && result.queued > 0)) {
			continue;
		}
		break;
	}
	/* A sample with no picture (parameter sets, SEI) shows nothing. */
	if (frame && !*decoded && takeToken(d, number) == frame) {
		d->dropped++;
		vd.emit(d->session, frame, noErr, kVTDecodeInfo_FrameDropped, NULL);
	}
	return noErr;
}

/* Gives up the host session: what it held is emitted as errors, and the
 * next sync sample starts Apple's decoder. */
static void dropHost(Decoder *d, const char *why)
{
	os_log_error(logger(), "StelaVD: %{public}s session in %{public}s[%d] leaves NVDEC: %{public}s (%{public}s); Apple's "
	    "software decoder takes over at the next sync sample",
	    codecName(d->codec), getprogname(), getpid(), why, d->why);
	failTokens(d, kVTVideoDecoderMalfunctionErr);
	if (d->host) {
		host()->close(d->host);
		d->host = NULL;
	}
	releaseSpares(d);
	d->backend = BackendNone;
}

/* Opens the host session for the first frame, in display order when
 * VideoToolbox processes temporally and in decode order when not. */
static bool openHost(Decoder *d)
{
	const StelaDecoderAPI *api = host();
	StelaDecoderStatus status = api->open(d->codec == kCMVideoCodecType_HEVC ? StelaDecoderHEVC : StelaDecoderH264,
	    CFDataGetBytePtr(d->record), (size_t)CFDataGetLength(d->record), &d->stream, &d->host, d->why,
	    sizeof(d->why));
	if (status != StelaDecoderOk) {
		d->host = NULL;
		os_log(logger(), "StelaVD: %{public}s session in %{public}s[%d]: the host decodes it not (%d: %{public}s); "
		    "Apple's software decoder",
		    codecName(d->codec), getprogname(), getpid(), (int)status, d->why);
		return false;
	}
	OSType wanted = d->stream.format == StelaDecoderP010 ? kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
	                                                     : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
	if (wanted != d->pixelFormat) {
		snprintf(d->why, sizeof(d->why), "the host shows %u-bit pictures, the format description says otherwise",
		    d->stream.bit_depth);
		api->close(d->host);
		d->host = NULL;
		os_log(logger(), "StelaVD: %{public}s session in %{public}s[%d]: %{public}s; Apple's software decoder",
		    codecName(d->codec), getprogname(), getpid(), d->why);
		return false;
	}
	/* The pool's buffers are the pictures' size. */
	CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(d->format);
	if ((uint32_t)size.width != d->stream.display_width || (uint32_t)size.height != d->stream.display_height) {
		CFDictionaryRef attributes = copyPictureAttributes(d->pixelFormat, (int32_t)d->stream.display_width,
		    (int32_t)d->stream.display_height);
		vd.setPixelBufferAttributes(d->session, attributes);
		CFRelease(attributes);
	}
	os_log(logger(), "StelaVD: %{public}s session in %{public}s[%d] on NVDEC: coded %ux%u, shown %ux%u, %u-bit "
	    "%{public}s, %u slots on %u surfaces, %{public}s order",
	    codecName(d->codec), getprogname(), getpid(), d->stream.coded_width, d->stream.coded_height,
	    d->stream.display_width, d->stream.display_height, d->stream.bit_depth,
	    d->stream.format == StelaDecoderP010 ? "x420" : "420v", d->stream.slots, d->stream.surfaces,
	    d->decodeOrder ? "decode" : "display");
	return true;
}

/* Whether `sample` is a sync sample (an IDR or IRAP). */
static bool isSync(CMSampleBufferRef sample)
{
	CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
	if (!attachments || CFArrayGetCount(attachments) == 0) {
		return true;
	}
	CFDictionaryRef first = CFArrayGetValueAtIndex(attachments, 0);
	CFBooleanRef notSync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
	return !(notSync && CFBooleanGetValue(notSync));
}

/* The sample's bytes, contiguous: `*copy` holds them when the block buffer
 * did not (free it); NULL on failure. */
static const void *sampleBytes(CMSampleBufferRef sample, size_t *length, void **copy)
{
	*copy = NULL;
	CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
	if (!block) {
		return NULL;
	}
	size_t total = CMBlockBufferGetDataLength(block);
	size_t atOffset = 0;
	char *data = NULL;
	*length = total;
	if (CMBlockBufferGetDataPointer(block, 0, &atOffset, NULL, &data) == kCMBlockBufferNoErr && atOffset >= total) {
		return data;
	}
	*copy = malloc(total ? total : 1);
	if (!*copy || CMBlockBufferCopyDataBytes(block, 0, total, *copy) != kCMBlockBufferNoErr) {
		free(*copy);
		*copy = NULL;
		return NULL;
	}
	return *copy;
}

/* ---- The decoder class -------------------------------------------------- */

static void logEnd(Decoder *d)
{
	if (!d->format) {
		return;
	}
	CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(d->format);
	uint64_t pictures = d->hostPictures ? d->hostPictures : 1;
	uint64_t shown = d->shown ? d->shown : 1;
	os_log(logger(), "StelaVD: %{public}s %dx%d session in %{public}s ended: %{public}s, %llu frames, decode %.2f ms "
	    "average, %.2f ms longest, %llu errors; NVDEC %llu pictures, %llu shown, %llu dropped, gpu %llu us, "
	    "readback %llu us, copy %llu us average, %llu gate calls",
	    codecName(d->codec), size.width, size.height, getprogname(), backendName(d->backend), d->frames,
	    d->frames ? d->decodeNs / 1e6 / d->frames : 0.0, d->maxNs / 1e6, d->errors, d->hostPictures, d->shown,
	    d->dropped, d->gpuUs / pictures, d->ceUs / shown, d->copyUs / shown, d->hostCalls);
}

static void finalize(VDBaseObjectRef object)
{
	Decoder *d = vd.derivedStorage(object);
	logEnd(d);
	endInner(d);
	if (d->host) {
		host()->close(d->host);
		d->host = NULL;
	}
	releaseSpares(d);
	if (d->record) {
		CFRelease(d->record);
		d->record = NULL;
	}
	if (d->format) {
		CFRelease(d->format);
		d->format = NULL;
	}
}

static OSStatus invalidate(VDBaseObjectRef object)
{
	Decoder *d = vd.derivedStorage(object);
	endInner(d);
	if (d->host) {
		/* Frames the host still held are never shown. */
		failTokens(d, kVTInvalidSessionErr);
		host()->close(d->host);
		d->host = NULL;
	}
	releaseSpares(d);
	return noErr;
}

static CFStringRef copyDebugDescription(VDBaseObjectRef object)
{
	Decoder *d = vd.derivedStorage(object);
	return CFStringCreateWithFormat(NULL, NULL, CFSTR("<StelaVD %s on %s, %llu frames>"), codecName(d->codec),
	    backendName(d->backend), d->frames);
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

/* The stream's configuration record (avcC or hvcC), retained; NULL when the
 * format description carries none (an Annex B stream). */
static CFDataRef copyRecord(CMVideoFormatDescriptionRef format, CMVideoCodecType codec)
{
	CFDictionaryRef atoms = CMFormatDescriptionGetExtension(format,
	    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms);
	if (!atoms || CFGetTypeID(atoms) != CFDictionaryGetTypeID()) {
		return NULL;
	}
	CFDataRef record = CFDictionaryGetValue(atoms, codec == kCMVideoCodecType_HEVC ? CFSTR("hvcC") : CFSTR("avcC"));
	if (!record || CFGetTypeID(record) != CFDataGetTypeID() || CFDataGetLength(record) < 7) {
		return NULL;
	}
	return CFRetain(record);
}

static OSStatus startSession(VTVideoDecoderRef decoder, VTVideoDecoderSession session,
    CMVideoFormatDescriptionRef format)
{
	Decoder *d = storage(decoder);
	/* A session started again lets its host session go. */
	if (d->host) {
		failTokens(d, kVTInvalidSessionErr);
		host()->close(d->host);
		d->host = NULL;
	}
	releaseSpares(d);
	d->session = session;
	if (d->format) {
		CFRelease(d->format);
	}
	d->format = (CMVideoFormatDescriptionRef)CFRetain(format);
	endInner(d);
	if (d->record) {
		CFRelease(d->record);
	}
	d->record = copyRecord(format, d->codec);
	d->pixelFormat = pictureFormat(format);
	CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(format);
	char name[5] = {(char)(d->pixelFormat >> 24), (char)(d->pixelFormat >> 16), (char)(d->pixelFormat >> 8),
	    (char)d->pixelFormat, 0};
	/* The host's NVDEC takes the stream at the first frame, when it has a
	 * decoder, the stream a record and its pictures a format StelaVD makes. */
	if (host() && d->record && d->pixelFormat && (vd.pixelBufferPool || vd.createPixelBuffer)) {
		CFDictionaryRef attributes = copyPictureAttributes(d->pixelFormat, 0, 0);
		OSStatus status = vd.setPixelBufferAttributes(session, attributes);
		CFRelease(attributes);
		if (status == noErr) {
			d->backend = BackendHost;
			os_log(logger(), "StelaVD: %{public}s %dx%d session in %{public}s[%d], pictures %{public}s, backend "
			    "NVDEC (the host opens it at the first frame)",
			    codecName(d->codec), size.width, size.height, getprogname(), getpid(), name);
			return noErr;
		}
	}
	OSStatus status = startApple(d);
	if (status == noErr) {
		os_log(logger(), "StelaVD: %{public}s %dx%d session in %{public}s[%d], pictures %{public}s, backend Apple "
		    "software (%{public}s)",
		    codecName(d->codec), size.width, size.height, getprogname(), getpid(), d->pixelFormat ? name : "native",
		    !host() ? "no host decoder in this process" : !d->record ? "no configuration record" : "its format");
	} else {
		os_log_error(logger(), "StelaVD: %{public}s %dx%d session in %{public}s refused: %d",
		    codecName(d->codec), size.width, size.height, getprogname(), (int)status);
	}
	return status;
}

static OSStatus decodeFrame(VTVideoDecoderRef decoder, VTVideoDecoderFrame frame, CMSampleBufferRef sample,
    VTDecodeFrameFlags flags, VTDecodeInfoFlags *info)
{
	(void)info;
	Decoder *d = storage(decoder);
	uint64_t start = nowNs();
	d->frames++;
	bool sync = isSync(sample);
	/* The host failed: errors until a sync sample starts Apple's decoder. */
	if (d->backend == BackendNone) {
		if (!sync || startApple(d) != noErr) {
			d->errors++;
			vd.emit(d->session, frame, kVTVideoDecoderBadDataErr, 0, NULL);
			return noErr;
		}
		os_log(logger(), "StelaVD: %{public}s session in %{public}s[%d] goes on with Apple's software decoder",
		    codecName(d->codec), getprogname(), getpid());
	}
	if (d->backend == BackendHost && !d->host) {
		d->decodeOrder = !(flags & kVTDecodeFrame_EnableTemporalProcessing);
		if (!openHost(d)) {
			if (startApple(d) != noErr) {
				vd.emit(d->session, frame, kVTInvalidSessionErr, 0, NULL);
				return noErr;
			}
		}
	}
	if (d->backend == BackendApple) {
		if (d->inner) {
			decodeApple(d, frame, sample);
		} else {
			vd.emit(d->session, frame, kVTInvalidSessionErr, 0, NULL);
		}
	} else {
		void *copy = NULL;
		size_t length = 0;
		const void *bytes = sampleBytes(sample, &length, &copy);
		if (!bytes) {
			d->errors++;
			vd.emit(d->session, frame, kVTVideoDecoderBadDataErr, 0, NULL);
		} else {
			StelaDecoderStatus refused = StelaDecoderOk;
			bool decoded = false;
			OSStatus status = decodeHost(d, frame, bytes, length, 0, &refused, &decoded);
			if (status != noErr) {
				/* This frame never shows. */
				if (takeToken(d, d->nextNumber) == frame) {
					d->errors++;
					vd.emit(d->session, frame, status, 0, NULL);
				}
				if (refused == StelaDecoderDropped && ++d->drops < MOST_DROPS) {
					os_log_error(logger(), "StelaVD: %{public}s session in %{public}s[%d]: NVDEC refused a "
					    "sample: %{public}s", codecName(d->codec), getprogname(), getpid(), d->why);
				} else {
					dropHost(d, refused == StelaDecoderLost ? "the host lost the session"
					    : refused == StelaDecoderDropped     ? "three samples refused in a row"
					                                         : "the host refused a sample");
				}
			} else {
				d->drops = 0;
			}
		}
		free(copy);
	}
	uint64_t ns = nowNs() - start;
	d->decodeNs += ns;
	if (ns > d->maxNs) {
		d->maxNs = ns;
	}
	return noErr;
}

/* The end of the stream: every picture still held is emitted. */
static OSStatus finishDelayedFrames(VTVideoDecoderRef decoder)
{
	Decoder *d = storage(decoder);
	if (d->inner) {
		VTDecompressionSessionFinishDelayedFrames(d->inner);
		VTDecompressionSessionWaitForAsynchronousFrames(d->inner);
		return noErr;
	}
	if (!d->host) {
		return noErr;
	}
	StelaDecoderStatus refused = StelaDecoderOk;
	bool decoded = false;
	if (decodeHost(d, NULL, NULL, 0, StelaDecodeFlush, &refused, &decoded) != noErr) {
		dropHost(d, "the host refused the end of the stream");
		return noErr;
	}
	/* Pictures the stream never shows. */
	failTokens(d, kVTVideoDecoderBadDataErr);
	return noErr;
}

static OSStatus copySupportedPropertyDictionary(VTVideoDecoderRef decoder, CFDictionaryRef *out)
{
	(void)decoder;
	*out = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	return noErr;
}

/* Apple's decoder takes any description of its codec. The host's session
 * holds the parameter sets of its record: only the same record goes on,
 * anything else makes VideoToolbox start a new session. */
static Boolean canAcceptFormatDescription(VTVideoDecoderRef decoder, CMFormatDescriptionRef format)
{
	Decoder *d = storage(decoder);
	if (CMFormatDescriptionGetMediaSubType(format) != d->codec) {
		return false;
	}
	if (d->backend != BackendHost) {
		return true;
	}
	CFDataRef record = copyRecord((CMVideoFormatDescriptionRef)format, d->codec);
	Boolean same = record && d->record && CFEqual(record, d->record);
	if (record) {
		CFRelease(record);
	}
	return same;
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
	.finishDelayedFrames = finishDelayedFrames,
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
