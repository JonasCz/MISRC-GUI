/*
 * MISRC GUI - Magewell raw-ADC settings panel (inputs, front end, level sliders)
 * and its click / drag handling. gui_ui.c calls it while a Magewell card is selected.
 *
 * Maintained in the mwadc repository (https://github.com/JonasCz/mwadc, misrc-gui/)
 * and copied here by its scripts/build-all.ps1: make changes there.
 */

#include "gui_ui_mwadc.h"

#include "gui_dropdown.h"
#include "../input/gui_mwadc.h"
#include <clay.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

// Same small helpers as gui_ui.c (static there).
static inline Clay_Color to_clay_color(Color c) {
    return (Clay_Color){ c.r, c.g, c.b, c.a };
}

static Clay_String make_string(const char *str) {
    return (Clay_String){ .isStaticallyAllocated = false, .length = (int32_t)strlen(str), .chars = str };
}

static Color ui_disabled_color(Color c) {
    // Dim and slightly transparent.
    return (Color){ (unsigned char)(c.r * 0.55f), (unsigned char)(c.g * 0.55f), (unsigned char)(c.b * 0.55f), (unsigned char)(c.a * 0.80f) };
}

// True while a Magewell card is the selected device.
bool gui_ui_selected_device_is_mwadc(const gui_app_t *app)
{
    if (!app) return false;
    if (app->selected_device < 0 || app->selected_device >= app->device_count) return false;
    return app->devices[app->selected_device].type == DEVICE_TYPE_MWADC;
}

// Magewell rows: a fixed-width control on the left, the setting's name and a short
// always-visible description on the right (the GUI has no tooltip widget).
#define MW_CONTROL_W 200.0f // fits "Component Pb (-> ADC2)"
// Cap on the Magewell section width: its long help texts would otherwise widen the settings
// panel past its maximum (horizontal scroll); with the cap they wrap.
#define MW_SECTION_MAX_W 600.0f

static void render_mwadc_text_column(int uid, const char *label, const char *help)
{
    CLAY(CLAY_IDI("MwadcText", uid), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIT(0) }, .layoutDirection = CLAY_TOP_TO_BOTTOM, .childGap = 1 } }) {
        CLAY_TEXT(make_string(label), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(COLOR_TEXT) }));
        CLAY_TEXT(make_string(help), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_STATS, .textColor = to_clay_color(COLOR_TEXT_DIM) }));
    }
}

// A row whose control opens a menu (render_mwadc_menu): the value and a "v" marker.
static void render_mwadc_dropdown_row(int n, const char *button_text, bool open, bool busy, const char *label,
                                      const char *help)
{
    CLAY(CLAY_IDI("MwadcRow", n), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIT(0) }, .layoutDirection = CLAY_LEFT_TO_RIGHT, .childAlignment = { .y = CLAY_ALIGN_Y_TOP }, .childGap = 10 } }) {
        CLAY(CLAY_IDI("MwadcButton", n), { .layout = { .sizing = { CLAY_SIZING_FIXED(MW_CONTROL_W), CLAY_SIZING_FIXED(28) }, .childAlignment = { .x = CLAY_ALIGN_X_LEFT, .y = CLAY_ALIGN_Y_CENTER }, .padding = { 8, 8, 0, 0 }, .childGap = 6 }, .backgroundColor = to_clay_color(open || busy ? COLOR_BUTTON_ACTIVE : COLOR_BUTTON), .cornerRadius = CLAY_CORNER_RADIUS(4) }) {
            CLAY_TEXT(make_string(button_text), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(busy ? COLOR_METER_YELLOW : COLOR_TEXT), .wrapMode = CLAY_TEXT_WRAP_NONE }));
            CLAY(CLAY_IDI("MwadcBtnGap", n), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIT(0) } } }) {}
            if (!busy)
                CLAY_TEXT(CLAY_STRING("v"), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_STATS, .textColor = to_clay_color(COLOR_TEXT_DIM) }));
        }
        render_mwadc_text_column(n, label, help);
    }
}

static void render_mwadc_group(const char *title)
{
    CLAY_TEXT(make_string(title), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_STATS, .textColor = to_clay_color(COLOR_TEXT_DIM) }));
}

enum { MW_ROW_IN_A, MW_ROW_IN_B, MW_ROW_RATE, MW_ROW_AA, MW_ROW_INOFF };

// Level sliders, in the order the card applies them (gui_mwadc.h): analog gain (shared), then
// offset and digital gain per channel's ADC.
enum { MW_SL_AFE, MW_SL_OFF_A, MW_SL_GAIN_A, MW_SL_OFF_B, MW_SL_GAIN_B, MW_SL_COUNT };

// Menus: one gui_dropdown id, indexed by row.
#define MW_DROPDOWN "Mwadc"
#define MW_MENU_W 270.0f
#define MW_MENU_ROW_H 22.0f

// A new input or rate re-locks the card, which it cannot do while streaming: the capture restarts.
// The restart blocks the UI thread for a moment, so it runs a few frames after the click, once the
// row's "Switching..." has been drawn.
static int s_mw_switch_row = -1;        // row whose change is restarting the capture, -1 = none
static int s_mw_switch_frames = 0;      // frames left before that restart

static int s_mw_drag = -1;              // slider being dragged, -1 = none
static bool s_mw_apply_pending = false; // level change not yet sent to the card
static double s_mw_last_apply = 0.0;

// Valid range [lo, hi] and current value of a slider; false when the slider does not exist
// (channel off). The track shows [*dlo, *dhi] (optional; here always the valid range).
static bool gui_ui_mwadc_slider_ex(const gui_app_t *app, int id, int *adc, int *lo, int *hi, int *val,
                                   int *dlo, int *dhi)
{
    int dl, dh;
    if (!dlo) dlo = &dl;
    if (!dhi) dhi = &dh;
    if (id == MW_SL_AFE) {
        *adc = -1;
        *lo = *dlo = 0;
        *hi = *dhi = 31;
        *val = gui_mwadc_afe_gain_of(app);
        return true;
    }
    const int ch = id >= MW_SL_GAIN_B ? 1 : 0;
    const bool is_gain = id == MW_SL_GAIN_A || id == MW_SL_GAIN_B;
    *adc = gui_mwadc_channel_adc(app, ch);
    if (*adc < 0) return false;
    if (is_gain) {
        *lo = *dlo = gui_mwadc_gain_min(*adc);
        *hi = *dhi = GUI_MWADC_GAIN_MAX;
        *val = gui_mwadc_gain_of(app, *adc);
    } else {
        *lo = *dlo = GUI_MWADC_OFFSET_MIN;
        *hi = *dhi = GUI_MWADC_OFFSET_MAX;
        *val = gui_mwadc_offset_of(app, *adc);
    }
    return true;
}

static bool gui_ui_mwadc_slider(const gui_app_t *app, int id, int *adc, int *lo, int *hi, int *val)
{
    return gui_ui_mwadc_slider_ex(app, id, adc, lo, hi, val, NULL, NULL);
}

// Default value of a slider (its reset button).
static int gui_ui_mwadc_slider_default(const gui_app_t *app, int id)
{
    if (id == MW_SL_AFE) return 31;
    const int adc = gui_mwadc_channel_adc(app, id >= MW_SL_GAIN_B ? 1 : 0);
    if (adc < 0) return 0;
    return (id == MW_SL_GAIN_A || id == MW_SL_GAIN_B) ? gui_mwadc_default_gain(adc) : 0;
}

// Set a slider's value (clamped). Returns true when it changed.
static bool gui_ui_mwadc_slider_set(gui_app_t *app, int id, int v)
{
    int adc, lo, hi, cur;
    if (!gui_ui_mwadc_slider(app, id, &adc, &lo, &hi, &cur)) return false;
    if (v < lo) v = lo;
    if (v > hi) v = hi;
    gui_settings_t *s = &app->settings;
    if (id == MW_SL_AFE) {
        if (s->mwadc_afe_gain == v) return false;
        s->mwadc_afe_gain = v;
    } else if (id == MW_SL_GAIN_A || id == MW_SL_GAIN_B) {
        if (s->mwadc_gain[adc] == v) return false;
        s->mwadc_gain[adc] = v;
    } else {
        if (s->mwadc_offset[adc] == v) return false;
        s->mwadc_offset[adc] = v;
    }
    return true;
}

// Send pending level changes to the card (live while capturing), at most every 50 ms unless forced.
static void gui_ui_mwadc_apply_levels(gui_app_t *app, bool force)
{
    if (!s_mw_apply_pending) return;
    const double now = GetTime();
    if (!force && now - s_mw_last_apply < 0.05) return;
    s_mw_apply_pending = false;
    s_mw_last_apply = now;
    if (app->is_capturing) gui_mwadc_apply_settings(app);
}

static bool gui_ui_mwadc_slider_from_mouse(gui_app_t *app, int id)
{
    Clay_ElementData track = Clay_GetElementData(CLAY_IDI("MwadcTrack", id));
    int adc, lo, hi, cur, dlo, dhi;
    if (!track.found || track.boundingBox.width <= 1.0f ||
        !gui_ui_mwadc_slider_ex(app, id, &adc, &lo, &hi, &cur, &dlo, &dhi))
        return false;
    // Same mapping as the knob (render_mwadc_slider): its centre spans the track minus its width.
    const float span = track.boundingBox.width > 8.0f ? track.boundingBox.width - 8.0f : track.boundingBox.width;
    float t = (gui_ui_get_mouse_position().x - track.boundingBox.x - 4.0f) / span;
    if (t < 0.0f) t = 0.0f;
    if (t > 1.0f) t = 1.0f;
    return gui_ui_mwadc_slider_set(app, id, dlo + (int)lround((double)t * (dhi - dlo))); // clamps to [lo, hi]
}

// Current CH A / CH B inputs as the panel shows them (B -1 = off).
static void mw_inputs(const gui_app_t *app, int *ia, int *ib)
{
    gui_mwadc_effective_inputs(app, ia, ib);
}

// The k-th input the selected card has (its menu lists only those), or -1.
static int mw_card_input(const gui_app_t *app, int k)
{
    for (int i = 0; i < gui_mwadc_input_count; i++)
        if (gui_mwadc_input_on_card(app, i) && k-- == 0) return i;
    return -1;
}

static int mw_card_input_count(const gui_app_t *app)
{
    int n = 0;
    for (int i = 0; i < gui_mwadc_input_count; i++) n += gui_mwadc_input_on_card(app, i);
    return n;
}

static int mw_menu_open_row(void)
{
    const int rows[] = { MW_ROW_IN_A, MW_ROW_IN_B, MW_ROW_RATE, MW_ROW_AA, MW_ROW_INOFF };
    for (int k = 0; k < 5; k++)
        if (gui_dropdown_is_open(MW_DROPDOWN, (uint32_t)rows[k])) return rows[k];
    return -1;
}

static int mw_menu_count(const gui_app_t *app, int row)
{
    if (row == MW_ROW_IN_A) return mw_card_input_count(app);
    if (row == MW_ROW_IN_B) return mw_card_input_count(app) + 1; // option 0 = Off
    if (row == MW_ROW_RATE) return gui_mwadc_rate_choice_count;
    if (row == MW_ROW_INOFF) return gui_mwadc_input_offset_choice_count;
    return gui_mwadc_aa_choice_count;
}

// Settings value of option i, and the current one.
static int mw_menu_value(const gui_app_t *app, int row, int i)
{
    if (row == MW_ROW_IN_A) return mw_card_input(app, i);
    if (row == MW_ROW_IN_B) return i == 0 ? -1 : mw_card_input(app, i - 1);
    if (row == MW_ROW_RATE) return (int)gui_mwadc_rate_choices[i];
    if (row == MW_ROW_INOFF) return gui_mwadc_input_offset_choices[i];
    return gui_mwadc_aa_choices[i];
}

static int mw_menu_current(const gui_app_t *app, int row)
{
    const gui_settings_t *s = &app->settings;
    int ia, ib;
    mw_inputs(app, &ia, &ib);
    if (row == MW_ROW_RATE) return (int)gui_mwadc_effective_rate(app);
    if (row == MW_ROW_INOFF) return gui_mwadc_input_offset_of(app);
    return row == MW_ROW_IN_A ? ia : row == MW_ROW_IN_B ? ib : s->mwadc_aa_mhz;
}

// Label of option i; false when it cannot be chosen (an input on the other channel's ADC, a rate
// these inputs cannot sustain).
static bool mw_menu_label(const gui_app_t *app, int row, int i, char *dst, size_t n)
{
    const int v = mw_menu_value(app, row, i);
    if (row == MW_ROW_RATE) {
        const bool ok = (uint32_t)v <= gui_mwadc_max_rate(app);
        snprintf(dst, n, ok ? "%.3f MSPS" : "%.3f MSPS - B off, A on ADC0", v / 1e6);
        return ok;
    }
    if (row == MW_ROW_INOFF) {
        snprintf(dst, n, v ? "%+d" : "0", v);
        return true;
    }
    if (row == MW_ROW_AA) {
        if (v > 0) snprintf(dst, n, "%d MHz", v);
        else snprintf(dst, n, "Off");
        return true;
    }
    if (v < 0) {
        snprintf(dst, n, "Off");
        return true;
    }
    int ia, ib;
    mw_inputs(app, &ia, &ib);
    const int other = row == MW_ROW_IN_A ? ib : ia;
    const bool used = other >= 0 && gui_mwadc_inputs[other].adc == gui_mwadc_inputs[v].adc;
    if (used) snprintf(dst, n, "%s - CH %c", gui_mwadc_inputs[v].label, row == MW_ROW_IN_A ? 'B' : 'A');
    else snprintf(dst, n, "%s", gui_mwadc_inputs[v].label);
    return !used;
}

// The open menu, below its button (above it when there is no room), over everything else.
static void render_mwadc_menu(const gui_app_t *app, int row)
{
    const Clay_ElementData btn = Clay_GetElementData(CLAY_IDI("MwadcButton", row));
    if (!btn.found) return;
    const int count = mw_menu_count(app, row);
    const float h = count * MW_MENU_ROW_H + 4.0f;
    const float lw = (float)gui_ui_get_layout_width(), lh = (float)gui_ui_get_layout_height();
    float x = btn.boundingBox.x, y = btn.boundingBox.y + btn.boundingBox.height + 2.0f;
    if (y + h > lh - 4.0f) y = btn.boundingBox.y - h - 2.0f;
    if (y < 4.0f) y = 4.0f;
    if (x + MW_MENU_W > lw - 4.0f) x = lw - 4.0f - MW_MENU_W;
    if (x < 4.0f) x = 4.0f;
    const int cur = mw_menu_current(app, row);
    static char labels[16][64];
    CLAY(CLAY_ID("MwadcMenu"), {
        .layout = { .sizing = { CLAY_SIZING_FIXED(MW_MENU_W), CLAY_SIZING_FIXED(h) }, .layoutDirection = CLAY_TOP_TO_BOTTOM, .padding = { 0, 0, 2, 2 } },
        .floating = { .attachTo = CLAY_ATTACH_TO_ROOT, .offset = { x, y }, .zIndex = 1, .clipTo = CLAY_CLIP_TO_NONE,
                      .attachPoints = { .element = CLAY_ATTACH_POINT_LEFT_TOP, .parent = CLAY_ATTACH_POINT_LEFT_TOP } },
        .backgroundColor = to_clay_color(COLOR_PANEL_BG),
        .border = { .color = to_clay_color(COLOR_BUTTON_HOVER), .width = CLAY_BORDER_OUTSIDE(1) },
        .cornerRadius = CLAY_CORNER_RADIUS(3)
    }) {
        for (int i = 0; i < count && i < 16; i++) {
            const bool enabled = mw_menu_label(app, row, i, labels[i], sizeof(labels[i]));
            const Clay_ElementId id = CLAY_IDI("MwadcOpt", i);
            const bool selected = mw_menu_value(app, row, i) == cur;
            CLAY(id, { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIXED(MW_MENU_ROW_H) }, .childAlignment = { .y = CLAY_ALIGN_Y_CENTER }, .padding = { 8, 8, 0, 0 } },
                       .backgroundColor = to_clay_color(gui_dropdown_option_color(selected, enabled && Clay_PointerOver(id))) }) {
                CLAY_TEXT(make_string(labels[i]), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_DROPDOWN_OPT, .textColor = to_clay_color(enabled ? COLOR_TEXT : ui_disabled_color(COLOR_TEXT_DIM)), .wrapMode = CLAY_TEXT_WRAP_NONE }));
            }
        }
    }
}

static void mw_begin_switch(gui_app_t *app, int row)
{
    s_mw_switch_row = row;
    s_mw_switch_frames = 3;
    gui_app_set_status(app, row == MW_ROW_RATE ? "Magewell: switching sample rate..." : "Magewell: switching input...");
}

// A menu option was chosen: save it, then apply it live (low-pass) or restart the capture (inputs,
// rate).
static void mw_menu_choose(gui_app_t *app, int row, int i)
{
    gui_settings_t *s = &app->settings;
    const int v = mw_menu_value(app, row, i);
    if (v == mw_menu_current(app, row)) return;
    if (row == MW_ROW_IN_A) s->mwadc_input_a = v;
    else if (row == MW_ROW_IN_B) s->mwadc_input_b = v;
    else if (row == MW_ROW_RATE) s->mwadc_rate_hz = (uint32_t)v;
    else if (row == MW_ROW_INOFF) s->mwadc_input_offset = v;
    else s->mwadc_aa_mhz = v;
    gui_settings_save(s);
    if (!app->is_capturing) return;
    if (row == MW_ROW_AA || row == MW_ROW_INOFF) gui_mwadc_apply_settings(app);
    else mw_begin_switch(app, row);
}

// True while a level slider is being dragged, or the mouse is pressed on one this frame: the main
// loop then turns off the settings panel's drag scrolling, so dragging a slider does not also scroll.
// Call after Clay_SetPointerState (it hit-tests against the previous frame's layout).
bool gui_ui_mwadc_blocks_drag_scroll(const gui_app_t *app)
{
    if (s_mw_drag >= 0) return true;
    if (!IsMouseButtonPressed(MOUSE_LEFT_BUTTON) || !gui_ui_selected_device_is_mwadc(app)) return false;
    for (int id = 0; id < MW_SL_COUNT; id++)
        if (Clay_PointerOver(CLAY_IDI("MwadcTrack", id))) return true;
    return false;
}

// Per frame: follow a slider drag, apply throttled, save when it ends.
void gui_ui_mwadc_drag_tick(gui_app_t *app)
{
    if (mw_menu_open_row() >= 0 &&
        (IsKeyPressed(KEY_ESCAPE) || !app->settings_panel_open || !gui_ui_selected_device_is_mwadc(app) ||
         app->is_recording || s_mw_switch_row >= 0))
        gui_dropdown_close_all();
    // The pending input / rate restart, once "Switching..." is on screen.
    if (s_mw_switch_row >= 0 && --s_mw_switch_frames <= 0) {
        s_mw_switch_row = -1;
        if (app->is_capturing && !app->is_recording && gui_ui_selected_device_is_mwadc(app)) {
            gui_app_stop_capture(app);
            (void)gui_app_start_capture(app);
        }
    }
    if (s_mw_drag >= 0) {
        if (!IsMouseButtonDown(MOUSE_LEFT_BUTTON) || !gui_ui_selected_device_is_mwadc(app)) {
            s_mw_drag = -1;
            gui_settings_save(&app->settings);
            gui_ui_mwadc_apply_levels(app, true);
            return;
        }
        if (gui_ui_mwadc_slider_from_mouse(app, s_mw_drag)) s_mw_apply_pending = true;
    }
    gui_ui_mwadc_apply_levels(app, s_mw_drag < 0);
}

static void render_mwadc_slider(gui_app_t *app, int id, const char *label, const char *value_text,
                                const char *readout, const char *help)
{
    int adc, lo, hi, val, dlo, dhi;
    if (!gui_ui_mwadc_slider_ex(app, id, &adc, &lo, &hi, &val, &dlo, &dhi)) return;
    const float dspan = dhi > dlo ? (float)(dhi - dlo) : 1.0f;
    const float frac = (float)(val - dlo) / dspan;
    const float frac_lo = (float)(lo - dlo) / dspan, frac_hi = (float)(hi - dlo) / dspan;
    Clay_ElementData prev = Clay_GetElementData(CLAY_IDI("MwadcTrack", id));
    const float track_w = prev.found ? prev.boundingBox.width : 0.0f;
    const float knob_w = 8.0f;
    const bool dragging = s_mw_drag == id;
    const bool at_default = val == gui_ui_mwadc_slider_default(app, id);
    CLAY(CLAY_IDI("MwadcSlider", id), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIT(0) }, .layoutDirection = CLAY_TOP_TO_BOTTOM, .childGap = 3 } }) {
        CLAY(CLAY_IDI("MwadcSlHead", id), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIT(0) }, .layoutDirection = CLAY_LEFT_TO_RIGHT, .childGap = 8 } }) {
            CLAY(CLAY_IDI("MwadcSlLabel", id), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIT(0) } } }) {
                CLAY_TEXT(make_string(label), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(COLOR_TEXT) }));
            }
            CLAY_TEXT(make_string(value_text), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .fontId = 1, .textColor = to_clay_color(COLOR_METER_YELLOW), .wrapMode = CLAY_TEXT_WRAP_NONE }));
        }
        CLAY(CLAY_IDI("MwadcSlRow", id), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIXED(22) }, .layoutDirection = CLAY_LEFT_TO_RIGHT, .childAlignment = { .y = CLAY_ALIGN_Y_CENTER }, .childGap = 6 } }) {
            CLAY(CLAY_IDI("MwadcMinus", id), { .layout = { .sizing = { CLAY_SIZING_FIXED(22), CLAY_SIZING_FIXED(22) }, .childAlignment = { .x = CLAY_ALIGN_X_CENTER, .y = CLAY_ALIGN_Y_CENTER } }, .backgroundColor = to_clay_color(COLOR_BUTTON), .cornerRadius = CLAY_CORNER_RADIUS(4) }) {
                CLAY_TEXT(CLAY_STRING("-"), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(COLOR_TEXT) }));
            }
            // Unavailable | fill | knob | rest | unavailable, all in the layout flow (no floating
            // elements: those escape the settings panel's scroll clip and show below the fold).
            CLAY(CLAY_IDI("MwadcTrack", id), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIXED(18) }, .layoutDirection = CLAY_LEFT_TO_RIGHT, .childAlignment = { .y = CLAY_ALIGN_Y_CENTER } } }) {
                const float w = track_w > knob_w ? track_w - knob_w : 0.0f;
                const Color unavailable = { 70, 40, 40, 255 }, empty = { 25, 25, 30, 255 };
                const float off_l = w * frac_lo, fill_w = w * (frac - frac_lo), rest_w = w * (frac_hi - frac);
                if (off_l >= 1.0f)
                    CLAY(CLAY_IDI("MwadcOffL", id), { .layout = { .sizing = { CLAY_SIZING_FIXED(off_l), CLAY_SIZING_FIXED(6) } }, .backgroundColor = to_clay_color(unavailable) }) {}
                if (fill_w >= 1.0f)
                    CLAY(CLAY_IDI("MwadcFill", id), { .layout = { .sizing = { CLAY_SIZING_FIXED(fill_w), CLAY_SIZING_FIXED(12) } }, .backgroundColor = to_clay_color(COLOR_BUTTON_ACTIVE) }) {}
                if (track_w > knob_w) {
                    CLAY(CLAY_IDI("MwadcKnob", id), {
                        .layout = { .sizing = { CLAY_SIZING_FIXED(knob_w), CLAY_SIZING_FIXED(18) } },
                        .backgroundColor = to_clay_color(dragging ? COLOR_METER_YELLOW : COLOR_TEXT),
                        .cornerRadius = CLAY_CORNER_RADIUS(3)
                    }) {}
                }
                if (rest_w >= 1.0f)
                    CLAY(CLAY_IDI("MwadcRest", id), { .layout = { .sizing = { CLAY_SIZING_FIXED(rest_w), CLAY_SIZING_FIXED(12) } }, .backgroundColor = to_clay_color(empty) }) {}
                // Whatever is left (the unavailable top part, or rounding) fills the track.
                CLAY(CLAY_IDI("MwadcOffR", id), { .layout = { .sizing = { CLAY_SIZING_GROW(0), CLAY_SIZING_FIXED(frac_hi < 0.999f ? 6 : 12) } }, .backgroundColor = to_clay_color(frac_hi < 0.999f ? unavailable : empty) }) {}
            }
            CLAY(CLAY_IDI("MwadcPlus", id), { .layout = { .sizing = { CLAY_SIZING_FIXED(22), CLAY_SIZING_FIXED(22) }, .childAlignment = { .x = CLAY_ALIGN_X_CENTER, .y = CLAY_ALIGN_Y_CENTER } }, .backgroundColor = to_clay_color(COLOR_BUTTON), .cornerRadius = CLAY_CORNER_RADIUS(4) }) {
                CLAY_TEXT(CLAY_STRING("+"), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(COLOR_TEXT) }));
            }
            // Reset to the default (dimmed while already there).
            CLAY(CLAY_IDI("MwadcReset", id), { .layout = { .sizing = { CLAY_SIZING_FIXED(22), CLAY_SIZING_FIXED(22) }, .childAlignment = { .x = CLAY_ALIGN_X_CENTER, .y = CLAY_ALIGN_Y_CENTER } }, .backgroundColor = to_clay_color(at_default ? ui_disabled_color(COLOR_BUTTON) : COLOR_BUTTON), .cornerRadius = CLAY_CORNER_RADIUS(4) }) {
                CLAY_TEXT(CLAY_STRING("\xE2\x8E\x8C") /* U+238C undo symbol */, CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(at_default ? ui_disabled_color(COLOR_TEXT) : COLOR_TEXT) }));
            }
        }
        if (readout && readout[0])
            CLAY_TEXT(make_string(readout), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_STATS, .fontId = 1, .textColor = to_clay_color(COLOR_TEXT) }));
        CLAY_TEXT(make_string(help), CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_STATS, .textColor = to_clay_color(COLOR_TEXT_DIM) }));
    }
}

static void render_mwadc_levels(gui_app_t *app)
{
    static char afe_val[48], afe_ro[96];
    static char gain_lbl[2][48], gain_val[2][32], gain_ro[2][96], gain_help[2][420], off_lbl[2][48], off_val[2][32],
        off_ro[2][96];
    const int afe = gui_mwadc_afe_gain_of(app);
    const double sens = gui_mwadc_afe_sensitivity(afe);
    snprintf(afe_val, sizeof(afe_val), "%d (0x%02X)", afe, afe);
    snprintf(afe_ro, sizeof(afe_ro), "sensitivity x%.3f (%.1f dB), input range x%.2f%s", sens, 20.0 * log10(sens),
             1.0 / sens, afe == 31 ? " (normal, the maximum)" : afe == 0 ? " (widest)" : "");

    render_mwadc_group("LEVELS (in signal order: analog gain -> ADC -> offset -> digital gain)");

    render_mwadc_slider(app, MW_SL_AFE, "1. Analog gain (all channels)", afe_val, afe_ro,
        "Before the ADCs, shared by all inputs. It only attenuates: 31 (the default) is the most sensitive "
        "setting. Lower values widen the input range (up to ~1.9x at 0) for signals that would otherwise clip "
        "the ADC, but also raise the noise floor. Use this only if you cannot attenuate the signal before the "
        "input, keep it at 31 otherwise. Moves the resting level slightly.");

    for (int ch = 0; ch < 2; ch++) {
        const int adc = gui_mwadc_channel_adc(app, ch);
        if (adc < 0) continue;
        const int g = gui_mwadc_gain_of(app, adc), c = gui_mwadc_centre_of(app, adc);
        const char cn = ch ? 'B' : 'A';
        double lo, hi, span, code_bits, enob;
        gui_mwadc_adc_window(adc, g, c, &lo, &hi);
        gui_mwadc_effective_bits(adc, g, c, &span, &code_bits, &enob);

        const int o = gui_mwadc_offset_of(app, adc);
        snprintf(off_lbl[ch], sizeof(off_lbl[ch]), "2. Offset - CH %c (ADC%d)", cn, adc);
        snprintf(off_val[ch], sizeof(off_val[ch]), "%+.2f", o / 4.0);
        snprintf(off_ro[ch], sizeof(off_ro[ch]), "Output shows ADC codes %.0f..%.0f", lo, hi);
        snprintf(gain_lbl[ch], sizeof(gain_lbl[ch]), "3. Digital gain - CH %c (ADC%d)", cn, adc);
        snprintf(gain_val[ch], sizeof(gain_val[ch]), "x%.4f (0x%03X)", g / 256.0, g);
        snprintf(gain_ro[ch], sizeof(gain_ro[ch]), "Effective %.1f bits based on current settings", enob);
        snprintf(gain_help[ch], sizeof(gain_help[ch]),
                 "Zooms in on the ADC range around the output's middle: on the ADC's 12-bit data, before the driver's cut to "
                 "10 bits, so it has access to a small amount of extra resolution. Use only if you don't have enough "
                 "analog gain or amplification before the Magewell's input, leave it on x%.1f (the minimum) otherwise.",
                 gui_mwadc_gain_min(adc) / 256.0);

        render_mwadc_slider(app, ch ? MW_SL_OFF_B : MW_SL_OFF_A, off_lbl[ch], off_val[ch], off_ro[ch],
            "Value to add / subtract from the ADC's output. This is only useful if you need to remove more DC "
            "offset than the analog input offset allows, or if using a digital gain value above 1x, where it "
            "can be used to centre the signal prior to gain.");
        render_mwadc_slider(app, ch ? MW_SL_GAIN_B : MW_SL_GAIN_A, gain_lbl[ch], gain_val[ch], gain_ro[ch],
                            gain_help[ch]);
    }
}

void gui_ui_mwadc_render_settings(gui_app_t *app)
{
    const gui_settings_t *s = &app->settings;
    static char in_a[32], in_b[32], aa[24], rate[32], inoff[16];
    int ia, ib;
    mw_inputs(app, &ia, &ib);
    snprintf(in_a, sizeof(in_a), "%s", gui_mwadc_inputs[ia].label);
    snprintf(in_b, sizeof(in_b), "%s", ib >= 0 ? gui_mwadc_inputs[ib].label : "Off");
    if (s->mwadc_aa_mhz > 0) snprintf(aa, sizeof(aa), "%d MHz", s->mwadc_aa_mhz);
    else snprintf(aa, sizeof(aa), "Off");
    const int io = gui_mwadc_input_offset_of(app);
    snprintf(inoff, sizeof(inoff), io ? "%+d" : "0", io);
    const uint32_t eff_rate = gui_mwadc_effective_rate(app);
    snprintf(rate, sizeof(rate), "%.3f MSPS", eff_rate / 1e6);

    CLAY_TEXT(CLAY_STRING("Magewell ADC:"),
        CLAY_TEXT_CONFIG({ .fontSize = FONT_SIZE_NORMAL, .textColor = to_clay_color(COLOR_TEXT_DIM) }));
    CLAY(CLAY_ID("MwadcSection"), { .layout = { .sizing = { CLAY_SIZING_GROW(0, MW_SECTION_MAX_W), CLAY_SIZING_FIT(0) }, .layoutDirection = CLAY_TOP_TO_BOTTOM, .childGap = 6, .padding = { 8, 0, 0, 4 } } }) {
        render_mwadc_group("INPUTS");
        const int open = mw_menu_open_row();
        render_mwadc_dropdown_row(MW_ROW_IN_A, s_mw_switch_row == MW_ROW_IN_A ? "Switching..." : in_a,
                                  open == MW_ROW_IN_A, s_mw_switch_row == MW_ROW_IN_A, "Channel A",
                                  "ADC0 (Y, CVBS, S-Y) has 10 bits; ADC1/ADC2 have 9.");
        render_mwadc_dropdown_row(MW_ROW_IN_B, s_mw_switch_row == MW_ROW_IN_B ? "Switching..." : in_b,
                                  open == MW_ROW_IN_B, s_mw_switch_row == MW_ROW_IN_B, "Channel B",
                                  "2nd ADC channel. 3 channel capture is available with mwadc CLI");
        render_mwadc_dropdown_row(MW_ROW_RATE, s_mw_switch_row == MW_ROW_RATE ? "Switching..." : rate,
                                  open == MW_ROW_RATE, s_mw_switch_row == MW_ROW_RATE, "Sample rate",
                                  "Up to 100 MSPS with two channels, 160 MSPS with one.");

        render_mwadc_group("ANALOG FRONT END");
        render_mwadc_dropdown_row(MW_ROW_AA, aa, open == MW_ROW_AA, false, "Low-pass",
                                  "Analog low-pass before the ADCs, shared for all ADCs.");
        render_mwadc_dropdown_row(MW_ROW_INOFF, inoff, open == MW_ROW_INOFF, false, "Input offset",
                                  "Analog DC shift before the ADCs, in ADC codes, shared for all ADCs. +120 (the "
                                  "default) puts an idle input near the middle of the ADC range; lower it if your "
                                  "signal clips at the top.");

        render_mwadc_levels(app);
        if (open >= 0) render_mwadc_menu(app, open);
    }
}

// Click handling for the Magewell rows. Returns true when a click was consumed.
bool gui_ui_mwadc_handle_click(gui_app_t *app)
{
    gui_settings_t *s = &app->settings;
    bool changed = false;
    // An open menu: an option chooses it, any other click closes the menu (and still acts).
    const int open = mw_menu_open_row();
    if (open >= 0) {
        for (int i = 0; i < mw_menu_count(app, open); i++) {
            if (!Clay_PointerOver(CLAY_IDI("MwadcOpt", i))) continue;
            char label[64];
            if (!mw_menu_label(app, open, i, label, sizeof(label))) return true; // greyed out
            gui_dropdown_close_all();
            mw_menu_choose(app, open, i);
            return true;
        }
        gui_dropdown_close_all();
        if (Clay_PointerOver(CLAY_ID("MwadcMenu")) || Clay_PointerOver(CLAY_IDI("MwadcButton", open))) return true;
    }
    // Inputs and rate re-lock the card: not while recording (the settings panel is locked then
    // anyway), nor while the previous change is still restarting the capture.
    const bool relock = Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_IN_A)) ||
                        Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_IN_B)) ||
                        Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_RATE));
    if (relock && (app->is_recording || s_mw_switch_row >= 0)) {
        if (app->is_recording) gui_app_set_status(app, "Stop recording to change the Magewell input or sample rate");
        return true;
    }
    if (Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_IN_A))) {
        gui_dropdown_toggle(MW_DROPDOWN, MW_ROW_IN_A);
        return true;
    } else if (Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_IN_B))) {
        gui_dropdown_toggle(MW_DROPDOWN, MW_ROW_IN_B);
        return true;
    } else if (Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_RATE))) {
        gui_dropdown_toggle(MW_DROPDOWN, MW_ROW_RATE);
        return true;
    } else if (Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_AA))) {
        gui_dropdown_toggle(MW_DROPDOWN, MW_ROW_AA);
        return true;
    } else if (Clay_PointerOver(CLAY_IDI("MwadcButton", MW_ROW_INOFF))) {
        gui_dropdown_toggle(MW_DROPDOWN, MW_ROW_INOFF);
        return true;
    } else {
        // Level sliders: [-]/[+] = one raw step (shift: 16), the track starts a drag
        // (gui_ui_mwadc_drag_tick follows it, applies live and saves on release).
        const int step = (IsKeyDown(KEY_LEFT_SHIFT) || IsKeyDown(KEY_RIGHT_SHIFT)) ? 16 : 1;
        for (int id = 0; id < MW_SL_COUNT; id++) {
            int adc, lo, hi, val;
            if (!gui_ui_mwadc_slider(app, id, &adc, &lo, &hi, &val)) continue;
            if (Clay_PointerOver(CLAY_IDI("MwadcReset", id))) {
                changed = gui_ui_mwadc_slider_set(app, id, gui_ui_mwadc_slider_default(app, id));
                if (!changed) return true;
                break;
            } else if (Clay_PointerOver(CLAY_IDI("MwadcMinus", id))) {
                changed = gui_ui_mwadc_slider_set(app, id, val - step);
                if (!changed) return true;
                break;
            } else if (Clay_PointerOver(CLAY_IDI("MwadcPlus", id))) {
                changed = gui_ui_mwadc_slider_set(app, id, val + step);
                if (!changed) return true;
                break;
            } else if (Clay_PointerOver(CLAY_IDI("MwadcTrack", id))) {
                s_mw_drag = id;
                if (gui_ui_mwadc_slider_from_mouse(app, id)) s_mw_apply_pending = true;
                gui_ui_mwadc_apply_levels(app, true);
                return true;
            }
        }
    }
    if (!changed) return false;
    gui_settings_save(s);
    if (app->is_capturing) gui_mwadc_apply_settings(app);
    return true;
}
