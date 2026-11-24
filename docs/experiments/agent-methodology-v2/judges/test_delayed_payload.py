"""Exploratory replay: the SOCKS handshake can complete before any bulk payload."""
import time
import unittest

import test_window_loopback as loopback


class DelayedPayloadBench(loopback.ControlledBench):
    def serve(self):
        try:
            with self.listener.accept()[0] as connection:
                connection.settimeout(10)
                loopback.receive(connection, 3)
                connection.sendall(b"\x05\x00")
                header = loopback.receive(connection, 5)
                loopback.receive(connection, header[4] + 2)
                connection.sendall(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")
                time.sleep(4.2)
                self.active_start = time.monotonic()
                self.active_end = self.active_start + self.transfer_seconds
                while time.monotonic() < self.active_end:
                    connection.sendall(b"x" * 4096)
                    time.sleep(0.02)
        except (BrokenPipeError, ConnectionResetError):
            pass


class PayloadOnly(unittest.TestCase):
    def test_connected_but_idle_period_does_not_count_as_loaded(self):
        fixture = loopback.LoopbackWindow()
        self.addCleanup(fixture.doCleanups)
        original = loopback.ControlledBench
        try:
            loopback.ControlledBench = DelayedPayloadBench
            fixture.check_contained(0.0, 0.25, 2)
        finally:
            loopback.ControlledBench = original
