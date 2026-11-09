/*
 * Native Huawei L2HC (Low-Latency High-Definition Codec) Audio Encoder
 * Clean-room implementation in C reverse-engineered via Ghidra.
 *
 * SPDX-License-Identifier: MIT
 */

#include "l2hc_native.h"
#include "l2hc_tables.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

#define L2HC_MAX_CHANNELS 2
#define L2HC_MAX_FRAME_SAMPLES 960
#define L2HC_NUM_BANDS 32

/* Bitstream writer matching L2HC 64-bit word format (big-endian WordsToBytes) */
typedef struct {
    uint64_t words[512];
    int word_idx;
    int bits_left;
    int total_bits;
} L2hcBitWriter;

static void bw_init(L2hcBitWriter *w) {
    memset(w->words, 0, sizeof(w->words));
    w->word_idx = 0;
    w->bits_left = 64;
    w->total_bits = 0;
}

static void bw_write(L2hcBitWriter *w, uint64_t val, int nbits) {
    if (nbits <= 0) return;
    w->total_bits += nbits;
    val &= (nbits == 64) ? ~0ULL : ((1ULL << nbits) - 1ULL);
    if (w->bits_left >= nbits) {
        w->bits_left -= nbits;
        w->words[w->word_idx] |= (val << w->bits_left);
    } else {
        int top = w->bits_left;
        int bot = nbits - top;
        w->words[w->word_idx] |= (val >> bot);
        w->word_idx++;
        w->bits_left = 64 - bot;
        w->words[w->word_idx] = (val << w->bits_left);
    }
}

static void bw_flush(L2hcBitWriter *w, uint8_t *out, int target_bytes) {
    int byte_idx = 0;
    for (int i = 0; byte_idx < target_bytes; i++) {
        uint64_t word = w->words[i];
        for (int b = 7; b >= 0 && byte_idx < target_bytes; b--) {
            out[byte_idx++] = (uint8_t)(word >> (b * 8));
        }
    }
}

/* Global MDCT precomputed tables */
static float *g_cos_table_240 = NULL;
static float *g_cos_table_480 = NULL;
static float *g_cos_table_960 = NULL;

static void ensure_cos_tables(void) {
    if (!g_cos_table_240) {
        float *tbl = (float *)malloc(240 * 480 * sizeof(float));
        const double factor = M_PI / 240.0;
        for (int k = 0; k < 240; k++) {
            const double k_term = ((double)k + 0.5) * factor;
            for (int n = 0; n < 480; n++) {
                double angle = ((double)n + 0.5 + 120.0) * k_term;
                tbl[k * 480 + n] = (float)cos(angle);
            }
        }
        g_cos_table_240 = tbl;
    }
    if (!g_cos_table_480) {
        float *tbl = (float *)malloc(480 * 960 * sizeof(float));
        const double factor = M_PI / 480.0;
        for (int k = 0; k < 480; k++) {
            const double k_term = ((double)k + 0.5) * factor;
            for (int n = 0; n < 960; n++) {
                double angle = ((double)n + 0.5 + 240.0) * k_term;
                tbl[k * 960 + n] = (float)cos(angle);
            }
        }
        g_cos_table_480 = tbl;
    }
    if (!g_cos_table_960) {
        float *tbl = (float *)malloc(960 * 1920 * sizeof(float));
        const double factor = M_PI / 960.0;
        for (int k = 0; k < 960; k++) {
            const double k_term = ((double)k + 0.5) * factor;
            for (int n = 0; n < 1920; n++) {
                double angle = ((double)n + 0.5 + 480.0) * k_term;
                tbl[k * 1920 + n] = (float)cos(angle);
            }
        }
        g_cos_table_960 = tbl;
    }
}

/*
 * MDCT normalization matches original libl2hc:
 * In dct4_apply: 1/sqrt(N/2)
 * In CodecTypeCheckGetMdctValues / Mdct_00132d40:
 * Net normalization factor matches reference libl2hc: sqrt(2.0 / N) = 1 / sqrt(N/2).
 */
static inline void compute_fast_mdct(const float *in_2n, float *out_n, int N) {
    const float *tbl = (N == 960) ? g_cos_table_960 : ((N == 240) ? g_cos_table_240 : g_cos_table_480);
    const int stride = 2 * N;
    const float norm = 2.0f / (float)N; /* Exact MDCT scale matches CodecTypeCheckGetMdctValues */
    for (int k = 0; k < N; k++) {
        const float *row = &tbl[k * stride];
        float sum = 0.0f;
#if defined(__GNUC__)
        #pragma GCC unroll 8
#endif
        for (int n = 0; n < stride; n++) {
            sum += in_2n[n] * row[n];
        }
        out_n[k] = sum * norm;
    }
}

typedef struct {
    uint32_t magic;
    uint32_t sample_rate;
    int16_t  bps;
    int16_t  channels;
    int16_t  frame_samples;
    int16_t  bitrate_kbps;

    int target_frame_bytes;
    int target_frame_bits;

    /* Overlap-add input history */
    float prev_samples[L2HC_MAX_CHANNELS][L2HC_MAX_FRAME_SAMPLES];

    /* Working buffers */
    float mdct_buf[L2HC_MAX_CHANNELS][L2HC_MAX_FRAME_SAMPLES];
    int32_t q_mdct[L2HC_MAX_CHANNELS][L2HC_MAX_FRAME_SAMPLES];
    int16_t q_sign[L2HC_MAX_CHANNELS][L2HC_MAX_FRAME_SAMPLES];
    int16_t scale_factors[L2HC_MAX_CHANNELS][L2HC_NUM_BANDS];
    int16_t bit_depths[L2HC_MAX_CHANNELS][L2HC_NUM_BANDS];
    int16_t quant_scale[L2HC_MAX_CHANNELS][L2HC_NUM_BANDS];

    int ms_flag;
    int global_gain;
    int frac_gain;
    int band_table_idx;
    int diff_sf_flag;
    int t1_idx[L2HC_MAX_CHANNELS];
    int t2_idx[L2HC_MAX_CHANNELS];
    int t4_idx[L2HC_MAX_CHANNELS];

    const float *window;
} L2hcEncoderState;

#define L2HC_MAGIC 0x49825349

int AudioL2hcEncGetSize(uint32_t *size) {
    if (!size) return -1;
    *size = (uint32_t)sizeof(L2hcEncoderState);
    return 0;
}

int AudioL2hcEncInit(void **encoder, void *buffer, uint32_t size) {
    if (!encoder || !buffer || size < sizeof(L2hcEncoderState)) {
        return -1;
    }
    ensure_cos_tables();

    L2hcEncoderState *st = (L2hcEncoderState *)buffer;
    memset(st, 0, sizeof(L2hcEncoderState));
    st->magic = L2HC_MAGIC;
    st->sample_rate = 48000;
    st->bps = 16;
    st->channels = 2;
    st->frame_samples = 480;
    st->bitrate_kbps = 320;
    st->target_frame_bytes = 400;
    st->target_frame_bits = 3200;
    st->window = MDCT_HRA_WINDOW_480_10MS;

    *encoder = st;
    return 0;
}

int AudioL2hcEncSetParam(void *encoder, const L2hcEncParam *param) {
    if (!encoder || !param) return -1;
    L2hcEncoderState *st = (L2hcEncoderState *)encoder;
    if (st->magic != L2HC_MAGIC) return -1;

    st->sample_rate = param->sample_rate;
    st->bps = param->bps;
    st->channels = param->channels;
    st->frame_samples = param->frame_samples;
    st->bitrate_kbps = param->bitrate_kbps;

    int frame_ms = 10;
    if (st->sample_rate > 0 && st->frame_samples > 0) {
        frame_ms = (int)((st->frame_samples * 1000 + (st->sample_rate / 2)) / st->sample_rate);
    }
    if (frame_ms < 5) frame_ms = 5;

    st->target_frame_bytes = (st->bitrate_kbps * frame_ms) / 8;
    st->target_frame_bits = st->target_frame_bytes * 8;

    if (st->frame_samples >= 960) {
        st->window = MDCT_HRA_WINDOW_960_10MS;
    } else if (st->frame_samples == 480) {
        if (st->sample_rate >= 88200) {
            st->window = MDCT_HRA_WINDOW_960_5MS;
        } else {
            st->window = MDCT_HRA_WINDOW_480_10MS;
        }
    } else if (st->frame_samples == 240) {
        st->window = MDCT_HRA_WINDOW_480_5MS;
    } else {
        st->window = MDCT_HRA_WINDOW_480_10MS;
    }

    return 0;
}

static inline const int16_t *get_active_band_table(const L2hcEncoderState *st) {
    if (st->frame_samples == 480) {
        return &BAND_480[st->band_table_idx * 33];
    } else if (st->frame_samples == 960) {
        return &BAND_960[st->band_table_idx * 33];
    } else if (st->frame_samples == 240) {
        return &BAND_240[st->band_table_idx * 33];
    } else if (st->frame_samples == 120) {
        return &BAND_120[st->band_table_idx * 33];
    }
    return &BAND_480[0];
}

/*
 * Dynamic Band Table Selection and Scale Factor Computation.
 * Matches UpdateBandIdAndSF_0013c254.c and UpdateBandIdAndSFGetTotalScale_0013c094.c:
 *   Evaluates tables 0..7 to find the table that minimizes total scale factor cost.
 *   For 48kHz full-band stereo, selects Table 4 (full 480 bins, 32 balanced bands).
 *   Range: [-8, 23]
 */
static void select_band_table_and_compute_sf(L2hcEncoderState *st) {
    const int N = st->frame_samples;
    const int16_t *band_tables = BAND_480;
    if (N == 960) band_tables = BAND_960;
    else if (N == 240) band_tables = BAND_240;
    else if (N == 120) band_tables = BAND_120;

    int best_t = 4;
    float min_cost = 1e30f;

    for (int t = 0; t < 8; t++) {
        const int16_t *tbl = &band_tables[t * 33];
        if (tbl[0] != 0 || tbl[32] != N) continue;
        float cost = 0.0f;
        for (int ch = 0; ch < st->channels; ch++) {
            const float *spec = st->mdct_buf[ch];
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                int start = tbl[b];
                int end = tbl[b + 1];
                float peak = 1e-12f;
                for (int k = start; k < end; k++) {
                    float a = fabsf(spec[k]);
                    if (a > peak) peak = a;
                }
                int sf = (int)(logf(peak) * 1.4426950408889634f);
                if (sf < -8) sf = -8;
                if (sf > 23) sf = 23;
                cost += (float)((end - start) * (sf > 0 ? sf : 0));
            }
        }
        if (cost < min_cost) {
            min_cost = cost;
            best_t = t;
        }
    }

    st->band_table_idx = best_t;
    const int16_t *tbl = &band_tables[best_t * 33];
    for (int ch = 0; ch < st->channels; ch++) {
        const float *spec = st->mdct_buf[ch];
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int start = tbl[b];
            int end = tbl[b + 1];
            float peak = 1e-12f;
            for (int k = start; k < end; k++) {
                float a = fabsf(spec[k]);
                if (a > peak) peak = a;
            }
            int sf = (int)(logf(peak) * 1.4426950408889634f);
            if (sf < -8) sf = -8;
            if (sf > 23) sf = 23;
            st->scale_factors[ch][b] = (int16_t)sf;
        }
    }

    /* Check if scale factors qualify for Huffman differential encoding (delta <= 6) */
    st->diff_sf_flag = 1;
    for (int ch = 0; ch < st->channels; ch++) {
        for (int b = 1; b < L2HC_NUM_BANDS; b++) {
            int delta = st->scale_factors[ch][b] - st->scale_factors[ch][b - 1];
            if (abs(delta) > 6) {
                st->diff_sf_flag = 0;
                return;
            }
        }
    }
}

/*
 * Quantize spectral bins.
 * Matches QuantMDCT_001340ec.c:
 *   step     = 2^global_gain (since bd > 0, psy_scale[b] = global_gain)
 *   inv_step = 1.0 / step = ldexpf(1.0, -global_gain)
 *   rounding = 0.375 for bd==1, 0.5 for bd>1
 *   q_val    = clamp((int)(fabsf(coeff) * inv_step + rounding), 0, max_val)
 *   sign     = 1 for positive, 0 for negative/zero
 */
static int count_bits_rough(const int16_t *bds, int num_bands, const int16_t *band_widths) {
    float loc[32];
    for (int i = 0; i < num_bands - 1; i++) {
        int b = (bds[i] >= 1) && (bds[i + 1] < 1);
        loc[i] = b ? 0.7f : 1.0f;
    }
    loc[num_bands - 1] = 1.0f;

    for (int i = 0; i < num_bands - 1; i++) {
        int b = (bds[i + 1] >= 1) && (bds[i] < 1);
        if (b) {
            loc[i + 1] = (float)(0.7 * (double)loc[i + 1]);
        }
    }

    int bits = 0;
    for (int i = 0; i < num_bands; i++) {
        int bd = bds[i];
        if (bd <= 0) continue;
        int w = band_widths[i];
        if (bd == 2) {
            double d15 = (double)(2 - 1) * 0.1 + 0.5;
            if ((double)loc[i] <= d15) d15 = (double)loc[i];
            bits += (int)((double)w * 1.9) + (int)((float)w * (float)d15);
        } else if (bd == 1) {
            double d15 = (double)(1 - 1) * 0.1 + 0.5;
            if ((double)loc[i] <= d15) d15 = (double)loc[i];
            bits += (int)((double)w * 0.96) + (int)((float)w * (float)d15);
        } else {
            double d15 = (double)(bd - 1) * 0.1 + 0.5;
            if ((double)loc[i] <= d15) d15 = (double)loc[i];
            bits += (int)((double)w * ((double)bd + 2.79 - 3.0)) + (int)((float)w * (float)d15);
        }
    }
    return bits;
}

static int count_frame_bits_frac(L2hcEncoderState *st, int gain, int fg) {
    const int16_t *tbl = get_active_band_table(st);
    int16_t widths[L2HC_NUM_BANDS];
    for (int b = 0; b < L2HC_NUM_BANDS; b++) {
        widths[b] = tbl[b + 1] - tbl[b];
    }

    int bits = 21; /* 7 header + 14 control bits */

    /* Scale factor bits */
    if (st->diff_sf_flag) {
        bits += st->channels * 5;
        for (int ch = 0; ch < st->channels; ch++) {
            for (int b = 1; b < L2HC_NUM_BANDS; b++) {
                int delta = abs(st->scale_factors[ch][b] - st->scale_factors[ch][b - 1]);
                if (delta > 6) delta = 6;
                int len = HUF_ENC_DIFF_SF[delta * 3 + 2];
                bits += len + (delta > 0 ? 1 : 0);
            }
        }
    } else {
        bits += st->channels * L2HC_NUM_BANDS * 5;
    }

    /* 6 bits per channel for table indices */
    bits += st->channels * 6;

    for (int ch = 0; ch < st->channels; ch++) {
        int16_t bds[L2HC_NUM_BANDS];
        int active_samples = 0;
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int bd = st->scale_factors[ch][b] - gain;
            if (bd < 0) bd = 0;
            bds[b] = (int16_t)bd;
            if (bd > 0) {
                active_samples += widths[b];
            }
        }

        /* Apply frac_gain demotion matching UpdatePsyQuantScaleQuater */
        if (fg > 0) {
            int demote_samples = (active_samples * fg) >> 3;
            for (int b = L2HC_NUM_BANDS - 1; b >= 0 && demote_samples > 0; b--) {
                if (bds[b] > 0) {
                    bds[b]--;
                    demote_samples -= widths[b];
                }
            }
        }

        bits += count_bits_rough(bds, L2HC_NUM_BANDS, widths);
    }
    return bits;
}



static void quantize_spectrum(L2hcEncoderState *st, int ch) {
    const int N = st->frame_samples;
    const float *spec = st->mdct_buf[ch];
    int32_t *q = st->q_mdct[ch];
    int16_t *sign = st->q_sign[ch];
    const int16_t *tbl = get_active_band_table(st);

    int active_samples = 0;
    for (int b = 0; b < L2HC_NUM_BANDS; b++) {
        int sf = st->scale_factors[ch][b];
        int bd = sf - st->global_gain;
        if (bd < 0) bd = 0;
        st->bit_depths[ch][b] = (int16_t)bd;
        st->quant_scale[ch][b] = (int16_t)st->global_gain;
        if (bd > 0) {
            active_samples += (tbl[b + 1] - tbl[b]);
        }
    }

    /* Apply frac_gain demotion (matches UpdatePsyQuantScaleQuater) */
    if (st->frac_gain > 0) {
        int demote_samples = (active_samples * st->frac_gain) >> 3;
        for (int b = L2HC_NUM_BANDS - 1; b >= 0 && demote_samples > 0; b--) {
            if (st->bit_depths[ch][b] > 0) {
                st->bit_depths[ch][b]--;
                st->quant_scale[ch][b]++;
                demote_samples -= (tbl[b + 1] - tbl[b]);
            }
        }
    }

    for (int b = 0; b < L2HC_NUM_BANDS; b++) {
        int start_bin = tbl[b];
        int end_bin = tbl[b + 1];
        if (start_bin >= N) break;
        if (end_bin > N) end_bin = N;

        int bd = st->bit_depths[ch][b];
        if (bd == 0) {
            for (int k = start_bin; k < end_bin; k++) {
                q[k] = 0;
                sign[k] = 0;
            }
            continue;
        }

        int max_val = (1 << bd) - 1;
        float inv_step = ldexpf(1.0f, -st->quant_scale[ch][b]);
        float rounding = (bd == 1) ? 0.375f : 0.5f;

        for (int k = start_bin; k < end_bin; k++) {
            float val = spec[k];
            int q_val = (int)(fabsf(val) * inv_step + rounding);
            if (q_val > max_val) q_val = max_val;
            q[k] = q_val;
            sign[k] = (val > 0.0f) ? 1 : 0;
        }
    }
}

/*
 * Exact Bitstream Packing matching:
 * 1. PackHeader (7 bits)
 * 2. PackMDCTBits (low_br: 1, ms_flag: 1, gain: 5, frac_gain: 3, band_table_idx: 3, diff_sf: 1)
 * 3. PackScaleFactor (Ch0 SF, Ch1 SF)
 * 4. PackMDCT table indices: 3 x 2 bits per channel (4tup, 2tup, 1tup) = 12 bits!
 * 5. PackMDCTHighBitRate:
 *    For each channel:
 *      Pass 1: Pack1TupThree MSB Huffman symbols across all bd>=3 bands
 *      Pass 2: Pack1TupThree LSB residual bits across all bd>=3 bands
 *      Pack2TupTwo
 *      Pack4TupOne
 *      Sign bits (1 bit per non-zero coeff)
 * 6. Zero padding to target_frame_bits
 */

static const int16_t *g_huf_1tup_tables[4] = {
    HUF_ENC_1TUP_THREE_1, HUF_ENC_1TUP_THREE_2, HUF_ENC_1TUP_THREE_3, HUF_ENC_1TUP_THREE_4
};
static const int16_t *g_huf_2tup_tables[4] = {
    HUF_ENC_2TUP_TWO_1, HUF_ENC_2TUP_TWO_2, HUF_ENC_2TUP_TWO_3, HUF_ENC_2TUP_TWO_4
};
static const int16_t *g_huf_4tup_tables[4] = {
    HUF_ENC_4TUP_ONE_1, HUF_ENC_4TUP_ONE_2, HUF_ENC_4TUP_ONE_3, HUF_ENC_4TUP_ONE_4
};

static int select_1tup_table(const L2hcEncoderState *st, int ch) {
    const int16_t *tbl = get_active_band_table(st);
    const int32_t *q = st->q_mdct[ch];
    int best_t = 0;
    int min_bits = 0x7fffffff;

    for (int t = 0; t < 4; t++) {
        const int16_t *htbl = g_huf_1tup_tables[t];
        int bits = 0;
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int bd = st->bit_depths[ch][b];
            if (bd < 3) continue;
            int start = tbl[b];
            int end = tbl[b + 1];
            int shift = bd - 3;
            for (int k = start; k < end; k++) {
                int msb = q[k] >> shift;
                if (msb > 7) msb = 7;
                bits += htbl[msb * 3 + 2];
            }
        }
        if (bits < min_bits) {
            min_bits = bits;
            best_t = t;
        }
    }
    return best_t;
}

static int select_2tup_table(const L2hcEncoderState *st, int ch) {
    const int16_t *tbl = get_active_band_table(st);
    const int32_t *q = st->q_mdct[ch];
    int best_t = 0;
    int min_bits = 0x7fffffff;

    for (int t = 0; t < 4; t++) {
        const int16_t *htbl = g_huf_2tup_tables[t];
        int bits = 0;
        int acc = 0, count = 0;
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            if (st->bit_depths[ch][b] != 2) continue;
            int start = tbl[b];
            int end = tbl[b + 1];
            for (int k = start; k < end; k++) {
                acc = (acc << 2) | (q[k] & 3);
                count++;
                if (count == 2) {
                    bits += htbl[acc * 3 + 2];
                    acc = 0;
                    count = 0;
                }
            }
        }
        if (count > 0) {
            bits += 2; /* 1 leftover sample: 2 raw bits */
        }
        if (bits < min_bits) {
            min_bits = bits;
            best_t = t;
        }
    }
    return best_t;
}

static int select_4tup_table(const L2hcEncoderState *st, int ch) {
    const int16_t *tbl = get_active_band_table(st);
    const int32_t *q = st->q_mdct[ch];
    int best_t = 0;
    int min_bits = 0x7fffffff;

    for (int t = 0; t < 4; t++) {
        const int16_t *htbl = g_huf_4tup_tables[t];
        int bits = 0;
        int acc = 0, count = 0;
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            if (st->bit_depths[ch][b] != 1) continue;
            int start = tbl[b];
            int end = tbl[b + 1];
            for (int k = start; k < end; k++) {
                acc = (acc << 1) | (q[k] & 1);
                count++;
                if (count == 4) {
                    bits += htbl[acc * 3 + 2];
                    acc = 0;
                    count = 0;
                }
            }
        }
        if (count > 0) {
            bits += count; /* leftover samples: count raw bits */
        }
        if (bits < min_bits) {
            min_bits = bits;
            best_t = t;
        }
    }
    return best_t;
}


static int count_frame_exact_bits(L2hcEncoderState *st) {
    const int16_t *tbl = get_active_band_table(st);
    int bits = 20 + (st->channels == 2 ? 1 : 0);

    /* Scale factors */
    if (st->diff_sf_flag) {
        for (int ch = 0; ch < st->channels; ch++) {
            bits += 5;
            for (int b = 1; b < L2HC_NUM_BANDS; b++) {
                int delta = st->scale_factors[ch][b] - st->scale_factors[ch][b - 1];
                int abs_d = abs(delta);
                if (abs_d > 6) abs_d = 6;
                bits += HUF_ENC_DIFF_SF[abs_d * 3 + 2];
                if (abs_d > 0) bits += 1;
            }
        }
    } else {
        bits += st->channels * L2HC_NUM_BANDS * 5;
    }

    /* Table selection indices: 6 bits per channel */
    bits += st->channels * 6;

    for (int ch = 0; ch < st->channels; ch++) {
        st->t1_idx[ch] = select_1tup_table(st, ch);
        st->t2_idx[ch] = select_2tup_table(st, ch);
        st->t4_idx[ch] = select_4tup_table(st, ch);

        const int32_t *q = st->q_mdct[ch];
        const int16_t *tbl1 = g_huf_1tup_tables[st->t1_idx[ch]];
        const int16_t *tbl2 = g_huf_2tup_tables[st->t2_idx[ch]];
        const int16_t *tbl4 = g_huf_4tup_tables[st->t4_idx[ch]];

        /* 5a. 1tup MSB */
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int bd = st->bit_depths[ch][b];
            if (bd < 3) continue;
            int start = tbl[b], end = tbl[b + 1], shift = bd - 3;
            for (int k = start; k < end; k++) {
                int msb = q[k] >> shift;
                if (msb > 7) msb = 7;
                bits += tbl1[msb * 3 + 2];
            }
        }

        /* 5b. 1tup LSB */
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int bd = st->bit_depths[ch][b];
            if (bd < 3) continue;
            int shift = bd - 3;
            if (shift > 0) {
                bits += (tbl[b + 1] - tbl[b]) * shift;
            }
        }

        /* 5c. 2tup */
        {
            int count = 0, acc = 0;
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                if (st->bit_depths[ch][b] != 2) continue;
                int start = tbl[b], end = tbl[b + 1];
                for (int k = start; k < end; k++) {
                    acc = (acc << 2) | (q[k] & 3);
                    count++;
                    if (count == 2) {
                        bits += tbl2[acc * 3 + 2];
                        count = 0; acc = 0;
                    }
                }
            }
            if (count > 0) bits += 2;
        }

        /* 5d. 4tup */
        {
            int count = 0, acc = 0;
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                if (st->bit_depths[ch][b] != 1) continue;
                int start = tbl[b], end = tbl[b + 1];
                for (int k = start; k < end; k++) {
                    acc = (acc << 1) | (q[k] & 1);
                    count++;
                    if (count == 4) {
                        bits += tbl4[acc * 3 + 2];
                        count = 0; acc = 0;
                    }
                }
            }
            if (count > 0) bits += count;
        }

        /* 5e. Signs */
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            if (st->bit_depths[ch][b] <= 0) continue;
            int start = tbl[b], end = tbl[b + 1];
            for (int k = start; k < end; k++) {
                if (q[k] > 0) bits += 1;
            }
        }
    }
    return bits;
}

static void optimize_and_quantize(L2hcEncoderState *st) {
    /* 1. Fast initial search using CountBitsRough */
    int best_gain = 23;
    int best_fg = 0;

    for (int g = 23; g >= -8; g--) {
        int fits = 0;
        for (int fg = 0; fg < 8; fg++) {
            int bits = count_frame_bits_frac(st, g, fg);
            if (bits <= st->target_frame_bits) {
                best_gain = g;
                best_fg = fg;
                fits = 1;
                break;
            }
        }
        if (!fits) {
            break;
        }
    }

    st->global_gain = best_gain;
    st->frac_gain = best_fg;

    /* 2. Exact bit verification loop matching Ghidra EncodeMDCTCountBits_0013e084.c */
    for (;;) {
        for (int ch = 0; ch < st->channels; ch++) {
            quantize_spectrum(st, ch);
        }

        int exact_bits = count_frame_exact_bits(st);
        if (exact_bits <= st->target_frame_bits || (st->global_gain >= 23 && st->frac_gain >= 7)) {
            break;
        }

        st->frac_gain++;
        if (st->frac_gain == 8) {
            st->frac_gain = 0;
            st->global_gain++;
        }
        if (st->global_gain > 23) {
            st->global_gain = 23;
            st->frac_gain = 7;
            break;
        }
    }
}

static void pack_frame(L2hcEncoderState *st, L2hcBitWriter *w) {
    const int16_t *tbl = get_active_band_table(st);

    /* 1. Header (7 bits) */
    int sr_idx = 1; /* 48k */
    if (st->sample_rate == 44100) sr_idx = 0;
    else if (st->sample_rate == 48000) sr_idx = 1;
    else if (st->sample_rate == 88200) sr_idx = 2;
    else if (st->sample_rate == 96000) sr_idx = 3;

    int ch_idx = (st->channels == 2) ? 1 : 0;
    int fl_idx = 2;
    if (st->frame_samples >= 960) {
        fl_idx = 3; /* 10 ms @ 96k */
    } else if (st->frame_samples == 480) {
        fl_idx = 2; /* 10 ms @ 48k or 5 ms @ 96k */
    } else if (st->frame_samples == 240) {
        fl_idx = 1; /* 5 ms @ 48k */
    } else if (st->frame_samples <= 120) {
        fl_idx = 0; /* 2.5 ms */
    }
    uint32_t header = (1 << 5) | (sr_idx << 3) | (ch_idx << 2) | fl_idx;
    bw_write(w, header, 7);

    /* 2. Control Flags */
    bw_write(w, 0, 1); /* low_bitrate_flag = 0 for >= 320 kbps */
    if (st->channels == 2) {
        bw_write(w, st->ms_flag, 1); /* ms_flag */
    }
    bw_write(w, (uint64_t)(st->global_gain + 8), 5); /* global_gain (5 bits) */
    bw_write(w, (uint64_t)st->frac_gain, 3); /* frac_gain (3 bits: 0x109a) */
    bw_write(w, (uint64_t)st->band_table_idx, 3); /* band_table_idx (3 bits: 0x1016) */
    bw_write(w, st->diff_sf_flag, 1); /* diff_sf_flag */

    /* 3. Scale Factors */
    for (int ch = 0; ch < st->channels; ch++) {
        if (st->diff_sf_flag) {
            bw_write(w, (uint64_t)(st->scale_factors[ch][0] + 8), 5);
            for (int b = 1; b < L2HC_NUM_BANDS; b++) {
                int delta = st->scale_factors[ch][b] - st->scale_factors[ch][b - 1];
                int abs_d = abs(delta);
                if (abs_d > 6) abs_d = 6;
                int code = HUF_ENC_DIFF_SF[abs_d * 3 + 1];
                int len  = HUF_ENC_DIFF_SF[abs_d * 3 + 2];
                bw_write(w, code, len);
                if (abs_d > 0) {
                    bw_write(w, (delta > 0) ? 1 : 0, 1);
                }
            }
        } else {
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                int sf = st->scale_factors[ch][b] + 8;
                if (sf < 0) sf = 0;
                if (sf > 31) sf = 31;
                bw_write(w, (uint64_t)sf, 5);
            }
        }
    }

    /* 4. Table selection: 3 x 2 bits per channel (4tup, 2tup, 1tup) */
    for (int ch = 0; ch < st->channels; ch++) {
        bw_write(w, (uint64_t)st->t4_idx[ch], 2);
        bw_write(w, (uint64_t)st->t2_idx[ch], 2);
        bw_write(w, (uint64_t)st->t1_idx[ch], 2);
    }

    /* 5. Spectral Data per Channel (PackMDCTHighBitRate) */
    for (int ch = 0; ch < st->channels; ch++) {
        const int32_t *q = st->q_mdct[ch];

        /* 5a. Pack1TupThree - PASS 1: MSB Huffman symbols across all bd>=3 bands */
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int bd = st->bit_depths[ch][b];
            if (bd < 3) continue;
            int start_bin = tbl[b];
            int end_bin = tbl[b + 1];
            int shift = bd - 3;
            for (int k = start_bin; k < end_bin; k++) {
                int msb = q[k] >> shift;
                if (msb > 7) msb = 7;
                const int16_t *tbl1 = g_huf_1tup_tables[st->t1_idx[ch]];
                int code = tbl1[msb * 3 + 1];
                int len  = tbl1[msb * 3 + 2];
                bw_write(w, code, len);
            }
        }

        /* 5b. Pack1TupThree - PASS 2: LSB residual bits across all bd>=3 bands */
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int bd = st->bit_depths[ch][b];
            if (bd < 3) continue;
            int start_bin = tbl[b];
            int end_bin = tbl[b + 1];
            int shift = bd - 3;
            if (shift > 0) {
                uint32_t mask = (1 << shift) - 1;
                for (int k = start_bin; k < end_bin; k++) {
                    bw_write(w, q[k] & mask, shift);
                }
            }
        }

        /* 5c. Pack2TupTwo (bands with bd == 2) */
        {
            int acc = 0, count = 0;
            const int16_t *tbl2 = g_huf_2tup_tables[st->t2_idx[ch]];
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                if (st->bit_depths[ch][b] != 2) continue;
                int start_bin = tbl[b];
                int end_bin = tbl[b + 1];
                for (int k = start_bin; k < end_bin; k++) {
                    acc = (acc << 2) | (q[k] & 3);
                    count++;
                    if (count == 2) {
                        int code = tbl2[acc * 3 + 1];
                        int len  = tbl2[acc * 3 + 2];
                        bw_write(w, code, len);
                        acc = 0;
                        count = 0;
                    }
                }
            }
            if (count > 0) {
                bw_write(w, acc & 3, 2);
            }
        }

        /* 5d. Pack4TupOne (bands with bd == 1) */
        {
            int acc = 0, count = 0;
            const int16_t *tbl4 = g_huf_4tup_tables[st->t4_idx[ch]];
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                if (st->bit_depths[ch][b] != 1) continue;
                int start_bin = tbl[b];
                int end_bin = tbl[b + 1];
                for (int k = start_bin; k < end_bin; k++) {
                    acc = (acc << 1) | (q[k] & 1);
                    count++;
                    if (count == 4) {
                        int code = tbl4[acc * 3 + 1];
                        int len  = tbl4[acc * 3 + 2];
                        bw_write(w, code, len);
                        acc = 0;
                        count = 0;
                    }
                }
            }
            if (count > 0) {
                bw_write(w, acc & ((1 << count) - 1), count);
            }
        }

        /* 5e. Sign bits (for all active bands with bd > 0, 1 bit per non-zero coeff) */
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            if (st->bit_depths[ch][b] <= 0) continue;
            int start_bin = tbl[b];
            int end_bin = tbl[b + 1];
            for (int k = start_bin; k < end_bin; k++) {
                if (q[k] > 0) {
                    bw_write(w, st->q_sign[ch][k], 1);
                }
            }
        }
    }

    /* 6. PackMDCTRes: fine residual bits across ranked bands (matches Ghidra PackMDCTResBranch_0013e648) */
    {
        const int N = st->frame_samples;
        int peak_band = 0;
        int max_sf = -100;
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            for (int ch = 0; ch < st->channels; ch++) {
                if (st->scale_factors[ch][b] > max_sf) {
                    max_sf = st->scale_factors[ch][b];
                    peak_band = b;
                }
            }
        }

        int total_bands = st->channels * L2HC_NUM_BANDS;
        int ranked[64];
        int priorities[64];
        for (int b = 0; b < L2HC_NUM_BANDS; b++) {
            int dist = abs(b - peak_band);
            for (int ch = 0; ch < st->channels; ch++) {
                int idx = b * st->channels + ch;
                ranked[idx] = idx;
                if (st->bit_depths[ch][b] == 0) {
                    priorities[idx] = L2HC_NUM_BANDS - dist;
                } else {
                    priorities[idx] = -dist;
                }
            }
        }

        for (int i = 0; i < total_bands - 1; i++) {
            for (int j = 0; j < total_bands - 1 - i; j++) {
                if (priorities[j] < priorities[j + 1]) {
                    int tp = priorities[j]; priorities[j] = priorities[j + 1]; priorities[j + 1] = tp;
                    int tr = ranked[j];     ranked[j]     = ranked[j + 1];     ranked[j + 1]     = tr;
                }
            }
        }

        for (int r = 0; r < total_bands; r++) {
            if (w->total_bits >= st->target_frame_bits) break;
            int idx = ranked[r];
            int band = idx / st->channels;
            int ch   = idx % st->channels;

            int bd = st->bit_depths[ch][band];
            int start_bin = tbl[band];
            int end_bin = tbl[band + 1];
            if (start_bin >= N) continue;
            if (end_bin > N) end_bin = N;

            const float *spec = st->mdct_buf[ch];
            const int32_t *q = st->q_mdct[ch];

            if (bd < 1) {
                int sf = st->scale_factors[ch][band];
                float inv_scale = ldexpf(1.0f, 1 - sf);
                for (int k = start_bin; k < end_bin; k++) {
                    if (w->total_bits >= st->target_frame_bits) break;
                    float mag = fabsf(spec[k]);
                    int has_energy = ((int)(inv_scale * mag + 0.375f) > 0) ? 1 : 0;
                    bw_write(w, has_energy, 1);
                    if (has_energy && w->total_bits < st->target_frame_bits) {
                        bw_write(w, (spec[k] > 0.0f) ? 1 : 0, 1);
                    }
                }
            } else {
                float offset = (bd == 1) ? 0.125f : 0.0f;
                float step = ldexpf(1.0f, st->quant_scale[ch][band]);
                for (int k = start_bin; k < end_bin; k++) {
                    if (w->total_bits >= st->target_frame_bits) break;
                    if (q[k] != 0) {
                        float threshold = step * (offset + (float)q[k]);
                        int bit = (fabsf(spec[k]) >= threshold) ? 1 : 0;
                        bw_write(w, bit, 1);
                    }
                }
            }
        }

        int remaining_bits = st->target_frame_bits - w->total_bits;
        while (remaining_bits >= 64) {
            bw_write(w, 0ULL, 64);
            remaining_bits -= 64;
        }
        if (remaining_bits > 0) {
            bw_write(w, 0ULL, remaining_bits);
        }
    }
}

int AudioL2hcEncApply(void *encoder, L2hcEncIo *io) {
    if (!encoder || !io || !io->in_buffer || !io->out_buffer) {
        return -1;
    }
    L2hcEncoderState *st = (L2hcEncoderState *)encoder;
    if (st->magic != L2HC_MAGIC) return -1;

    const int N = st->frame_samples;
    const int channels = st->channels;
    const int bytes_per_sample = st->bps / 8;
    const uint32_t expected_in = N * channels * bytes_per_sample;

    if (io->in_bytes < expected_in) {
        return -2;
    }

    /*
     * 1. Unpack PCM input to 24-bit float range [-8388608, 8388607]
     * Matches LoadInputAudio:
     *   16-bit: s16 * 256.0f
     *   24-bit: sign_extend_24(s24)
     *   32-bit: s32 / 256.0f
     */
    float curr_samples[L2HC_MAX_CHANNELS][L2HC_MAX_FRAME_SAMPLES];
    const uint8_t *in = io->in_buffer;

    for (int i = 0; i < N; i++) {
        for (int ch = 0; ch < channels; ch++) {
            float s = 0.0f;
            if (bytes_per_sample == 2) {
                int16_t val = (int16_t)(in[0] | (in[1] << 8));
                s = (float)val * 256.0f;
                in += 2;
            } else if (bytes_per_sample == 3) {
                int32_t val = (int32_t)((in[0] << 8) | (in[1] << 16) | (in[2] << 24));
                s = (float)(val >> 8);
                in += 3;
            } else if (bytes_per_sample == 4) {
                int32_t val = (int32_t)(in[0] | (in[1] << 8) | (in[2] << 16) | (in[3] << 24));
                s = (float)val / 256.0f;
                in += 4;
            }
            curr_samples[ch][i] = s;
        }
    }

    /* 2. Windowing and Fast MDCT on continuous raw L/R time-domain samples */
    float win_in[2 * L2HC_MAX_FRAME_SAMPLES];
    for (int ch = 0; ch < channels; ch++) {
        for (int i = 0; i < N; i++) {
            win_in[i]     = st->prev_samples[ch][i] * st->window[i];
            win_in[N + i] = curr_samples[ch][i]     * st->window[N + i];
        }
        memcpy(st->prev_samples[ch], curr_samples[ch], sizeof(float) * N);
        compute_fast_mdct(win_in, st->mdct_buf[ch], N);
    }

    /* 3. Select optimal Band Table and compute Scale Factors on L/R */
    select_band_table_and_compute_sf(st);

    /* 4. CheckMS: Mid/Side evaluation matching CheckMS_0013c62c.c */
    st->ms_flag = 0;
    if (channels == 2) {
        const int16_t *tbl = get_active_band_table(st);
        int cost_lr = 0;
        for (int ch = 0; ch < 2; ch++) {
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                int w = tbl[b + 1] - tbl[b];
                cost_lr += w * (int)st->scale_factors[ch][b];
            }
        }

        /* Forward M/S transform (matches MdctMS_001330b8.c) */
        for (int k = 0; k < N; k++) {
            float l = st->mdct_buf[0][k];
            float r = st->mdct_buf[1][k];
            st->mdct_buf[0][k] = (l + r) * 0.5f;
            st->mdct_buf[1][k] = (l - r) * 0.5f;
        }

        /* Compute scale factors on Mid and Side */
        int16_t sf_ms[2][L2HC_NUM_BANDS];
        int cost_ms = 0;
        for (int ch = 0; ch < 2; ch++) {
            const float *spec = st->mdct_buf[ch];
            for (int b = 0; b < L2HC_NUM_BANDS; b++) {
                int start = tbl[b];
                int end = tbl[b + 1];
                float peak = 1e-10f;
                for (int k = start; k < end; k++) {
                    float a = fabsf(spec[k]);
                    if (a > peak) peak = a;
                }
                int sf = (int)(logf(peak) * 1.4426950408889634f);
                if (sf < -8) sf = -8;
                if (sf > 23) sf = 23;
                sf_ms[ch][b] = (int16_t)sf;
                cost_ms += (end - start) * sf;
            }
        }

        /* If M/S reduces bit cost, adopt it; otherwise revert to L/R (matches CheckMS) */
        if (cost_ms < cost_lr) {
            st->ms_flag = 1;
            memcpy(st->scale_factors, sf_ms, sizeof(sf_ms));
        } else {
            st->ms_flag = 0;
            /* Inverse M/S back to Left and Right (matches MdctInvMS_00133198.c) */
            for (int k = 0; k < N; k++) {
                float m = st->mdct_buf[0][k];
                float s = st->mdct_buf[1][k];
                st->mdct_buf[0][k] = m + s;
                st->mdct_buf[1][k] = m - s;
            }
        }

        /* Update diff_sf_flag for final scale factors */
        st->diff_sf_flag = 1;
        for (int ch = 0; ch < channels; ch++) {
            for (int b = 1; b < L2HC_NUM_BANDS; b++) {
                int delta = st->scale_factors[ch][b] - st->scale_factors[ch][b - 1];
                if (abs(delta) > 6) {
                    st->diff_sf_flag = 0;
                    break;
                }
            }
            if (!st->diff_sf_flag) break;
        }
    }

    /* 5. Optimize Global Gain and Quantize with Exact Bit Count Feedback */
    optimize_and_quantize(st);

    /* 6. Assemble Bitstream */
    L2hcBitWriter bw;
    bw_init(&bw);
    pack_frame(st, &bw);

    /* 7. Flush to output buffer */
    bw_flush(&bw, io->out_buffer, st->target_frame_bytes);
    io->out_bytes = (uint32_t)st->target_frame_bytes;

    return 0;
}

/* High-level direct API */
l2hc_native_t l2hc_native_create(void) {
    uint32_t size = 0;
    AudioL2hcEncGetSize(&size);
    void *buf = malloc(size);
    if (!buf) return NULL;
    void *enc = NULL;
    if (AudioL2hcEncInit(&enc, buf, size) != 0) {
        free(buf);
        return NULL;
    }
    return (l2hc_native_t)enc;
}

int l2hc_native_set_param(l2hc_native_t enc, const L2hcEncParam *param) {
    return AudioL2hcEncSetParam(enc, param);
}

int l2hc_native_encode(l2hc_native_t enc, const uint8_t *in_pcm, uint32_t in_bytes,
                      uint8_t *out_frame, uint32_t *out_bytes) {
    L2hcEncIo io = {
        .in_buffer = in_pcm,
        .in_bytes = in_bytes,
        .out_buffer = out_frame,
        .out_bytes = 0
    };
    int ret = AudioL2hcEncApply(enc, &io);
    if (ret == 0) {
        *out_bytes = io.out_bytes;
    }
    return ret;
}

void l2hc_native_destroy(l2hc_native_t enc) {
    if (enc) free(enc);
}
