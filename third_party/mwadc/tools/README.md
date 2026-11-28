# tools

Python 3 helpers. Capture analysis needs numpy and scipy.

- `raw2wav.py <session> [--slow 1000] [--excerpt-ms 20] [--line-labels]`: turn an `mwadc` capture into a real-time
  WAV (mean of `--slow` samples, e.g. 40 MSPS → 40 kHz) and a full-rate excerpt slowed by the same factor, each with an
  Audacity label track (frames, lines, re-centring pulses).
- `scope/scope_html.py out.html <dir/name>[:title[:note]]… [--tones 1000,3700,0]`: build the interactive capture viewer
  (one self-contained HTML page from `scope/template.html`) from `raw2wav.py` output: zoomable full-rate window with a
  residual view (fitted sine removed per channel) and markers, plus a whole-run DC strip.

`re/`: closed-driver reverse engineering (doc 11 §7), run with `python3 -I`:
- `DecompAll.java`: Ghidra headless post-script, decompiles every function into one C file
  (`analyzeHeadless <proj> <name> -import ProCaptureLib_64.lib -scriptPath tools/re -postScript DecompAll.java out.c`).
- `fn.sh <out.c> <func>…`: print the named functions from that file.
- `i2c_writes.py <out.c> <lo> <hi>`: constant-address ADV7842 I2C reads/writes per function.
- `adv7842_regtable.py <lib> [section off len]`: dump a `g_regItems*` (addr, reg, value) init table.
- `ks_property_sets.py <MWProCapture.sys>`: list the KS property sets/items of the Windows driver (doc 07).
