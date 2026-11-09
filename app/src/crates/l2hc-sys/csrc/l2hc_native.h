/*
 * Native Huawei L2HC (Low-Latency High-Definition Codec) Audio Encoder
 * Clean-room implementation in C/C++ without emulation or Android Bionic dependencies.
 *
 * SPDX-License-Identifier: MIT
 */

#ifndef L2HC_NATIVE_H
#define L2HC_NATIVE_H

#include <stdint.h>
#include <stddef.h>

/* BlueCaster: MSVC has no GNU attributes. On x86-64 / ARM64 the natural
 * layout of both structs below is already identical to the packed / 8-byte
 * aligned layout, so the attributes are only needed for other toolchains. */
#if defined(_MSC_VER)
#define L2HC_PACKED
#define L2HC_ALIGNED(x)
#else
#define L2HC_PACKED __attribute__((packed))
#define L2HC_ALIGNED(x) __attribute__((aligned(x)))
#endif

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    uint32_t sample_rate;  /* 44100, 48000, 88200, 96000 */
    int16_t  bps;          /* 16, 24, 32 */
    int16_t  channels;     /* 1 (mono), 2 (stereo) */
    int16_t  frame_samples;/* 480 (10ms @ 48kHz), 960 (10ms @ 96kHz) */
    int16_t  bitrate_kbps; /* 320, 480, 640, 960 */
} L2HC_PACKED L2hcEncParam;

typedef struct {
    const uint8_t *in_buffer;
    uint32_t       in_bytes;
    uint32_t       reserved;
    uint8_t       *out_buffer;
    uint32_t       out_bytes;
} L2HC_ALIGNED(8) L2hcEncIo;

/* Standard L2HC C ABI exports (matches libl2hc.so) */
int AudioL2hcEncGetSize(uint32_t *size);
int AudioL2hcEncInit(void **encoder, void *buffer, uint32_t size);
int AudioL2hcEncSetParam(void *encoder, const L2hcEncParam *param);
int AudioL2hcEncApply(void *encoder, L2hcEncIo *io);

/* High-level direct API */
typedef void* l2hc_native_t;

l2hc_native_t l2hc_native_create(void);
int l2hc_native_set_param(l2hc_native_t enc, const L2hcEncParam *param);
int l2hc_native_encode(l2hc_native_t enc, const uint8_t *in_pcm, uint32_t in_bytes,
                      uint8_t *out_frame, uint32_t *out_bytes);
void l2hc_native_destroy(l2hc_native_t enc);

#ifdef __cplusplus
}
#endif

#endif /* L2HC_NATIVE_H */
