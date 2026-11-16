/*
 * Host tests of stela_pcm.h, which the kext compiles unchanged: the
 * sample path StelaVirtIOSound's engine clips through, with a C model of
 * IOAudioFamily's IOAF_Float32ToNativeInt16 as its converter, and the
 * sample buffer's layout as the TX transfers read it. Prints "ok" when
 * every check passes.
 */
#include <math.h>
#include <stdio.h>
#include <string.h>

#include "../stela_pcm.h"

static int failures;

#define CHECK(condition)                                                       \
	do {                                                                   \
		if (!(condition)) {                                            \
			printf("%s:%d: %s\n", __FILE__, __LINE__, #condition); \
			failures++;                                            \
		}                                                              \
	} while (0)

/*
 * IOAF_Float32ToNativeInt16 as 26B123 has it (IOAudioFamily's
 * Float32ToNativeInt16): x x 2^31 + 32768, rounded toward minus infinity
 * and saturated to 32 bits, then its top 16 bits.
 */
static void
model_convert(const float *src, int16_t *dst, uint32_t count)
{
	for (uint32_t index = 0; index < count; index++) {
		double value = floor((double)src[index] * 2147483648.0 + 32768.0);
		if (value > 2147483647.0) {
			value = 2147483647.0;
		} else if (value < -2147483648.0) {
			value = -2147483648.0;
		}
		int32_t wide = (int32_t)value;
		dst[index] = (int16_t)(wide >> 16);
	}
}

static uint32_t
bits_of(float value)
{
	uint32_t bits;
	memcpy(&bits, &value, sizeof bits);
	return bits;
}

static void
test_model(void)
{
	float in[] = {0.0f, 0.5f, -0.5f, 1.0f, -1.0f, 2.0f, -2.0f, 1.0f / 32768.0f, -1.0f / 32768.0f};
	int16_t want[] = {0, 16384, -16384, 32767, -32768, 32767, -32768, 1, -1};
	int16_t out[9];
	model_convert(in, out, 9);
	for (int index = 0; index < 9; index++) {
		CHECK(out[index] == want[index]);
	}
}

static void
test_float_peak(void)
{
	CHECK(stela_pcm_float_peak(bits_of(0.0f)) == 0);
	CHECK(stela_pcm_float_peak(bits_of(-0.0f)) == 0);
	CHECK(stela_pcm_float_peak(bits_of(1.0f)) == 32768);
	CHECK(stela_pcm_float_peak(bits_of(-1.0f)) == 32768);
	CHECK(stela_pcm_float_peak(bits_of(0.5f)) == 16384);
	CHECK(stela_pcm_float_peak(bits_of(-0.25f)) == 8192);
	CHECK(stela_pcm_float_peak(bits_of(2.0f)) == 65536);
	CHECK(stela_pcm_float_peak(bits_of(1.0f / 32768.0f)) == 1);
	CHECK(stela_pcm_float_peak(bits_of(0.75f / 32768.0f)) == 0);
	CHECK(stela_pcm_float_peak(bits_of(1e-30f)) == 0);
	CHECK(stela_pcm_float_peak(bits_of(65535.0f)) == 65535u * 32768u);
	CHECK(stela_pcm_float_peak(bits_of(131072.0f)) == UINT32_MAX);
	CHECK(stela_pcm_float_peak(bits_of(1e10f)) == UINT32_MAX);
	CHECK(stela_pcm_float_peak(bits_of(INFINITY)) == UINT32_MAX);
	CHECK(stela_pcm_float_peak(bits_of(NAN)) == UINT32_MAX);
	/* Against the arithmetic, over a sweep of magnitudes. */
	for (float value = 1e-6f; value < 100000.0f; value *= 1.37f) {
		double want = floor((double)value * 32768.0);
		CHECK(stela_pcm_float_peak(bits_of(value)) == (uint32_t)want);
		CHECK(stela_pcm_float_peak(bits_of(-value)) == (uint32_t)want);
	}
}

static void
test_peaks(void)
{
	float mix[] = {0.1f, -0.7f, 0.3f, 0.0f};
	CHECK(stela_pcm_mix_peak(mix, 4) == (uint32_t)floor(0.7f * 32768.0));
	CHECK(stela_pcm_mix_peak(mix, 0) == 0);
	int16_t samples[] = {5, -32768, 32767, 0};
	CHECK(stela_pcm_s16_peak(samples, 4) == 32768);
	CHECK(stela_pcm_s16_peak(samples + 2, 2) == 32767);
	CHECK(stela_pcm_s16_peak(samples, 0) == 0);
}

static void
test_gain(void)
{
	CHECK(stela_pcm_gain(STELA_PCM_VOLUME_STEPS) == STELA_PCM_UNITY);
	CHECK(stela_pcm_gain(-1) == stela_pcm_gain(0));
	CHECK(stela_pcm_gain(1000) == STELA_PCM_UNITY);
	for (int32_t step = 0; step <= STELA_PCM_VOLUME_STEPS; step++) {
		double want = 65536.0 * pow(10.0, (step - STELA_PCM_VOLUME_STEPS) / 20.0);
		double got = stela_pcm_gain(step);
		CHECK(fabs(got - want) <= 1.0);
		if (step > 0) {
			CHECK(stela_pcm_gain(step) > stela_pcm_gain(step - 1));
		}
	}
	int16_t samples[] = {16384, -16384, 32767, -32768};
	stela_pcm_apply_gain(samples, 4, STELA_PCM_UNITY);
	CHECK(samples[0] == 16384 && samples[3] == -32768);
	stela_pcm_apply_gain(samples, 4, STELA_PCM_UNITY / 2);
	CHECK(samples[0] == 8192 && samples[1] == -8192 && samples[2] == 16383 && samples[3] == -16384);
}

/* A mix buffer and a sample buffer as the stream has them, with guards past each end. */
static float mix[STELA_PCM_BUFFER_FRAMES * STELA_PCM_CHANNELS + 8];
static int16_t sample[STELA_PCM_BUFFER_FRAMES * STELA_PCM_CHANNELS + 8];

static void
fill(float value)
{
	for (uint32_t index = 0; index < sizeof mix / sizeof mix[0]; index++) {
		mix[index] = value;
	}
	for (uint32_t index = 0; index < sizeof sample / sizeof sample[0]; index++) {
		sample[index] = 0x5a5a;
	}
}

static void
test_clip(void)
{
	const uint32_t samples = STELA_PCM_BUFFER_FRAMES * STELA_PCM_CHANNELS;

	/* Full volume: the converter's values, only in the range. */
	fill(0.5f);
	mix[10 * 2] = 1.0f;
	mix[10 * 2 + 1] = -1.0f;
	CHECK(stela_pcm_clip(mix, sample, 10, 100, STELA_PCM_UNITY, 0, model_convert) == 32768);
	CHECK(sample[10 * 2] == 32767 && sample[10 * 2 + 1] == -32768);
	CHECK(sample[11 * 2] == 16384 && sample[109 * 2 + 1] == 16384);
	CHECK(sample[10 * 2 - 1] == 0x5a5a && sample[110 * 2] == 0x5a5a);

	/* -6 dB (step 58): half, near enough. */
	fill(0.5f);
	CHECK(stela_pcm_clip(mix, sample, 0, 1, stela_pcm_gain(58), 0, model_convert) == 16384);
	CHECK(sample[0] == (16384 * 32846) >> 16 && sample[1] == sample[0]);

	/* The lowest step is quiet, but not silent. */
	fill(0.5f);
	stela_pcm_clip(mix, sample, 0, 1, stela_pcm_gain(0), 0, model_convert);
	CHECK(sample[0] == 10);

	/* Muted: zeros, and the mix still metered. */
	fill(0.5f);
	CHECK(stela_pcm_clip(mix, sample, 5, 3, STELA_PCM_UNITY, 1, model_convert) == 16384);
	CHECK(sample[5 * 2] == 0 && sample[7 * 2 + 1] == 0 && sample[8 * 2] == 0x5a5a && sample[5 * 2 - 1] == 0x5a5a);

	/* Silence in, silence out: what K5's boot read, from a mix never clipped. */
	fill(0.0f);
	CHECK(stela_pcm_clip(mix, sample, 0, STELA_PCM_BUFFER_FRAMES, STELA_PCM_UNITY, 0, model_convert) == 0);
	CHECK(stela_pcm_s16_peak(sample, samples) == 0);

	/* The whole buffer, and up to its end; nothing past it. */
	fill(-0.25f);
	CHECK(stela_pcm_clip(mix, sample, 0, STELA_PCM_BUFFER_FRAMES, STELA_PCM_UNITY, 0, model_convert) == 8192);
	CHECK(sample[0] == -8192 && sample[samples - 1] == -8192 && sample[samples] == 0x5a5a);
	fill(0.25f);
	CHECK(stela_pcm_clip(mix, sample, STELA_PCM_BUFFER_FRAMES - 1, 1, STELA_PCM_UNITY, 0, model_convert) == 8192);
	CHECK(sample[samples - 1] == 8192 && sample[samples] == 0x5a5a);
	CHECK(stela_pcm_clip(mix, sample, STELA_PCM_BUFFER_FRAMES, 0, STELA_PCM_UNITY, 0, model_convert) == 0);
	CHECK(stela_pcm_clip(mix, sample, STELA_PCM_BUFFER_FRAMES, 1, STELA_PCM_UNITY, 0, model_convert) == -1);
	CHECK(stela_pcm_clip(mix, sample, 1, STELA_PCM_BUFFER_FRAMES, STELA_PCM_UNITY, 0, model_convert) == -1);
	CHECK(stela_pcm_clip(mix, sample, STELA_PCM_BUFFER_FRAMES + 1, 0, STELA_PCM_UNITY, 0, model_convert) == -1);
	CHECK(sample[samples] == 0x5a5a);
}

/*
 * The buffer as the TX transfers read it: the periods tile the sample
 * buffer from its start to its end, and the bytes clip writes for frame f
 * are the bytes of period f / PERIOD_FRAMES the device reads for it.
 */
static void
test_layout(void)
{
	CHECK(STELA_PCM_FRAME_BYTES == STELA_PCM_CHANNELS * sizeof(int16_t));
	CHECK(STELA_PCM_PERIOD_BYTES == 1920 && STELA_PCM_BUFFER_BYTES == 15360);
	CHECK(STELA_PCM_BUFFER_BYTES <= 0x4000);
	uint32_t end = 0;
	for (uint32_t period = 0; period < STELA_PCM_PERIODS; period++) {
		CHECK(stela_pcm_period_offset(period) == end);
		end = stela_pcm_period_offset(period) + STELA_PCM_PERIOD_BYTES;
	}
	CHECK(end == STELA_PCM_BUFFER_BYTES);
	fill(0.0f);
	for (uint32_t frame = 0; frame < STELA_PCM_BUFFER_FRAMES; frame++) {
		mix[frame * 2] = (float)frame / 32768.0f;
		mix[frame * 2 + 1] = -(float)frame / 32768.0f;
	}
	stela_pcm_clip(mix, sample, 0, STELA_PCM_BUFFER_FRAMES, STELA_PCM_UNITY, 0, model_convert);
	const uint8_t *bytes = (const uint8_t *)sample;
	for (uint32_t frame = 0; frame < STELA_PCM_BUFFER_FRAMES; frame++) {
		uint32_t period = frame / STELA_PCM_PERIOD_FRAMES;
		uint32_t at = stela_pcm_period_offset(period) +
		    (frame % STELA_PCM_PERIOD_FRAMES) * STELA_PCM_FRAME_BYTES;
		CHECK(at == stela_pcm_frame_offset(frame));
		int16_t left, right;
		memcpy(&left, bytes + at, 2);
		memcpy(&right, bytes + at + 2, 2);
		CHECK(left == (int16_t)frame && right == -(int16_t)frame);
	}
}

int
main(void)
{
	test_model();
	test_float_peak();
	test_peaks();
	test_gain();
	test_clip();
	test_layout();
	if (failures) {
		return 1;
	}
	printf("ok\n");
	return 0;
}
