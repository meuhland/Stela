/*
 * The host's video encoder for StelaVT (stelavt/StelaEncoder.h): NVENC
 * sessions through the gate of a Metal device this process opened, so
 * StelaVT opens no IOKit connection of its own.
 *
 * Each device notes itself here once its gate negotiated
 * (`stela_encoder_note_device`). The first device whose host answers
 * EncoderCapabilities serves every session of the process; the bundle keeps
 * it. A process where no device opened, as when a sandbox refuses the
 * accelerator's user client, has no gate: StelaVT then encodes H.264 with
 * x264 and offers no HEVC, and says why once in its log.
 *
 * The calls wait as every gate call does: one answered Pending is made again
 * a moment later (Gate.m), while the host stores the picture and the engine
 * codes it.
 */
#include "Bundle.h"

struct StelaEncoderSession {
	StelaGate *gate;
	uint64_t encoder;
	uint32_t codec;
};

static os_unfair_lock lock = OS_UNFAIR_LOCK_INIT;
/* The devices that noted themselves, oldest first, which the bundle keeps:
 * a Metal device lives as long as its process anyway. */
static NSMutableArray *noted;
/* The device whose host serves the encoder, once one answered, and the
 * HEVC registration's offer, made once. */
static StelaGPUDevice *serving;
static bool hevcOffered;

static void copy_reason(char *reason, size_t bytes, const char *text)
{
	if (reason && bytes) {
		strlcpy(reason, text, bytes);
	}
}

static StelaEncoderStatus ask_capabilities(StelaGate *gate, StelaEncoderCapabilities *out, StelaFailure *failure)
{
	StelaEmpty nothing = {};
	StelaEncoderCapabilitiesResponse response = {};
	uint32_t status = STELA_CALL(gate, EncoderCapabilities, &nothing, &response, failure);
	if (status == StelaStatusOk && out) {
		*out = (StelaEncoderCapabilities){
			.codecs = response.codecs,
			.inputs = response.inputs,
			.max_sessions = response.max_sessions,
			.open_sessions = response.open_sessions,
			.max_width = response.max_width,
			.max_height = response.max_height,
		};
	}
	return (StelaEncoderStatus)status;
}

void stela_encoder_note_device(StelaGPUDevice *device)
{
	os_unfair_lock_lock(&lock);
	noted = noted ?: [NSMutableArray arrayWithCapacity:2];
	[noted addObject:device];
	bool offer = !hevcOffered;
	os_unfair_lock_unlock(&lock);
	if (!offer) {
		return;
	}
	/* HEVC is registered once a host encodes it: a refusal at each
	 * session would send VideoToolbox to Apple's software HEVC. */
	StelaEncoderCapabilities capabilities;
	StelaFailure failure;
	if (ask_capabilities(&device->_gate, &capabilities, &failure) == StelaEncoderOk &&
	    (capabilities.codecs & (1u << StelaEncoderHEVC))) {
		os_unfair_lock_lock(&lock);
		offer = !hevcOffered;
		hevcOffered = true;
		os_unfair_lock_unlock(&lock);
		if (offer) {
			stela_video_encoder_offer_hevc();
		}
	}
}

/* The gate of the device serving the encoder, or NULL with `failure` set. */
static StelaGate *serving_gate(StelaFailure *failure)
{
	os_unfair_lock_lock(&lock);
	StelaGPUDevice *device = serving;
	NSArray *candidates = device ? nil : [noted copy];
	os_unfair_lock_unlock(&lock);
	if (device) {
		return &device->_gate;
	}
	if (candidates.count == 0) {
		/* Metal makes the devices: a process that never asked has none. */
		NSArray *devices = MTLCopyAllDevices();
		(void)devices;
		os_unfair_lock_lock(&lock);
		candidates = [noted copy];
		os_unfair_lock_unlock(&lock);
	}
	*failure = (StelaFailure){.status = StelaEncoderNoGate};
	if (candidates.count == 0) {
		snprintf(failure->reason, sizeof(failure->reason), "no StelaGPU device opened in %s[%d]: %s",
			 getprogname(), getpid(),
			 stela_device_open_failure[0] ? stela_device_open_failure : "Metal opened none");
		return NULL;
	}
	for (NSUInteger i = 0; i < candidates.count; i++) {
		StelaGPUDevice *candidate = candidates[i];
		if (ask_capabilities(&candidate->_gate, NULL, failure) == StelaEncoderOk) {
			os_unfair_lock_lock(&lock);
			serving = serving ?: candidate;
			device = serving;
			os_unfair_lock_unlock(&lock);
			return &device->_gate;
		}
	}
	return NULL;
}

static StelaEncoderStatus capabilities(StelaEncoderCapabilities *out, char *reason, size_t reason_bytes)
{
	StelaFailure failure = {};
	StelaGate *gate = serving_gate(&failure);
	StelaEncoderStatus status = gate ? ask_capabilities(gate, out, &failure) : (StelaEncoderStatus)failure.status;
	if (status != StelaEncoderOk) {
		copy_reason(reason, reason_bytes, failure.reason);
	}
	return status;
}

static StelaEncoderStatus open_session(const StelaEncoderConfig *config, void *sets, size_t sets_capacity,
				       size_t *sets_length, uint64_t *max_frame_bytes, StelaEncoderSession **session,
				       char *reason, size_t reason_bytes)
{
	StelaFailure failure = {};
	StelaGate *gate = serving_gate(&failure);
	if (!gate) {
		copy_reason(reason, reason_bytes, failure.reason);
		return (StelaEncoderStatus)failure.status;
	}
	StelaEncoderSession *made = calloc(1, sizeof(*made));
	if (!made) {
		copy_reason(reason, reason_bytes, "out of memory");
		return StelaEncoderNoMemory;
	}
	StelaEncoderOpenRequest request = {
		.codec = config->codec,
		.width = config->width,
		.height = config->height,
		.fps_milli = config->fps_milli,
		.rate = config->rate,
		.bitrate = config->bitrate,
		.qp = config->qp,
		.flags = 0,
		.parameter_sets = stela_span(sets, sets_capacity),
	};
	/* Names the open across its calls answered Pending. */
	while (request.token == 0) {
		arc4random_buf(&request.token, sizeof(request.token));
	}
	StelaEncoderOpenResponse response = {};
	StelaSpans spans = {.writes = &request.parameter_sets, .write_count = 1};
	uint32_t status = STELA_CALL_SPANS(gate, EncoderOpen, &request, &response, spans, &failure);
	if (status != StelaStatusOk) {
		free(made);
		copy_reason(reason, reason_bytes, failure.reason);
		return (StelaEncoderStatus)status;
	}
	*made = (StelaEncoderSession){.gate = gate, .encoder = response.encoder, .codec = config->codec};
	*sets_length = response.parameter_sets_length;
	*max_frame_bytes = response.max_frame_bytes;
	*session = made;
	return StelaEncoderOk;
}

static StelaEncoderStatus encode(StelaEncoderSession *session, const StelaEncoderFrame *frame,
				 StelaEncoderResult *result, char *reason, size_t reason_bytes)
{
	StelaEncoderEncodeRequest request = {
		.encoder = session->encoder,
		.frame = frame->frame,
		.luma = stela_span(frame->luma, frame->luma_bytes),
		.chroma = stela_span(frame->chroma, frame->chroma_bytes),
		.luma_pitch = frame->luma_pitch,
		.chroma_pitch = frame->chroma_pitch,
		.input = frame->input,
		.flags = frame->force_idr ? STELA_ENCODE_FORCE_IDR : 0,
		.bitrate = frame->bitrate,
		.out = stela_span(frame->out, frame->out_bytes),
	};
	/* The picture is read and the output written by the host: their pages
	 * are mapped before each try. */
	StelaSpan reads[2] = {request.luma, request.chroma};
	StelaSpans spans = {
		.reads = reads,
		.read_count = frame->chroma_bytes ? 2 : 1,
		.writes = &request.out,
		.write_count = 1,
	};
	StelaEncoderEncodeResponse response = {};
	StelaFailure failure = {};
	uint32_t status = STELA_CALL_SPANS(session->gate, EncoderEncode, &request, &response, spans, &failure);
	if (status != StelaStatusOk) {
		copy_reason(reason, reason_bytes, failure.reason);
		return (StelaEncoderStatus)status;
	}
	*result = (StelaEncoderResult){
		.bytes = response.bytes,
		.idr = response.idr,
		.picture_index = response.picture_index,
		.error_status = response.error_status,
		.average_qp = response.average_qp,
		.convert_us = response.convert_us,
		.gpu_us = response.gpu_us,
	};
	return StelaEncoderOk;
}

static void close_session(StelaEncoderSession *session)
{
	if (!session) {
		return;
	}
	StelaHandle handle = {.handle = session->encoder};
	StelaEmpty empty;
	STELA_CALL(session->gate, EncoderClose, &handle, &empty, NULL);
	free(session);
}

static const StelaEncoderAPI api = {
	.version = STELA_ENCODER_API_VERSION,
	.capabilities = capabilities,
	.open = open_session,
	.encode = encode,
	.close = close_session,
};

const StelaEncoderAPI *stela_encoder_api(void)
{
	return &api;
}

/* The table by class, for a StelaVT this plugin's constructor did not load,
 * such as one a test registers in its own process: the Objective-C runtime
 * finds a class of any image, where dlsym finds nothing of this one. */
@interface StelaGPUEncoders : NSObject
+ (const StelaEncoderAPI *)api;
@end

@implementation StelaGPUEncoders
+ (const StelaEncoderAPI *)api
{
	return &api;
}
@end
