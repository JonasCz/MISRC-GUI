/*
 * Hardware smoke test of mwadc_ffi.dll through its C API, loaded at run time the way MISRC-GUI
 * does (LoadLibrary + GetProcAddress). Needs a Pro Capture HDMI.
 *
 *   gcc -O2 -I../include c_smoke.c -o c_smoke.exe        (MSYS2 UCRT64 or any C compiler)
 *   c_smoke.exe <board> [calibrate-seconds]
 *
 * Arms ADC0 = Component Y, ADC2 = Component Pb, streams ~2 s, changes clamp / anti-alias / gain while
 * streaming, checks the frames keep coming, stops, optionally calibrates, closes (card restored).
 * Line-in audio runs from before arming until after the stream stops, to show it is independent of it.
 * Then a second stream with RF-locked audio: the audio must cover exactly that stream
 * (RF samples x 48000 / rate), from RF sample 0.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <stdatomic.h>
#include <windows.h>

#define MWADC_STATIC
#include "mwadc.h"

static struct {
    uint32_t (*api_version)(void);
    const char *(*last_error)(void);
    void (*set_log)(mwadc_log_fn, void *, int);
    int (*list)(const char *, mwadc_device_info *, int);
    mwadc_config *(*config_new)(void);
    void (*config_free)(mwadc_config *);
    int (*config_set)(mwadc_config *, const char *, const char *);
    mwadc_device *(*open)(const char *, int, int);
    void (*close)(mwadc_device *);
    int (*arm)(mwadc_device *, const mwadc_config *, mwadc_stream_info *);
    int (*start)(mwadc_device *, uint32_t, mwadc_frame_fn, void *);
    int (*stop)(mwadc_device *);
    int (*calibrate)(mwadc_device *, const mwadc_config *, double, int, mwadc_progress_fn, void *, mwadc_cal_result *, int);
    int (*audio_start)(mwadc_device *, uint32_t, mwadc_audio_fn, void *, mwadc_audio_info *);
    int (*audio_stop)(mwadc_device *);
} mw;

static atomic_ullong frames, gaps, sum_a;
static mwadc_stream_info info;

static void on_log(void *u, int level, const char *line) {
    (void)u;
    if (level >= MWADC_LOG_INFO) printf("  [dll] %s\n", line);
}

static void on_frame(void *u, const mwadc_frame *f) {
    (void)u;
    const mwadc_field *a = &info.field[0];
    const uint32_t *w = (const uint32_t *)f->data;
    uint64_t s = 0;
    size_t n = (size_t)info.cx * info.cy;
    for (size_t i = 0; i < n; i += 64) s += (w[i] >> a->lsb) & ((1u << a->bits) - 1);
    atomic_fetch_add(&sum_a, s * 64 / n);
    atomic_fetch_add(&frames, 1);
    atomic_fetch_add(&gaps, f->gap_frames);
}

static atomic_ullong a_blocks, a_samples, a_missed;
static atomic_llong a_ts0 = -1, a_ts1;
static atomic_uint a_peak;

static void on_audio(void *u, const mwadc_audio_block *b) {
    (void)u;
    uint32_t pk = atomic_load(&a_peak);
    for (uint32_t i = 0; i < 2 * b->frames; i++) {
        int32_t v = b->samples[i] >> 8;
        uint32_t m = (uint32_t)(v < 0 ? -v : v);
        if (m > pk) pk = m;
    }
    atomic_store(&a_peak, pk);
    long long none = -1;
    atomic_compare_exchange_strong(&a_ts0, &none, b->timestamp_100ns);
    atomic_store(&a_ts1, b->timestamp_100ns);
    atomic_fetch_add(&a_blocks, 1);
    atomic_fetch_add(&a_samples, b->frames);
    atomic_fetch_add(&a_missed, b->missed);
}

// RF-locked audio: positions must be contiguous from 0, and the stream's last block flagged.
static atomic_ullong l_samples, l_blocks, l_first_pos = ~0ull, l_next_pos, l_bad, l_last;
static double l_rf_hz, l_err_max, l_step;

static void on_locked(void *u, const mwadc_audio_block *b) {
    (void)u;
    if (!b->locked) { atomic_fetch_add(&l_bad, 1); return; }
    unsigned long long none = ~0ull;
    atomic_compare_exchange_strong(&l_first_pos, &none, b->position);
    if (atomic_load(&l_blocks) && b->position != atomic_load(&l_next_pos)) atomic_fetch_add(&l_bad, 1);
    atomic_store(&l_next_pos, b->position + b->frames);
    atomic_fetch_add(&l_samples, b->frames);
    atomic_fetch_add(&l_blocks, 1);
    if (b->segment & MWADC_AUDIO_SEGMENT_LAST) atomic_store(&l_last, 1);
    l_rf_hz = b->rf_rate_hz;
    l_step = b->step_ppm;
    if (fabs(b->error) > l_err_max) l_err_max = fabs(b->error);
}

static void on_progress(void *u, double frac, const double *mean, const double *rms) {
    (void)u; (void)rms;
    static int last = -1;
    int p = (int)(frac * 10);
    if (p != last) { last = p; printf("  calibrate %3.0f%%  mean %.1f / %.1f / %.1f\n", frac * 100, mean[0], mean[1], mean[2]); }
}

#define CHECK(x) do { if ((x) < 0) { printf("FAIL %s: %s\n", #x, mw.last_error()); return 1; } } while (0)

static int set(mwadc_config *c, const char *k, const char *v) {
    int r = mw.config_set(c, k, v);
    if (r < 0) printf("config %s=%s: %s\n", k, v, mw.last_error());
    return r;
}

static uint64_t stream_for(mwadc_device *d, int ms) {
    uint64_t f0 = atomic_load(&frames);
    Sleep(ms);
    return atomic_load(&frames) - f0;
}

int main(int argc, char **argv) {
    int board = argc > 1 ? atoi(argv[1]) : -1;
    double cal_s = argc > 2 ? atof(argv[2]) : 0;
    HMODULE h = LoadLibraryW(L"mwadc_ffi.dll");
    if (!h) { printf("FAIL: mwadc_ffi.dll not found\n"); return 1; }
#define L(f, n) { FARPROC p_ = GetProcAddress(h, n); memcpy(&mw.f, &p_, sizeof p_); } if (!mw.f) { printf("FAIL: missing %s\n", n); return 1; }
    L(api_version, "mwadc_api_version") L(last_error, "mwadc_last_error") L(set_log, "mwadc_set_log")
    L(list, "mwadc_list") L(config_new, "mwadc_config_new") L(config_free, "mwadc_config_free")
    L(config_set, "mwadc_config_set") L(open, "mwadc_open") L(close, "mwadc_close") L(arm, "mwadc_arm")
    L(start, "mwadc_start") L(stop, "mwadc_stop") L(calibrate, "mwadc_calibrate")
    L(audio_start, "mwadc_audio_start") L(audio_stop, "mwadc_audio_stop")
    if (mw.api_version() != MWADC_API_VERSION) { printf("FAIL: API version\n"); return 1; }
    mw.set_log(on_log, NULL, MWADC_LOG_WARN);

    mwadc_device_info di[8];
    int n = mw.list(NULL, di, 8);
    CHECK(n);
    for (int i = 0; i < n; i++) printf("card [%d] board %u serial '%s' %s\n", di[i].index, di[i].board, di[i].serial, di[i].product);

    mwadc_device *d = mw.open(NULL, board, -1);
    if (!d) { printf("FAIL open: %s\n", mw.last_error()); return 1; }
    mwadc_audio_info ai;
    CHECK(mw.audio_start(d, 0, on_audio, NULL, &ai));
    printf("audio: %u Hz, %u bit, %u ch, valid %#x, source %#x\n", ai.sample_rate, ai.bits_per_sample, ai.channels, ai.channel_valid, ai.source);
    Sleep(500);
    uint64_t a0 = atomic_load(&a_blocks);
    printf("audio before arming: %llu blocks in 0.5 s\n", (unsigned long long)a0);
    int twice = mw.audio_start(d, 0, on_audio, NULL, NULL);
    printf("second audio start refused: %s\n", twice < 0 ? "yes" : "NO");

    mwadc_config *c = mw.config_new();
    set(c, "adc0", "comp-y"); set(c, "adc1", "off"); set(c, "adc2", "comp-pb");
    set(c, "lines", "512"); set(c, "clamp", "auto");
    CHECK(mw.arm(d, c, &info));
    printf("armed: %u x %u, stride %u, %.6f MSPS, clamp %d, calibrated %d, A lsb %u gain %#x off %#x, C lsb %u gain %#x off %#x\n",
           info.cx, info.cy, info.stride, info.sample_rate_hz / 1e6, info.clamp, info.calibrated,
           info.field[0].lsb, info.field[0].gain, info.field[0].centre, info.field[2].lsb, info.field[2].gain, info.field[2].centre);
    CHECK(mw.start(d, 0, on_frame, NULL));
    uint64_t f1 = stream_for(d, 1500);
    printf("stream: %llu frames in 1.5 s (expect ~%.0f)\n", (unsigned long long)f1, 1.5 * info.sample_rate_hz / ((double)info.cx * info.cy));

    // Live front-end changes while streaming.
    set(c, "aa", "on"); set(c, "aa_bw", "16"); set(c, "gain0", "2.0"); set(c, "range", "wide");
    CHECK(mw.arm(d, c, &info));
    printf("live update ok: aa %#x @ %u MHz, gain A %#x\n", info.aa_enable, info.aa_mhz, info.field[0].gain);
    uint64_t f2 = stream_for(d, 1000);
    printf("stream after update: %llu frames in 1 s\n", (unsigned long long)f2);

    // A change that needs a re-lock must be refused while streaming.
    set(c, "adc2", "off");
    int r = mw.arm(d, c, &info);
    printf("re-lock while streaming refused: %s (%s)\n", r < 0 ? "yes" : "NO", r < 0 ? mw.last_error() : "");
    set(c, "adc2", "comp-pb");

    CHECK(mw.stop(d));
    printf("stopped: %llu frames total, %llu skipped\n", (unsigned long long)atomic_load(&frames), (unsigned long long)atomic_load(&gaps));
    uint64_t a1 = atomic_load(&a_blocks);
    Sleep(300);
    uint64_t a2 = atomic_load(&a_blocks);
    CHECK(mw.audio_stop(d));
    double span = (double)(atomic_load(&a_ts1) - atomic_load(&a_ts0)) * 1e-7;
    printf("audio: %llu blocks, %llu samples, %llu missed, %.1f samples/s by device time, running after stream stop: %s, peak %.1f dBFS\n",
           (unsigned long long)a2, (unsigned long long)atomic_load(&a_samples), (unsigned long long)atomic_load(&a_missed),
           span > 0 ? (double)(atomic_load(&a_samples) - 192) / span : 0.0, a2 > a1 ? "yes" : "NO",
           20 * log10((atomic_load(&a_peak) + 0.5) / 8388608.0));

    // RF-locked audio over a second stream (the card stays armed).
    uint64_t lf0 = atomic_load(&frames), lg0 = atomic_load(&gaps);
    CHECK(mw.audio_start(d, MWADC_AUDIO_LOCK_RF, on_locked, NULL, NULL));
    Sleep(200);
    uint64_t l_before = atomic_load(&l_samples);
    CHECK(mw.start(d, 0, on_frame, NULL));
    Sleep(3000);
    CHECK(mw.stop(d));
    for (int i = 0; i < 100 && !atomic_load(&l_last); i++) Sleep(10);
    CHECK(mw.audio_stop(d));
    uint64_t rf = (atomic_load(&frames) - lf0 + atomic_load(&gaps) - lg0) * (uint64_t)info.cx * info.cy;
    long long want = l_rf_hz > 0 ? llround((double)rf * ai.sample_rate / l_rf_hz) : -1;
    int lock_ok = l_before == 0 && atomic_load(&l_first_pos) == 0 && atomic_load(&l_last) && !atomic_load(&l_bad) &&
                  (long long)atomic_load(&l_samples) == want;
    printf("locked audio: %llu samples (none before the stream: %s), first position %llu, last block flagged: %s, "
           "RF %llu samples x %u / %.0f = %lld, max |error| %.2f, step %+.2f ppm: %s\n",
           (unsigned long long)atomic_load(&l_samples), l_before == 0 ? "yes" : "NO",
           (unsigned long long)atomic_load(&l_first_pos), atomic_load(&l_last) ? "yes" : "NO",
           (unsigned long long)rf, ai.sample_rate, l_rf_hz, want, l_err_max, l_step, lock_ok ? "ok" : "FAIL");

    if (cal_s > 0) {
        set(c, "aa", "off"); set(c, "gain0", "auto"); set(c, "range", "normal");
        mwadc_cal_result res[3];
        int k = mw.calibrate(d, c, cal_s, 0, on_progress, NULL, res, 3);
        CHECK(k);
        for (int i = 0; i < k; i++)
            printf("cal ADC%u Ain%u: rest %.1f tau %.1f noise %.2f -> gain %#x centre %#x\n", res[i].adc, res[i].pin,
                   res[i].rest, res[i].tau_s, res[i].noise_rms, res[i].gain, res[i].centre);
    }
    mw.config_free(c);
    mw.close(d);
    int ok = f1 > 0 && f2 > 0 && r < 0 && a0 > 0 && twice < 0 && a2 > a1 && atomic_load(&a_missed) == 0 && lock_ok;
    printf(ok ? "PASS\n" : "FAIL\n");
    return ok ? 0 : 1;
}
