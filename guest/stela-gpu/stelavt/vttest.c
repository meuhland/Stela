/*
 * vttest BUNDLE_EXECUTABLE OUT.h264 [frames] [register]: encodes moving
 * 1080p NV12 frames through VideoToolbox, asking for a hardware, low-latency
 * H.264 encoder as Parsec does. With "register" it first registers StelaVT's
 * factory in this process, so the plugin is tested without installing it.
 * Writes the stream as Annex B and reports which encoder ran and its CPU.
 */
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>

typedef OSStatus (*Factory)(FourCharCode, CFAllocatorRef, void **);
typedef OSStatus (*Register)(CMVideoCodecType, CFDictionaryRef, Factory);

static FILE *out;
static int emitted, keyframes, errors;
static size_t bytes;

static void writeStart(const uint8_t *nal, size_t length)
{
	static const uint8_t start[4] = {0, 0, 0, 1};
	fwrite(start, 1, 4, out);
	fwrite(nal, 1, length, out);
}

static void encoded(void *context, void *source, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample)
{
	if (status != noErr || !sample) {
		errors++;
		return;
	}
	CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
	bool key = true;
	if (attachments && CFArrayGetCount(attachments) > 0) {
		CFDictionaryRef first = CFArrayGetValueAtIndex(attachments, 0);
		CFBooleanRef notSync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
		key = !(notSync && CFBooleanGetValue(notSync));
	}
	CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
	if (key) {
		keyframes++;
		size_t count = 0;
		CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, NULL, NULL, &count, NULL);
		for (size_t i = 0; i < count; i++) {
			const uint8_t *set;
			size_t length;
			CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, i, &set, &length, NULL, NULL);
			writeStart(set, length);
		}
	}
	CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
	size_t total = CMBlockBufferGetDataLength(block);
	uint8_t *data = malloc(total);
	CMBlockBufferCopyDataBytes(block, 0, total, data);
	for (size_t at = 0; at + 4 <= total;) {
		uint32_t length = (uint32_t)data[at] << 24 | (uint32_t)data[at + 1] << 16 | (uint32_t)data[at + 2] << 8 | data[at + 3];
		writeStart(data + at + 4, length);
		at += 4 + length;
	}
	free(data);
	bytes += total;
	emitted++;
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

static void fill(CVPixelBufferRef buffer, int frame)
{
	CVPixelBufferLockBaseAddress(buffer, 0);
	size_t w = CVPixelBufferGetWidth(buffer), h = CVPixelBufferGetHeight(buffer);
	uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
	size_t ys = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
	uint8_t *uv = CVPixelBufferGetBaseAddressOfPlane(buffer, 1);
	size_t uvs = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
	for (size_t r = 0; r < h; r++) {
		for (size_t c = 0; c < w; c++) {
			/* A desktop-like scene: flat bands, text-like stripes, one moving box. */
			uint8_t v = (uint8_t)(40 + (r / 120) * 20);
			if ((r % 24) < 2 && (c % 300) < 220) {
				v = 230;
			}
			if (c >= (size_t)(frame * 6 % (int)w) && c < (size_t)(frame * 6 % (int)w) + 200 && r >= 400 && r < 600) {
				v = (uint8_t)(120 + frame % 100);
			}
			y[r * ys + c] = v;
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

int main(int argc, char **argv)
{
	if (argc < 3) {
		fprintf(stderr, "usage: vttest BUNDLE_EXECUTABLE OUT.h264 [frames] [register]\n");
		return 2;
	}
	int frames = argc > 3 ? atoi(argv[3]) : 300;
	bool registerIt = argc > 4 && strcmp(argv[4], "register") == 0;
	if (registerIt) {
		void *plugin = dlopen(argv[1], RTLD_NOW);
		Factory factory = plugin ? (Factory)dlsym(plugin, "StelaVT_CreateInstance") : NULL;
		Register reg = (Register)dlsym(RTLD_DEFAULT, "VTRegisterVideoEncoderWithInfo");
		if (!factory || !reg) {
			fprintf(stderr, "cannot load the plugin or the registration call: %s\n", dlerror());
			return 1;
		}
		CFMutableDictionaryRef info = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,
		    &kCFTypeDictionaryValueCallBacks);
		CFStringRef hw = (CFStringRef) * (CFStringRef *)dlsym(RTLD_DEFAULT, "kVTVideoEncoder_IsHardwareAccelerated");
		CFDictionarySetValue(info, hw, kCFBooleanTrue);
		OSStatus r = reg(kCMVideoCodecType_H264, info, factory);
		printf("registered StelaVT in-process: %d\n", (int)r);
		CFRelease(info);
	}
	out = fopen(argv[2], "wb");
	/* argv[5]: hw+ll (Parsec's first try), ll (its second), id (StelaVT by ID), none. */
	const char *mode = argc > 5 ? argv[5] : "hw+ll";
	CFMutableDictionaryRef spec = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,
	    &kCFTypeDictionaryValueCallBacks);
	if (strstr(mode, "hw")) {
		CFDictionarySetValue(spec, kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue);
	}
	if (strstr(mode, "ll")) {
		CFDictionarySetValue(spec, kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue);
	}
	if (strncmp(mode, "list", 4) == 0) {
		CFArrayRef list = NULL;
		VTCopyVideoEncoderList(NULL, &list);
		for (CFIndex i = 0; list && i < CFArrayGetCount(list); i++) {
			CFDictionaryRef item = CFArrayGetValueAtIndex(list, i);
			CFNumberRef codec = CFDictionaryGetValue(item, kVTVideoEncoderList_CodecType);
			int type = 0;
			CFNumberGetValue(codec, kCFNumberIntType, &type);
			if (type != kCMVideoCodecType_H264) {
				continue;
			}
			char id[256] = "", nm[256] = "";
			CFStringGetCString(CFDictionaryGetValue(item, kVTVideoEncoderList_EncoderID), id, sizeof id, kCFStringEncodingUTF8);
			CFStringRef display = CFDictionaryGetValue(item, kVTVideoEncoderList_DisplayName);
			if (display) {
				CFStringGetCString(display, nm, sizeof nm, kCFStringEncodingUTF8);
			}
			CFBooleanRef hw = CFDictionaryGetValue(item, kVTVideoEncoderList_IsHardwareAccelerated);
			CFBooleanRef ll = CFDictionaryGetValue(item, kVTVideoEncoderList_SupportsFrameReordering);
			printf("avc1 encoder: id=%s name=\"%s\" hardware=%s reorder=%s\n", id, nm,
			    hw == kCFBooleanTrue ? "yes" : "no", ll == kCFBooleanTrue ? "yes" : ll == kCFBooleanFalse ? "no" : "?");
		}
		return 0;
	}
	const char *id = strstr(mode, "id=");
	if (id) {
		CFStringRef value = CFStringCreateWithCString(NULL, id + 3, kCFStringEncodingUTF8);
		CFDictionarySetValue(spec, kVTVideoEncoderSpecification_EncoderID, value);
		CFRelease(value);
	}
	printf("mode %s\n", mode);
	VTCompressionSessionRef session = NULL;
	OSStatus status = VTCompressionSessionCreate(NULL, 1920, 1080, kCMVideoCodecType_H264, spec, NULL, NULL, encoded, NULL,
	    &session);
	printf("session: %d\n", (int)status);
	if (status != noErr) {
		return 1;
	}
	CFStringRef encoderID = NULL;
	VTSessionCopyProperty(session, kVTCompressionPropertyKey_EncoderID, NULL, &encoderID);
	char name[256] = "?";
	if (encoderID) {
		CFStringGetCString(encoderID, name, sizeof name, kCFStringEncodingUTF8);
	}
	CFBooleanRef hardware = NULL;
	VTSessionCopyProperty(session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, NULL, &hardware);
	printf("encoder: %s, hardware: %s\n", name, hardware == kCFBooleanTrue ? "yes" : "no");
	int bitrate = 10000000, fps = 60, keyint = 600;
	CFNumberRef n;
	n = CFNumberCreate(NULL, kCFNumberIntType, &bitrate);
	printf("set AverageBitRate: %d\n", (int)VTSessionSetProperty(session, kVTCompressionPropertyKey_AverageBitRate, n));
	CFRelease(n);
	n = CFNumberCreate(NULL, kCFNumberIntType, &fps);
	printf("set ExpectedFrameRate: %d\n", (int)VTSessionSetProperty(session, kVTCompressionPropertyKey_ExpectedFrameRate, n));
	CFRelease(n);
	n = CFNumberCreate(NULL, kCFNumberIntType, &keyint);
	printf("set MaxKeyFrameInterval: %d\n", (int)VTSessionSetProperty(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, n));
	CFRelease(n);
	printf("set RealTime: %d\n", (int)VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue));
	CVPixelBufferPoolRef pool = VTCompressionSessionGetPixelBufferPool(session);
	double cpu0 = cpuSeconds(), t0 = now();
	for (int i = 0; i < frames; i++) {
		CVPixelBufferRef buffer = NULL;
		if (pool) {
			CVPixelBufferPoolCreatePixelBuffer(NULL, pool, &buffer);
		} else {
			CVPixelBufferCreate(NULL, 1920, 1080, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, NULL, &buffer);
		}
		fill(buffer, i);
		status = VTCompressionSessionEncodeFrame(session, buffer, CMTimeMake(i, fps), CMTimeMake(1, fps), NULL, NULL, NULL);
		CVPixelBufferRelease(buffer);
		if (status != noErr) {
			printf("encode frame %d: %d\n", i, (int)status);
			break;
		}
	}
	VTCompressionSessionCompleteFrames(session, kCMTimeInvalid);
	double cpu = cpuSeconds() - cpu0, wall = now() - t0;
	printf("frames in %d, out %d, keyframes %d, errors %d, %zu bytes (%.1f Mbit/s at %d fps)\n", frames, emitted,
	    keyframes, errors, bytes, emitted ? bytes * 8.0 * fps / emitted / 1e6 : 0, fps);
	printf("wall %.2fs (%.1f fps), process cpu %.2fs = %.1f ms/frame (includes drawing the test frames)\n", wall,
	    frames / wall, cpu, cpu * 1000 / frames);
	VTCompressionSessionInvalidate(session);
	CFRelease(session);
	fclose(out);
	return 0;
}
