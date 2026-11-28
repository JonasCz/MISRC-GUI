# third_party/mwadc is a mirror: do not edit it here

Everything in this directory is copied from the standalone mwadc repository,
https://github.com/JonasCz/mwadc (commit 43c71b4 plus uncommitted changes), by that repository's
`scripts/build-all.ps1`. So are the GUI's Magewell files, from its `misrc-gui/` folder:
`misrc_tools/misrc_gui/input/gui_mwadc.c`, `gui_mwadc.h` (the capture backend) and
`misrc_tools/misrc_gui/ui/gui_ui_mwadc.c`, `gui_ui_mwadc.h` (the settings panel and its texts).

**Coding agents and contributors:** make changes to these files in https://github.com/JonasCz/mwadc,
then run its `scripts/build-all.ps1` to bring them here. The script refuses to copy over a file
that was changed here, so an edit made here has to be ported to mwadc first. If you only have this
checkout, describe the needed change instead of editing these files.

The rest of the integration belongs to MISRC-GUI and is edited here as usual: the `ENABLE_MWADC`
hooks in other GUI files and the `mwadc` parts of `misrc_tools/meson.build` / `meson_options.txt`.
