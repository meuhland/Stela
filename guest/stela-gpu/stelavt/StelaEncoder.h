/*
 * The video encoder calls StelaGPU.bundle gives StelaVT: the host's NVENC
 * sessions, through the gate connection the bundle already holds in the
 * process (each Metal device's), so StelaVT opens no IOKit connection of its
 * own. Shared by the bundle (bundle/Encoder.m) and StelaVT (StelaVT.c),
 * which is built on a Mac against this header alone.
 *
 * The bundle's linker exports nothing, so StelaVT cannot dlsym the bundle:
 * the bundle's constructor (bundle/VideoEncoder.m) dlopens StelaVT, dlsyms
 * its STELAVT_USE_ENCODERS export and hands it the table, before it
 * registers StelaVT with VideoToolbox. A StelaVT loaded any other way, as
 * vttest's `--register` loads one, finds the table by class instead:
 * objc_getClass(STELA_ENCODER_CLASS) answers +api, once the bundle is loaded
 * (any Metal device loads it). Without either it encodes H.264 with x264
 * alone.
 *
 * Every call answers a StelaEncoderStatus: the gate's status
 * (crates/stela-driver-gpu/src/metal/service/wire.rs) or the bundle's own.
 * `reason`, when given, receives why a call failed, NUL-terminated.
 */
#pragma once

#include <stddef.h>
#include <stdint.h>

#define STELA_ENCODER_API_VERSION 1u
#define STELAVT_USE_ENCODERS "StelaVT_UseEncoders"
#define STELA_ENCODER_CLASS "StelaGPUEncoders"

/* The gate's codes: EncoderCodec, EncoderRate and EncoderInput. */
enum {
	StelaEncoderH264 = 1,
	StelaEncoderHEVC = 2,
};
enum {
	StelaEncoderCBR = 0,
	StelaEncoderConstQP = 1,
};
enum {
	/* '420v': Y, then interleaved CbCr, video range. */
	StelaEncoderNV12 = 0,
	/* 'BGRA', which the host converts on its CPU. */
	StelaEncoderBGRA = 1,
};

typedef enum {
	StelaEncoderOk = 0,
	StelaEncoderInvalid = 1,
	/* No encoder on this host, no session left, or a request it does
	 * not take: encode another way. */
	StelaEncoderUnsupported = 2,
	StelaEncoderOutOfMemory = 3,
	StelaEncoderMalformed = 6,
	StelaEncoderStaleHandle = 7,
	StelaEncoderClosed = 8,
	/* The session stopped answering: close it, encode another way. */
	StelaEncoderLost = 12,
	/* The engine refused this frame: nothing was coded for it, and the
	 * session's next frame is an IDR. */
	StelaEncoderDropped = 13,
	/* The bundle's own: no Metal device of this process opened its gate. */
	StelaEncoderNoGate = 100,
	/* The bundle's own: out of memory. */
	StelaEncoderNoMemory = 101,
} StelaEncoderStatus;

typedef struct {
	/* 1 << codec for each codec, 1 << input for each input. */
	uint32_t codecs, inputs;
	uint32_t max_sessions, open_sessions;
	uint32_t max_width, max_height;
} StelaEncoderCapabilities;

typedef struct {
	uint32_t codec;
	/* Even, 16 to 4096. */
	uint32_t width, height;
	/* Frames a second times 1000; 0 for 60. */
	uint32_t fps_milli;
	/* CBR at `bitrate` bits a second (0: 20 Mb/s), or a fixed `qp`. */
	uint32_t rate, bitrate, qp;
} StelaEncoderConfig;

typedef struct {
	/* Raised for each new frame; the same number goes on with a frame. */
	uint64_t frame;
	/* The planes, each `pitch` bytes a row, `bytes` long: NV12's Y and
	 * CbCr, or BGRA's one plane at `luma`. */
	const void *luma;
	size_t luma_bytes;
	const void *chroma;
	size_t chroma_bytes;
	uint32_t luma_pitch, chroma_pitch;
	uint32_t input;
	uint32_t force_idr;
	/* Bits a second from this frame on; 0 leaves it. */
	uint32_t bitrate;
	/* Receives the frame's Annex B bytes. */
	void *out;
	size_t out_bytes;
} StelaEncoderFrame;

typedef struct {
	/* The frame's Annex B length: written to `out` when it fits; when
	 * not, the same frame again with a longer `out` gets it. An IDR
	 * begins with the parameter sets. */
	uint64_t bytes;
	uint32_t idr, picture_index;
	/* 2 is what every frame reports, and is accepted. */
	uint32_t error_status, average_qp;
	/* The host's input pass, and submission to completion. */
	uint32_t convert_us, gpu_us;
} StelaEncoderResult;

typedef struct StelaEncoderSession StelaEncoderSession;

typedef struct {
	uint32_t version;
	/* What the host's encoder takes, through this process's first
	 * device that has one. */
	StelaEncoderStatus (*capabilities)(StelaEncoderCapabilities *out, char *reason, size_t reason_bytes);
	/* A session of `config`: its parameter sets (SPS and PPS; HEVC: VPS,
	 * SPS and PPS) as Annex B into `sets`, their length in
	 * `*sets_length`, and the most bytes a frame takes in
	 * `*max_frame_bytes`. */
	StelaEncoderStatus (*open)(const StelaEncoderConfig *config, void *sets, size_t sets_capacity,
	    size_t *sets_length, uint64_t *max_frame_bytes, StelaEncoderSession **session, char *reason,
	    size_t reason_bytes);
	/* Encodes one frame, waiting for it. */
	StelaEncoderStatus (*encode)(StelaEncoderSession *session, const StelaEncoderFrame *frame,
	    StelaEncoderResult *result, char *reason, size_t reason_bytes);
	void (*close)(StelaEncoderSession *session);
} StelaEncoderAPI;

/* StelaVT's export: keeps `api`, which lives as long as the process. */
typedef void (*StelaVTUseEncoders)(const StelaEncoderAPI *api);
