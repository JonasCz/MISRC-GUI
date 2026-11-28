#!/usr/bin/env python3
"""Turn an mwadc capture session into WAV files plus Audacity label tracks.

Two views of the same capture:

* ``<name>_full.wav``: every ADC sample of a short excerpt, written with a sample rate of
  ``rate / slow`` (default slow = 1000, so 40 MSPS becomes a 40 kHz file and 1 ms of signal lasts
  1 s). Use it to look at what happens inside single lines (clamp steps, line boundaries).
* ``<name>_rt.wav``: the whole capture in real time, boxcar-averaged by ``slow`` samples (40 MSPS ->
  40 kHz). Use it to hear/see slow effects: DC drift, re-centring pulses, frame boundaries.

Channels: 1 = ADC0 (RG10 bits 10-19, Component Y by default), 2 = ADC1 (bits 0-9, Pr), 3 = ADC2
(bits 20-29, Pb); one channel for Y16 captures (ADC0). Samples are written as signed
16-bit: (code - 512) * 64, so code 0 and 1023 are the rails.

Label tracks (Audacity: File > Import > Labels), one per WAV, in that WAV's own time base:
frame boundaries (with skipped/torn flags), re-centring pulses (from log.txt, device time), the
session's mode, and in the full-rate excerpt every line start (``--line-labels``).

If ``audio.wav`` exists (``--audio``), ``<name>_audio_labels.txt`` marks the video frames on the
audio's time base (both use the card's 100 ns device clock).

usage: raw2wav.py SESSION [--out DIR] [--name NAME] [--slow 1000] [--excerpt-ms 20]
                  [--excerpt-at-ms 0] [--line-labels] [--note TEXT]
"""
import argparse
import csv
import json
import os
import re
import wave

import numpy as np


def load(session):
    meta = json.load(open(os.path.join(session, 'capture.json')))
    frames = list(csv.DictReader(open(os.path.join(session, 'frames.csv'))))
    log = open(os.path.join(session, 'log.txt'), encoding='utf-8', errors='replace').read()
    return meta, frames, log


def write_wav(path, rate, data):
    """data: (n, channels) float codes 0..1023."""
    pcm = np.clip((data - 512.0) * 64.0, -32768, 32767).astype('<i2')
    with wave.open(path, 'wb') as w:
        w.setnchannels(pcm.shape[1])
        w.setsampwidth(2)
        w.setframerate(int(round(rate)))
        w.writeframes(pcm.tobytes())


class Frames:
    def __init__(self, session, meta):
        self.y16 = meta['format'].lower().startswith('y16')
        self.cx, self.cy = meta['cx'], meta['cy']
        self.words_per_frame = meta['frame_bytes'] // (2 if self.y16 else 4)
        self.stride_words = meta['stride'] // (2 if self.y16 else 4)
        self.mm = np.memmap(os.path.join(session, 'frames.raw'), dtype='<u2' if self.y16 else '<u4', mode='r')
        self.n = len(self.mm) // self.words_per_frame
        self.channels = 1 if self.y16 else 3

    def get(self, k):
        """Frame k as (samples, channels) codes, lines concatenated."""
        a = np.asarray(self.mm[k * self.words_per_frame:(k + 1) * self.words_per_frame])
        a = a.reshape(self.cy, self.stride_words)[:, :self.cx].ravel()
        if self.y16:
            return (a >> 6).astype(np.float32)[:, None]
        return np.stack([(a >> s) & 0x3FF for s in (10, 0, 20)], 1).astype(np.float32)


def recenter_pulses(log):
    """(device time 100 ns, hold ms) per pulse logged by mwadc."""
    out = []
    for m in re.finditer(r'recenter #\d+: voltage clamp on for ([\d.]+) ms at device time (\d+)', log):
        out.append((int(m.group(2)), float(m.group(1))))
    return out


def label_file(path, labels):
    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        for a, b, t in sorted(labels):
            f.write(f'{a:.6f}\t{b:.6f}\t{t}\n')


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('session')
    ap.add_argument('--out', help='output directory (default: the session)')
    ap.add_argument('--name', help='file name prefix (default: session folder name)')
    ap.add_argument('--slow', type=int, default=1000, help='slow-down / decimation factor (default 1000)')
    ap.add_argument('--excerpt-ms', type=float, default=20.0, help='real-time length of the full-rate excerpt')
    ap.add_argument('--excerpt-at-ms', type=float, default=0.0, help='start of the excerpt (ms from capture start)')
    ap.add_argument('--line-labels', action='store_true', help='label every line start in the excerpt')
    ap.add_argument('--note', default='', help='extra text for the mode label')
    a = ap.parse_args()

    meta, rows, log = load(a.session)
    fr = Frames(a.session, meta)
    fs = meta['sample_rate_hz']
    out = a.out or a.session
    os.makedirs(out, exist_ok=True)
    name = a.name or os.path.basename(os.path.normpath(a.session))
    spf = fr.cx * fr.cy                      # samples per frame
    t0 = int(rows[0]['start_100ns'])         # device time of the first sample

    def frame_start_sample(i):
        # Gapless capture: frame i starts at i * spf, unless frames were skipped (gap_frames).
        return sum(spf * (1 + int(r['gap_frames'])) for r in rows[:i])

    starts = [frame_start_sample(i) for i in range(fr.n)]
    cmd = re.search(r'^command: .*?mwadc(?:\.exe)? (.*)$', log, re.M)
    mode = (cmd.group(1).strip() if cmd else meta.get('mode', '')) + (f' | {a.note}' if a.note else '')
    pulses = recenter_pulses(log)

    # ---- real-time, decimated
    D = a.slow
    rt_rate = fs / D
    chunks, carry = [], np.zeros((0, fr.channels), np.float32)
    for k in range(fr.n):
        x = np.concatenate([carry, fr.get(k)])
        m = len(x) // D * D
        chunks.append(x[:m].reshape(-1, D, fr.channels).mean(1))
        carry = x[m:]
        gap = int(rows[k]['gap_frames']) if k < len(rows) else 0
        if gap:   # keep time aligned: fill skipped frames with mid-scale
            chunks.append(np.full((gap * spf // D, fr.channels), 512, np.float32))
    rt = np.concatenate(chunks)
    write_wav(os.path.join(out, f'{name}_rt.wav'), rt_rate, rt)
    lab = [(0.0, len(rt) / rt_rate, f'mode: {mode}')]
    for i, s in enumerate(starts):
        r = rows[i] if i < len(rows) else {}
        flag = (' SKIPPED %s before' % r['gap_frames'] if r and int(r['gap_frames']) else '') + (' TORN' if r and int(r['torn']) else '')
        lab.append((s / fs, s / fs, f'frame {i}{flag}'))
    for t, hold in pulses:
        s = (t - t0) * 1e-7
        if 0 <= s <= len(rt) / rt_rate:
            lab.append((s, s + hold / 1e3, f'recenter pulse ({hold:g} ms, clamp on)'))
    label_file(os.path.join(out, f'{name}_rt_labels.txt'), lab)

    # ---- full-rate excerpt, slowed down
    first = int(a.excerpt_at_ms * 1e-3 * fs)
    n = int(a.excerpt_ms * 1e-3 * fs)
    parts, k = [], first // spf
    off = first - k * spf
    while sum(len(p) for p in parts) < n and k < fr.n:
        x = fr.get(k)[off:]
        parts.append(x)
        k, off = k + 1, 0
    ex = np.concatenate(parts)[:n]
    write_wav(os.path.join(out, f'{name}_full.wav'), fs / D, ex)
    slow_t = lambda s: (s - first) / fs * D   # sample index -> seconds in the slowed file
    lab = [(0.0, slow_t(first + len(ex)), f'mode: {mode} | full rate, slowed x{D}: 1 s here = {1e3 / D:g} ms real')]
    for i, s in enumerate(starts):
        if first <= s < first + len(ex):
            lab.append((slow_t(s), slow_t(s), f'frame {i} start'))
    if a.line_labels:
        l0 = -(-first // fr.cx)
        for L in range(l0, (first + len(ex)) // fr.cx + 1):
            s = L * fr.cx
            if first <= s < first + len(ex):
                lab.append((slow_t(s), slow_t(s), f'line {L % fr.cy}'))
    for t, hold in pulses:
        s = (t - t0) * 1e-7 * fs
        if first <= s < first + len(ex):
            lab.append((slow_t(s), slow_t(s + hold * 1e-3 * fs), f'recenter pulse ({hold:g} ms)'))
    label_file(os.path.join(out, f'{name}_full_labels.txt'), lab)

    # ---- card audio input, if recorded
    apath = os.path.join(a.session, 'audio.csv')
    if os.path.exists(apath):
        arows = list(csv.DictReader(open(apath)))
        if arows:
            ta0 = int(arows[0]['timestamp_100ns'])
            lab = [(max(0.0, (int(r['start_100ns']) - ta0) * 1e-7), max(0.0, (int(r['start_100ns']) - ta0) * 1e-7),
                    f'video frame {i}') for i, r in enumerate(rows)]
            label_file(os.path.join(out, f'{name}_audio_labels.txt'), lab)

    print(f'{name}: {fr.n} frames {fr.cx}x{fr.cy} at {fs / 1e6:.6f} MSPS ({fr.channels} ch)')
    print(f'  {name}_rt.wav    {len(rt) / rt_rate:7.2f} s at {rt_rate:.1f} Hz (mean of {D} samples)')
    print(f'  {name}_full.wav  {len(ex) / fs * 1e3:7.2f} ms real -> {len(ex) / fs * D:.1f} s at {fs / D:.1f} Hz')
    if abs(rt_rate - round(rt_rate)) > 1e-3:
        print(f'  note: WAV headers carry {round(rt_rate)} Hz; the exact rate is {rt_rate:.4f} Hz')


if __name__ == '__main__':
    main()
