#!/usr/bin/env python3
"""Tests of the host halves of the adapter suite's audio and latency checks.

Run from the repository root: python3 -m unittest discover -s scripts -p 'test_*.py'
"""
import math
import sys
import tempfile
import unittest
from pathlib import Path

ADAPTER = Path(__file__).resolve().parent.parent / 'tests/e2e/adapter'
sys.path.insert(0, str(ADAPTER))
import audio  # noqa: E402
import latency  # noqa: E402

RATE = 48000


def sine(seconds, skips=(), zeros=()):
    """A 997 Hz sine at a quarter of full scale. `skips` are (sample, frames
    dropped) and `zeros` (sample, frames silenced)."""
    out, phase = [], 0
    skips = dict(skips)
    for i in range(int(seconds * RATE)):
        phase += skips.get(i, 0)
        out.append(round(8192 * math.sin(2 * math.pi * 997 * (i + phase) / RATE)))
    for start, n in zeros:
        out[start:start + n] = [0] * n
    return [0] * 4800 + out + [0] * 4800


class Analyse(unittest.TestCase):
    def test_a_clean_sine_has_no_break(self):
        r = audio.analyse(sine(2), RATE, 997)
        self.assertEqual((r['discontinuities'], r['gaps']), (0, 0))
        self.assertAlmostEqual(r['signal_seconds'], 2, places=2)
        self.assertAlmostEqual(r['lead_in_seconds'], 0.1, places=2)
        self.assertAlmostEqual(r['frequency_hz'], 997, delta=2)

    def test_dropped_frames_are_one_discontinuity_each(self):
        r = audio.analyse(sine(2, skips=((30000, 17), (60000, 256))), RATE, 997)
        self.assertEqual((r['discontinuities'], r['gaps']), (2, 0))
        self.assertEqual(r['first_discontinuities_s'], [0.725, 1.35])

    def test_inserted_silence_is_a_gap_with_a_break_at_each_end(self):
        r = audio.analyse(sine(2, zeros=((40000, 256),)), RATE, 997)
        self.assertEqual((r['discontinuities'], r['gaps']), (2, 1))
        self.assertAlmostEqual(r['gap_seconds'], 256 / RATE, places=3)

    def test_breaks_are_sorted_by_their_phase_jump(self):
        # A period of 480 frames replaced by the sound of 960 frames earlier, a
        # stretch of silence, and 768 frames lost.
        stale = sine(3)
        stale[4800 + 48000:4800 + 48480] = stale[4800 + 47040:4800 + 47520]
        r = audio.analyse(stale, RATE, 997)
        self.assertEqual((r['discontinuities'], r['displaced'], r['shifts'], r['gaps']), (2, 1, 0, 0))
        self.assertEqual(r['displaced_frames'], {480: 1})

        r = audio.analyse(sine(3, zeros=((48000, 256),)), RATE, 997)
        self.assertEqual((r['discontinuities'], r['displaced'], r['shifts'], r['gaps']), (2, 0, 0, 1))

        r = audio.analyse(sine(3, skips=((48000, 768),)), RATE, 997)
        self.assertEqual((r['discontinuities'], r['displaced'], r['shifts'], r['gaps']), (1, 0, 1, 0))
        self.assertAlmostEqual(r['first_shifts'][0]['jump_samples'], 768 % (RATE / 997) - RATE / 997, delta=0.3)

    def test_a_silent_capture_is_an_error(self):
        self.assertEqual(audio.analyse([0] * RATE, RATE, 997)['error'], 'the capture is silent')


class HvLog(unittest.TestCase):
    def test_underrun_lines_after_the_offset_are_summed(self):
        line = ('{"level":"WARN","message":"PipeWire playback underrun: underruns=%d silence_frames=%d '
                'late_callbacks=%d longest_gap_ms=%s interval_s=10 queue_frames=%d"}\n')
        old = line % (9, 900, 9, '99.0', 512)
        new = line % (2, 356, 1, '30.0', 768) + '{"message":"other"}\n' + line % (1, 100, 0, '0.0', 1024)
        with tempfile.NamedTemporaryFile('w', suffix='.log') as f:
            f.write(old + new)
            f.flush()
            r = audio.hv_underruns(f.name, len(old))
        self.assertEqual(r, {'lines': 2, 'underruns': 3, 'silence_frames': 456, 'late_callbacks': 1,
                             'longest_gap_ms': 30.0, 'queue_frames': 1024})

    def test_a_missing_log_is_reported(self):
        self.assertIn('error', audio.hv_underruns('/nonexistent/vmlab-hv.log', 0))


class Latency(unittest.TestCase):
    def test_summary_percentiles(self):
        s = latency.summary([float(v) for v in range(1, 101)])
        self.assertEqual((s['n'], s['p50'], s['p90'], s['p99'], s['max']), (100, 51.0, 91.0, 100.0, 100.0))

    def test_a_sample_is_one_palette_colour_or_nothing(self):
        red = bytes((0, 0, 255, 0)) * 64  # B G R X
        self.assertEqual(latency.colour_of(bytearray(red)), 'red')
        mixed = bytearray(red)
        mixed[0:4] = bytes((255, 0, 0, 0))
        self.assertIsNone(latency.colour_of(mixed))
        self.assertIsNone(latency.colour_of(bytearray(bytes((50, 50, 50, 0)) * 64)))


if __name__ == '__main__':
    unittest.main()
