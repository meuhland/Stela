/*
 * The video decoder calls StelaGPU.bundle gives StelaVD: the host's NVDEC
 * sessions, through the gate connection the bundle already holds in the
 * process (each Metal device's), as StelaEncoder.h gives StelaVT the host's
 * NVENC. Shared by the bundle (bundle/Decoder.m) and StelaVD (StelaVD.c),
 * which is built on a Mac against this header alone.
 *
 * The bundle's constructor (bundle/VideoEncoder.m) dlopens StelaVT, dlsyms
 * its STELAVD_USE_DECODERS export and hands it the table before it registers
 * StelaVD with VideoToolbox. A StelaVT loaded any other way finds the table
 * by class instead: objc_getClass(STELA_DECODER_CLASS) answers +api once the
 * bundle is loaded. Without either, StelaVD decodes with Apple's software
 * decoder alone, as before NVDEC.
 *
 * The host parses the stream (its avcC/hvcC record, then each sample of
 * length-prefixed NAL units), decodes on NVDEC, and writes each picture it
 * shows, in display order (or in decode order, as open_flags asks), into a
 * destination the caller gave: the planes of
 * a pixel buffer, 420v (NV12) or x420 (P010) as the open answered, the
 * stream's displayed size. A sample may show no picture (a B-frame stream's
 * first ones), one, or several; every picture decoded is shown once, with
 * the `frame` of the sample that decoded it.
 *
 * Every call answers a StelaDecoderStatus: the gate's status
 * (crates/stela-driver-gpu/src/metal/service/wire.rs) or the bundle's own.
 * `reason`, when given, receives why a call failed, NUL-terminated.
 */
#pragma once

#include <stddef.h>
#include <stdint.h>

/* Version 2 adds `open_flags` (the order pictures are shown in) after the
 * version 1 table, which stays as it was: a version 2 table serves a
 * StelaVD built for version 1, and StelaVD takes a version 1 table, whose
 * sessions show their pictures in display order. */
#define STELA_DECODER_API_VERSION 2u
/* The oldest table StelaVD takes. */
#define STELA_DECODER_API_MIN_VERSION 1u
#define STELAVD_USE_DECODERS "StelaVD_UseDecoders"
#define STELA_DECODER_CLASS "StelaGPUDecoders"
/* The most destinations one decode gives. */
#define STELA_DECODER_MAX_DESTINATIONS 16u

/* The gate's codes: EncoderCodec (shared with the encoder) and DecoderFormat. */
enum {
	StelaDecoderH264 = 1,
	StelaDecoderHEVC = 2,
};
enum {
	/* '420v': 8-bit Y, then interleaved CbCr, video range. */
	StelaDecoderNV12 = 0,
	/* 'x420': 16-bit samples, the 10 bits high, then interleaved CbCr. */
	StelaDecoderP010 = 1,
};
/* open_flags' flags (the gate's DECODER_OPEN_*). */
enum {
	/* Show each picture right after its decode, in decode order
	 * (VideoToolbox without temporal processing), rather than in display
	 * order. */
	StelaDecoderOpenDecodeOrder = 1,
};
/* StelaDecoderSample.flags. */
enum {
	/* The end of the stream (finishDelayedFrames): after the sample, if
	 * any, every picture still waiting is shown. */
	StelaDecodeFlush = 1,
	/* Forget every picture before this sample, as for a seek: the sample
	 * must be a sync sample. */
	StelaDecodeReset = 2,
};
/* StelaDecoderDestination.flags, which the host writes. */
enum {
	StelaDecodedWritten = 1,
	/* The engine reported errors in the picture, which it concealed. */
	StelaDecodedErrors = 2,
};

typedef enum {
	StelaDecoderOk = 0,
	StelaDecoderInvalid = 1,
	/* No decoder on this host, no session left, or a stream it does not
	 * take: decode another way. */
	StelaDecoderUnsupported = 2,
	StelaDecoderOutOfMemory = 3,
	StelaDecoderMalformed = 6,
	StelaDecoderStaleHandle = 7,
	StelaDecoderClosed = 8,
	/* A destination's pages stayed out of the host's reach. */
	StelaDecoderUnmapped = 9,
	/* The session stopped answering: close it, decode another way. */
	StelaDecoderLost = 12,
	/* The host's parser or the engine refused the sample: it showed
	 * nothing, and the session goes on with the next. */
	StelaDecoderDropped = 13,
	/* The host closed the session, idle, to make room for another or to
	 * give its video memory back: nothing of the call took effect. Close it
	 * and open a new one at the stream's next sync sample. */
	StelaDecoderEvicted = 14,
	/* The bundle's own: no Metal device of this process opened its gate. */
	StelaDecoderNoGate = 100,
	/* The bundle's own: out of memory. */
	StelaDecoderNoMemory = 101,
} StelaDecoderStatus;

typedef struct {
	/* 1 << codec for each codec. */
	uint32_t codecs;
	/* H.264: 1 Baseline, 2 Main, 4 High; HEVC: 1 Main, 2 Main 10. */
	uint32_t profiles_h264, profiles_hevc;
	/* 1 << bits for each bit depth; 1 << format for each format. */
	uint32_t depths, formats;
	uint32_t max_sessions, open_sessions;
	uint32_t max_width, max_height, min_width, min_height;
	uint32_t max_destinations;
} StelaDecoderCapabilities;

/* What an open answers: the pixel buffers are made of it. */
typedef struct {
	uint32_t coded_width, coded_height;
	/* Every picture shown is this size. */
	uint32_t display_width, display_height;
	uint32_t bit_depth;
	/* StelaDecoderNV12 or StelaDecoderP010. */
	uint32_t format;
	/* The stream's reference slots, and the surfaces the host made. */
	uint32_t slots, surfaces;
} StelaDecoderStream;

typedef struct {
	/* The planes a picture may be shown in, each `pitch` bytes a row and
	 * `bytes` long: Y, then interleaved CbCr. */
	void *luma;
	size_t luma_bytes;
	void *chroma;
	size_t chroma_bytes;
	uint32_t luma_pitch, chroma_pitch;
	/* Written by the host when it shows a picture here: the frame of the
	 * sample that decoded it, its picture order count, StelaDecoded*. */
	uint64_t frame;
	int32_t poc;
	uint32_t flags;
} StelaDecoderDestination;

typedef struct {
	/* A number each new sample changes; the same number goes on with a
	 * sample (after StelaDecoderOk with `taken` 0, with fresh
	 * destinations). */
	uint64_t frame;
	/* One access unit of length-prefixed NAL units, as the record says;
	 * none (0 bytes) to only show what waits, or flush. */
	const void *sample;
	size_t sample_bytes;
	uint32_t flags;
	/* `count` destinations, filled in display order. */
	StelaDecoderDestination *destinations;
	uint32_t count;
} StelaDecoderSample;

typedef struct {
	/* 1 once the sample is decoded; 0 when the destinations ran out first
	 * (`queued` pictures wait): give more, the same sample and frame. */
	uint32_t taken;
	/* The sample held a picture. */
	uint32_t decoded;
	/* Destinations filled, and pictures still waiting. */
	uint32_t outputs, queued;
	/* What the engine reported of the sample's picture. */
	uint32_t mbs_in_error, error_status;
	/* Microseconds: the engine's decode, the copy engine's readback and the
	 * host's copy into the destinations; and the gate calls it took. */
	uint32_t gpu_us, ce_us, copy_us;
	uint32_t calls;
} StelaDecoderResult;

typedef struct StelaDecoderSession StelaDecoderSession;

typedef struct {
	uint32_t version;
	/* What the host's decoder takes, through this process's first device
	 * that has one. */
	StelaDecoderStatus (*capabilities)(StelaDecoderCapabilities *out, char *reason, size_t reason_bytes);
	/* A session of `codec` for the stream its avcC (H.264) or hvcC (HEVC)
	 * record `config` describes. */
	StelaDecoderStatus (*open)(uint32_t codec, const void *config, size_t config_bytes, StelaDecoderStream *stream,
				   StelaDecoderSession **session, char *reason, size_t reason_bytes);
	/* Decodes one sample and shows what is ready, waiting for it. */
	StelaDecoderStatus (*decode)(StelaDecoderSession *session, StelaDecoderSample *sample, StelaDecoderResult *result,
				     char *reason, size_t reason_bytes);
	/* Closes a session, an evicted one too. */
	void (*close)(StelaDecoderSession *session);
	/* Version 2: `open` with StelaDecoderOpen* `flags`. */
	StelaDecoderStatus (*open_flags)(uint32_t codec, uint32_t flags, const void *config, size_t config_bytes,
					 StelaDecoderStream *stream, StelaDecoderSession **session, char *reason,
					 size_t reason_bytes);
} StelaDecoderAPI;

/* StelaVT's export for StelaVD: keeps `api`, which lives as long as the
 * process. */
typedef void (*StelaVDUseDecoders)(const StelaDecoderAPI *api);
