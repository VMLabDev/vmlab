#!/usr/bin/env python3
"""Count the breaks in a capture of the tone probe's sine (card #447).

    audio.py <capture.wav> <out.json> [--hz 997] [--hv-log FILE --hv-log-offset BYTES]

The tone probe (tests/e2e/adapter/probes/tone_probe.cpp) plays one sine. The
capture is what vmlab-hv played into the host's PipeWire, S16 at 48 kHz, taken
with pw-record from the VM's own node. On the first channel, between the first
and the last loud sample, this counts

  discontinuities  places where the curve bends more than a sine of this
                   frequency and amplitude can: the second difference of three
                   samples in a row is above 1.5 times the sine's largest, plus
                   4 units for rounding. Spikes less than 1 ms apart are one
                   place. A dropped, repeated or zeroed stretch makes one at
                   each of its ends.
  gaps             stretches of at least 1 ms that are silent.

With --hv-log it also sums the underruns vmlab-hv wrote to the VM's log after
the byte offset: the pulls of the host's sound server that the VM could not
fill, and the frames of silence it put in their place.

The result is one JSON object in <out.json>; tests/e2e/adapter/judge.py
decides. Only the standard library.
"""
import argparse
import array
import json
import math
import re
import sys
import wave


def analyse(samples, rate, hz):
    peak = max((abs(v) for v in samples), default=0)
    res = {'peak': peak, 'peak_of_full_scale': round(peak / 32768, 4)}
    if peak < 100:
        res['error'] = 'the capture is silent'
        return res
    loud = peak * 0.02
    first = next(i for i, v in enumerate(samples) if abs(v) > loud)
    last = next(i for i in range(len(samples) - 1, -1, -1) if abs(samples[i]) > loud)
    signal = samples[first:last + 1]
    res['signal_seconds'] = round(len(signal) / rate, 3)
    res['lead_in_seconds'] = round(first / rate, 3)
    crossings = sum(1 for i in range(1, len(signal)) if signal[i - 1] <= 0 < signal[i])
    res['frequency_hz'] = round(crossings * rate / len(signal), 2) if signal else 0
    # A sine A*sin(w*n) has a second difference of at most A*w*w.
    w = 2 * math.pi * hz / rate
    limit = 1.5 * peak * w * w + 4
    res['second_difference_limit'] = round(limit, 1)
    merge = rate // 1000
    silent = peak * 0.001
    places, spikes, gaps, run, last_spike = [], 0, [], 0, None
    for i in range(1, len(signal) - 1):
        if abs(signal[i + 1] - 2 * signal[i] + signal[i - 1]) > limit:
            spikes += 1
            if last_spike is None or i - last_spike > merge:
                places.append(i)
            last_spike = i
        if abs(signal[i]) <= silent:
            run += 1
        else:
            if run >= merge:
                gaps.append((i - run, run))
            run = 0
    res['spikes'] = spikes
    res['discontinuities'] = len(places)
    res['gaps'] = len(gaps)
    res['gap_seconds'] = round(sum(n for _, n in gaps) / rate, 4)
    res['first_discontinuities_s'] = [round((first + i) / rate, 3) for i in places[:20]]
    res['first_gaps'] = [{'at_s': round((first + i) / rate, 3), 'ms': round(n * 1000 / rate, 1)} for i, n in gaps[:20]]
    return res


def hv_underruns(path, offset):
    """What vmlab-hv logged about playback underruns after `offset`."""
    out = {'lines': 0, 'underruns': 0, 'silence_frames': 0, 'late_callbacks': 0, 'longest_gap_ms': 0.0,
           'queue_frames': None}
    try:
        with open(path, 'rb') as f:
            f.seek(offset)
            text = f.read().decode('utf-8', 'replace')
    except OSError as e:
        return {'error': str(e)}
    for line in text.splitlines():
        if 'PipeWire playback underrun' not in line:
            continue
        out['lines'] += 1
        fields = dict(re.findall(r'(\w+)=([0-9.]+)', line))
        for key in ('underruns', 'silence_frames', 'late_callbacks'):
            out[key] += int(float(fields.get(key, 0)))
        out['longest_gap_ms'] = max(out['longest_gap_ms'], float(fields.get('longest_gap_ms', 0)))
        if 'queue_frames' in fields:
            out['queue_frames'] = int(float(fields['queue_frames']))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('capture')
    ap.add_argument('out')
    ap.add_argument('--hz', type=float, default=997)
    ap.add_argument('--hv-log')
    ap.add_argument('--hv-log-offset', type=int, default=0)
    a = ap.parse_args()
    res = {'capture': a.capture, 'hz': a.hz, 'error': None}
    try:
        with wave.open(a.capture) as wav:
            if wav.getsampwidth() != 2:
                raise ValueError(f'the capture has {wav.getsampwidth() * 8}-bit samples; S16 is expected')
            rate, channels = wav.getframerate(), wav.getnchannels()
            data = array.array('h')
            data.frombytes(wav.readframes(wav.getnframes()))
        if sys.byteorder == 'big':
            data.byteswap()
        res.update(rate=rate, channels=channels, capture_seconds=round(len(data) / channels / rate, 3))
        res.update(analyse(data[::channels], rate, a.hz))
    except (OSError, EOFError, ValueError, wave.Error) as e:
        res['error'] = f'{type(e).__name__}: {e}'
    if a.hv_log:
        res['hv'] = hv_underruns(a.hv_log, a.hv_log_offset)
    with open(a.out, 'w') as f:
        json.dump(res, f, indent=2)
    print(f"audio: {res.get('signal_seconds', 0)} s of signal, {res.get('discontinuities', '?')} discontinuities, "
          f"{res.get('gaps', '?')} gaps" + (f"; vmlab-hv underruns: {res['hv'].get('underruns')}" if a.hv_log else '')
          + (f"; error: {res['error']}" if res['error'] else ''))
    return 1 if res['error'] else 0


if __name__ == '__main__':
    sys.exit(main())
