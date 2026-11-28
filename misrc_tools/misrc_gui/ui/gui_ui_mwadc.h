/*
 * MISRC GUI - Magewell raw-ADC settings panel (gui_ui_mwadc.c).
 *
 * Maintained in the mwadc repository (https://github.com/JonasCz/mwadc, misrc-gui/)
 * and copied here by its scripts/build-all.ps1: make changes there.
 */

#ifndef GUI_UI_MWADC_H
#define GUI_UI_MWADC_H

#include <stdbool.h>

#include "gui_ui.h"

// True while a Magewell card is the selected device.
bool gui_ui_selected_device_is_mwadc(const gui_app_t *app);
// Settings rows for the selected Magewell card (inside the settings panel's Clay layout).
void gui_ui_mwadc_render_settings(gui_app_t *app);
// Per frame: follow a level-slider drag, apply changes live (throttled), save when it ends.
void gui_ui_mwadc_drag_tick(gui_app_t *app);
// True while a level slider is pressed or dragged: the main loop disables drag scrolling then.
bool gui_ui_mwadc_blocks_drag_scroll(const gui_app_t *app);
// Click on a Magewell settings control; true when the click was consumed.
bool gui_ui_mwadc_handle_click(gui_app_t *app);

#endif /* GUI_UI_MWADC_H */
