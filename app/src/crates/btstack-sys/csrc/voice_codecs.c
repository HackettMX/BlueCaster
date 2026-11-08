/*
 * voice_codecs.c - HFP voice codec primitives for the headset microphone:
 * BTstack's Bluedroid mSBC encoder and decoder (the decoder with BTstack's
 * SBC packet loss concealment) and BTstack's CVSD packet loss concealment,
 * behind opaque heap instances for the safe layer in src/voice.rs.
 *
 * None of these touch BTstack's run loop or HCI: they are plain codecs and
 * may run on any thread, one thread per instance at a time. (The Bluedroid
 * encoder's two globals, s32LRDiff / s32LRSum, are used by joint stereo
 * only, never by mSBC.)
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "classic/btstack_sbc.h"
#include "classic/btstack_sbc_bluedroid.h"
#include "classic/btstack_cvsd_plc.h"

/* mSBC: 120 samples at 16 kHz per 57-byte frame */
#define VOICE_MSBC_SAMPLES 120
#define VOICE_MSBC_FRAME 57
/* Decoded PCM waiting to be taken: 16 mSBC frames (120 ms) */
#define VOICE_MSBC_PCM_CAP (16 * VOICE_MSBC_SAMPLES)

/* ---- mSBC encoder ---- */

typedef struct {
    btstack_sbc_encoder_bluedroid_t state;
    const btstack_sbc_encoder_t *codec;
    /* SBC_Encoder writes the frame here; larger than one mSBC frame */
    uint8_t frame[128];
} bluecaster_msbc_encoder_t;

void *bluecaster_msbc_encoder_new(void) {
    bluecaster_msbc_encoder_t *e = (bluecaster_msbc_encoder_t *)calloc(1, sizeof(bluecaster_msbc_encoder_t));
    if (e == NULL) return NULL;
    e->codec = btstack_sbc_encoder_bluedroid_init_instance(&e->state);
    /* The mSBC parameters are fixed: the values passed here are ignored */
    if (e->codec->configure(&e->state, SBC_MODE_mSBC, 15, 8, SBC_ALLOCATION_METHOD_LOUDNESS, 16000, 26,
                            SBC_CHANNEL_MODE_MONO) != 0) {
        free(e);
        return NULL;
    }
    return e;
}

/* Encodes 120 samples into one 57-byte frame (`out` has room for 57).
 * Returns the frame length, or -1 if it is not 57. */
int bluecaster_msbc_encode(void *encoder, const int16_t *pcm, uint8_t *out) {
    bluecaster_msbc_encoder_t *e = (bluecaster_msbc_encoder_t *)encoder;
    e->codec->encode_signed_16(&e->state, pcm, e->frame);
    uint16_t n = e->codec->sbc_buffer_length(&e->state);
    if (n != VOICE_MSBC_FRAME) return -1;
    memcpy(out, e->frame, VOICE_MSBC_FRAME);
    return VOICE_MSBC_FRAME;
}

void bluecaster_msbc_encoder_free(void *encoder) {
    free(encoder);
}

/* ---- mSBC decoder ---- */

typedef struct {
    btstack_sbc_decoder_bluedroid_t state;
    const btstack_sbc_decoder_t *codec;
    int16_t pcm[VOICE_MSBC_PCM_CAP];
    uint32_t pcm_len;
    /* Samples that did not fit in `pcm` (the caller drains too rarely) */
    uint32_t overflow;
} bluecaster_msbc_decoder_t;

static void msbc_pcm_handler(int16_t *data, int num_samples, int num_channels, int sample_rate, void *context) {
    (void)sample_rate;
    bluecaster_msbc_decoder_t *d = (bluecaster_msbc_decoder_t *)context;
    if (num_samples <= 0 || num_channels != 1) return;
    uint32_t n = (uint32_t)num_samples;
    uint32_t room = VOICE_MSBC_PCM_CAP - d->pcm_len;
    if (n > room) {
        d->overflow += n - room;
        n = room;
    }
    memcpy(d->pcm + d->pcm_len, data, n * sizeof(int16_t));
    d->pcm_len += n;
}

void *bluecaster_msbc_decoder_new(void) {
    bluecaster_msbc_decoder_t *d = (bluecaster_msbc_decoder_t *)calloc(1, sizeof(bluecaster_msbc_decoder_t));
    if (d == NULL) return NULL;
    d->codec = btstack_sbc_decoder_bluedroid_init_instance(&d->state);
    d->codec->configure(&d->state, SBC_MODE_mSBC, msbc_pcm_handler, d);
    return d;
}

/* Feeds SCO payload bytes (any length) with the HCI packet status flag.
 * Decoded and concealed frames collect inside; returns how many samples are
 * waiting (take them with bluecaster_msbc_decoder_take). */
int bluecaster_msbc_decode(void *decoder, uint8_t packet_status_flag, const uint8_t *data, uint16_t len) {
    bluecaster_msbc_decoder_t *d = (bluecaster_msbc_decoder_t *)decoder;
    if (len > 0) d->codec->decode_signed_16(&d->state, packet_status_flag, data, len);
    return (int)d->pcm_len;
}

/* Moves up to `cap` waiting samples to `out`; returns how many */
int bluecaster_msbc_decoder_take(void *decoder, int16_t *out, uint32_t cap) {
    bluecaster_msbc_decoder_t *d = (bluecaster_msbc_decoder_t *)decoder;
    uint32_t n = d->pcm_len < cap ? d->pcm_len : cap;
    memcpy(out, d->pcm, n * sizeof(int16_t));
    memmove(d->pcm, d->pcm + n, (d->pcm_len - n) * sizeof(int16_t));
    d->pcm_len -= n;
    return (int)n;
}

/* Frame counters: good (decoded), bad (concealed), zero (all-zero frames
 * concealed), and samples lost to a full PCM buffer */
void bluecaster_msbc_decoder_stats(void *decoder, uint32_t *good, uint32_t *bad, uint32_t *zero,
                                   uint32_t *overflow) {
    bluecaster_msbc_decoder_t *d = (bluecaster_msbc_decoder_t *)decoder;
    *good = (uint32_t)d->state.good_frames_nr;
    *bad = (uint32_t)d->state.bad_frames_nr;
    *zero = (uint32_t)d->state.zero_frames_nr;
    *overflow = d->overflow;
}

void bluecaster_msbc_decoder_free(void *decoder) {
    free(decoder);
}

/* ---- CVSD packet loss concealment ---- */

void *bluecaster_cvsd_plc_new(void) {
    btstack_cvsd_plc_state_t *s = (btstack_cvsd_plc_state_t *)calloc(1, sizeof(btstack_cvsd_plc_state_t));
    if (s == NULL) return NULL;
    btstack_cvsd_plc_init(s);
    return s;
}

/* `num_samples` (1..=CVSD_FS, 60) samples from `in` to `out`, with
 * concealment of a bad frame (`in` is read in either case). `in` and `out`
 * may not overlap. */
void bluecaster_cvsd_plc_process(void *plc, int bad_frame, int16_t *in, uint16_t num_samples, int16_t *out) {
    btstack_cvsd_plc_process_data((btstack_cvsd_plc_state_t *)plc, bad_frame != 0, in, num_samples, out);
}

void bluecaster_cvsd_plc_stats(void *plc, uint32_t *good, uint32_t *bad) {
    btstack_cvsd_plc_state_t *s = (btstack_cvsd_plc_state_t *)plc;
    *good = (uint32_t)s->good_frames_nr;
    *bad = (uint32_t)s->bad_frames_nr;
}

void bluecaster_cvsd_plc_free(void *plc) {
    free(plc);
}
