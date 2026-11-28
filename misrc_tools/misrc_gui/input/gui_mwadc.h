/*
 * MISRC GUI - Magewell Pro Capture HDMI / DVI / AIO raw-ADC backend (mwadc)
 *
 * Maintained in the mwadc repository (https://github.com/JonasCz/mwadc, misrc-gui/)
 * and copied here by its scripts/build-all.ps1: make changes there.
 *
 * The card's ADV7842 runs as a free-running 3-channel 10-bit digitizer
 * (mwadc project). Two of its ADCs feed channel A and B of the shared
 * capture pipeline: samples are packed into the hsdaoh 32-bit capture
 * format and written to BUF_CAPTURE_RF, like the RTL-SDR backend.
 *
 * No build dependency: mwadc_ffi.dll (next to the exe, or on the DLL search
 * path) is loaded at run time; without it, or without a card, no device is
 * listed. Windows only.
 *
 * The card stays armed between capture stop/start (inputs with the clamp
 * off need 1-2 minutes to settle after arming); it is put back to normal
 * video use when another device starts capturing or the GUI exits.
 */

#ifndef GUI_MWADC_H
#define GUI_MWADC_H

#ifdef ENABLE_MWADC

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct gui_app gui_app_t;

// Card kinds (mwadc docs/15): which analog jacks a card has.
enum { MWADC_CARD_HDMI = 1, MWADC_CARD_DVI = 2, MWADC_CARD_AIO = 4 };
// MWADC_CARD_* of an SDK product id; 0 = not supported (no ADV7842 analog front end).
unsigned gui_mwadc_card_of_product(unsigned product_id);

typedef struct {
    int  board;          // rotary switch number
    unsigned card;       // MWADC_CARD_*
    int  sdk_index;
    char name[64];
    char serial[32];
} mwadc_gui_device_t;

// Inputs a channel can use: which ADC, which jack. Index = settings value.
typedef struct {
    const char *label;   // short UI label
    int         adc;     // 0..2
    const char *pin;     // mwadc input name
    unsigned    cards;   // MWADC_CARD_* that have this jack
} mwadc_gui_input_t;

extern const mwadc_gui_input_t gui_mwadc_inputs[];
extern const int gui_mwadc_input_count;
// True when input i exists on the selected card (the DVI card has VGA only, the HDMI card no VGA).
bool gui_mwadc_input_on_card(const gui_app_t *app, int i);
// CH A / CH B inputs as used on the selected card (B = -1: off); A falls back to the card's first
// input when the setting names one the card does not have.
void gui_mwadc_effective_inputs(const gui_app_t *app, int *ia, int *ib);

// Anti-alias corner choices (MHz, 0 = filter off) for the UI cycle.
extern const int gui_mwadc_aa_choices[];
extern const int gui_mwadc_aa_choice_count;
// Analog input offset choices (ADC codes; AFE 0x11[1:0]), shared by all ADCs; and the current one.
extern const int gui_mwadc_input_offset_choices[];
extern const int gui_mwadc_input_offset_choice_count;
int gui_mwadc_input_offset_of(const gui_app_t *app);
// Sample-rate choices (Hz) for the UI cycle.
extern const uint32_t gui_mwadc_rate_choices[];
extern const int gui_mwadc_rate_choice_count;

// Highest sample rate for the current inputs: 160 MSPS with CH A on ADC0 and CH B off
// (Y16), else 100 MSPS (RG10). The effective rate is the setting capped at that.
uint32_t gui_mwadc_max_rate(const gui_app_t *app);
uint32_t gui_mwadc_effective_rate(const gui_app_t *app);

// Levels, in the order the card applies them (mwadc docs/14 §1, §3a), set by hand (no calibration):
//   1. analog gain  AFE 0x0C[4:0] 0..31, shared by all ADCs (31 = normal, 0 = ~1.9x input range)
//   2. ADC          10-bit code, rails at ~2 and ~1023
//   3. offset       per ADC, added to the ADC code before the gain, quarter ADC codes (-2047..+2048 =
//                   -511.75..+512); sent to the card as the centre C = 2048 - offset (mwadc `centreN`,
//                   the ADC level that lands on output code 512)
//   4. digital gain CP gain per ADC, 2.8 fixed point, on the 12-bit ADC data, zooming around the centre:
//                   ADC0: out = clamp(512 + G*(adc - C/4), 0, 1023); ADC1/2: clamp(512 + 2G*(adc - C/4), 0, 1022)
// Defaults (offset 0, gain 1.0 / 0.5) output the raw ADC code. Settings hold raw values per ADC;
// the *_of() getters return them clamped to what the card takes.
#define GUI_MWADC_GAIN_MAX   0x3FF // 3.996
#define GUI_MWADC_OFFSET_MIN (-2047) // centre 4095 (ADC code 1023.75)
#define GUI_MWADC_OFFSET_MAX 2048    // centre 0
int    gui_mwadc_default_gain(int adc);
int    gui_mwadc_gain_min(int adc);
int    gui_mwadc_afe_gain_of(const gui_app_t *app);
int    gui_mwadc_gain_of(const gui_app_t *app, int adc);
int    gui_mwadc_offset_of(const gui_app_t *app, int adc);
// Centre for the card (12-bit ADC units): 2048 - offset.
int    gui_mwadc_centre_of(const gui_app_t *app, int adc);
double gui_mwadc_afe_sensitivity(int afe);
// ADC codes (adc10) that reach the output (not clipped) at this gain and centre, within the ADC rails.
void   gui_mwadc_adc_window(int adc, int gain, int centre, double *lo, double *hi);
// Resolution these levels give: `span` = ADC10 steps of the ADC range that land inside the output,
// `code_bits` = log2 of the distinct output levels over it (12-bit limited), `enob` = noise-limited
// effective bits for a sine filling that span (analog noise ~0.56 ADC10 steps rms, docs/14 §5b).
void   gui_mwadc_effective_bits(int adc, int gain, int centre, double *span, double *code_bits, double *enob);
// ADC (0..2) behind GUI channel 0 (A) / 1 (B); -1 when that channel is off.
int    gui_mwadc_channel_adc(const gui_app_t *app, int ch);
// Seconds since the card was last armed with its current inputs / rate (inputs with the clamp
// off drift for 1-2 minutes after that); -1 when not armed.
int    gui_mwadc_settle_seconds(void);

// True when mwadc_ffi.dll loaded (first call tries to load it).
bool gui_mwadc_available(void);

// Enumerate cards. Returns the count (0 when the DLL or SDK is missing).
int gui_mwadc_enumerate(mwadc_gui_device_t *devices, int max_devices);

// True when the last enumeration failed because the Magewell Capture SDK runtime
// (LibMWCapture.dll) is not installed AND a supported card (Pro Capture HDMI/DVI/AIO) is in the
// machine (seen on the PCI bus without the SDK). The GUI then lists the card with board index -1;
// selecting it or starting a capture on it shows gui_mwadc_show_sdk_missing(). gui_mwadc_missing_card()
// is that card's model name.
bool gui_mwadc_sdk_missing(void);
const char *gui_mwadc_missing_card(void);
// Dialog: install MWCaptureRT.exe from https://github.com/jonasCz/mwadc/releases.
void gui_mwadc_show_sdk_missing(void);

// Open (if needed), arm with the current settings and start streaming into
// BUF_CAPTURE_RF + extraction + display. Returns 0 on success.
int gui_mwadc_start(gui_app_t *app, int board);

// Stop streaming (the card stays armed).
void gui_mwadc_stop(gui_app_t *app);

bool gui_mwadc_is_running(void);

// The card's line-in feeds BUF_CAPTURE_AUDIO (always; started by gui_mwadc_start,
// stopped by gui_mwadc_stop). The caller runs gui_audio_start/stop around it.
bool gui_mwadc_line_audio_running(void);

// The line-in is locked to the RF clock and BUF_CAPTURE_AUDIO trails BUF_CAPTURE_RF: the audio
// ring position (tail cursor, wrapping at twice the ring size) of the line-in sample that belongs
// to RF ring byte `rf_byte` (BUF_CAPTURE_RF bytes_written). False while the line-in is off. The
// cursor can be ahead of the audio ring's tail by up to a frame: that audio is still to come.
bool gui_mwadc_line_cursor(const gui_app_t *app, uint64_t rf_byte, size_t *cursor);

// Re-apply the settings. Anti-alias and all levels apply live; input or
// rate changes need a capture restart (returns 1 then, 0 when applied,
// -1 on error). Safe to call when not capturing (no-op).
int gui_mwadc_apply_settings(gui_app_t *app);

// Put the card back to normal video use and close it (app exit, other device).
void gui_mwadc_release(void);

#endif // ENABLE_MWADC

#endif // GUI_MWADC_H
