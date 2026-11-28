#!/usr/bin/env python3
"""Build the interactive capture viewer (one self-contained HTML page) from raw2wav.py output.

Each MODE is a raw2wav name prefix: DIR/NAME reads NAME_full.wav, NAME_full_labels.txt and, if present,
NAME_rt.wav / NAME_rt_labels.txt in DIR. Optionally DIR/NAME:Title:Note. Without a note, the label track's
"FINDING ..." text is used.

The page shows a WINDOW of every sample per mode (zoom/pan, residual after a fitted sine per channel,
line-start ticks, re-centring pulses, detected jumps on any channel) and a 1 ms min/max/mean strip of the
whole run. The window starts at 0, or 1.5 ms before the first re-centring pulse if there is one.

usage: scope_html.py OUT.html MODE [MODE ...] [--tones 1000,3700,0] [--window-ms 4] [--subtitle TEXT]
"""
import argparse
import base64
import json
import os
import wave

import numpy as np
from scipy.ndimage import median_filter

HERE = os.path.dirname(os.path.abspath(__file__))


def rd_wav(p):
    w = wave.open(p)
    d = np.frombuffer(w.readframes(w.getnframes()), '<i2').reshape(-1, w.getnchannels())
    return w.getframerate(), (d.astype(np.int32) // 64 + 512)


def labels(p):
    if not os.path.exists(p):
        return []
    return [(float(a), float(b), t) for a, b, t in (l.rstrip('\n').split('\t') for l in open(p, encoding='utf-8'))]


def fit(x, f0, fs):
    """[cycles/sample, sin, cos, dc] of the best sine near f0 (f0 = 0: dc only)."""
    if not f0:
        return [0.0, 0.0, 0.0, float(x.mean())]
    n = np.arange(len(x)); best = None
    for ff in np.linspace(f0 * 0.998, f0 * 1.002, 81) / fs:
        A = np.stack([np.sin(2 * np.pi * ff * n[::7]), np.cos(2 * np.pi * ff * n[::7]), np.ones(len(n[::7]))], 1)
        co, *_ = np.linalg.lstsq(A, x[::7].astype(float), rcond=None)
        r = np.std(x[::7] - A @ co)
        if best is None or r < best[0]:
            best = (r, ff, co)
    _, ff, co = best
    return [float(ff), float(co[0]), float(co[1]), float(co[2])]


def jumps(x, threshold=90, k=32):
    """Steps between adjacent k-sample means larger than threshold codes (slow content removed)."""
    s = np.convolve(x.astype(float), np.ones(k) / k, 'valid')
    j = s[k:] - s[:-k]
    j = j - median_filter(j, 1001)
    ev = []
    for i in np.where(np.abs(j) > threshold)[0]:
        if not ev or i - ev[-1][0] > 300:
            ev.append([int(i), float(j[i])])
        elif abs(j[i]) > abs(ev[-1][1]):
            ev[-1] = [int(i), float(j[i])]
    return [[e[0] + k, round(e[1])] for e in ev]


def b64(a, dt):
    return base64.b64encode(np.ascontiguousarray(a, dtype=dt).tobytes()).decode()


def mode(spec, tones, win_ms, slow):
    parts = spec.split(':', 2)
    path = parts[0]
    d, name = os.path.dirname(path) or '.', os.path.basename(path)
    r, full = rd_wav(os.path.join(d, f'{name}_full.wav'))
    fs = r * slow
    fl = labels(os.path.join(d, f'{name}_full_labels.txt'))
    lines = sorted(a for a, b, t in fl if t.startswith('line '))
    cx = int(round((lines[1] - lines[0]) * r)) if len(lines) > 1 else 0
    pulses_all = [(int(a * r), int(b * r)) for a, b, t in fl if t.startswith('recenter')]
    s0 = max(0, pulses_all[0][0] - int(1.5e-3 * fs)) if pulses_all else 0
    n = min(int(win_ms * 1e-3 * fs), len(full) - s0)
    win = full[s0:s0 + n]
    line_phase = (int(round(lines[0] * r)) - s0) % cx if cx else 0
    finding = next((t for a, b, t in fl if t.startswith('FINDING')), '')
    m = dict(
        id=name, title=parts[1] if len(parts) > 1 else name, note=parts[2] if len(parts) > 2 else finding,
        signal=', '.join(f'ch{c + 1}: {t:g} Hz' if t else f'ch{c + 1}: no tone' for c, t in enumerate(tones)),
        n=int(n), line_phase=int(line_phase), cx=cx,
        pulses=[[a - s0, b - s0] for a, b in pulses_all],
        fits=[fit(win[:, c], tones[c] if c < len(tones) else 0, fs) for c in range(win.shape[1])],
        glitches=sorted(j for c in range(win.shape[1]) for j in jumps(win[:, c])),
        data=b64(win.reshape(-1), '<u2'), rt=None)
    rtp = os.path.join(d, f'{name}_rt.wav')
    if os.path.exists(rtp):
        rr, rt = rd_wav(rtp)
        B = int(rr / 1000)                                   # 1 ms blocks
        nb = len(rt) // B
        blk = rt[:nb * B].reshape(nb, B, -1)
        rtd = np.stack([blk.min(1), blk.max(1), np.round(blk.mean(1))], 2).reshape(nb, -1)
        rpul = [[a, b] for a, b, t in labels(os.path.join(d, f'{name}_rt_labels.txt')) if t.startswith('recenter')]
        m['rt'] = dict(n=int(nb), block_ms=1.0, data=b64(rtd.reshape(-1), '<i2'), pulses=rpul)
    return fs, m


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('out')
    ap.add_argument('modes', nargs='+')
    ap.add_argument('--tones', default='1000,3700,0', help='tone per channel in Hz for the residual fit (0 = none)')
    ap.add_argument('--window-ms', type=float, default=4.0)
    ap.add_argument('--slow', type=int, default=1000, help='raw2wav --slow factor of the inputs')
    ap.add_argument('--subtitle', default='Raw ADC captures, 10-bit codes.')
    a = ap.parse_args()
    tones = [float(t) for t in a.tones.split(',')]
    built = [mode(s, tones, a.window_ms, a.slow) for s in a.modes]
    fs = built[0][0]
    data = dict(fs=fs, cx=built[0][1]['cx'], modes=[m for _, m in built])
    t = open(os.path.join(HERE, 'template.html'), encoding='utf-8').read()
    html = t.replace('__SUBTITLE__', a.subtitle).replace('__DATA__', json.dumps(data))
    open(a.out, 'w', encoding='utf-8', newline='\n').write(html)
    print(f'{a.out}: {len(built)} modes, {os.path.getsize(a.out) / 1e6:.1f} MB')


if __name__ == '__main__':
    main()
