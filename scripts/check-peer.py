#!/usr/bin/env python3
"""Run the local-device LPC profile against the independent OpenEnergyTwin uMSB."""

import argparse
import json
import os
import queue
import socket
import subprocess
import tempfile
import threading
import time
import uuid
from contextlib import ExitStack
from datetime import UTC, datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlencode, urlsplit
from urllib.request import Request, urlopen

PEER_REVISION = "6de729c3e1b7a1407892d3f0de9eac4ea449730a"


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def await_listen(port, peer):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline and peer.poll() is None:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("uMSB did not start")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "peer",
        nargs="?",
        type=Path,
        help="clean simulation-umsb checkout at " + PEER_REVISION,
    )
    parser.add_argument("port", type=int, help="dedicated loopback MQTT broker port")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as temp:
        if args.peer is None:
            peer = Path(temp)
            subprocess.run(["git", "init", "-q", peer], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    peer,
                    "fetch",
                    "-q",
                    "--depth=1",
                    "https://github.com/OpenEnergyTwin/simulation-umsb",
                    PEER_REVISION,
                ],
                timeout=60,
                check=True,
            )
            subprocess.run(
                ["git", "-C", peer, "checkout", "-q", "--detach", PEER_REVISION],
                check=True,
            )
        else:
            peer = args.peer.resolve()
        if subprocess.check_output(
            ["git", "-C", peer, "rev-parse", "HEAD"], text=True
        ).strip() != PEER_REVISION or subprocess.check_output(
            ["git", "-C", peer, "status", "--porcelain", "--untracked-files=no"]
        ):
            parser.error("supply the unchanged pinned peer")
        check(peer, args.port)


def check(peer, port):
    root = Path(__file__).resolve().parents[1]

    def build(cwd, options):
        report = subprocess.check_output(
            ["cargo", "build", "--locked", "--message-format=json", *options],
            cwd=cwd,
            text=True,
        )
        return next(
            e["executable"]
            for line in report.splitlines()
            if (e := json.loads(line)).get("executable")
        )

    device_bin = build(root, ["--features", "mqtt", "--example", "mqtt_device"])
    deferred_bin = build(
        root,
        [
            "--manifest-path",
            "interop/device-peer/Cargo.toml",
            "--example",
            "deferred_device",
        ],
    )
    reader = build(root, ["--features", "mqtt", "--example", "mqtt_read"])
    peer_bin = build(peer, ["--bin", "umsb"])
    callbacks = queue.Queue()

    class Callback(BaseHTTPRequestHandler):
        def do_POST(self):
            callbacks.put(parse_qs(urlsplit(self.path).query))
            self.send_response(200)
            self.end_headers()

        def log_message(self, *unused):
            pass

    with ExitStack() as cleanup, tempfile.TemporaryFile(mode="w+") as device_log:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            peer_port = sock.getsockname()[1]
        server = ThreadingHTTPServer(("127.0.0.1", 0), Callback)
        cleanup.callback(server.server_close)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        cleanup.callback(server.shutdown)
        app = subprocess.Popen(
            [peer_bin],
            env={
                **os.environ,
                "SERVER_HOST": "127.0.0.1",
                "SERVER_PORT": str(peer_port),
                "MQTT_URL": f"mqtt://127.0.0.1:{port}?client_id=umsb-local-test",
                "WEBHOOK_BASE_URL": f"http://127.0.0.1:{server.server_port}",
            },
            stdout=subprocess.DEVNULL,
        )
        cleanup.callback(stop, app)
        await_listen(peer_port, app)
        device = subprocess.Popen([device_bin, str(port)], stdout=device_log)
        cleanup.callback(stop, device)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            # pread does not change the child's shared file offset.
            if b"Ready for controls" in os.pread(device_log.fileno(), 4096, 0):
                break
            if device.poll() is not None:
                raise RuntimeError("local device exited")
            time.sleep(0.05)
        else:
            raise RuntimeError("local device did not subscribe")

        def submit(location, action, watts_kw=None):
            field = "commandControl" if action == "konfiguration" else "commandRegular"
            ref = str(uuid.uuid4())
            timestamp = datetime.now(UTC).isoformat(timespec="seconds")
            control = {"executionTimeFrom": timestamp}
            if action == "konfiguration":
                control["maximumPowerValue"] = watts_kw
            params = urlencode({"locationId": location, field: json.dumps(control)})
            url = f"http://127.0.0.1:{peer_port}/%5BPost%5D/steuerbefehl/{action}/?{params}"
            # uMSB's literal bracket routes are matched on the raw path by axum.
            req = Request(
                url.replace("%5BPost%5D", "[Post]"),
                method="POST",
                headers={"transactionId": ref, "creationDateTime": timestamp},
            )
            with urlopen(req, timeout=5):
                pass
            return ref

        terminal = {}

        def await_result(ref):
            end = time.monotonic() + 35
            while ref not in terminal:
                remaining = end - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"missing terminal callback for {ref}")
                reply = callbacks.get(timeout=remaining)
                for field, key in [
                    ("resultPositive", "statePositive"),
                    ("resultNegative", "stateNegative"),
                ]:
                    if field in reply:
                        terminal[reply["referenceId"][0]] = json.loads(reply[field][0])[
                            key
                        ]
            return terminal[ref]

        for action in ("konfiguration", "initialZustand"):
            ref = submit("demo", action, "4.2")
            assert await_result(ref) == "succeeded", terminal
        output = os.pread(device_log.fileno(), 8192, 0).decode()
        for expected in ["4200 W (active=true)", "0 W (active=false)"]:
            if expected not in output:
                raise RuntimeError(
                    "equipment did not observe expected control: " + expected
                )
        print("Independent backend confirmed both simulated equipment outcomes.")

        with tempfile.TemporaryFile() as trace:
            fixture = subprocess.Popen(
                [deferred_bin, str(port)], stdin=subprocess.PIPE, stdout=trace
            )
            try:

                def events():
                    return os.pread(trace.fileno(), 65536, 0).decode().splitlines()

                def wait_for(line):
                    until = time.monotonic() + 10
                    while time.monotonic() < until and fixture.poll() is None:
                        if line in events():
                            return
                        time.sleep(0.025)
                    raise AssertionError(f"missing {line!r}: {events()}")

                def send(command):
                    fixture.stdin.write((command + "\n").encode())
                    fixture.stdin.flush()

                wait_for("READY")
                first = submit("overlap", "konfiguration", "4.2")
                wait_for(f"WRITE {first} 4200")
                second = submit("overlap", "konfiguration", "3.1")
                wait_for(f"ACK {second} 3")
                assert await_result(second) == "communicationFailure", terminal
                send(first)
                wait_for(f"ACK {first} 0")
                send("read")
                wait_for("SETTINGS TIMEOUT")
                reply = subprocess.check_output(
                    [
                        reader,
                        str(port),
                        "clseedi/to-localdevice/overlap",
                        "clseedi/from-localdevice/overlap",
                        "--field",
                        "limits",
                    ],
                    timeout=15,
                    text=True,
                )
                limit = json.loads(reply)["data"]["limits"]["power"]["active"][
                    "consumption"
                ]
                assert limit["value"] == 4200 and limit["active"] is True, limit
                assert [e for e in events() if e.startswith("WRITE ")] == [
                    f"WRITE {first} 4200"
                ]
                # A successful subsequent command brackets the unanswered read and
                # drains callbacks through a known terminal event on the same route.
                reset = submit("overlap", "initialZustand")
                wait_for(f"WRITE {reset} 0")
                send(reset)
                assert await_result(reset) == "succeeded", terminal
                assert first not in terminal, terminal
                print(
                    "Overlap: one 4200 W write, second ACK 3 and negative callback, first ACK 0; readback 4200 W."
                )
                print(
                    "uMSB: no settings response within two seconds; local-device readback still succeeded."
                )
            finally:
                stop(fixture)
                fixture.stdin.close()


if __name__ == "__main__":
    main()
