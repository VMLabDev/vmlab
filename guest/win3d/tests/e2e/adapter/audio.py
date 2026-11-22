#!/usr/bin/env python3
"""Count the breaks in a capture of the tone probe's sine (card #447).

    audio.py <capture.wav> <out.json> [--hz 997] [--hv-log FILE --hv-log-offset BYTES]
             [--raw RATE]

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

Each discontinuity gets the jump of the sine's phase across it, in samples
(modulo one cycle), and the breaks are sorted by what they look like:

  displaced  two discontinuities at most 30 ms apart whose jumps cancel: the
             stretch between them is sound from another time. A period the
             guest refilled too late looks like this: the sound card model
             reads the period that was there before. It is made in the guest's
             half of the path.
  gaps       silence put in by the host half: vmlab-hv's queue ran empty, or
             the sound server had no buffer from it in time.
  shifts     the rest: the phase jumps and stays. Frames were lost or
             repeated on the way, in the sound server or in the capture.

With --hv-log it also sums the underruns vmlab-hv wrote to the VM's log after
the byte offset: the pulls of the host's sound server that the VM could not
fill, and the frames of silence it put in their place.

With --raw the capture is headerless S16LE stereo at that rate: the file
vmlab-hv appends each period it hands to the host to, when it runs with
VMLAB_HV_TEST_AUDIO_PLAYBACK=<file> and VMLAB_HV_TEST_AUDIO_TEE=1. A break
that is in that file was made before the host's sound server.

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
    res.update(classify(signal, places, gaps, w, rate))
    res['gaps'] = len(gaps)
    res['gap_seconds'] = round(sum(n for _, n in gaps) / rate, 4)
    res['first_discontinuities_s'] = [round((first + i) / rate, 3) for i in places[:20]]
    res['first_gaps'] = [{'at_s': round((first + i) / rate, 3), 'ms': round(n * 1000 / rate, 1)} for i, n in gaps[:20]]
    return res


def phase_jump(signal, at, w):
    """The jump of the sine's phase across sample `at`, in samples, in half a
    cycle either way. Two cycles are fitted on each side."""
    cycle = 2 * math.pi / w
    n = int(2 * cycle)

    def phase(start):
        seg = signal[start:start + n]
        if start < 0 or len(seg) < n:
            return None
        s = sum(v * math.sin(w * i) for i, v in enumerate(seg, start))
        c = sum(v * math.cos(w * i) for i, v in enumerate(seg, start))
        return math.atan2(c, s)

    before, after = phase(at - 6 - n), phase(at + 6)
    if before is None or after is None:
        return None
    return ((after - before + math.pi) % (2 * math.pi) - math.pi) / w


def classify(signal, places, gaps, w, rate):
    """Sort the discontinuities into displaced stretches, gap edges and shifts."""
    near_gap = set()
    for start, n in gaps:
        near_gap.update(i for i in places if start - rate // 500 <= i <= start + n + rate // 500)
    rest = [i for i in places if i not in near_gap]
    jumps = {i: phase_jump(signal, i, w) for i in rest}
    displaced, shifts, k = [], [], 0
    while k < len(rest):
        a = rest[k]
        b = rest[k + 1] if k + 1 < len(rest) else None
        ja, jb = jumps[a], jumps.get(b)
        if b is not None and b - a <= 0.03 * rate and ja is not None and jb is not None and abs(ja + jb) < 0.75:
            displaced.append({'at': a, 'frames': b - a, 'jump_samples': round(ja, 2)})
            k += 2
        else:
            shifts.append({'at': a, 'jump_samples': None if ja is None else round(ja, 2)})
            k += 1
    lengths = {}
    for d in displaced:
        lengths[d['frames']] = lengths.get(d['frames'], 0) + 1
    return {'displaced': len(displaced), 'shifts': len(shifts),
            'displaced_frames': dict(sorted(lengths.items(), key=lambda kv: -kv[1])[:6]),
            'first_shifts': shifts[:10]}


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
    ap.add_argument('--raw', type=int, metavar='RATE')
    a = ap.parse_args()
    res = {'capture': a.capture, 'hz': a.hz, 'error': None}
    try:
        data = array.array('h')
        if a.raw:
            rate, channels = a.raw, 2
            with open(a.capture, 'rb') as f:
                raw = f.read()
            data.frombytes(raw[:len(raw) - len(raw) % 4])
        else:
            with wave.open(a.capture) as wav:
                if wav.getsampwidth() != 2:
                    raise ValueError(f'the capture has {wav.getsampwidth() * 8}-bit samples; S16 is expected')
                rate, channels = wav.getframerate(), wav.getnchannels()
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
    print(f"audio: {res.get('signal_seconds', 0)} s of signal, {res.get('discontinuities', '?')} discontinuities "
          f"({res.get('displaced', '?')} displaced stretches, {res.get('shifts', '?')} shifts), "
          f"{res.get('gaps', '?')} gaps" + (f"; vmlab-hv underruns: {res['hv'].get('underruns')}" if a.hv_log else '')
          + (f"; error: {res['error']}" if res['error'] else ''))
    return 1 if res['error'] else 0


if __name__ == '__main__':
    sys.exit(main())
