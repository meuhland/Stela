/*
 * Registers the StelaVT H.264 encoder, when installed, in each process that
 * loads this plugin and has VideoToolbox: Stela has no Apple video encoder,
 * so real-time clients otherwise fall back to Apple's software HEVC.
 *
 * VideoToolbox lists only Apple-signed encoder plugins from
 * /Library/Video/Plug-Ins, and its low-latency rate control runs an inner
 * session in the video encoder service that needs a hardware encoder there.
 * This plugin is loaded into that service with Metal, so registering
 * StelaVT here makes it the service's hardware H.264 encoder. Nothing
 * happens without VideoToolbox in the process or without StelaVT installed.
 */
#include "Apple.h"

#define STELAVT "/Library/Video/Plug-Ins/StelaVT.bundle/Contents/MacOS/StelaVT"
#define RTLD_DEFAULT ((void *)-2)
/* libsandbox's sandbox_check() filter and flag. */
#define SANDBOX_FILTER_PATH 1
#define SANDBOX_CHECK_NO_REPORT 0x40000000

typedef int32_t (*Factory)(uint32_t, const void *, const void **);
typedef int32_t (*Register)(uint32_t, const void *, Factory);

__attribute__((constructor)) static void register_stela_video_encoder(void)
{
	Register registerEncoder = (Register)dlsym(RTLD_DEFAULT, "VTRegisterVideoEncoderWithInfo");
	const void *const *hardwareKey = dlsym(RTLD_DEFAULT, "kVTVideoEncoder_IsHardwareAccelerated");
	if (!registerEncoder || !hardwareKey) {
		return;
	}
	/* A sandbox that may not map StelaVT (WindowServer's) would only log a
	 * violation for each attempt; such a process encodes nothing anyway. */
	int (*sandboxCheck)(int, const char *, int, ...) = dlsym(RTLD_DEFAULT, "sandbox_check");
	int (*processID)(void) = dlsym(RTLD_DEFAULT, "getpid");
	if (sandboxCheck && processID &&
	    sandboxCheck(processID(), "file-map-executable", SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT, STELAVT) > 0) {
		return;
	}
	void *library = dlopen(STELAVT, RTLD_NOW | RTLD_LOCAL);
	Factory factory = library ? (Factory)dlsym(library, "StelaVT_CreateInstance") : NULL;
	if (!factory) {
		NSLog(@"StelaGPU: StelaVT is not installed or does not load; no H.264 encoder registered");
		return;
	}
	void *(*createDictionary)(const void *, long, const void *, const void *) = dlsym(RTLD_DEFAULT,
	    "CFDictionaryCreateMutable");
	void (*setValue)(void *, const void *, const void *) = dlsym(RTLD_DEFAULT, "CFDictionarySetValue");
	const void *const *booleanTrue = dlsym(RTLD_DEFAULT, "kCFBooleanTrue");
	const void *keyCallBacks = dlsym(RTLD_DEFAULT, "kCFTypeDictionaryKeyCallBacks");
	const void *valueCallBacks = dlsym(RTLD_DEFAULT, "kCFTypeDictionaryValueCallBacks");
	if (!createDictionary || !setValue || !booleanTrue || !keyCallBacks || !valueCallBacks) {
		return;
	}
	void *info = createDictionary(NULL, 0, keyCallBacks, valueCallBacks);
	setValue(info, *hardwareKey, *booleanTrue);
	/* 'avc1' */
	int32_t status = registerEncoder(0x61766331, info, factory);
	CFRelease(info);
	if (status == 0) {
		NSLog(@"StelaGPU: StelaVT registered as this process's hardware H.264 encoder");
	} else {
		NSLog(@"StelaGPU: VideoToolbox refused to register StelaVT");
	}
}
