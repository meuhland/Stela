/*
 * vttest: encodes moving 1080p frames through VideoToolbox, as Parsec does,
 * once for each codec asked for, and checks what ran.
 *
 *   vttest [--codec h264|hevc|both] [--frames N] [--mode hw+ll|ll|hw|none|list]
 *          (hw+ll is the default)
 *          [--input pool|bgra|420v] [--out PREFIX] [--register BUNDLE_EXECUTABLE]
 *          [--size WxH]
 *
 * Modes:
 *
 * - hw+ll (the default) is a real-time streaming client, as Parsec's
 *   sessions show themselves to the host: low-latency rate control asked
 *   for in the specification (hardware not required: VideoProcessing
 *   refuses that pair, -12902 "Cannot require hardware accelerated
 *   encoder"), then RealTime, no frame reordering, 60 fps expected, no key
 *   frame interval (an infinite GOP: key frames only when asked), a bitrate
 *   that starts at 10 Mb/s and rises 750 kb/s every 30 frames, and frames
 *   submitted at 60 fps. When VideoToolbox refuses the low-latency session,
 *   vttest prints the status and VideoToolbox's own log lines from this
 *   process and VTEncoderXPCService, tries the low-latency session made
 *   other ways (each opened and closed at once: the source pixel buffer
 *   attributes a capture gives, EnableHardwareAcceleratedVideoEncoder, the
 *   hardware encoder's ID pinned, 1280x720, no output callback) and keeps
 *   the first that opens and that it can encode with. If none opens, it goes
 *   on as a client does: the same session without low-latency rate control,
 *   and the result line says so. PASS needs the encoder that ran to be
 *   hardware.
 * - ll: the low-latency key alone in the specification, and no property
 *   set: Apple's minimal low-latency session. Any encoder passes; a
 *   refused session fails, with the log lines.
 * - hw: a hardware encoder required, no low-latency rate control, the
 *   bitrate, frame rate, key frame interval and RealTime set. PASS needs
 *   hardware.
 * - none: an empty specification, the same properties as hw; any encoder
 *   passes.
 *
 * A refusal at VTCompressionSessionCreate comes before any property is
 * set: it is the specification, the size or the process, never a property.
 *
 * Inputs: `pool` (the default) draws frames from the session's pixel buffer
 * pool, in the format the encoder asked for first (BGRA where StelaVT
 * encodes on NVENC); `bgra` and `420v` make buffers of that format
 * (Parsec's capture is BGRA), which VideoToolbox converts if the encoder
 * does not take it.
 *
 * For each codec it reports the encoder VideoToolbox chose, whether it is
 * hardware (the session's UsingHardwareAcceleratedVideoEncoder, or the
 * encoder list's entry for the session's encoder ID) and whether it is
 * StelaVT's, the frames in and out, the key frames (the first and one asked
 * for halfway), the bitrate, the frames a second and the process's CPU a
 * frame, each frame's encode latency (submission to its output: average,
 * 95th percentile and longest), and writes the stream as Annex B to
 * PREFIX.h264 or PREFIX.hevc. It then decodes the stream with
 * VideoToolbox's decoder and compares each picture's luma with the frame it
 * was made from. A codec passes when its session opens (on a hardware
 * encoder for hw and hw+ll), every frame comes out with both key frames,
 * and every one decodes at 30 dB or more; vttest exits 1 when one does not.
 *
 * With --register it first registers the StelaVT executable given for both
 * codecs in this process, after loading Metal's devices so StelaGPU.bundle
 * is loaded too: the plugin is tested without installing it, and StelaVT
 * finds the bundle's host encoder by class. "--mode list" lists the
 * registered H.264 and HEVC encoders.
 */
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <dlfcn.h>
#include <math.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

typedef OSStatus (*Factory)(FourCharCode, CFAllocatorRef, void **);
typedef OSStatus (*Register)(CMVideoCodecType, CFDictionaryRef, Factory);

enum { kMostFrames = 4096 };

static struct {
	CMVideoCodecType codec;
	/* The input frames' pixel format, which sets the luma they decode to. */
	OSType format;
	/* When each frame was submitted, and its encode latency, in seconds. */
	double submitted[kMostFrames];
	double latency[kMostFrames];
	int latencies;
	FILE *out;
	int emitted, keyframes, errors, dropped;
	size_t bytes;
	/* Each emitted sample, kept for the decode, in order. */
	CMSampleBufferRef samples[kMostFrames];
	/* The decode: pictures, failures, and the luma PSNR's lowest and sum. */
	int decoded, undecodable;
	double lowest, sum;
	int width, height;
} run;

static void writeStart(const uint8_t *nal, size_t length)
{
	static const uint8_t start[4] = {0, 0, 0, 1};
	if (run.out) {
		fwrite(start, 1, 4, run.out);
		fwrite(nal, 1, length, run.out);
	}
}

static void writeParameterSets(CMFormatDescriptionRef format)
{
	size_t count = 0;
	bool hevc = run.codec == kCMVideoCodecType_HEVC;
	if (hevc) {
		CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, 0, NULL, NULL, &count, NULL);
	} else {
		CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, NULL, NULL, &count, NULL);
	}
	for (size_t i = 0; i < count; i++) {
		const uint8_t *set = NULL;
		size_t length = 0;
		if (hevc) {
			CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, i, &set, &length, NULL, NULL);
		} else {
			CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &set, &length, NULL, NULL);
		}
		writeStart(set, length);
	}
}

static double now(void);

static void encoded(void *context, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample)
{
	/* The source refcon is the frame's number plus one. */
	intptr_t number = (intptr_t)source - 1;
	if (number >= 0 && number < kMostFrames && run.submitted[number] > 0 && run.latencies < kMostFrames) {
		run.latency[run.latencies++] = now() - run.submitted[number];
	}
	if (flags & kVTEncodeInfo_FrameDropped) {
		run.dropped++;
		return;
	}
	if (status != noErr || !sample) {
		run.errors++;
		return;
	}
	CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
	bool key = true;
	if (attachments && CFArrayGetCount(attachments) > 0) {
		CFDictionaryRef first = CFArrayGetValueAtIndex(attachments, 0);
		CFBooleanRef notSync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
		key = !(notSync && CFBooleanGetValue(notSync));
	}
	if (key) {
		run.keyframes++;
		writeParameterSets(CMSampleBufferGetFormatDescription(sample));
	}
	CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
	size_t total = CMBlockBufferGetDataLength(block);
	uint8_t *data = malloc(total);
	CMBlockBufferCopyDataBytes(block, 0, total, data);
	for (size_t at = 0; at + 4 <= total;) {
		uint32_t length = (uint32_t)data[at] << 24 | (uint32_t)data[at + 1] << 16 | (uint32_t)data[at + 2] << 8 | data[at + 3];
		if (at + 4 + length > total) {
			run.errors++;
			break;
		}
		writeStart(data + at + 4, length);
		at += 4 + length;
	}
	free(data);
	run.bytes += total;
	if (run.emitted < kMostFrames) {
		run.samples[run.emitted] = (CMSampleBufferRef)CFRetain(sample);
	}
	run.emitted++;
	(void)context;
	(void)source;
}

static double cpuSeconds(void)
{
	struct rusage usage;
	getrusage(RUSAGE_SELF, &usage);
	return usage.ru_utime.tv_sec + usage.ru_utime.tv_usec / 1e6 + usage.ru_stime.tv_sec + usage.ru_stime.tv_usec / 1e6;
}

static double now(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

/* A desktop-like scene: flat bands, text-like stripes, one moving box. */
static uint8_t sceneAt(int frame, size_t w, size_t r, size_t c)
{
	uint8_t v = (uint8_t)(40 + (r / 120) * 20);
	if ((r % 24) < 2 && (c % 300) < 220) {
		v = 230;
	}
	if (c >= (size_t)(frame * 6 % (int)w) && c < (size_t)(frame * 6 % (int)w) + 200 && r >= 400 && r < 600) {
		v = (uint8_t)(120 + frame % 100);
	}
	return v;
}

/* The luma a frame decodes to: the scene's value as NV12, or what grey
 * BGRA of that value becomes in BT.709 limited range (the host's and
 * StelaVT's conversion, and VideoToolbox's). */
static uint8_t lumaAt(int frame, size_t w, size_t r, size_t c)
{
	uint8_t v = sceneAt(frame, w, r, c);
	return run.format == kCVPixelFormatType_32BGRA ? (uint8_t)(16 + ((220 * v + 128) >> 8)) : v;
}

static void fill(CVPixelBufferRef buffer, int frame)
{
	CVPixelBufferLockBaseAddress(buffer, 0);
	size_t w = CVPixelBufferGetWidth(buffer), h = CVPixelBufferGetHeight(buffer);
	if (CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_32BGRA) {
		/* Grey: the decoded luma follows, the chroma stays neutral. */
		uint8_t *pixels = CVPixelBufferGetBaseAddress(buffer);
		size_t pitch = CVPixelBufferGetBytesPerRow(buffer);
		for (size_t r = 0; r < h; r++) {
			for (size_t c = 0; c < w; c++) {
				uint8_t v = sceneAt(frame, w, r, c);
				uint8_t *p = pixels + r * pitch + 4 * c;
				p[0] = p[1] = p[2] = v;
				p[3] = 255;
			}
		}
		CVPixelBufferUnlockBaseAddress(buffer, 0);
		return;
	}
	uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
	size_t ys = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
	uint8_t *uv = CVPixelBufferGetBaseAddressOfPlane(buffer, 1);
	size_t uvs = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
	for (size_t r = 0; r < h; r++) {
		for (size_t c = 0; c < w; c++) {
			y[r * ys + c] = sceneAt(frame, w, r, c);
		}
	}
	for (size_t r = 0; r < h / 2; r++) {
		for (size_t c = 0; c < w / 2; c++) {
			uv[r * uvs + 2 * c] = (uint8_t)(128 + (c / 60) % 3 * 10);
			uv[r * uvs + 2 * c + 1] = 128;
		}
	}
	CVPixelBufferUnlockBaseAddress(buffer, 0);
}

/* A decoded picture: its luma against the frame its pts names. */
static void decoded(void *context, void *source, OSStatus status, VTDecodeInfoFlags flags, CVImageBufferRef image,
    CMTime time, CMTime duration)
{
	if (status != noErr || !image) {
		run.undecodable++;
		return;
	}
	int frame = (int)time.value;
	CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
	size_t w = CVPixelBufferGetWidth(image), h = CVPixelBufferGetHeight(image);
	const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(image, 0);
	size_t ys = CVPixelBufferGetBytesPerRowOfPlane(image, 0);
	double squared = 0;
	for (size_t r = 0; r < h; r++) {
		for (size_t c = 0; c < w; c++) {
			double d = (double)y[r * ys + c] - lumaAt(frame, (size_t)run.width, r, c);
			squared += d * d;
		}
	}
	CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
	double mse = squared / (double)(w * h);
	double psnr = mse > 0 ? 10 * log10(255.0 * 255.0 / mse) : 99;
	run.lowest = run.decoded == 0 || psnr < run.lowest ? psnr : run.lowest;
	run.sum += psnr;
	run.decoded++;
	(void)context;
	(void)source;
	(void)flags;
	(void)duration;
}

/* Decodes the kept samples, a decoder per format description. */
static void decodeAll(void)
{
	VTDecompressionSessionRef session = NULL;
	CMFormatDescriptionRef format = NULL;
	int kept = run.emitted < kMostFrames ? run.emitted : kMostFrames;
	int formats[] = {kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange};
	CFNumberRef number = CFNumberCreate(NULL, kCFNumberIntType, &formats[0]);
	const void *keys[] = {kCVPixelBufferPixelFormatTypeKey};
	const void *values[] = {number};
	CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 1, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	VTDecompressionOutputCallbackRecord callback = {decoded, NULL};
	for (int i = 0; i < kept; i++) {
		CMSampleBufferRef sample = run.samples[i];
		CMFormatDescriptionRef sampleFormat = CMSampleBufferGetFormatDescription(sample);
		if (!session || !CFEqual(sampleFormat, format)) {
			if (session) {
				VTDecompressionSessionWaitForAsynchronousFrames(session);
				VTDecompressionSessionInvalidate(session);
				CFRelease(session);
				session = NULL;
			}
			format = sampleFormat;
			OSStatus status = VTDecompressionSessionCreate(NULL, format, NULL, attributes, &callback, &session);
			if (status != noErr) {
				printf("  decoder: %d\n", (int)status);
				run.undecodable += kept - i;
				break;
			}
		}
		if (VTDecompressionSessionDecodeFrame(session, sample, 0, NULL, NULL) != noErr) {
			run.undecodable++;
		}
	}
	if (session) {
		VTDecompressionSessionWaitForAsynchronousFrames(session);
		VTDecompressionSessionInvalidate(session);
		CFRelease(session);
	}
	for (int i = 0; i < kept; i++) {
		CFRelease(run.samples[i]);
	}
	CFRelease(attributes);
	CFRelease(number);
}

/* The encoder list's entry for `encoderID`: whether it is hardware and
 * whether it is StelaVT's (its ID, or its name, says Stela). False when the
 * list has no such entry. */
static bool listed(CFStringRef encoderID, bool *hardware, bool *stela)
{
	CFArrayRef encoders = NULL;
	bool found = false;
	VTCopyVideoEncoderList(NULL, &encoders);
	for (CFIndex i = 0; encoderID && encoders && i < CFArrayGetCount(encoders) && !found; i++) {
		CFDictionaryRef item = CFArrayGetValueAtIndex(encoders, i);
		CFStringRef id = CFDictionaryGetValue(item, kVTVideoEncoderList_EncoderID);
		if (!id || !CFEqual(id, encoderID)) {
			continue;
		}
		found = true;
		*hardware = CFDictionaryGetValue(item, kVTVideoEncoderList_IsHardwareAccelerated) == kCFBooleanTrue;
		CFStringRef name = CFDictionaryGetValue(item, kVTVideoEncoderList_EncoderName);
		CFStringRef display = CFDictionaryGetValue(item, kVTVideoEncoderList_DisplayName);
		*stela = CFStringHasPrefix(id, CFSTR("org.stela."))
		    || (name && CFStringFind(name, CFSTR("Stela"), 0).location != kCFNotFound)
		    || (display && CFStringFind(display, CFSTR("Stela"), 0).location != kCFNotFound);
	}
	if (encoders) {
		CFRelease(encoders);
	}
	return found;
}

static int compareDoubles(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;
	return x < y ? -1 : x > y;
}

static void list(void)
{
	CFArrayRef encoders = NULL;
	VTCopyVideoEncoderList(NULL, &encoders);
	for (CFIndex i = 0; encoders && i < CFArrayGetCount(encoders); i++) {
		CFDictionaryRef item = CFArrayGetValueAtIndex(encoders, i);
		CFNumberRef codec = CFDictionaryGetValue(item, kVTVideoEncoderList_CodecType);
		int type = 0;
		CFNumberGetValue(codec, kCFNumberIntType, &type);
		if (type != kCMVideoCodecType_H264 && type != kCMVideoCodecType_HEVC) {
			continue;
		}
		char id[256] = "", name[256] = "";
		CFStringGetCString(CFDictionaryGetValue(item, kVTVideoEncoderList_EncoderID), id, sizeof id, kCFStringEncodingUTF8);
		CFStringRef display = CFDictionaryGetValue(item, kVTVideoEncoderList_DisplayName);
		if (display) {
			CFStringGetCString(display, name, sizeof name, kCFStringEncodingUTF8);
		}
		CFBooleanRef hardware = CFDictionaryGetValue(item, kVTVideoEncoderList_IsHardwareAccelerated);
		printf("%s encoder: id=%s name=\"%s\" hardware=%s\n", type == kCMVideoCodecType_HEVC ? "hvc1" : "avc1", id,
		    name, hardware == kCFBooleanTrue ? "yes" : "no");
	}
	if (encoders) {
		CFRelease(encoders);
	}
}

/* Registers the StelaVT executable at `path` in this process for both codecs. */
static bool registerPlugin(const char *path)
{
	/* StelaGPU.bundle loads with Metal's first device; StelaVT then finds its
	 * host encoder by class. */
	void *(*copyDevices)(void) = dlsym(RTLD_DEFAULT, "MTLCopyAllDevices");
	if (!copyDevices) {
		void *metal = dlopen("/System/Library/Frameworks/Metal.framework/Metal", RTLD_NOW);
		copyDevices = metal ? dlsym(metal, "MTLCopyAllDevices") : NULL;
	}
	if (copyDevices) {
		CFTypeRef devices = copyDevices();
		printf("Metal devices: %ld\n", devices ? (long)CFArrayGetCount(devices) : 0L);
	}
	void *plugin = dlopen(path, RTLD_NOW);
	Factory factory = plugin ? (Factory)dlsym(plugin, "StelaVT_CreateInstance") : NULL;
	Register reg = (Register)dlsym(RTLD_DEFAULT, "VTRegisterVideoEncoderWithInfo");
	if (!factory || !reg) {
		fprintf(stderr, "cannot load the plugin or the registration call: %s\n", dlerror());
		return false;
	}
	CFMutableDictionaryRef info = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	CFStringRef hardware = *(CFStringRef *)dlsym(RTLD_DEFAULT, "kVTVideoEncoder_IsHardwareAccelerated");
	CFDictionarySetValue(info, hardware, kCFBooleanTrue);
	printf("registered StelaVT in-process: avc1 %d, hvc1 %d\n", (int)reg(kCMVideoCodecType_H264, info, factory),
	    (int)reg(kCMVideoCodecType_HEVC, info, factory));
	CFRelease(info);
	return true;
}

static void setNumber(VTCompressionSessionRef session, CFStringRef key, int value, const char *name)
{
	CFNumberRef number = CFNumberCreate(NULL, kCFNumberIntType, &value);
	printf("  set %s %d: %d\n", name, value, (int)VTSessionSetProperty(session, key, number));
	CFRelease(number);
}

static void setBoolean(VTCompressionSessionRef session, CFStringRef key, bool value, const char *name)
{
	printf("  set %s %s: %d\n", name, value ? "true" : "false",
	    (int)VTSessionSetProperty(session, key, value ? kCFBooleanTrue : kCFBooleanFalse));
}

typedef enum {
	ModeHardwareLowLatency,
	ModeLowLatency,
	ModeHardware,
	ModeNone,
} Mode;

static const char *modeName(Mode mode)
{
	switch (mode) {
	case ModeHardwareLowLatency:
		return "hw+ll";
	case ModeLowLatency:
		return "ll";
	case ModeHardware:
		return "hw";
	case ModeNone:
		break;
	}
	return "none";
}

static bool parseMode(const char *text, Mode *mode)
{
	for (Mode m = ModeHardwareLowLatency; m <= ModeNone; m++) {
		if (strcmp(text, modeName(m)) == 0) {
			*mode = m;
			return true;
		}
	}
	return false;
}

/* An encoder specification: low-latency rate control and hardware asked
 * for or required, as given. */
static CFMutableDictionaryRef specification(bool lowLatency, bool requireHardware)
{
	CFMutableDictionaryRef spec = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	if (requireHardware) {
		CFDictionarySetValue(spec, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue);
	}
	if (lowLatency) {
		CFDictionarySetValue(spec, kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue);
	}
	return spec;
}

/* Source pixel buffer attributes as a screen capture gives its buffers:
 * BGRA of the session's size, IOSurface-backed. */
static CFDictionaryRef captureAttributes(int width, int height)
{
	int format = kCVPixelFormatType_32BGRA;
	CFNumberRef numbers[] = {CFNumberCreate(NULL, kCFNumberIntType, &format),
	    CFNumberCreate(NULL, kCFNumberIntType, &width), CFNumberCreate(NULL, kCFNumberIntType, &height)};
	CFDictionaryRef surface = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	const void *keys[] = {kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey,
	    kCVPixelBufferIOSurfacePropertiesKey};
	const void *values[] = {numbers[0], numbers[1], numbers[2], surface};
	CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 4, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	for (int i = 0; i < 3; i++) {
		CFRelease(numbers[i]);
	}
	CFRelease(surface);
	return attributes;
}

/* The encoder list's hardware encoder for `codec`, StelaVT's first; NULL
 * when there is none. */
static CFStringRef copyHardwareEncoderID(CMVideoCodecType codec)
{
	CFArrayRef encoders = NULL;
	CFStringRef found = NULL;
	VTCopyVideoEncoderList(NULL, &encoders);
	for (CFIndex i = 0; encoders && i < CFArrayGetCount(encoders); i++) {
		CFDictionaryRef item = CFArrayGetValueAtIndex(encoders, i);
		int type = 0;
		CFNumberRef number = CFDictionaryGetValue(item, kVTVideoEncoderList_CodecType);
		if (!number || !CFNumberGetValue(number, kCFNumberIntType, &type) || (CMVideoCodecType)type != codec
		    || CFDictionaryGetValue(item, kVTVideoEncoderList_IsHardwareAccelerated) != kCFBooleanTrue) {
			continue;
		}
		CFStringRef id = CFDictionaryGetValue(item, kVTVideoEncoderList_EncoderID);
		bool stela = false, hardware = false;
		if (!found || (listed(id, &hardware, &stela) && stela)) {
			if (found) {
				CFRelease(found);
			}
			found = CFRetain(id);
		}
	}
	if (encoders) {
		CFRelease(encoders);
	}
	return found;
}

/* VideoToolbox's lines in the log from this process and from
 * VTEncoderXPCService over the last seconds that say why a session was
 * refused. `log show` may need an admin user; its command is printed so it
 * can be run again by hand. */
static void showLog(void)
{
	char command[512];
	snprintf(command, sizeof command,
	    "/usr/bin/log show --last 20s --style compact --info --debug --predicate "
	    "'processID == %d OR process == \"VTEncoderXPCService\"' 2>&1",
	    (int)getpid());
	printf("  the log's reasons (%s):\n", command);
	FILE *log = popen(command, "r");
	if (!log) {
		printf("    log show did not start\n");
		return;
	}
	static const char *const words[] = {"12902", "atency", "VCP", "RateControl", "ardware", "rror", "efus", "ailed",
	    "nvalid", "nsupported", "Stela"};
	char line[1024];
	int shown = 0, read = 0;
	while (fgets(line, sizeof line, log)) {
		read++;
		for (size_t w = 0; w < sizeof words / sizeof words[0] && shown < 40; w++) {
			if (strstr(line, words[w])) {
				printf("    %s", line);
				shown++;
				break;
			}
		}
	}
	pclose(log);
	if (!shown) {
		printf("    none of %d lines names a reason\n", read);
	}
}

/* The low-latency session made other ways, each opened and closed at once:
 * which part of vttest's VideoToolbox refuses. The first that opens and
 * that the frames can go through (its size and output callback vttest's) is
 * kept in *kept, and its way named in `used`. */
static void tryLowLatency(CMVideoCodecType codec, int width, int height, VTCompressionSessionRef *kept, char *used,
    size_t usedBytes)
{
	static const char *const ways[] = {
	    "source attributes as a capture gives (BGRA, the size, IOSurface)",
	    "EnableHardwareAcceleratedVideoEncoder",
	    "the hardware encoder's ID pinned",
	    "1280x720",
	    "no output callback (a client of the output handler)",
	};
	for (size_t way = 0; way < sizeof ways / sizeof ways[0]; way++) {
		CFMutableDictionaryRef spec = specification(true, false);
		CFDictionaryRef attributes = NULL;
		CFStringRef id = NULL;
		int w = width, h = height;
		VTCompressionOutputCallback callback = encoded;
		switch (way) {
		case 0:
			attributes = captureAttributes(width, height);
			break;
		case 1:
			CFDictionarySetValue(spec, kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
			    kCFBooleanTrue);
			break;
		case 2:
			id = copyHardwareEncoderID(codec);
			if (id) {
				CFDictionarySetValue(spec, kVTVideoEncoderSpecification_EncoderID, id);
			}
			break;
		case 3:
			w = 1280;
			h = 720;
			break;
		default:
			callback = NULL;
			break;
		}
		if (way == 2 && !id) {
			printf("  low latency with %s: no hardware encoder listed\n", ways[way]);
			CFRelease(spec);
			continue;
		}
		VTCompressionSessionRef session = NULL;
		OSStatus status = VTCompressionSessionCreate(NULL, w, h, codec, spec, attributes, NULL, callback, NULL, &session);
		char name[256] = "";
		if (id) {
			CFStringGetCString(id, name, sizeof name, kCFStringEncodingUTF8);
		}
		printf("  low latency with %s%s%s: %d\n", ways[way], id ? " " : "", name, (int)status);
		if (status == noErr && !*kept && w == width && h == height && callback) {
			*kept = session;
			snprintf(used, usedBytes, "%s", ways[way]);
		} else if (status == noErr) {
			VTCompressionSessionInvalidate(session);
			CFRelease(session);
		}
		CFRelease(spec);
		if (attributes) {
			CFRelease(attributes);
		}
		if (id) {
			CFRelease(id);
		}
	}
}

/* Encodes `frames` frames of `codec`; whether it passed. */
static bool encode(CMVideoCodecType codec, Mode mode, const char *input, int frames, int width, int height,
    const char *prefix)
{
	bool hevc = codec == kCMVideoCodecType_HEVC;
	const char *label = hevc ? "hevc" : "h264";
	memset(&run, 0, sizeof run);
	run.codec = codec;
	run.width = width;
	run.height = height;
	if (prefix) {
		char path[1024];
		snprintf(path, sizeof path, "%s.%s", prefix, label);
		run.out = fopen(path, "wb");
	}
	bool lowLatency = mode == ModeHardwareLowLatency || mode == ModeLowLatency;
	bool wantsHardware = mode == ModeHardwareLowLatency || mode == ModeHardware;
	/* A real-time client: paced at its frame rate, its bitrate moving. */
	bool client = mode == ModeHardwareLowLatency;
	static const char *const descriptions[] = {
	    "low-latency rate control, hardware not required, a real-time client's properties, paced",
	    "low-latency rate control alone, no property set",
	    "hardware required",
	    "an empty specification",
	};
	printf("== %s %dx%d, mode %s (%s), input %s\n", label, width, height, modeName(mode), descriptions[mode], input);
	CFMutableDictionaryRef spec = specification(lowLatency, mode == ModeHardware);
	VTCompressionSessionRef session = NULL;
	OSStatus status = VTCompressionSessionCreate(NULL, width, height, codec, spec, NULL, NULL, encoded, NULL, &session);
	CFRelease(spec);
	printf("  session: %d\n", (int)status);
	/* What the result line adds: how the low-latency session opened, or
	 * that it did not. */
	char note[320] = "";
	if (status != noErr && lowLatency) {
		showLog();
	}
	if (status != noErr && client) {
		OSStatus refused = status;
		char used[160] = "";
		tryLowLatency(codec, width, height, &session, used, sizeof used);
		if (session) {
			status = noErr;
			printf("  low-latency session: opened with %s\n", used);
			snprintf(note, sizeof note, " (low latency with %s)", used);
		} else {
			spec = specification(false, false);
			status = VTCompressionSessionCreate(NULL, width, height, codec, spec, NULL, NULL, encoded, NULL, &session);
			CFRelease(spec);
			printf("  low latency refused (%d) every way: a real-time session without it, as a client goes on: %d\n",
			    (int)refused, (int)status);
			snprintf(note, sizeof note, " (low latency refused, %d: a real-time session without it)", (int)refused);
		}
	}
	if (status != noErr) {
		printf("%s FAIL: no session (%d)\n", label, (int)status);
		return false;
	}
	CFStringRef encoderID = NULL;
	VTSessionCopyProperty(session, kVTCompressionPropertyKey_EncoderID, NULL, &encoderID);
	char name[256] = "?";
	if (encoderID) {
		CFStringGetCString(encoderID, name, sizeof name, kCFStringEncodingUTF8);
	}
	CFBooleanRef hardware = NULL;
	VTSessionCopyProperty(session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hardware);
	bool listHardware = false, stela = false;
	bool inList = listed(encoderID, &listHardware, &stela);
	/* A runtime-registered encoder (anon-N) may answer no through the
	 * session though its registration says hardware: either counts. */
	bool isHardware = hardware == kCFBooleanTrue || (inList && listHardware);
	printf("  encoder: %s, hardware: %s (session property %s, encoder list %s), StelaVT: %s\n", name,
	    isHardware ? "yes" : "no", hardware == kCFBooleanTrue ? "yes" : hardware ? "no" : "unanswered",
	    inList ? (listHardware ? "yes" : "no") : "not listed", stela ? "yes" : "no");
	if (encoderID) {
		CFRelease(encoderID);
	}
	if (hardware) {
		CFRelease(hardware);
	}
	int fps = 60;
	int bitrate = 10000000;
	/* A real-time client's bitrate rises this much every so many frames, as
	 * Parsec's did on the host (2 to 50 Mb/s in steps of 750 kb/s). */
	const int step = 750000, every = 30;
	if (client) {
		setBoolean(session, kVTCompressionPropertyKey_RealTime, true, "RealTime");
		setBoolean(session, kVTCompressionPropertyKey_AllowFrameReordering, false, "AllowFrameReordering");
		setNumber(session, kVTCompressionPropertyKey_ExpectedFrameRate, fps, "ExpectedFrameRate");
		setNumber(session, kVTCompressionPropertyKey_AverageBitRate, bitrate, "AverageBitRate");
		printf("  no MaxKeyFrameInterval: an infinite GOP, key frames only when asked\n");
	} else if (mode != ModeLowLatency) {
		setNumber(session, kVTCompressionPropertyKey_AverageBitRate, bitrate, "AverageBitRate");
		setNumber(session, kVTCompressionPropertyKey_ExpectedFrameRate, fps, "ExpectedFrameRate");
		setNumber(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, 100000, "MaxKeyFrameInterval");
		setBoolean(session, kVTCompressionPropertyKey_RealTime, true, "RealTime");
	}
	CVPixelBufferPoolRef pool = VTCompressionSessionGetPixelBufferPool(session);
	const void *forceKeys[] = {kVTEncodeFrameOptionKey_ForceKeyFrame};
	const void *forceValues[] = {kCFBooleanTrue};
	CFDictionaryRef forceKey = CFDictionaryCreate(NULL, forceKeys, forceValues, 1, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	double cpu0 = cpuSeconds(), t0 = now(), encodeSeconds = 0;
	int submitted = 0, bitrateChanges = 0, bitrateRefused = 0;
	OSType made = strcmp(input, "bgra") == 0 ? kCVPixelFormatType_32BGRA : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
	bool fromPool = pool && strcmp(input, "pool") == 0;
	for (int i = 0; i < frames; i++) {
		CVPixelBufferRef buffer = NULL;
		if (fromPool) {
			CVPixelBufferPoolCreatePixelBuffer(NULL, pool, &buffer);
		} else {
			/* IOSurface-backed, as a capture's buffers are. */
			CFDictionaryRef surface = CFDictionaryCreate(NULL, NULL, NULL, 0, &kCFTypeDictionaryKeyCallBacks,
			    &kCFTypeDictionaryValueCallBacks);
			const void *keys[] = {kCVPixelBufferIOSurfacePropertiesKey};
			const void *values[] = {surface};
			CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 1, &kCFTypeDictionaryKeyCallBacks,
			    &kCFTypeDictionaryValueCallBacks);
			CVPixelBufferCreate(NULL, width, height, made, attributes, &buffer);
			CFRelease(attributes);
			CFRelease(surface);
		}
		if (!buffer) {
			printf("  frame %d: no pixel buffer\n", i);
			break;
		}
		if (i == 0) {
			run.format = CVPixelBufferGetPixelFormatType(buffer);
			char code[5] = {(char)(run.format >> 24), (char)(run.format >> 16), (char)(run.format >> 8), (char)run.format, 0};
			printf("  input frames: '%s'%s\n", code, fromPool ? ", from the session's pool" : "");
		}
		fill(buffer, i);
		if (client && i > 0 && i % every == 0) {
			bitrate += step;
			CFNumberRef number = CFNumberCreate(NULL, kCFNumberIntType, &bitrate);
			bitrateRefused += VTSessionSetProperty(session, kVTCompressionPropertyKey_AverageBitRate, number) != noErr;
			bitrateChanges++;
			CFRelease(number);
		}
		if (client) {
			/* Each frame at its 60 fps slot, as a capture delivers them. */
			double wait = t0 + (double)i / fps - now();
			if (wait > 0) {
				usleep((useconds_t)(wait * 1e6));
			}
		}
		double before = now();
		if (i < kMostFrames) {
			run.submitted[i] = before;
		}
		status = VTCompressionSessionEncodeFrame(session, buffer, CMTimeMake(i, fps), CMTimeMake(1, fps),
		    i == frames / 2 ? forceKey : NULL, (void *)(intptr_t)(i + 1), NULL);
		encodeSeconds += now() - before;
		CVPixelBufferRelease(buffer);
		if (status != noErr) {
			printf("  encode frame %d: %d\n", i, (int)status);
			break;
		}
		submitted++;
	}
	VTCompressionSessionCompleteFrames(session, kCMTimeInvalid);
	double cpu = cpuSeconds() - cpu0, wall = now() - t0;
	CFRelease(forceKey);
	if (bitrateChanges) {
		printf("  bitrate: %d changes, %d refused, up to %.2f Mbit/s\n", bitrateChanges, bitrateRefused, bitrate / 1e6);
	}
	printf("  frames in %d, out %d, key frames %d, dropped %d, errors %d, %zu bytes (%.1f Mbit/s at %d fps)\n",
	    submitted, run.emitted, run.keyframes, run.dropped, run.errors, run.bytes,
	    run.emitted ? run.bytes * 8.0 * fps / run.emitted / 1e6 : 0, fps);
	printf("  %.1f fps encoding alone (%.2f ms a frame); wall %.2fs (%.1f fps with drawing%s); process cpu %.2f ms a "
	       "frame, drawing the test frames included\n",
	    submitted / encodeSeconds, encodeSeconds * 1000 / (submitted ? submitted : 1), wall, submitted / wall,
	    client ? ", paced" : "", cpu * 1000 / (submitted ? submitted : 1));
	if (run.latencies > 0) {
		double sum = 0;
		for (int i = 0; i < run.latencies; i++) {
			sum += run.latency[i];
		}
		qsort(run.latency, (size_t)run.latencies, sizeof(double), compareDoubles);
		int p95 = (run.latencies * 95 + 99) / 100 - 1;
		printf("  encode latency (submission to output) over %d frames: average %.2f ms, p95 %.2f ms, longest %.2f ms\n",
		    run.latencies, sum * 1000 / run.latencies, run.latency[p95 < 0 ? 0 : p95] * 1000,
		    run.latency[run.latencies - 1] * 1000);
	}
	VTCompressionSessionInvalidate(session);
	CFRelease(session);
	if (run.out) {
		fclose(run.out);
	}
	decodeAll();
	printf("  decoded %d of %d, %d undecodable, luma PSNR lowest %.1f dB, average %.1f dB\n", run.decoded,
	    run.emitted, run.undecodable, run.lowest, run.decoded ? run.sum / run.decoded : 0);
	bool passed = (isHardware || !wantsHardware) && submitted == frames && run.emitted + run.dropped == frames
	    && run.errors == 0 && run.keyframes >= (frames > 1 ? 2 : 1) && run.decoded == run.emitted && run.lowest >= 30;
	printf("%s %s: %s%s\n", label, passed ? "PASS" : "FAIL", name, note);
	return passed;
}

int main(int argc, char **argv)
{
	const char *codecs = "both", *input = "pool", *prefix = NULL, *plugin = NULL;
	Mode mode = ModeHardwareLowLatency;
	bool listing = false;
	int frames = 300, width = 1920, height = 1080;
	for (int i = 1; i < argc; i++) {
		const char *value = i + 1 < argc ? argv[i + 1] : NULL;
		if (strcmp(argv[i], "--codec") == 0 && value) {
			codecs = argv[++i];
		} else if (strcmp(argv[i], "--frames") == 0 && value) {
			frames = atoi(argv[++i]);
		} else if (strcmp(argv[i], "--mode") == 0 && value && strcmp(value, "list") == 0) {
			listing = true;
			i++;
		} else if (strcmp(argv[i], "--mode") == 0 && value && parseMode(value, &mode)) {
			i++;
		} else if (strcmp(argv[i], "--input") == 0 && value
		    && (strcmp(value, "pool") == 0 || strcmp(value, "bgra") == 0 || strcmp(value, "420v") == 0)) {
			input = argv[++i];
		} else if (strcmp(argv[i], "--out") == 0 && value) {
			prefix = argv[++i];
		} else if (strcmp(argv[i], "--register") == 0 && value) {
			plugin = argv[++i];
		} else if (strcmp(argv[i], "--size") == 0 && value && sscanf(argv[i + 1], "%dx%d", &width, &height) == 2) {
			i++;
		} else {
			fprintf(stderr, "usage: vttest [--codec h264|hevc|both] [--frames N] [--mode hw+ll|ll|hw|none|list] "
			    "[--input pool|bgra|420v] [--out PREFIX] [--register BUNDLE_EXECUTABLE] [--size WxH]\n");
			return 2;
		}
	}
	if (plugin && !registerPlugin(plugin)) {
		return 1;
	}
	if (listing) {
		list();
		return 0;
	}
	bool passed = true;
	if (strcmp(codecs, "h264") == 0 || strcmp(codecs, "both") == 0) {
		passed &= encode(kCMVideoCodecType_H264, mode, input, frames, width, height, prefix);
	}
	if (strcmp(codecs, "hevc") == 0 || strcmp(codecs, "both") == 0) {
		passed &= encode(kCMVideoCodecType_HEVC, mode, input, frames, width, height, prefix);
	}
	printf("vttest %s\n", passed ? "PASS" : "FAIL");
	return passed ? 0 : 1;
}
