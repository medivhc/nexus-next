"""Independent runtime replay of the window contract using a local SOCKS fixture."""
import os
from pathlib import Path
import runpy
import socket
import subprocess
import sys
import threading
import time
from types import SimpleNamespace
import unittest

TARGET = Path(os.environ["TARGET"]).resolve()
Bench = runpy.run_path(str(TARGET))["Bench"]
OUTSIDE_SAMPLE = 7777.0


def receive(connection, size):
    data = bytearray()
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise RuntimeError("fixture: premature handshake EOF")
        data.extend(chunk)
    return data


class ControlledBench(Bench):
    def __init__(self, listener, startup_seconds, transfer_seconds, probe_seconds):
        self.args = SimpleNamespace(seconds=8.0, verbose=False)
        self.started = None
        self.listener = listener
        self.startup_seconds = startup_seconds
        self.transfer_seconds = transfer_seconds
        self.probe_seconds = probe_seconds
        self.active_start = None
        self.active_end = None
        self.starts = []
        self.stopped = set()
        self.child = None
        self.server = threading.Thread(target=self.serve, daemon=True)
        self.server.start()

    def serve(self):
        try:
            with self.listener.accept()[0] as connection:
                connection.settimeout(10)
                receive(connection, 3)
                connection.sendall(b"\x05\x00")
                header = receive(connection, 5)
                receive(connection, header[4] + 2)
                time.sleep(self.startup_seconds)
                connection.sendall(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")
                self.active_start = time.monotonic()
                self.active_end = self.active_start + self.transfer_seconds
                while time.monotonic() < self.active_end:
                    connection.sendall(b"x" * 4096)
                    time.sleep(0.02)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def probe(self):
        if self.child is None:
            return 1.0
        start = time.monotonic()
        self.starts.append(start)
        time.sleep(self.probe_seconds)
        end = time.monotonic()
        if (self.active_start is None or start < self.active_start
                or end > min(self.active_end, self.active_start + self.args.seconds) + 0.08):
            return OUTSIDE_SAMPLE
        return 30.0

    def in_ns(self, node, code, port, seconds):
        self.started = time.monotonic()
        self.child = subprocess.Popen(
            [sys.executable, "-c", code, str(self.listener.getsockname()[1]), str(seconds)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        return self.child

    def stop_daemon(self, node):
        self.stopped.add(node)

    def close(self):
        if self.child is not None:
            if self.child.poll() is None:
                self.child.kill()
            self.child.communicate(timeout=3)
        self.server.join(timeout=3)


class LoopbackWindow(unittest.TestCase):
    def create_bench(self, delay, transfer, probe):
        listener = socket.socket()
        self.addCleanup(listener.close)
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(12)
        bench = ControlledBench(listener, delay, transfer, probe)
        self.addCleanup(bench.close)
        return bench

    def check_contained(self, delay, probe, minimum):
        bench = self.create_bench(delay, 15.0, probe)
        result = bench.run(True)
        self.assertGreaterEqual(result["loaded"]["n"], minimum)
        self.assertEqual(result["loaded"]["max_ms"], 30.0)
        self.assertLessEqual(max(bench.starts), bench.active_start + bench.args.seconds + 0.08)
        self.assertEqual(bench.stopped, {"home", "laptop"})

    def test_control_normal_start_fast_probes(self):
        self.check_contained(0.0, 0.04, 5)

    def test_slow_probes_straddle_end(self):
        self.check_contained(0.0, 0.62, 2)

    def test_delayed_handshake_still_yields_contained_samples(self):
        self.check_contained(3.6, 0.25, 2)

    def test_empty_transfer_fails_cleanly(self):
        bench = self.create_bench(0.0, 0.2, 0.15)
        with self.assertRaises(RuntimeError):
            bench.run(True)
