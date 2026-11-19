/*
 * The host's video decoder for StelaVD (stelavt/StelaDecoder.h): NVDEC
 * sessions through the gate of a Metal device this process opened, as
 * Encoder.m gives StelaVT the host's NVENC.
 *
 * Each device notes itself here once its gate negotiated
 * (`stela_decoder_note_device`). The first device whose host answers
 * DecoderCapabilities serves every session of the process. A process where
 * no device opened, or whose host serves no decoder (`--no-nvdec`, no
 * NVDEC), has none: StelaVD then decodes with Apple's software decoder.
 *
 * A decode's call carries the sample (read by the host), the destination
 * records (read and written) and each destination's planes (written): the
 * gate call maps their pages before each try, and makes the call again,
 * after the wait its response asks, while the host answers Pending.
 */
#include "Bundle.h"

struct StelaDecoderSession {
	StelaGate *gate;
	uint64_t decoder;
	uint32_t codec;
};

static os_unfair_lock lock = OS_UNFAIR_LOCK_INIT;
/* The devices that noted themselves, oldest first, and the one whose host
 * serves the decoder, once one answered. */
static NSMutableArray *noted;
static StelaGPUDevice *serving;

static void copy_reason(char *reason, size_t bytes, const char *text)
{
	if (reason && bytes) {
		strlcpy(reason, text, bytes);
	}
}

static StelaDecoderStatus ask_capabilities(StelaGate *gate, StelaDecoderCapabilities *out, StelaFailure *failure)
{
	StelaEmpty nothing = {};
	StelaDecoderCapabilitiesResponse response = {};
	uint32_t status = STELA_CALL(gate, DecoderCapabilities, &nothing, &response, failure);
	if (status == StelaStatusOk && out) {
		*out = (StelaDecoderCapabilities){
			.codecs = response.codecs,
			.profiles_h264 = response.profiles_h264,
			.profiles_hevc = response.profiles_hevc,
			.depths = response.depths,
			.formats = response.formats,
			.max_sessions = response.max_sessions,
			.open_sessions = response.open_sessions,
			.max_width = response.max_width,
			.max_height = response.max_height,
			.min_width = response.min_width,
			.min_height = response.min_height,
			.max_destinations = response.max_destinations,
		};
	}
	return (StelaDecoderStatus)status;
}

void stela_decoder_note_device(StelaGPUDevice *device)
{
	os_unfair_lock_lock(&lock);
	noted = noted ?: [NSMutableArray arrayWithCapacity:2];
	[noted addObject:device];
	os_unfair_lock_unlock(&lock);
}

/* The gate of the device serving the decoder, or NULL with `failure` set. */
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
	*failure = (StelaFailure){.status = StelaDecoderNoGate};
	if (candidates.count == 0) {
		snprintf(failure->reason, sizeof(failure->reason), "no StelaGPU device opened in %s[%d]: %s",
			 getprogname(), getpid(),
			 stela_device_open_failure[0] ? stela_device_open_failure : "Metal opened none");
		return NULL;
	}
	for (NSUInteger i = 0; i < candidates.count; i++) {
		StelaGPUDevice *candidate = candidates[i];
		if (ask_capabilities(&candidate->_gate, NULL, failure) == StelaDecoderOk) {
			os_unfair_lock_lock(&lock);
			serving = serving ?: candidate;
			device = serving;
			os_unfair_lock_unlock(&lock);
			return &device->_gate;
		}
	}
	return NULL;
}

static StelaDecoderStatus capabilities(StelaDecoderCapabilities *out, char *reason, size_t reason_bytes)
{
	StelaFailure failure = {};
	StelaGate *gate = serving_gate(&failure);
	StelaDecoderStatus status = gate ? ask_capabilities(gate, out, &failure) : (StelaDecoderStatus)failure.status;
	if (status != StelaDecoderOk) {
		copy_reason(reason, reason_bytes, failure.reason);
	}
	return status;
}

static StelaDecoderStatus open_session(uint32_t codec, const void *config, size_t config_bytes,
				       StelaDecoderStream *stream, StelaDecoderSession **session, char *reason,
				       size_t reason_bytes)
{
	StelaFailure failure = {};
	StelaGate *gate = serving_gate(&failure);
	if (!gate) {
		copy_reason(reason, reason_bytes, failure.reason);
		return (StelaDecoderStatus)failure.status;
	}
	StelaDecoderSession *made = calloc(1, sizeof(*made));
	if (!made) {
		copy_reason(reason, reason_bytes, "out of memory");
		return StelaDecoderNoMemory;
	}
	StelaDecoderOpenRequest request = {
		.codec = codec,
		.flags = 0,
		.config = stela_span(config, config_bytes),
	};
	/* Names the open across its calls answered Pending. */
	while (request.token == 0) {
		arc4random_buf(&request.token, sizeof(request.token));
	}
	StelaDecoderOpenResponse response = {};
	StelaSpans spans = {.reads = &request.config, .read_count = 1};
	uint32_t status = STELA_CALL_SPANS(gate, DecoderOpen, &request, &response, spans, &failure);
	if (status != StelaStatusOk) {
		free(made);
		copy_reason(reason, reason_bytes, failure.reason);
		return (StelaDecoderStatus)status;
	}
	*made = (StelaDecoderSession){.gate = gate, .decoder = response.decoder, .codec = codec};
	*stream = (StelaDecoderStream){
		.coded_width = response.coded_width,
		.coded_height = response.coded_height,
		.display_width = response.display_width,
		.display_height = response.display_height,
		.bit_depth = response.bit_depth,
		.format = response.format,
		.slots = response.slots,
		.surfaces = response.surfaces,
	};
	*session = made;
	return StelaDecoderOk;
}

static StelaDecoderStatus decode(StelaDecoderSession *session, StelaDecoderSample *sample,
				 StelaDecoderResult *result, char *reason, size_t reason_bytes)
{
	uint32_t count = sample->count;
	if (count > STELA_DECODER_MAX_DESTINATIONS) {
		copy_reason(reason, reason_bytes, "more destinations than a decode takes");
		return StelaDecoderInvalid;
	}
	/* The destination records as the gate reads and writes them, and the
	 * spans the host writes: the records, then each plane. */
	StelaDecoderPlanes planes[STELA_DECODER_MAX_DESTINATIONS];
	StelaSpan writes[1 + 2 * STELA_DECODER_MAX_DESTINATIONS];
	size_t write_count = 0;
	if (count > 0) {
		writes[write_count++] = stela_span(planes, count * sizeof(planes[0]));
	}
	for (uint32_t i = 0; i < count; i++) {
		const StelaDecoderDestination *given = &sample->destinations[i];
		planes[i] = (StelaDecoderPlanes){
			.luma = stela_span(given->luma, given->luma_bytes),
			.chroma = stela_span(given->chroma, given->chroma_bytes),
			.luma_pitch = given->luma_pitch,
			.chroma_pitch = given->chroma_pitch,
		};
		writes[write_count++] = planes[i].luma;
		writes[write_count++] = planes[i].chroma;
	}
	StelaDecoderDecodeRequest request = {
		.decoder = session->decoder,
		.frame = sample->frame,
		.sample = stela_span(sample->sample, sample->sample_bytes),
		.destinations = stela_span(count ? planes : NULL, count * sizeof(planes[0])),
		.count = count,
		.flags = sample->flags,
	};
	StelaSpans spans = {
		.reads = &request.sample,
		.read_count = sample->sample_bytes ? 1 : 0,
		.writes = writes,
		.write_count = write_count,
	};
	StelaDecoderDecodeResponse response = {};
	StelaFailure failure = {};
	uint32_t status = STELA_CALL_SPANS(session->gate, DecoderDecode, &request, &response, spans, &failure);
	if (status != StelaStatusOk) {
		copy_reason(reason, reason_bytes, failure.reason);
		return (StelaDecoderStatus)status;
	}
	for (uint32_t i = 0; i < count; i++) {
		StelaDecoderDestination *given = &sample->destinations[i];
		given->frame = planes[i].frame;
		given->poc = planes[i].poc;
		given->flags = i < response.outputs ? planes[i].flags : 0;
	}
	*result = (StelaDecoderResult){
		.taken = response.taken,
		.decoded = response.decoded,
		.outputs = response.outputs,
		.queued = response.queued,
		.mbs_in_error = response.mbs_in_error,
		.error_status = response.error_status,
		.gpu_us = response.gpu_us,
		.ce_us = response.ce_us,
		.copy_us = response.copy_us,
		.calls = response.calls,
	};
	return StelaDecoderOk;
}

static void close_session(StelaDecoderSession *session)
{
	if (!session) {
		return;
	}
	StelaHandle handle = {.handle = session->decoder};
	StelaEmpty empty;
	STELA_CALL(session->gate, DecoderClose, &handle, &empty, NULL);
	free(session);
}

static const StelaDecoderAPI api = {
	.version = STELA_DECODER_API_VERSION,
	.capabilities = capabilities,
	.open = open_session,
	.decode = decode,
	.close = close_session,
};

const StelaDecoderAPI *stela_decoder_api(void)
{
	return &api;
}

/* The table by class, for a StelaVT this plugin's constructor did not load,
 * as Encoder.m's StelaGPUEncoders. */
@interface StelaGPUDecoders : NSObject
+ (const StelaDecoderAPI *)api;
@end

@implementation StelaGPUDecoders
+ (const StelaDecoderAPI *)api
{
	return &api;
}
@end
