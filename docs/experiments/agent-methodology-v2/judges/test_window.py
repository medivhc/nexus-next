"""Run against a candidate by setting TARGET to its sched_bench.py."""
import os
from pathlib import Path
import runpy
from types import SimpleNamespace
import unittest
from unittest.mock import patch

TARGET = Path(os.environ["TARGET"]).resolve()
mod = runpy.run_path(str(TARGET))
Bench = mod["Bench"]


class Clock:
    def __init__(self):
        self.now = 0.0

    def sleep(self, duration):
        self.now += duration

    def monotonic(self):
        return self.now

    time = monotonic


class Transfer:
    def __init__(self, clock, duration, exit_code=0):
        self.clock = clock
        self.end = clock.now + duration
        self.returncode = None
        self.exit_code = exit_code

    def poll(self):
        if self.clock.now >= self.end:
            self.returncode = self.exit_code
        return self.returncode

    def communicate(self, timeout=None):
        self.clock.sleep(max(0.0, self.end - self.clock.now))
        self.poll()
        return "10000000", ""


class ControlledBench(Bench):
    def __init__(self, clock, seconds, probe_delay, actual_duration=None, exit_code=0):
        self.clock = clock
        self.args = SimpleNamespace(seconds=seconds, verbose=False)
        self.probe_delay = probe_delay
        self.actual_duration = actual_duration
        self.exit_code = exit_code
        self.starts = []
        self.ends = []
        self.transfer = None
        self.stopped = []

    def probe(self):
        if self.transfer is None:
            self.clock.sleep(0.01)
            return 10.0
        self.starts.append(self.clock.now)
        self.clock.sleep(self.probe_delay)
        self.ends.append(self.clock.now)
        return 999.0 if self.clock.now > self.transfer.end + 1e-9 else 30.0

    def in_ns(self, node, code, port, seconds):
        self.started = self.clock.now
        duration = seconds if self.actual_duration is None else self.actual_duration
        self.transfer = Transfer(self.clock, duration, self.exit_code)
        return self.transfer

    def stop_daemon(self, node):
        self.stopped.append(node)


class WindowContract(unittest.TestCase):
    def run_bench(self, seconds, delay, **kwargs):
        clock = Clock()
        bench = ControlledBench(clock, seconds, delay, **kwargs)
        with patch.multiple(Bench.run.__globals__["time"], sleep=clock.sleep,
                            monotonic=clock.monotonic, time=clock.time):
            return bench, bench.run(True)

    def test_fast_probes_keep_a_meaningful_loaded_sample(self):
        bench, result = self.run_bench(15, 0.05)
        self.assertGreaterEqual(result["loaded"]["n"], 10)
        self.assertEqual(result["loaded"]["max_ms"], 30)
        self.assertEqual(set(bench.stopped), {"home", "laptop"})

    def test_slow_probes_cannot_contaminate_loaded_metrics(self):
        for seconds, delay in ((15, 0.60), (10, 0.80), (20, 1.30)):
            with self.subTest(seconds=seconds, delay=delay):
                bench, result = self.run_bench(seconds, delay)
                self.assertGreaterEqual(result["loaded"]["n"], 2)
                self.assertEqual(result["loaded"]["max_ms"], 30.0)
                self.assertTrue(all(start < bench.transfer.end for start in bench.starts))

    def test_empty_or_failed_transfer_cannot_report_loaded_metrics(self):
        for duration, exit_code in ((1, 0), (15, 3)):
            clock = Clock()
            bench = ControlledBench(clock, 15, 0.6, duration, exit_code)
            with self.subTest(duration=duration, exit_code=exit_code):
                with patch.multiple(Bench.run.__globals__["time"], sleep=clock.sleep,
                                    monotonic=clock.monotonic, time=clock.time):
                    with self.assertRaises(RuntimeError):
                        bench.run(True)
