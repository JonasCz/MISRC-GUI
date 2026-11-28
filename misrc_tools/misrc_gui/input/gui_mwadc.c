/*
 * MISRC GUI - Magewell Pro Capture HDMI / DVI / AIO raw-ADC backend (mwadc)
 *
 * Maintained in the mwadc repository (https://github.com/JonasCz/mwadc, misrc-gui/)
 * and copied here by its scripts/build-all.ps1: make changes there.
 *
 * mwadc_ffi.dll does the card work (arm, DMA thread, front end). Its frame
 * callback runs on the DLL's sink thread with one whole frame (cx * cy samples,
 * RG10 words or Y16); this file converts the two selected ADC fields into the
 * hsdaoh 32-bit capture format in one linear pass, straight into
 * BUF_CAPTURE_RF, and the shared extraction thread does the rest (display,
 * record, stats), as for the RTL-SDR backend.
 *
 * The card's analog line-in (stereo, 48 kHz, 24 bit) arrives through a second
 * DLL callback on its own thread, resampled by the DLL onto the RF sample clock
 * (48000 samples per 40 M RF samples at 40 MSPS, sample 0 = RF sample 0). The
 * frame callback moves it to BUF_CAPTURE_AUDIO as CH1/CH2 behind the RF it
 * belongs to, so the audio monitor, meters and WAV/FLAC outputs work as for the
 * other backends and a recording's audio is cut at the RF cut (see "Line-in").
 * CH3/CH4 do not exist on this source (gui_audio_capture_channel_mask).
 */

#ifdef ENABLE_MWADC

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <stdatomic.h>
#include <math.h>
#include <wchar.h>

#include "gui_mwadc.h"
#include "../../../third_party/mwadc/ffi/include/mwadc.h"
#include "../core/gui_app.h"
#include "../processing/gui_extract.h"
#include "../processing/gui_display_thread.h"
#include "../output/gui_record.h"
#include "../ui/gui_popup.h"
#include "../net/gui_monitor_audio.h"
#include "../../common/buffer_manager.h"
#include "../../common/threading.h"

extern atomic_int do_exit;

// Win32 without windows.h (raylib name clashes), as common/threading.h does.
extern __declspec(dllimport) void *__stdcall LoadLibraryW(const wchar_t *);
extern __declspec(dllimport) void *__stdcall GetProcAddress(void *, const char *);
extern __declspec(dllimport) void __stdcall AcquireSRWLockExclusive(void *);
extern __declspec(dllimport) void __stdcall ReleaseSRWLockExclusive(void *);

//-----------------------------------------------------------------------------
// Tables
//-----------------------------------------------------------------------------

// Which jack each ADC can reach (docs/13 §2 of the mwadc project) and which cards have it (docs/15),
// in UI order: by jack, then by ADC. The index is the saved setting, so new rows go at the end.
// A saved input the card does not have falls back to the card's first input (CH A) or off (CH B).
#define MW_HDMI_AIO (MWADC_CARD_HDMI | MWADC_CARD_AIO)
#define MW_DVI_AIO  (MWADC_CARD_DVI | MWADC_CARD_AIO)
const mwadc_gui_input_t gui_mwadc_inputs[] = {
    { "CVBS (\xE2\x86\x92 ADC0)",         0, "cvbs",    MW_HDMI_AIO },
    { "CVBS (\xE2\x86\x92 ADC1)",         1, "cvbs",    MW_HDMI_AIO },
    { "Component Y (\xE2\x86\x92 ADC0)",  0, "comp-y",  MW_HDMI_AIO },
    { "Component Y (\xE2\x86\x92 ADC1)",  1, "comp-y",  MW_HDMI_AIO },
    { "Component Y (\xE2\x86\x92 ADC2)",  2, "comp-y",  MW_HDMI_AIO },
    { "Component Pb (\xE2\x86\x92 ADC2)", 2, "comp-pb", MW_HDMI_AIO },
    { "Component Pr (\xE2\x86\x92 ADC1)", 1, "comp-pr", MW_HDMI_AIO },
    { "Component Pr (\xE2\x86\x92 ADC2)", 2, "comp-pr", MW_HDMI_AIO },
    { "S-video Y (\xE2\x86\x92 ADC0)",    0, "yc-y",    MW_HDMI_AIO },
    { "S-video C (\xE2\x86\x92 ADC2)",    2, "yc-c",    MW_HDMI_AIO },
    { "VGA G (\xE2\x86\x92 ADC0)",        0, "vga-g",   MW_DVI_AIO  },
    { "VGA R (\xE2\x86\x92 ADC1)",        1, "vga-r",   MW_DVI_AIO  },
    { "VGA B (\xE2\x86\x92 ADC2)",        2, "vga-b",   MW_DVI_AIO  },
};
const int gui_mwadc_input_count = (int)(sizeof(gui_mwadc_inputs) / sizeof(gui_mwadc_inputs[0]));

const int gui_mwadc_aa_choices[] = { 0, 10, 16, 27, 41, 59, 91, 145 };
const int gui_mwadc_input_offset_choices[] = { 120, 60, 0, -60 };
const int gui_mwadc_input_offset_choice_count = 4;

const int gui_mwadc_aa_choice_count = (int)(sizeof(gui_mwadc_aa_choices) / sizeof(gui_mwadc_aa_choices[0]));

// Up to 100 MSPS with two channels (RG10, all three ADCs in 4 bytes/sample: the PCIe Gen2 x1 limit);
// 120-160 MSPS only for one channel on ADC0 (Y16, 2 bytes/sample). mwadc docs/11 §4.
const uint32_t gui_mwadc_rate_choices[] = { 20000000u, 28636363u, 35795454u, 40000000u, 50000000u, 64000000u,
                                            80000000u, 100000000u, 120000000u, 140000000u, 160000000u };
const int gui_mwadc_rate_choice_count = (int)(sizeof(gui_mwadc_rate_choices) / sizeof(gui_mwadc_rate_choices[0]));

//-----------------------------------------------------------------------------
// DLL loading
//-----------------------------------------------------------------------------

static struct {
    uint32_t     (*api_version)(void);
    const char  *(*version)(void);
    const char  *(*last_error)(void);
    void         (*set_log)(mwadc_log_fn, void *, int);
    int          (*list)(const char *, mwadc_device_info *, int);
    mwadc_config *(*config_new)(void);
    void         (*config_free)(mwadc_config *);
    int          (*config_set)(mwadc_config *, const char *, const char *);
    mwadc_device *(*open)(const char *, int, int);
    void         (*close)(mwadc_device *);
    int          (*arm)(mwadc_device *, const mwadc_config *, mwadc_stream_info *);
    int          (*start)(mwadc_device *, uint32_t, mwadc_frame_fn, void *);
    int          (*stop)(mwadc_device *);
    int          (*restore)(mwadc_device *);
    int          (*audio_start)(mwadc_device *, uint32_t, mwadc_audio_fn, void *, mwadc_audio_info *);
    int          (*audio_stop)(mwadc_device *);
} mw;

static int s_load_state = 0; // 0 = not tried, 1 = loaded, -1 = unavailable

static void mwadc_log_cb(void *user, int level, const char *line) {
    (void)user;
    fprintf(stderr, "[MWADC]%s %s\n", level >= MWADC_LOG_WARN ? " !" : "", line);
}

bool gui_mwadc_available(void) {
    if (s_load_state) return s_load_state > 0;
    s_load_state = -1;
    void *h = LoadLibraryW(L"mwadc_ffi.dll");
    if (!h) return false;
#define MW_LOAD(field, name)                                         \
    do {                                                              \
        void *p_ = GetProcAddress(h, name);                           \
        memcpy(&mw.field, &p_, sizeof p_);                            \
        if (!mw.field) {                                              \
            fprintf(stderr, "[MWADC] mwadc_ffi.dll lacks %s\n", name); \
            return false;                                             \
        }                                                             \
    } while (0)
    MW_LOAD(api_version, "mwadc_api_version");
    MW_LOAD(version, "mwadc_version");
    MW_LOAD(last_error, "mwadc_last_error");
    MW_LOAD(set_log, "mwadc_set_log");
    MW_LOAD(list, "mwadc_list");
    MW_LOAD(config_new, "mwadc_config_new");
    MW_LOAD(config_free, "mwadc_config_free");
    MW_LOAD(config_set, "mwadc_config_set");
    MW_LOAD(open, "mwadc_open");
    MW_LOAD(close, "mwadc_close");
    MW_LOAD(arm, "mwadc_arm");
    MW_LOAD(start, "mwadc_start");
    MW_LOAD(stop, "mwadc_stop");
    MW_LOAD(restore, "mwadc_restore");
    MW_LOAD(audio_start, "mwadc_audio_start");
    MW_LOAD(audio_stop, "mwadc_audio_stop");
#undef MW_LOAD
    if (mw.api_version() != MWADC_API_VERSION) {
        fprintf(stderr, "[MWADC] mwadc_ffi.dll API %u, expected %u\n", mw.api_version(), MWADC_API_VERSION);
        return false;
    }
    mw.set_log(mwadc_log_cb, NULL, MWADC_LOG_INFO);
    fprintf(stderr, "[MWADC] mwadc_ffi.dll %s loaded\n", mw.version());
    s_load_state = 1;
    return true;
}

//-----------------------------------------------------------------------------
// State
//-----------------------------------------------------------------------------

static void *s_lock;               // SRWLOCK (pointer-sized, zero = unlocked)
static mwadc_device *s_dev;        // open card (kept armed between captures)
static int s_dev_board = -1;
static atomic_bool s_running;
static atomic_bool s_audio_running;
// When the card was last fully armed (inputs / rate / format), for the settling hint: inputs with
// the clamp off drift for 1-2 minutes after arming. 0 = not armed.
static atomic_uint_fast64_t s_armed_ms;
static char s_arm_key[64];

// Stream state read by the frame callback (set before mwadc_start).
static mwadc_stream_info s_info;
static int s_adc_a = 0, s_adc_b = -1;
// Input / rate settings the running stream was armed with (live updates keep them).
static int s_run_input_a, s_run_input_b;
static uint32_t s_run_rate_hz;
static gui_app_t *s_app;

// Line-in, RF-locked by the DLL (MWADC_AUDIO_LOCK_RF): locked sample n belongs to RF sample
// n * rf_hz / 48000 of the stream. Blocks wait in this FIFO (audio thread -> frame callback) until
// the frame callback has written the RF they belong to, and only then go to BUF_CAPTURE_AUDIO. So
// that ring holds round(RF samples in BUF_CAPTURE_RF * 48000 / rf_hz) samples after every frame
// and never runs ahead of the RF: a recording cut at an RF byte is cut at the matching audio
// sample (gui_mwadc_line_cursor). Both callbacks pump, so a sample leaves as soon as it and its RF
// are in: the line-in keeps the RF's timing (a frame's audio goes out with or just after the frame).
// Longer than the frame pool plus a frame (MW_POOL_SECONDS + 0.1 s), so line-in waiting for RF held
// up in the pool is kept, not replaced by silence.
#define LINE_FIFO_FRAMES 262144u // 5.5 s at 48 kHz; power of two
static int32_t s_line_fifo[LINE_FIFO_FRAMES * 2];
static atomic_uint_fast64_t s_line_wr; // locked position after the last queued sample (audio thread)
static atomic_uint_fast64_t s_line_rd; // next locked position to take (frame callback)
static atomic_bool s_line_on;          // FIFO and BUF_CAPTURE_AUDIO reset, pump running
static uint32_t s_line_rf_hz;          // RF rate the line-in is locked to (the configured rate)
static uint32_t s_line_hz;             // line-in rate (48000)
// Under s_pump_lock: RF samples on the RF timeline (skipped frames included), RF samples written to
// BUF_CAPTURE_RF, line-in samples written to BUF_CAPTURE_AUDIO. The pump runs from both the frame
// callback (new RF) and the audio callback (new line-in), so line-in goes out as soon as both its
// RF and its samples are in, never a frame late.
static uint64_t s_rf_timeline, s_rf_written, s_line_written;
static void *s_pump_lock; // SRWLOCK
static void line_pump(gui_app_t *app);

static void lock(void) { AcquireSRWLockExclusive(&s_lock); }
static void unlock(void) { ReleaseSRWLockExclusive(&s_lock); }
static void pump_lock(void) { AcquireSRWLockExclusive(&s_pump_lock); }
static void pump_unlock(void) { ReleaseSRWLockExclusive(&s_pump_lock); }

// Card kind (MWADC_CARD_*) of each rotary-switch number, from the last enumeration.
static unsigned s_card_of_board[16];

unsigned gui_mwadc_card_of_product(unsigned product_id) {
    switch (product_id) {
    case 0x104: return MWADC_CARD_HDMI;
    case 0x103: return MWADC_CARD_DVI;
    case 0x102: return MWADC_CARD_AIO;
    default:    return 0; // no ADV7842 analog front end
    }
}

static bool s_sdk_missing;
static char s_missing_card[48];

bool gui_mwadc_sdk_missing(void) { return s_sdk_missing; }
const char *gui_mwadc_missing_card(void) { return s_missing_card; }

void gui_mwadc_show_sdk_missing(void) {
    char msg[512];
    snprintf(msg, sizeof msg,
             "A Magewell %s was found, but the Magewell runtime (LibMWCapture.dll) needed to use it "
             "is not installed.\n\n"
             "Download and install MWCaptureRT.exe from:\n"
             "https://github.com/jonasCz/mwadc/releases\n\n"
             "Then restart MISRC GUI.",
             s_missing_card[0] ? s_missing_card : "Pro Capture card");
    gui_popup_info("Magewell runtime not installed", msg);
}

// A supported Magewell card (PCI 1CD7:0002/0003/0004) on the bus, found without the SDK from the
// Windows device tree (cfgmgr32; only the driver is needed). Writes its model name; false if none.
static bool pci_find_card(char *name, size_t n) {
    void *h = LoadLibraryW(L"cfgmgr32.dll");
    if (!h) return false;
    unsigned long (__stdcall *size_fn)(unsigned long *, const char *, unsigned long);
    unsigned long (__stdcall *list_fn)(const char *, char *, unsigned long, unsigned long);
    void *p = GetProcAddress(h, "CM_Get_Device_ID_List_SizeA");
    memcpy(&size_fn, &p, sizeof p);
    p = GetProcAddress(h, "CM_Get_Device_ID_ListA");
    memcpy(&list_fn, &p, sizeof p);
    if (!size_fn || !list_fn) return false;
    // CM_GETIDLIST_FILTER_ENUMERATOR (1) | CM_GETIDLIST_FILTER_PRESENT (0x100): present PCI devices.
    const unsigned long flags = 0x101;
    unsigned long len = 0;
    if (size_fn(&len, "PCI", flags) != 0 || len < 2) return false;
    char *buf = malloc(len);
    if (!buf) return false;
    bool found = false;
    if (list_fn("PCI", buf, len, flags) == 0) {
        for (const char *id = buf; *id; id += strlen(id) + 1) {
            unsigned dev;
            if (strncmp(id, "PCI\\VEN_1CD7&DEV_", 17) != 0 || sscanf(id + 17, "%4x", &dev) != 1) continue;
            const char *model = dev == 4 ? "Pro Capture HDMI" : dev == 3 ? "Pro Capture DVI"
                              : dev == 2 ? "Pro Capture AIO" : NULL;
            if (!model) continue;
            snprintf(name, n, "%s", model);
            found = true;
            break;
        }
    }
    free(buf);
    return found;
}

int gui_mwadc_enumerate(mwadc_gui_device_t *devices, int max_devices) {
    s_sdk_missing = false;
    s_missing_card[0] = 0;
    if (!gui_mwadc_available()) return 0;
    mwadc_device_info info[8];
    int n = mw.list(NULL, info, 8);
    if (n < 0) {
        const char *err = mw.last_error();
        fprintf(stderr, "[MWADC] list: %s\n", err);
        // mwadc's "SDK runtime not found" error names the DLL; other failures do not.
        // Only worth telling the user when a supported card is actually in the machine.
        s_sdk_missing = strstr(err, "LibMWCapture.dll") != NULL &&
                        pci_find_card(s_missing_card, sizeof s_missing_card);
        return 0;
    }
    int out = 0;
    for (int i = 0; i < n && i < 8 && out < max_devices; i++) {
        const unsigned card = gui_mwadc_card_of_product(info[i].product_id);
        if (!card) continue; // Pro Capture HDMI, DVI and AIO only (ADV7842)
        s_card_of_board[info[i].board & 15] = card;
        mwadc_gui_device_t *d = &devices[out++];
        d->board = info[i].board;
        d->card = card;
        d->sdk_index = info[i].index;
        snprintf(d->name, sizeof(d->name), "%s (board %X)", info[i].product, info[i].board);
        snprintf(d->serial, sizeof(d->serial), "MWADC_%s", info[i].serial);
    }
    // The SDK's enumeration order is not stable between calls: list by board number.
    for (int i = 1; i < out; i++)
        for (int j = i; j > 0 && devices[j - 1].board > devices[j].board; j--) {
            mwadc_gui_device_t t = devices[j];
            devices[j] = devices[j - 1];
            devices[j - 1] = t;
        }
    return out;
}

//-----------------------------------------------------------------------------
// Configuration from settings
//-----------------------------------------------------------------------------

static unsigned card_of_board(int board) {
    const unsigned c = (board >= 0 && board < 16) ? s_card_of_board[board] : 0;
    return c ? c : MWADC_CARD_HDMI;
}

// The card the settings panel shows: the selected one (else the open one).
static unsigned current_card(const gui_app_t *app) {
    if (app && app->selected_device >= 0 && app->selected_device < app->device_count &&
        app->devices[app->selected_device].type == DEVICE_TYPE_MWADC)
        return card_of_board(app->devices[app->selected_device].index);
    return card_of_board(s_dev_board);
}

bool gui_mwadc_input_on_card(const gui_app_t *app, int i) {
    return i >= 0 && i < gui_mwadc_input_count && (gui_mwadc_inputs[i].cards & current_card(app));
}

static int clamp_input(int i, unsigned card) {
    return (i >= 0 && i < gui_mwadc_input_count && (gui_mwadc_inputs[i].cards & card)) ? i : -1;
}

// Effective CH A / CH B input indexes on `card` (B = -1 when off, not on the card, or on the same
// ADC as A; A falls back to the card's first input).
static void effective_inputs(unsigned card, const gui_settings_t *s, int *ia, int *ib) {
    *ia = clamp_input(s->mwadc_input_a, card);
    for (int i = 0; *ia < 0 && i < gui_mwadc_input_count; i++) *ia = clamp_input(i, card);
    if (*ia < 0) *ia = 0;
    *ib = clamp_input(s->mwadc_input_b, card);
    if (*ib >= 0 && gui_mwadc_inputs[*ib].adc == gui_mwadc_inputs[*ia].adc) *ib = -1;
}

void gui_mwadc_effective_inputs(const gui_app_t *app, int *ia, int *ib) {
    effective_inputs(current_card(app), &app->settings, ia, ib);
}

static uint32_t max_rate_of(unsigned card, const gui_settings_t *s) {
    int ia, ib;
    effective_inputs(card, s, &ia, &ib);
    return (ib < 0 && gui_mwadc_inputs[ia].adc == 0) ? 160000000u : 100000000u;
}

static uint32_t effective_rate_of(unsigned card, const gui_settings_t *s) {
    uint32_t r = s->mwadc_rate_hz ? s->mwadc_rate_hz : 40000000u;
    uint32_t m = max_rate_of(card, s);
    return r > m ? m : r;
}

uint32_t gui_mwadc_max_rate(const gui_app_t *app) { return max_rate_of(current_card(app), &app->settings); }
uint32_t gui_mwadc_effective_rate(const gui_app_t *app) { return effective_rate_of(current_card(app), &app->settings); }

#define MW_LINES "2048"

// Host frame pool (between the DLL's DMA thread and the frame callback): about MW_POOL_SECONDS of
// frames, within MW_POOL_MAX_BYTES of pinned RAM, never fewer than MW_POOL_MIN.
#define MW_POOL_SECONDS   1.8 // under the GUI's 2 s no-callback timeout (misrc_gui.c)
#define MW_POOL_MAX_BYTES (512ull << 20)
#define MW_POOL_MIN       8u

static uint32_t pool_frames(const mwadc_stream_info *si) {
    const double spf = (double)si->cx * si->cy;
    const uint64_t bytes = si->frame_bytes ? si->frame_bytes : (uint64_t)si->stride * si->cy;
    if (spf <= 0 || si->sample_rate_hz <= 0 || !bytes) return MW_POOL_MIN;
    uint64_t n = (uint64_t)ceil(MW_POOL_SECONDS * si->sample_rate_hz / spf);
    const uint64_t cap = MW_POOL_MAX_BYTES / bytes;
    if (n > cap) n = cap;
    if (n < MW_POOL_MIN) n = MW_POOL_MIN;
    return (uint32_t)n;
}

// Build the mwadc configuration for the current settings; returns NULL on error.
// `setup` supplies the inputs and rate (the settings, or the running ones for a live update).
static mwadc_config *build_config(const gui_app_t *app, const gui_settings_t *setup, int *adc_a, int *adc_b) {
    const gui_settings_t *s = &app->settings;
    int ia, ib;
    const unsigned card = card_of_board(s_dev_board); // the card is open when this runs
    effective_inputs(card, setup, &ia, &ib); // one ADC per channel
    mwadc_config *c = mw.config_new();
    if (!c) return NULL;
    char v[64];
    const char *pins[3] = { "off", "off", "off" };
    pins[gui_mwadc_inputs[ia].adc] = gui_mwadc_inputs[ia].pin;
    if (ib >= 0) pins[gui_mwadc_inputs[ib].adc] = gui_mwadc_inputs[ib].pin;
    int ok = 0;
    ok |= mw.config_set(c, "adc0", pins[0]);
    ok |= mw.config_set(c, "adc1", pins[1]);
    ok |= mw.config_set(c, "adc2", pins[2]);
    const uint32_t rate = effective_rate_of(card, setup);
    snprintf(v, sizeof(v), "%u", rate);
    ok |= mw.config_set(c, "rate", v);
    // Above 100 MSPS only ADC0 alone fits the PCIe link: Y16 format (2 bytes/sample).
    ok |= mw.config_set(c, "format", rate > 100000000u ? "y16" : "rgb10");
    // 2048 lines, the card's maximum: the 8-slot framestore then holds the most time (~840 ms at
    // 40 MSPS, 105 ms frames of 16.8 MB) for a DMA thread that the system holds up. Costs coarser
    // display updates; the line-in does not wait for frames (see line_pump), so A/V stays aligned.
    ok |= mw.config_set(c, "lines", MW_LINES);
    ok |= mw.config_set(c, "clamp", "off"); // always: on distorts low frequencies (docs/14 §5a)
    // Levels are set by hand (settings sliders); stored calibrations are ignored.
    ok |= mw.config_set(c, "calibration", "off");
    if (s->mwadc_aa_mhz > 0) {
        snprintf(v, sizeof(v), "%d", s->mwadc_aa_mhz);
        ok |= mw.config_set(c, "aa_bw", v);
        ok |= mw.config_set(c, "aa", "on");
    } else {
        ok |= mw.config_set(c, "aa", "off");
    }
    // Levels in signal order: analog gain (AFE 0x0C, shared), then centre and CP gain per ADC.
    snprintf(v, sizeof(v), "%d", gui_mwadc_afe_gain_of(app));
    ok |= mw.config_set(c, "afe_gain", v);
    snprintf(v, sizeof(v), "%d", gui_mwadc_input_offset_of(app));
    ok |= mw.config_set(c, "input_offset", v);
    for (int adc = 0; adc < 3; adc++) {
        char key[12];
        snprintf(key, sizeof(key), "gain%d", adc);
        snprintf(v, sizeof(v), "%.6f", gui_mwadc_gain_of(app, adc) / 256.0);
        ok |= mw.config_set(c, key, v);
        snprintf(key, sizeof(key), "centre%d", adc);
        snprintf(v, sizeof(v), "%.2f", gui_mwadc_centre_of(app, adc) / 4.0);
        ok |= mw.config_set(c, key, v);
    }
    if (ok != 0) fprintf(stderr, "[MWADC] config: %s\n", mw.last_error());
    *adc_a = gui_mwadc_inputs[ia].adc;
    *adc_b = ib >= 0 ? gui_mwadc_inputs[ib].adc : -1;
    return c;
}

static int ensure_open(gui_app_t *app, int board) {
    if (s_dev && s_dev_board == board) return 0;
    if (s_dev) {
        mw.close(s_dev);
        s_dev = NULL;
    }
    atomic_store(&s_armed_ms, 0); // a newly opened card is armed (and settles) from scratch
    s_dev = mw.open(NULL, board, -1);
    if (!s_dev) {
        char msg[200];
        snprintf(msg, sizeof(msg), "Magewell: %s", mw.last_error());
        gui_app_set_status(app, msg);
        return -1;
    }
    s_dev_board = board;
    return 0;
}

//-----------------------------------------------------------------------------
// Frame callback (mwadc sink thread)
//-----------------------------------------------------------------------------

// hsdaoh capture word: A in bits 0-11, B in 20-31, each stored as 2047 - sample.
static inline uint32_t pack(int a, int b) {
    return (uint32_t)((2047 - a) & 0xFFF) | ((uint32_t)((2047 - b) & 0xFFF) << 20);
}

// Samples converted per BUF_CAPTURE_RF write: 256 KiB out, stays in L2 with its source.
#define MW_CHUNK 65536u

// h:mm:ss.mmm of a sample index.
static void fmt_time(char *out, size_t n, uint64_t sample, double rate) {
    const double t = rate > 0 ? sample / rate : 0.0;
    const uint64_t ms = (uint64_t)(t * 1000.0 + 0.5);
    snprintf(out, n, "%llu:%02llu:%02llu.%03llu", (unsigned long long)(ms / 3600000), (unsigned long long)(ms / 60000 % 60),
             (unsigned long long)(ms / 1000 % 60), (unsigned long long)(ms % 1000));
}

// A skipped or torn frame, logged to stderr and the recording's session log (*_misrc_capture.log)
// with where it sits: the capture sample and, while recording, the sample of the output files (one
// per RF sample, per channel) at which the RF after the gap starts. The files are not padded: the
// samples before and after that point are `lost` RF samples apart in real time.
static void log_frame_loss(gui_app_t *app, const mwadc_frame *f, uint64_t at) {
    const double rate = s_info.sample_rate_hz;
    const uint64_t lost = (uint64_t)f->gap_frames * s_info.cx * s_info.cy;
    char msg[400], t_cap[32], t_rec[32];
    fmt_time(t_cap, sizeof(t_cap), at, rate);
    int n;
    if (f->gap_frames)
        n = snprintf(msg, sizeof(msg), "Magewell: %u frame(s) skipped before frame %llu: %llu RF samples (%.1f ms) missing at capture sample %llu (%s)",
                     f->gap_frames, (unsigned long long)f->seq, (unsigned long long)lost, rate > 0 ? lost / rate * 1e3 : 0.0,
                     (unsigned long long)at, t_cap);
    else
        n = snprintf(msg, sizeof(msg), "Magewell: frame %llu torn (slot overwritten while read) at capture sample %llu (%s)",
                     (unsigned long long)f->seq, (unsigned long long)at, t_cap);
    uint64_t start_byte;
    if (n > 0 && (size_t)n < sizeof(msg) && app->is_recording && gui_extract_record_start_byte(&start_byte) &&
        at >= start_byte / sizeof(uint32_t)) {
        const uint64_t rec = at - start_byte / sizeof(uint32_t);
        fmt_time(t_rec, sizeof(t_rec), rec, rate);
        snprintf(msg + n, sizeof(msg) - (size_t)n, "; recording sample %llu (%s)", (unsigned long long)rec, t_rec);
    }
    fprintf(stderr, "[MWADC] %s\n", msg);
    gui_record_log_capture_event(app, "ERROR", msg, GUI_ERROR_CLASS_NONE, 0);
}

static void mwadc_frame_cb(void *user, const mwadc_frame *f) {
    gui_app_t *app = (gui_app_t *)user;
    if (!atomic_load(&s_running) || atomic_load(&do_exit)) return;
    atomic_fetch_add(&app->frame_count, 1);
    if (f->gap_frames || f->torn) {
        if (f->gap_frames) atomic_fetch_add(&app->missed_frame_count, f->gap_frames);
        gui_record_mark_source_loss(); // a recording across it is flagged incomplete
        pump_lock();
        const uint64_t at = s_rf_written;
        pump_unlock();
        log_frame_loss(app, f, at);
    }
    uint64_t written = 0;

    const mwadc_stream_info *si = &s_info;
    const size_t cx = si->cx, cy = si->cy;
    const mwadc_field *fa = &si->field[s_adc_a];
    const mwadc_field *fb = s_adc_b >= 0 ? &si->field[s_adc_b] : NULL;
    // Extraction swaps A/B in MISRC mode, but only when channel B exists; pre-swap so CH A stays the
    // first selected input. With one channel there is no swap: A must stay in the A slot.
    bool swap = fb && (app->is_recording ? app->capture_mode_runtime_misrc : app->user_capture_mode_misrc);
    if (swap && app->settings.misrc_v15_v25_ab_swap) swap = false;

    const uint32_t sa = fa->lsb, ma = (1u << fa->bits) - 1;
    const uint32_t sb = fb ? fb->lsb : 0, mb = fb ? (1u << fb->bits) - 1 : 0;
    const bool y16 = si->format == MWADC_FORMAT_Y16;

    size_t line = 0;
    while (line < cy) {
        size_t lines = MW_CHUNK / cx;
        if (lines < 1) lines = 1;
        if (lines > cy - line) lines = cy - line;
        const size_t n = lines * cx;
        uint32_t *out = bufmgr_write_begin(&app->buffers, BUF_CAPTURE_RF, n * sizeof(uint32_t), NULL);
        if (!out) {
            atomic_fetch_add(&app->rb_drop_count, 1);
            line += lines;
            continue;
        }
        uint32_t *o = out;
        for (size_t y = line; y < line + lines; y++) {
            const uint8_t *row = f->data + y * si->stride;
            if (y16) {
                const uint16_t *p = (const uint16_t *)row;
                for (size_t x = 0; x < cx; x++) {
                    int a = ((int)(p[x] >> 6) - 512) * 4; // 10-bit code -> 12-bit signed
                    *o++ = pack(a, 0);
                }
            } else {
                const uint32_t *p = (const uint32_t *)row;
                for (size_t x = 0; x < cx; x++) {
                    const uint32_t w = p[x];
                    int a = ((int)((w >> sa) & ma) - 512) * 4;
                    int b = fb ? ((int)((w >> sb) & mb) - 512) * 4 : 0;
                    *o++ = swap ? pack(b, a) : pack(a, b);
                }
            }
        }
        bufmgr_write_end(&app->buffers, BUF_CAPTURE_RF, n * sizeof(uint32_t));
        written += n;
        line += lines;
    }
    pump_lock();
    s_rf_timeline += (uint64_t)(f->gap_frames + 1u) * cx * cy;
    s_rf_written += written;
    line_pump(app);
    pump_unlock();
    atomic_store(&app->stream_synced, true);
    atomic_store(&app->last_callback_time_ms, get_time_ms());
}

//-----------------------------------------------------------------------------
// Line-in callback (mwadc audio thread, 192 stereo samples every 4 ms)
//-----------------------------------------------------------------------------

static void mwadc_audio_cb(void *user, const mwadc_audio_block *b) {
    gui_app_t *app = (gui_app_t *)user;
    if (!atomic_load(&s_audio_running) || atomic_load(&do_exit)) return;
    if (b->missed) gui_record_mark_audio_source_loss();
    if (!b->locked || !b->frames) return;
    static uint32_t warned_hz;
    const uint32_t rf_hz = (uint32_t)(b->rf_rate_hz + 0.5);
    if (rf_hz != s_line_rf_hz && rf_hz != warned_hz) {
        warned_hz = rf_hz;
        fprintf(stderr, "[MWADC] line-in locked to %u Hz, expected %u Hz\n", rf_hz, s_line_rf_hz);
    }
    uint64_t wr = atomic_load_explicit(&s_line_wr, memory_order_relaxed);
    const uint64_t rd = atomic_load_explicit(&s_line_rd, memory_order_acquire);
    const int32_t *src = b->samples;
    uint64_t pos = b->position, n = b->frames;
    if (pos < wr) { // overlap (not expected: the DLL's positions are contiguous)
        if (wr - pos >= n) return;
        src += 2 * (wr - pos);
        n -= wr - pos;
        pos = wr;
    }
    // Silence for a jump forward, and nothing when full (the RF stalled): the pump keeps the
    // positions right either way.
    for (; wr < pos && wr - rd < LINE_FIFO_FRAMES; wr++)
        s_line_fifo[2 * (wr & (LINE_FIFO_FRAMES - 1))] = s_line_fifo[2 * (wr & (LINE_FIFO_FRAMES - 1)) + 1] = 0;
    if (wr == pos)
        for (uint64_t i = 0; i < n && wr - rd < LINE_FIFO_FRAMES; i++, wr++)
            memcpy(&s_line_fifo[2 * (wr & (LINE_FIFO_FRAMES - 1))], src + 2 * i, 2 * sizeof(int32_t));
    atomic_store_explicit(&s_line_wr, wr, memory_order_release);
    // Line-in whose RF is already written goes out now, not with the next frame: with long frames
    // the samples at a frame's end arrive just after its callback.
    if (app) {
        pump_lock();
        line_pump(app);
        pump_unlock();
    }
}

static uint64_t line_count(uint64_t rf) {
    return (rf * s_line_hz + s_line_rf_hz / 2) / s_line_rf_hz;
}

// Move the line-in that belongs to the RF written so far into BUF_CAPTURE_AUDIO (s_pump_lock held;
// called by the frame callback after new RF and by the audio callback after new line-in).
static void line_pump(gui_app_t *app) {
    if (!atomic_load_explicit(&s_line_on, memory_order_acquire)) return;
    const uint64_t need = line_count(s_rf_written) - s_line_written;
    if (!need) return;
    // These ring samples are the newest `need` locked samples up to the RF timeline's end: the
    // line-in of skipped or dropped RF frames is left out, like the RF itself.
    const uint64_t end = line_count(s_rf_timeline);
    const uint64_t pos = end > need ? end - need : 0;
    const uint64_t wr = atomic_load_explicit(&s_line_wr, memory_order_acquire);
    uint64_t rd = atomic_load_explicit(&s_line_rd, memory_order_relaxed);
    if (rd < pos) rd = pos < wr ? pos : wr; // drop what belongs to RF that is not in the ring
    // Samples the FIFO no longer has (it overflowed, or the lock started late) become silence.
    const uint64_t zeros = rd > pos ? (rd - pos < need ? rd - pos : need) : 0;
    const uint64_t last = wr < end ? wr : end;
    uint64_t take = last > rd ? last - rd : 0;
    if (take > need - zeros) take = need - zeros;
    if (!zeros && !take) {
        atomic_store_explicit(&s_line_rd, rd, memory_order_release);
        return; // not here yet: the audio callback pumps it when it arrives
    }
    const size_t bytes = (size_t)(zeros + take) * 12;
    uint8_t *out = (uint8_t *)bufmgr_write_begin(&app->buffers, BUF_CAPTURE_AUDIO, bytes, NULL);
    if (!out) { // retried with the next frame, so ring positions stay those of the RF
        atomic_fetch_add(&app->rb_drop_count, 1);
        atomic_store_explicit(&s_line_rd, rd, memory_order_release);
        return;
    }
    // BUF_CAPTURE_AUDIO frame: 4 x s24le. The samples are left-justified 24-bit in int32, so the
    // sample is the word's top three bytes. CH3/CH4 stay 0 (no such inputs on this card).
    memset(out, 0, (size_t)zeros * 12);
    for (uint64_t i = 0; i < take; i++) {
        const int32_t *v = &s_line_fifo[2 * ((rd + i) & (LINE_FIFO_FRAMES - 1))];
        uint8_t *d = out + (size_t)(zeros + i) * 12;
        for (int c = 0; c < 2; c++) {
            const uint32_t w = (uint32_t)v[c];
            d[c * 3 + 0] = (uint8_t)(w >> 8);
            d[c * 3 + 1] = (uint8_t)(w >> 16);
            d[c * 3 + 2] = (uint8_t)(w >> 24);
        }
        memset(d + 6, 0, 6);
    }
    atomic_store_explicit(&s_line_rd, rd + take, memory_order_release);
    s_line_written += zeros + take;
    bufmgr_write_end(&app->buffers, BUF_CAPTURE_AUDIO, bytes);
    bufmgr_signal_data(&app->buffers, BUF_CAPTURE_AUDIO);
    gui_monitor_audio_server_tap(app, out, bytes);
}

bool gui_mwadc_line_cursor(const gui_app_t *app, uint64_t rf_byte, size_t *cursor) {
    if (!app || !atomic_load(&s_line_on)) return false;
    const ringbuffer_t *ring = &app->buffers.buffers[BUF_CAPTURE_AUDIO];
    *cursor = (size_t)(line_count(rf_byte / sizeof(uint32_t)) * 12 % (2 * (uint64_t)ring->buffer_size));
    return true;
}

// Start the line-in into BUF_CAPTURE_AUDIO (lock held, card open, RF streaming). Failure leaves the
// RF capture running. The DLL pads its output with silence back to RF sample 0, so starting the
// line-in a moment after the RF stream still lines BUF_CAPTURE_AUDIO up with BUF_CAPTURE_RF.
static void start_line_audio(gui_app_t *app, uint32_t rf_hz) {
    atomic_store(&s_line_on, false);
    if (bufmgr_ensure_init(&app->buffers, BUF_CAPTURE_AUDIO) != 0) {
        fprintf(stderr, "[MWADC] line-in: audio buffer unavailable\n");
        return;
    }
    bufmgr_reset(&app->buffers, BUF_CAPTURE_AUDIO);
    atomic_store(&s_line_wr, 0);
    atomic_store(&s_line_rd, 0);
    s_line_rf_hz = rf_hz;
    s_line_hz = 48000;
    atomic_store(&s_audio_running, true);
    mwadc_audio_info ai;
    if (mw.audio_start(s_dev, MWADC_AUDIO_LOCK_RF, mwadc_audio_cb, app, &ai) < 0) {
        fprintf(stderr, "[MWADC] line-in: %s\n", mw.last_error());
        atomic_store(&s_audio_running, false);
        return;
    }
    // gui_audio sizes the monitor resampler and WAV headers from this rate (48 kHz nominal: the
    // locked stream has exactly that many samples per RF second).
    atomic_store(&app->audio_sample_rate, ai.sample_rate);
    s_line_hz = ai.sample_rate;
    // The pump (frame and audio callbacks, under s_pump_lock) owns s_line_written once it runs.
    pump_lock();
    s_line_written = 0;
    atomic_store_explicit(&s_line_on, true, memory_order_release);
    pump_unlock();
    fprintf(stderr, "[MWADC] line-in: %u Hz, %u bit, locked to the RF clock -> audio CH1 (L) / CH2 (R)\n", ai.sample_rate, ai.bits_per_sample);
}

//-----------------------------------------------------------------------------
// Start / stop
//-----------------------------------------------------------------------------

static void reset_counters(gui_app_t *app) {
    bufmgr_reset_stats(&app->buffers, BUF_COUNT);
    atomic_store(&app->total_samples, 0);
    atomic_store(&app->samples_a, 0);
    atomic_store(&app->samples_b, 0);
    atomic_store(&app->frame_count, 0);
    atomic_store(&app->missed_frame_count, 0);
    atomic_store(&app->error_count, 0);
    atomic_store(&app->parser_error_count, 0);
    atomic_store(&app->system_error_count, 0);
    atomic_store(&app->error_count_a, 0);
    atomic_store(&app->error_count_b, 0);
    atomic_store(&app->clip_count_a_pos, 0);
    atomic_store(&app->clip_count_a_neg, 0);
    atomic_store(&app->clip_count_b_pos, 0);
    atomic_store(&app->clip_count_b_neg, 0);
    atomic_store(&app->rb_wait_count, 0);
    atomic_store(&app->rb_drop_count, 0);
    atomic_store(&app->stream_synced, false);
}

int gui_mwadc_start(gui_app_t *app, int board) {
    if (board < 0) { // card listed without the SDK runtime (gui_mwadc_sdk_missing)
        gui_mwadc_show_sdk_missing();
        return -1;
    }
    if (!app || !gui_mwadc_available()) return -1;
    lock();
    if (ensure_open(app, board) < 0) {
        unlock();
        return -1;
    }
    int adc_a, adc_b;
    mwadc_config *c = build_config(app, &app->settings, &adc_a, &adc_b);
    if (!c) {
        unlock();
        return -1;
    }
    gui_app_set_status(app, "Magewell: arming the card...");
    mwadc_stream_info info;
    int r = mw.arm(s_dev, c, &info);
    mw.config_free(c);
    if (r < 0) {
        char msg[200];
        snprintf(msg, sizeof(msg), "Magewell: %s", mw.last_error());
        gui_app_set_status(app, msg);
        unlock();
        return -1;
    }
    s_info = info;
    s_adc_a = adc_a;
    s_adc_b = adc_b;
    s_app = app;
    s_run_input_a = app->settings.mwadc_input_a;
    s_run_input_b = app->settings.mwadc_input_b;
    s_run_rate_hz = app->settings.mwadc_rate_hz;
    {   // A different input / rate re-locks the card: its inputs settle anew.
        const gui_settings_t *s = &app->settings;
        char key[64];
        snprintf(key, sizeof(key), "%d|%d|%d|%u", s_dev_board, s->mwadc_input_a, s->mwadc_input_b,
                 (unsigned)effective_rate_of(card_of_board(s_dev_board), s));
        if (strcmp(key, s_arm_key) != 0 || !atomic_load(&s_armed_ms)) {
            snprintf(s_arm_key, sizeof(s_arm_key), "%s", key);
            atomic_store(&s_armed_ms, get_time_ms());
        }
    }

    reset_counters(app);
    s_rf_timeline = s_rf_written = 0;
    atomic_store(&app->sample_rate, (uint32_t)(info.sample_rate_hz + 0.5));
    app->display_samples_available_a = 0;
    app->display_samples_available_b = 0;
    app->capture_backend_upstream = false;
    app->capture_has_channel_b = adc_b >= 0;
    app->capture_mode_runtime_misrc = app->user_capture_mode_misrc;
    if (bufmgr_ensure_init(&app->buffers, BUF_CAPTURE_RF) != 0) {
        gui_app_set_status(app, "Failed to initialize capture buffer");
        unlock();
        return -1;
    }
    bufmgr_reset(&app->buffers, BUF_CAPTURE_RF);
    atomic_store(&s_running, true);
    app->is_capturing = true;
    if (gui_extract_start(app) < 0) {
        gui_app_set_status(app, "Failed to start extraction");
        atomic_store(&s_running, false);
        app->is_capturing = false;
        unlock();
        return -1;
    }
    if (app->display_thread && gui_display_thread_start(app->display_thread, app, &app->buffers) < 0)
        fprintf(stderr, "[MWADC] display thread failed to start (non-fatal)\n");
    atomic_store(&app->last_callback_time_ms, get_time_ms());
    const uint32_t pool = pool_frames(&info);
    fprintf(stderr, "[MWADC] %ux%u frames (%.1f ms), pool %u frames (%.2f s, %.0f MB)\n", info.cx, info.cy,
            info.cx * (double)info.cy / info.sample_rate_hz * 1e3, pool,
            pool * (double)info.cx * info.cy / info.sample_rate_hz, pool * (double)info.frame_bytes / (1 << 20));
    if (mw.start(s_dev, pool, mwadc_frame_cb, app) < 0) {
        char msg[200];
        snprintf(msg, sizeof(msg), "Magewell: %s", mw.last_error());
        gui_app_set_status(app, msg);
        atomic_store(&s_running, false);
        app->is_capturing = false;
        if (app->display_thread) gui_display_thread_stop(app->display_thread);
        gui_extract_stop();
        unlock();
        return -1;
    }
    // The line-in always runs; the GUI's audio outputs decide what gets written.
    start_line_audio(app, effective_rate_of(card_of_board(s_dev_board), &app->settings));
    unlock();
    char msg[160];
    snprintf(msg, sizeof(msg), "Magewell capture: %.3f MSPS%s", info.sample_rate_hz / 1e6,
             atomic_load(&s_audio_running) ? ", line-in on" : ", line-in failed");
    gui_app_set_status(app, msg);
    return 0;
}

void gui_mwadc_stop(gui_app_t *app) {
    if (!atomic_load(&s_running)) return;
    app->is_capturing = false;
    atomic_store(&s_running, false);
    lock();
    if (s_dev) {
        mw.stop(s_dev); // joins the DLL's frame threads: no frame callback after this
        pump_lock();    // the audio callback may be pumping; none after this
        atomic_store(&s_line_on, false);
        pump_unlock();
        if (atomic_exchange(&s_audio_running, false)) mw.audio_stop(s_dev);
    }
    unlock();
    if (app->display_thread) gui_display_thread_stop(app->display_thread);
    gui_extract_stop();
    atomic_store(&app->stream_synced, false);
    gui_app_set_status(app, "Magewell capture stopped (card stays armed)");
}

bool gui_mwadc_is_running(void) {
    return atomic_load(&s_running);
}

bool gui_mwadc_line_audio_running(void) {
    return atomic_load(&s_audio_running);
}

int gui_mwadc_apply_settings(gui_app_t *app) {
    if (!atomic_load(&s_running) || !s_dev) return 0;
    // The front end (filter, levels) changes live; inputs and rate only at the next start.
    // Build the update with the running inputs and rate, so a pending input / rate change never
    // blocks level changes meanwhile.
    static gui_settings_t live;
    live = app->settings;
    live.mwadc_input_a = s_run_input_a;
    live.mwadc_input_b = s_run_input_b;
    live.mwadc_rate_hz = s_run_rate_hz;
    const bool pending = app->settings.mwadc_input_a != s_run_input_a ||
                         app->settings.mwadc_input_b != s_run_input_b ||
                         effective_rate_of(card_of_board(s_dev_board), &app->settings) !=
                             effective_rate_of(card_of_board(s_dev_board), &live);
    lock();
    int adc_a, adc_b;
    mwadc_config *c = build_config(app, &live, &adc_a, &adc_b);
    if (!c) {
        unlock();
        return -1;
    }
    int rc = pending ? 1 : 0;
    mwadc_stream_info info;
    if (mw.arm(s_dev, c, &info) < 0) {
        fprintf(stderr, "[MWADC] live update: %s\n", mw.last_error());
        rc = -1;
    } else {
        s_info = info;
    }
    mw.config_free(c);
    unlock();
    if (rc < 0) gui_app_set_status(app, "Magewell: live update failed (see log)");
    else if (rc == 1) gui_app_set_status(app, "Magewell: new input / rate applies when capture restarts");
    return rc;
}

//-----------------------------------------------------------------------------
// Levels (no calibration): analog gain -> ADC -> centre -> CP gain (docs/14 §1, §3a)
//-----------------------------------------------------------------------------

// Measured data path, with C = centre (12-bit ADC units) and G = CP gain:
//   ADC0:      out = clamp(512 + G*(adc10 - C/4), 0, 1023)
//   ADC1/ADC2: out = clamp(512 + 2G*(adc10 - C/4), 0, 1022)
// ADC rails at adc10 ~2 and ~1023 on every ADC.
#define MW_ADC_FLOOR 2.0
#define MW_ADC_TOP   1023.0

static int clampi(int v, int lo, int hi) { return v < lo ? lo : v > hi ? hi : v; }

int gui_mwadc_default_gain(int adc) { return adc == 0 ? 0x100 : 0x080; }
// The GUI's offset is the centre seen from the output: offset = 512 - centre, in quarter ADC codes
// (0 = the raw ADC code at the minimum gain).

// The lowest useful gain is the default: it already fits the whole ADC range into the output (ADC0
// 1.0; ADC1/ADC2 0.5, their output being 2x scale). Less only drops codes, it adds no headroom.
int gui_mwadc_gain_min(int adc) { return gui_mwadc_default_gain(adc); }

int gui_mwadc_gain_of(const gui_app_t *app, int adc) {
    if (adc < 0 || adc > 2) return 0x100;
    return clampi(app->settings.mwadc_gain[adc], gui_mwadc_gain_min(adc), GUI_MWADC_GAIN_MAX);
}

int gui_mwadc_offset_of(const gui_app_t *app, int adc) {
    if (adc < 0 || adc > 2) return 0;
    return clampi(app->settings.mwadc_offset[adc], GUI_MWADC_OFFSET_MIN, GUI_MWADC_OFFSET_MAX);
}

int gui_mwadc_centre_of(const gui_app_t *app, int adc) { return 2048 - gui_mwadc_offset_of(app, adc); }

int gui_mwadc_input_offset_of(const gui_app_t *app) {
    for (int k = 0; k < gui_mwadc_input_offset_choice_count; k++)
        if (app->settings.mwadc_input_offset == gui_mwadc_input_offset_choices[k]) return gui_mwadc_input_offset_choices[k];
    return 120;
}

int gui_mwadc_afe_gain_of(const gui_app_t *app) { return clampi(app->settings.mwadc_afe_gain, 0, 31); }

double gui_mwadc_afe_sensitivity(int afe) { return 0.531 + 0.0151 * (afe & 0x1F); }

void gui_mwadc_adc_window(int adc, int gain, int centre, double *lo, double *hi) {
    const double slope = (adc == 0 ? 1.0 : 2.0) * gain / 256.0; // output codes per ADC step
    const double c = centre / 4.0, top = adc == 0 ? 1023.0 : 1022.0;
    double l = c - 512.0 / slope, h = c + (top - 512.0) / slope;
    *lo = l < MW_ADC_FLOOR ? MW_ADC_FLOOR : l;
    *hi = h > MW_ADC_TOP ? MW_ADC_TOP : h;
}

// Analog noise of the front end in ADC10 steps rms (docs/14 §2: the slope of noise against gain).
#define MW_NOISE_LSB 0.56

void gui_mwadc_effective_bits(int adc, int gain, int centre, double *span, double *code_bits, double *enob) {
    const double g = gain / 256.0;
    double lo, hi;
    gui_mwadc_adc_window(adc, gain, centre, &lo, &hi);
    const double s = hi > lo ? hi - lo : 0.0;
    // The CP works on 12-bit data (0.25 ADC10 steps) and outputs one code per 1/g steps (ADC1/ADC2:
    // their 2v - 1024 output keeps every CP code, at two output codes each).
    const double step = 1.0 / (g < 4.0 ? g : 4.0); // input step that still makes a new code
    *span = s;
    *code_bits = s > 0.0 ? log2(s / step + 1.0) : 0.0;
    // Noise-limited ENOB for a sine filling that span: analog noise + 12-bit + output quantisation.
    const double q12 = 0.25, qo = 1.0 / g;
    const double sigma = sqrt(MW_NOISE_LSB * MW_NOISE_LSB + (q12 * q12 + qo * qo) / 12.0);
    const double snr = s > 0.0 ? 20.0 * log10(s / (2.0 * sqrt(2.0)) / sigma) : 0.0;
    *enob = snr > 1.76 ? (snr - 1.76) / 6.02 : 0.0;
}

int gui_mwadc_channel_adc(const gui_app_t *app, int ch) {
    int ia, ib;
    gui_mwadc_effective_inputs(app, &ia, &ib);
    if (ch == 0) return gui_mwadc_inputs[ia].adc;
    if (ib < 0) return -1;
    return gui_mwadc_inputs[ib].adc;
}

int gui_mwadc_settle_seconds(void) {
    const uint64_t t = atomic_load(&s_armed_ms);
    if (!t) return -1;
    const uint64_t now = get_time_ms();
    return now > t ? (int)((now - t) / 1000u) : 0;
}

void gui_mwadc_release(void) {
    if (s_load_state <= 0) return;
    if (s_app && atomic_load(&s_running)) gui_mwadc_stop(s_app);
    lock();
    if (s_dev) {
        mw.close(s_dev); // stops, restores the card
        s_dev = NULL;
        s_dev_board = -1;
    }
    atomic_store(&s_armed_ms, 0);
    s_arm_key[0] = 0;
    unlock();
}

#endif // ENABLE_MWADC
