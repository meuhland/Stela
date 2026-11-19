/*
 * Registers the StelaVT encoder, when installed, in each process that loads
 * this plugin and has VideoToolbox: Stela has no Apple video encoder, so
 * real-time clients otherwise fall back to Apple's software HEVC.
 *
 * VideoToolbox lists only Apple-signed encoder plugins from
 * /Library/Video/Plug-Ins, and its low-latency rate control runs an inner
 * session in the video encoder service that needs a hardware encoder there.
 * This plugin is loaded into that service with Metal, so registering
 * StelaVT here makes it the service's hardware H.264 encoder. Nothing
 * happens without VideoToolbox in the process or without StelaVT installed.
 *
 * StelaVT encodes on the host's NVENC through this plugin's gate
 * (Encoder.m), whose calls it gets here before it is registered, and
 * otherwise with x264. It is registered for HEVC as well once a device's
 * host says its encoder codes HEVC (`stela_video_encoder_offer_hevc`): x264
 * has no HEVC, and a hardware HEVC encoder that refuses every session would
 * send VideoToolbox to Apple's software one.
 *
 * StelaVT's decoder, StelaVD (stelavt/StelaVD.c), is registered here too, as
 * the process's hardware H.264 and HEVC decoder: Stela has no Apple video
 * decoder, so a hardware-only client gets none and the rest decode in
 * software. StelaVD decodes on the host's NVDEC through this plugin's gate
 * (Decoder.m), whose calls it gets here before it is registered
 * (STELAVD_USE_DECODERS), and runs Apple's software decoder inside each
 * session for every stream the host refuses or loses.
 * A file at STELAVD_OFF keeps it unregistered, without a rebuild.
 */
#include "Apple.h"
#include "Bundle.h"

#define STELAVT "/Library/Video/Plug-Ins/StelaVT.bundle/Contents/MacOS/StelaVT"
#define STELAVD_OFF "/Library/Video/Plug-Ins/StelaVD-off"
#define RTLD_DEFAULT ((void *)-2)
/* libsandbox's sandbox_check() filter and flag. */
#define SANDBOX_FILTER_PATH 1
#define SANDBOX_CHECK_NO_REPORT 0x40000000
/* 'avc1' and 'hvc1'. */
#define CODEC_H264 0x61766331u
#define CODEC_HEVC 0x68766331u

typedef int32_t (*Factory)(uint32_t, const void *, const void **);
typedef int32_t (*Register)(uint32_t, const void *, Factory);

/* What the constructor found, which the HEVC registration takes later. */
static struct {
	Register registerEncoder;
	Factory factory;
	const void *info;
	/* It encodes on the host, which HEVC needs. */
	bool hostEncoder;
} stelavt;

/* Registers StelaVT for `codec` with the hardware info: VideoToolbox's
 * status. */
static int32_t register_codec(uint32_t codec)
{
	return stelavt.registerEncoder(codec, stelavt.info, stelavt.factory);
}

/* Registers StelaVD, when this StelaVT has it and STELAVD_OFF is absent, as
 * the process's hardware H.264 and HEVC decoder. */
static void register_stela_video_decoder(void *library, void *(*createDictionary)(const void *, long, const void *,
    const void *), void (*setValue)(void *, const void *, const void *), const void *booleanTrue,
    const void *keyCallBacks, const void *valueCallBacks)
{
	Register registerDecoder = (Register)dlsym(RTLD_DEFAULT, "VTRegisterVideoDecoderWithInfo");
	const void *const *hardwareKey = dlsym(RTLD_DEFAULT, "kVTVideoDecoder_IsHardwareAccelerated");
	int (*accessPath)(const char *, int) = dlsym(RTLD_DEFAULT, "access");
	Factory factory = (Factory)dlsym(library, "StelaVD_CreateInstance");
	if (!registerDecoder || !hardwareKey || !factory || !accessPath) {
		return;
	}
	if (accessPath(STELAVD_OFF, 0) == 0) {
		NSLog(@"StelaGPU: StelaVD not registered: " STELAVD_OFF " exists");
		return;
	}
	/* The host's decoder, before VideoToolbox can make a session. A StelaVT
	 * built before NVDEC has no such export: its StelaVD decodes with
	 * Apple's software decoder alone. */
	StelaVDUseDecoders useDecoders = (StelaVDUseDecoders)dlsym(library, STELAVD_USE_DECODERS);
	if (useDecoders) {
		useDecoders(stela_decoder_api());
	} else {
		NSLog(@"StelaGPU: this StelaVD takes no host decoder; it decodes with Apple's software decoder");
	}
	void *info = createDictionary(NULL, 0, keyCallBacks, valueCallBacks);
	setValue(info, *hardwareKey, booleanTrue);
	int32_t h264 = registerDecoder(CODEC_H264, info, factory);
	int32_t hevc = registerDecoder(CODEC_HEVC, info, factory);
	NSLog(@"StelaGPU: StelaVD registered as this process's hardware decoder: H.264 %d, HEVC %d (0 is registered)",
	    (int)h264, (int)hevc);
}

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
	/* The host's encoder, before VideoToolbox can make a session. A StelaVT
	 * built before NVENC has no such export, and encodes with x264. */
	StelaVTUseEncoders useEncoders = (StelaVTUseEncoders)dlsym(library, STELAVT_USE_ENCODERS);
	if (useEncoders) {
		useEncoders(stela_encoder_api());
	} else {
		NSLog(@"StelaGPU: this StelaVT takes no host encoder; it encodes with x264");
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
	/* Kept for the HEVC registration, as long as the process. */
	stelavt.registerEncoder = registerEncoder;
	stelavt.factory = factory;
	stelavt.info = info;
	stelavt.hostEncoder = useEncoders != NULL;
	if (register_codec(CODEC_H264) == 0) {
		NSLog(@"StelaGPU: StelaVT registered as this process's hardware H.264 encoder");
	} else {
		NSLog(@"StelaGPU: VideoToolbox refused to register StelaVT");
	}
	register_stela_video_decoder(library, createDictionary, setValue, *booleanTrue, keyCallBacks, valueCallBacks);
}

void stela_video_encoder_offer_hevc(void)
{
	/* No StelaVT, or one that takes no host encoder. */
	if (!stelavt.factory || !stelavt.hostEncoder) {
		return;
	}
	if (register_codec(CODEC_HEVC) == 0) {
		NSLog(@"StelaGPU: StelaVT registered as this process's hardware HEVC encoder (the host's NVENC)");
	} else {
		NSLog(@"StelaGPU: VideoToolbox refused to register StelaVT for HEVC");
	}
}
