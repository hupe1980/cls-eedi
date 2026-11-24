#!/usr/bin/env python3
"""Kill and restart a local Rust simulation across two durable commit boundaries."""

import json
import selectors
import signal
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def interrupt(binary, directory, phase, expected):
    with subprocess.Popen(
        [binary, phase, directory], stdout=subprocess.PIPE, text=True
    ) as worker:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(worker.stdout, selectors.EVENT_READ)
                if not selector.select(timeout=10):
                    raise TimeoutError(f"worker never reached {expected}")
                actual = worker.stdout.readline().strip()
                if actual != expected:
                    raise RuntimeError(f"expected {expected!r}, got {actual!r}")
            worker.kill()
            assert worker.wait(timeout=5) == -signal.SIGKILL
            print(f"Terminated PID {worker.pid} after {expected}.", flush=True)
        finally:
            if worker.poll() is None:
                worker.kill()
                worker.wait(timeout=5)


def main():
    report = subprocess.check_output(
        ["cargo", "build", "--locked", "--example", "restart", "--message-format=json"],
        cwd=ROOT,
        text=True,
    )
    binary = next(
        msg["executable"]
        for line in report.splitlines()
        if (msg := json.loads(line)).get("executable")
    )
    with tempfile.TemporaryDirectory() as directory:
        interrupt(binary, directory, "submit", "equipment-accepted")
        interrupt(binary, directory, "reconcile", "ack-durable")
        for phase in ["recover", "verify-expiry"]:
            subprocess.run([binary, phase, directory], check=True, timeout=10)


if __name__ == "__main__":
    main()
