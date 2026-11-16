/*
 * StelaVirtIOSound's sample path in plain C99, with no IOKit and no
 * floating-point arithmetic, so the kext compiles it unchanged and the
 * host's tests run it (tests/pcm.c, through xtask's
 * guest::virtio_tests::the_kext_sample_path_passes_its_host_tests).
 *
 * The sample buffer is STELA_PCM_PERIODS periods of STELA_PCM_PERIOD_FRAMES
 * frames, each frame STELA_PCM_CHANNELS little-endian S16 samples. The
 * device reads period p, one TX transfer, at byte stela_pcm_period_offset(p)
 * of it. IOAudioFamily mixes its clients into a float mix buffer laid out
 * the same way, one float a sample, full scale +-1.0, and has the engine's
 * clipOutputSamples bring a range of frames into the sample buffer: that is
 * stela_pcm_clip. Its float to S16 conversion is the caller's: the kext
 * passes IOAudioFamily's own IOAF_Float32ToNativeInt16 (NEON, saturating),
 * the host tests a C model of it. Then the volume, as a 16.16 gain, or
 * silence while muted.
 *
 * The meters: the mix's peak is read from the floats' bits, as |x| x 32768
 * (full scale is 32768), and the sample buffer's as the largest |sample|.
 */
#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The stream, as SET_PARAMS sets it: 48 kHz stereo S16, 8 periods of 10 ms. */
#define STELA_PCM_RATE 48000u
#define STELA_PCM_CHANNELS 2u
#define STELA_PCM_FRAME_BYTES 4u
#define STELA_PCM_PERIOD_FRAMES 480u
#define STELA_PCM_PERIODS 8u
#define STELA_PCM_BUFFER_FRAMES (STELA_PCM_PERIOD_FRAMES * STELA_PCM_PERIODS)
#define STELA_PCM_PERIOD_BYTES (STELA_PCM_PERIOD_FRAMES * STELA_PCM_FRAME_BYTES)
#define STELA_PCM_BUFFER_BYTES (STELA_PCM_BUFFER_FRAMES * STELA_PCM_FRAME_BYTES)

/* The volume: steps 0 to STELA_PCM_VOLUME_STEPS, a dB each, the top 0 dB. */
#define STELA_PCM_VOLUME_STEPS 64
/* A gain of one, 16.16. */
#define STELA_PCM_UNITY 0x10000u

/* Converts `count` floats to S16, saturating: IOAF_Float32ToNativeInt16's shape. */
typedef void (*stela_pcm_convert)(const float *src, int16_t *dst, uint32_t count);

/* The byte offset of frame `frame` in the sample buffer. */
static inline uint32_t
stela_pcm_frame_offset(uint32_t frame)
{
	return frame * STELA_PCM_FRAME_BYTES;
}

/* The byte offset of period `period`: the TX transfer's frames for it. */
static inline uint32_t
stela_pcm_period_offset(uint32_t period)
{
	return stela_pcm_frame_offset(period * STELA_PCM_PERIOD_FRAMES);
}

/* Step `step`'s gain, 16.16: 65536 x 10^((step - 64) / 20), the step clamped. */
static inline uint32_t
stela_pcm_gain(int32_t step)
{
	static const uint32_t gains[STELA_PCM_VOLUME_STEPS + 1] = {
	    41, 46, 52, 58, 66, 74, 83, 93,
	    104, 117, 131, 147, 165, 185, 207, 233,
	    261, 293, 328, 369, 414, 464, 521, 584,
	    655, 735, 825, 926, 1039, 1165, 1308, 1467,
	    1646, 1847, 2072, 2325, 2609, 2927, 3285, 3685,
	    4135, 4640, 5206, 5841, 6554, 7353, 8250, 9257,
	    10387, 11654, 13076, 14672, 16462, 18471, 20724, 23253,
	    26090, 29274, 32846, 36854, 41350, 46396, 52057, 58409,
	    65536,
	};
	if (step < 0) {
		step = 0;
	} else if (step > STELA_PCM_VOLUME_STEPS) {
		step = STELA_PCM_VOLUME_STEPS;
	}
	return gains[step];
}

/*
 * |x| x 32768 of the IEEE single whose bits are `bits`, truncated:
 * 32768 for full scale, UINT32_MAX for an infinity, a NaN or anything at
 * or past 2^17, 0 below 2^-15.
 */
static inline uint32_t
stela_pcm_float_peak(uint32_t bits)
{
	uint32_t magnitude = bits & 0x7fffffffu;
	uint32_t exponent = magnitude >> 23;
	if (exponent == 0xff) {
		return UINT32_MAX;
	}
	if (exponent == 0) {
		return 0;
	}
	/* |x| = mantissa x 2^(exponent - 150), so |x| x 2^15 = mantissa x 2^(exponent - 135). */
	uint32_t mantissa = (magnitude & 0x7fffffu) | 0x800000u;
	int32_t shift = (int32_t)exponent - 135;
	if (shift >= 9) {
		return UINT32_MAX;
	}
	if (shift >= 0) {
		return mantissa << shift;
	}
	if (shift <= -24) {
		return 0;
	}
	return mantissa >> -shift;
}

/* The mix's peak over `count` floats, as stela_pcm_float_peak. */
static inline uint32_t
stela_pcm_mix_peak(const float *mix, uint32_t count)
{
	uint32_t largest = 0;
	for (uint32_t index = 0; index < count; index++) {
		uint32_t bits;
		__builtin_memcpy(&bits, mix + index, sizeof bits);
		/* A non-negative float's bits order as the float does; NaNs sort above infinity. */
		bits &= 0x7fffffffu;
		if (bits > largest) {
			largest = bits;
		}
	}
	return stela_pcm_float_peak(largest);
}

/* The largest |sample| of `count` S16 samples: 32768 for -32768. */
static inline uint32_t
stela_pcm_s16_peak(const int16_t *samples, uint32_t count)
{
	uint32_t largest = 0;
	for (uint32_t index = 0; index < count; index++) {
		int32_t sample = samples[index];
		uint32_t magnitude = (uint32_t)(sample < 0 ? -sample : sample);
		if (magnitude > largest) {
			largest = magnitude;
		}
	}
	return largest;
}

/* The volume on `count` S16 samples in place: `gain` 16.16, at most unity. */
static inline void
stela_pcm_apply_gain(int16_t *samples, uint32_t count, uint32_t gain)
{
	if (gain >= STELA_PCM_UNITY) {
		return;
	}
	for (uint32_t index = 0; index < count; index++) {
		samples[index] = (int16_t)(((int32_t)samples[index] * (int32_t)gain) >> 16);
	}
}

/*
 * The engine's clipOutputSamples: frames [first, first + frames) of the
 * float mix buffer `mix` into the same frames of the S16 sample buffer
 * `sample`, both STELA_PCM_BUFFER_FRAMES long, converted by `convert`,
 * then at `gain` (16.16), or zeros while `muted`. Returns the mix's peak
 * over the range (stela_pcm_float_peak), or -1 with nothing written for a
 * range past the buffer's end.
 */
static inline int64_t
stela_pcm_clip(const float *mix, int16_t *sample, uint32_t first, uint32_t frames, uint32_t gain, int muted,
    stela_pcm_convert convert)
{
	if (first > STELA_PCM_BUFFER_FRAMES || frames > STELA_PCM_BUFFER_FRAMES - first) {
		return -1;
	}
	const uint32_t count = frames * STELA_PCM_CHANNELS;
	const float *in = mix + first * STELA_PCM_CHANNELS;
	int16_t *out = sample + first * STELA_PCM_CHANNELS;
	uint32_t peak = stela_pcm_mix_peak(in, count);
	if (muted) {
		for (uint32_t index = 0; index < count; index++) {
			out[index] = 0;
		}
		return peak;
	}
	convert(in, out, count);
	stela_pcm_apply_gain(out, count, gain);
	return peak;
}

#ifdef __cplusplus
}
#endif
