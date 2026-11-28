/*
 * mwadc.h - C API of mwadc_ffi.dll: raw-ADC capture with the Magewell Pro Capture HDMI, DVI and AIO (ADV7842).
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * Model
 *   open    -> a card channel (by rotary-switch board number or SDK index)
 *   arm     -> program the card for raw capture with a configuration and leave it in capture
 *              mode. Arming again with the same clock/geometry/inputs only updates the front end
 *              (clamp, anti-alias filters, gains, centres) and works while streaming; anything else
 *              needs the stream stopped and re-locks the card (a few seconds).
 *   start   -> frames arrive on an mwadc thread through the frame callback
 *   stop    -> stream ends; the card stays armed (inputs stay settled)
 *   restore / close -> the card goes back to normal video use
 *   audio_start / audio_stop -> the card's analog line-in (stereo, 48 kHz, 24 bit) through an audio
 *              callback, independent of arming and of the frame stream (both can run at once);
 *              with MWADC_AUDIO_LOCK_RF resampled onto the RF sample clock of the device's stream
 *
 * Configuration is text key/value (mwadc_config_set), so new options need no ABI change;
 * mwadc_config_keys() lists them.
 *
 * Every function returning int returns 0 (or a count) on success and -1 on error; the message is
 * then in mwadc_last_error() (per thread). Strings are UTF-8.
 *
 * Threads: the frame callback runs on the stream's sink thread. It must return quickly-ish (the
 * stream has a small pool of frame buffers; a slow callback makes the card skip frames, reported
 * in mwadc_frame.gap_frames) and must not call mwadc_stop/mwadc_close/mwadc_arm on the same device.
 * The audio callback runs on its own thread (every 4 ms at 48 kHz) under the same rules, and must
 * not call mwadc_audio_stop/mwadc_close.
 */
#ifndef MWADC_H
#define MWADC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MWADC_API_VERSION 5

#if defined(_WIN32) && !defined(MWADC_STATIC)
#  define MWADC_FN __declspec(dllimport)
#else
#  define MWADC_FN
#endif

typedef struct mwadc_config mwadc_config;
typedef struct mwadc_device mwadc_device;

enum { MWADC_LOG_DEBUG = 0, MWADC_LOG_INFO = 1, MWADC_LOG_WARN = 2, MWADC_LOG_ERROR = 3 };
enum { MWADC_FORMAT_RGB10 = 0, MWADC_FORMAT_Y16 = 1 };
/* mwadc_audio_start flags (API 4) */
enum { MWADC_AUDIO_LOCK_RF = 1 };
/* mwadc_audio_block.segment bits (API 4) */
enum { MWADC_AUDIO_SEGMENT_FIRST = 1, MWADC_AUDIO_SEGMENT_LAST = 2 };

typedef struct {
    int32_t  index;          /* SDK enumeration index */
    uint8_t  board;          /* rotary switch 0..15 */
    uint8_t  channel;
    uint16_t product_id;     /* supported: 0x104 Pro Capture HDMI, 0x103 DVI, 0x102 AIO */
    char     serial[32];
    char     product[64];
} mwadc_device_info;

/* Where one ADC's samples sit in a sample word: code = (word >> lsb) & ((1 << bits) - 1).
 * A 0..1023 output code window; `step` = output codes per ADC step (1 for ADC0; 2 for ADC1/ADC2,
 * whose lowest bit is always 0). */
typedef struct {
    uint32_t pin;            /* ADV7842 Ain pin, 0 = ADC off */
    uint32_t lsb;
    uint32_t bits;
    uint32_t step;
    uint16_t gain;           /* CP gain, 2.8 fixed point (0x100 = 1.0) */
    uint16_t centre;         /* ADC level at output code 512, 12-bit ADC units (4 per ADC code) (API 5) */
} mwadc_field;

typedef struct {
    uint32_t format;         /* MWADC_FORMAT_* */
    uint32_t bytes_per_sample; /* 4 (RG10, little-endian u32) or 2 (Y16, u16) */
    uint32_t cx;             /* samples per line */
    uint32_t cy;             /* lines per frame */
    uint32_t stride;         /* bytes per line in a frame */
    uint32_t frame_bytes;
    double   sample_rate_hz;
    mwadc_field field[3];    /* ADC0, ADC1, ADC2 */
    int32_t  clamp;          /* analog clamp on (one switch for all ADCs) */
    int32_t  calibrated;     /* stored calibration levels were applied */
    uint32_t aa_enable;      /* bit n = anti-alias filter on ADCn */
    uint32_t aa_mhz;         /* anti-alias corner, MHz */
} mwadc_stream_info;

typedef struct {
    const uint8_t *data;     /* cy lines of `stride` bytes; valid only during the callback */
    uint32_t bytes;
    uint32_t gap_frames;     /* frames skipped right before this one (0 = continuous) */
    uint64_t seq;            /* frame number in this stream */
    int64_t  start_100ns;    /* device timestamp of the frame start */
    uint32_t torn;           /* slot overwritten while read */
    uint32_t read_us;
} mwadc_frame;

typedef struct {
    uint32_t adc;
    uint32_t pin;
    uint32_t clamp;
    uint32_t min_code, max_code;
    double   rest;           /* resting level (fit), output codes at neutral levels */
    double   tau_s;          /* settling time constant, 0 = flat */
    double   noise_rms;      /* codes rms with no input */
    uint16_t centre;         /* chosen centre, 12-bit ADC units (API 5) */
    uint16_t gain;           /* chosen CP gain (2.8) */
} mwadc_cal_result;

/* Line-in format as the card reports it when audio starts. (API 3) */
typedef struct {
    uint32_t sample_rate;    /* Hz (48000) */
    uint32_t bits_per_sample; /* significant high bits of each int32 sample (24) */
    uint32_t channels;       /* 2: L, R */
    uint32_t channel_valid;  /* SDK channel-pair mask (bit 0 = the line-in pair) */
    uint32_t source;         /* SDK audio input source (0x400 = line in) */
} mwadc_audio_info;

/* One block of line-in audio. (API 3; the lock fields API 4)
 * Locked (MWADC_AUDIO_LOCK_RF): the samples are resampled onto the RF sample clock: sample
 * `position` belongs to RF sample position * rf_rate_hz / sample_rate of the current stream, and
 * position 0 to its RF sample 0, so 48000 samples cover exactly 40 M RF samples at 40 MSPS. Blocks
 * then hold a varying number of samples (~192, more at the start), timestamp_100ns is the first
 * sample's time on the RF timeline, and missed/sdk_frame/buffered describe the card's blocks. */
typedef struct {
    const int32_t *samples;  /* `frames` interleaved L, R pairs, left-justified: value >> 8 = 24-bit sample */
    uint32_t frames;         /* 192 (locked: varies) */
    uint32_t missed;         /* blocks lost right before this one (0 = continuous); locked: replaced by silence */
    uint64_t seq;            /* block number in this audio stream */
    int64_t  timestamp_100ns; /* device timestamp; same clock as mwadc_frame.start_100ns */
    uint32_t sdk_frame;      /* SDK audio frame number */
    uint32_t buffered;       /* blocks the SDK still held */
    uint32_t locked;         /* 1: RF-locked samples (API 4) */
    uint32_t segment;        /* MWADC_AUDIO_SEGMENT_* : first / last block of an RF stream */
    uint64_t position;       /* locked: output index of samples[0] since RF sample 0 */
    double   rf_rate_hz;     /* locked: nominal RF rate the audio follows (the configured rate) */
    double   step_ppm;       /* locked: clock correction, card samples per output sample - 1 (ppm) */
    double   error;          /* locked: output count minus the RF target, samples (|error| < 1 when locked) */
} mwadc_audio_block;

typedef void (*mwadc_log_fn)(void *user, int level, const char *line);
typedef void (*mwadc_frame_fn)(void *user, const mwadc_frame *frame);
typedef void (*mwadc_audio_fn)(void *user, const mwadc_audio_block *block);
/* fraction 0..1; mean/rms per ADC of the latest analysed frame */
typedef void (*mwadc_progress_fn)(void *user, double fraction, const double mean[3], const double rms[3]);

MWADC_FN uint32_t     mwadc_api_version(void);
MWADC_FN const char  *mwadc_version(void);
MWADC_FN const char  *mwadc_last_error(void);
/* Log lines at or above min_level go to fn (from any thread); fn = NULL restores stderr. */
MWADC_FN void         mwadc_set_log(mwadc_log_fn fn, void *user, int min_level);

/* sdk_dll: path to LibMWCapture.dll or NULL for the default search. Returns the channel count. */
MWADC_FN int          mwadc_list(const char *sdk_dll, mwadc_device_info *out, int max);

/* Defaults: Component Y/Pr/Pb on ADC0/1/2 (HDMI and AIO cards). The DVI card has VGA jacks only:
 * set adc0/adc1/adc2 to vga-g/vga-r/vga-b. Arming with a pin the card does not wire logs a warning. */
MWADC_FN mwadc_config *mwadc_config_new(void);
MWADC_FN void         mwadc_config_free(mwadc_config *cfg);
MWADC_FN int          mwadc_config_set(mwadc_config *cfg, const char *key, const char *value);
/* "key\tdescription\n" lines; static string. */
MWADC_FN const char  *mwadc_config_keys(void);
/* Fill `info` with what arming `cfg` would set up (no hardware access). */
MWADC_FN int          mwadc_config_describe(const mwadc_config *cfg, mwadc_stream_info *info);

/* board: rotary switch 0..15, or -1; sdk_index used when board < 0 (-1 = the only card). */
MWADC_FN mwadc_device *mwadc_open(const char *sdk_dll, int board, int sdk_index);
/* Stops a running stream, restores the card, frees the handle. */
MWADC_FN void         mwadc_close(mwadc_device *dev);
MWADC_FN int          mwadc_info(mwadc_device *dev, mwadc_device_info *out);

MWADC_FN int          mwadc_arm(mwadc_device *dev, const mwadc_config *cfg, mwadc_stream_info *info);
MWADC_FN int          mwadc_is_armed(mwadc_device *dev);
/* pool: frame buffers (0 = default 6). */
MWADC_FN int          mwadc_start(mwadc_device *dev, uint32_t pool, mwadc_frame_fn fn, void *user);
MWADC_FN int          mwadc_is_streaming(mwadc_device *dev);
MWADC_FN int          mwadc_stop(mwadc_device *dev);
MWADC_FN int          mwadc_restore(mwadc_device *dev);

/* Line-in audio on its own thread (API 3; flags API 4). Works whether or not the card is armed or
 * streaming, and keeps running across mwadc_stop/mwadc_arm/mwadc_restore; mwadc_close stops it.
 * info (may be NULL) receives the format. Starting twice fails.
 * flags = 0: the card's samples on the card's audio clock (free-running against the RF clock).
 * flags = MWADC_AUDIO_LOCK_RF: resampled onto the RF clock of this device's frame stream (see
 *   mwadc_audio_block). Locking needs that stream: without it no blocks arrive (the last 2 s are held
 *   for a stream that starts). Audio that starts up to 2 s after the stream is padded with silence
 *   back to RF sample 0. Each stream is a segment (positions restart at 0); the last block of one
 *   arrives once the audio covering the stream's end is in. Adds 1 ms plus up to one frame of latency. */
MWADC_FN int          mwadc_audio_start(mwadc_device *dev, uint32_t flags, mwadc_audio_fn fn, void *user, mwadc_audio_info *info);
MWADC_FN int          mwadc_audio_is_running(mwadc_device *dev);
MWADC_FN int          mwadc_audio_stop(mwadc_device *dev);

/* Blocking. Measures the active ADCs of cfg with nothing playing for `seconds`, stores the result
 * (save != 0) and leaves the card armed with the new levels. Must not run while streaming.
 * Returns the number of results written to out (up to max). */
MWADC_FN int          mwadc_calibrate(mwadc_device *dev, const mwadc_config *cfg, double seconds, int save,
                                      mwadc_progress_fn progress, void *user, mwadc_cal_result *out, int max);
/* Path of the calibration store (static until the next call). */
MWADC_FN const char  *mwadc_calibration_path(void);

/* Stored calibration for a card (serial as in mwadc_device_info) and the input setup of cfg (ADC pins,
 * effective clamp, analog range). Calibrations are stored per card serial. */
typedef struct {
    uint32_t active;         /* active ADCs in cfg */
    uint32_t matched;        /* of those, how many have a stored calibration */
    char     updated[24];    /* UTC YYYYmmdd-HHMMSS of the card's last calibration, "" if none */
    uint16_t gain[3];        /* levels that would be applied (defaults where unmatched) */
    uint16_t centre[3];      /* (API 5) */
} mwadc_cal_info;

/* Returns 1 when every active ADC is calibrated, 0 when not (or partly), -1 on error. (API 2) */
MWADC_FN int          mwadc_calibration_lookup(const char *serial, const mwadc_config *cfg, mwadc_cal_info *out);
/* Highest gapless sample rate for cfg's format: 100 MSPS RG10 (1-3 channels), 160 MSPS Y16 (ADC0). (API 2) */
MWADC_FN double       mwadc_config_max_rate(const mwadc_config *cfg);

#ifdef __cplusplus
}
#endif
#endif /* MWADC_H */
