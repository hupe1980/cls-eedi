# /// script
# requires-python = ">=3.11,<3.15"
# dependencies = ["mosaik-api-v3==3.0.16", "paho-mqtt==2.1.0", "requests==2.34.2"]
# ///
"""Check the Rust backend against OpenEnergyTwin's virtual device on a disposable broker."""

import json
import math
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

import paho.mqtt.client as mqtt

ROOT = Path(__file__).resolve().parents[2]
PEER = "https://github.com/OpenEnergyTwin/simulation-vSMGW"
REVISION = "6145e61d7b715aa639df1be95d236315556159a4"
BROKER = (
    "emqx/emqx@sha256:a1e3d10fa1dcc8c94325b815a93b5b5974253a97cb36470b44e47d5d3ef9cd13"
)


def binary(*args):
    result = subprocess.check_output(
        ["cargo", "build", "--locked", "--message-format=json", *args],
        cwd=ROOT,
        text=True,
        env={**os.environ, "CARGO_TARGET_DIR": str(ROOT / "target")},
    )
    return next(
        msg["executable"]
        for line in result.splitlines()
        if (msg := json.loads(line)).get("executable")
    )


def broker_has(container, *command, needle):
    until = time.monotonic() + 45
    while time.monotonic() < until:
        result = subprocess.run(
            ["docker", "exec", container, "emqx", "ctl", *command],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=10,
            check=False,
        )
        if result.returncode == 0 and needle in result.stdout:
            return
        time.sleep(0.1)
    raise RuntimeError(f"EMQX did not report {needle!r} in time")


def check_virtual_device(container, port, driver, checkout):
    sys.path.insert(0, str(checkout / "src"))
    from vsmgw_sim import VSMGW  # pinned, unmodified upstream device

    device_id = "virtual-" + uuid.uuid4().hex
    # The measurement/HTTP interface stays unused; no external systems participate.
    peer = VSMGW(device_id, "127.0.0.1", 1, "127.0.0.1", port, None)
    try:
        peer.add_limit("synthetic-load", "active_power", "simulation")
        peer.update_time(10)
        broker_has(
            container,
            "subscriptions",
            "list",
            needle=f"clseedi/to-localdevice/{device_id}",
        )
        subprocess.run([driver, str(port), device_id], check=True, timeout=15)
        limit = peer.limits["simulation"][0]
        assert math.isclose(limit["value"], 0.0042) and limit["until"] == 70, limit
        peer.update_time(69)
        assert limit["active"] == 1, limit
        peer.update_time(70)
        assert limit["active"] == 0 and math.isinf(limit["value"]), limit
    finally:
        peer.mqtt_client.disconnect()
        peer.mqtt_client.loop_stop()
    print(
        "OpenEnergyTwin vSMGW applied 4200 W to its model and expired the limit at simulated second 70."
    )


def check_read_example(port, reader, inspector):
    # vSMGW implements LPC control only. Exercise the public read example
    # separately with this small Python peer, whose messages are assembled here.
    up, down = "read-test/from", "read-test/to"
    ship, suffix = "meter/42%", "/ship/meter%2F42%25"
    subscribed = threading.Event()
    received = []
    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, protocol=mqtt.MQTTv5)

    def on_connect(client, userdata, flags, reason, properties):
        client.subscribe([(down, 1), (down + suffix, 1)])

    def on_message(client, userdata, message):
        request = json.loads(message.payload)
        received.append(request)
        client.publish(up + "/ship/unprovisioned", b"{}", qos=1)
        for relation in ["unrelated-read", request["id"]]:
            reply = {
                "specversion": "1.0",
                "type": "de.keo-connectivity.clseedi.state",
                "source": "python-read-probe",
                "id": str(uuid.uuid4()),
                "relation": relation,
                "data": {
                    "protocol": "1.2.0",
                    "timestamp": 1800000000,
                    "limits": {
                        "power": {
                            "active": {
                                "consumption": {
                                    "active": True,
                                    "value": 4200,
                                    "duration": 50,
                                }
                            }
                        }
                    },
                },
            }
            reply["data"]["measurements"] = [
                {
                    "id": "grid-meter",
                    "source": "gcp",
                    "frequency": {"value": {"number": 4998, "scale": -2}},
                },
                {
                    "id": "load-meter",
                    "source": "controllable",
                    "power": {"total": {"value": {"number": -1255, "scale": -1}}},
                },
            ]
            if relation == "unrelated-read":
                # Echoed request IDs are valid fallback correlation only for ack.
                unsolicited = {**reply, "id": request["id"]}
                del unsolicited["relation"]
                client.publish(up + suffix, json.dumps(unsolicited), qos=1)
            client.publish(up + suffix, json.dumps(reply), qos=1)

    client.on_connect = on_connect
    client.on_subscribe = lambda *_: subscribed.set()
    client.on_message = on_message
    client.connect("127.0.0.1", port)
    client.loop_start()
    try:
        if not subscribed.wait(5):
            raise TimeoutError("Python read probe did not subscribe")
        result = subprocess.check_output(
            [
                reader,
                str(port),
                down,
                up,
                "--ship",
                ship,
                "--field",
                "limits",
                "--field",
                "measurements",
                "--timeout",
                "5",
            ],
            text=True,
            timeout=10,
        )
        reply = json.loads(result)
        assert (
            len(received) == 1
            and received[0]["type"] == "de.keo-connectivity.clseedi.read"
        )
        assert received[0]["data"]["parameters"] == ["limits", "measurements"]
        assert [meter["id"] for meter in reply["data"]["measurements"]] == [
            "grid-meter",
            "load-meter",
        ]
        assert reply["relation"] == received[0]["id"]
        assert (
            reply["data"]["limits"]["power"]["active"]["consumption"]["duration"] == 50
        )

        projection = json.loads(
            subprocess.check_output(
                [inspector, "meters"], input=result, text=True, timeout=5
            )
        )
        assert projection["timestamp"] == reply["data"]["timestamp"]
        assert projection["meterCount"] == 2
        assert [
            (row["meterId"], row["source"], row["property"], row["value"], row["unit"])
            for row in projection["rows"]
        ] == [
            ("grid-meter", "gcp", "frequency", "49.98", "Hz"),
            ("load-meter", "controllable", "power.total", "-125.5", "W"),
        ]
        assert all(
            row["timestamp"] == projection["timestamp"] for row in projection["rows"]
        )

        def reject(client, userdata, message):
            request = json.loads(message.payload)
            ack = {
                "type": "de.keo-connectivity.clseedi.ack",
                "id": request["id"],
                "source": "python-read-probe",
                "specversion": "1.0",
                "data": {"protocol": "1.2.0", "errorNumber": 4},
            }
            client.publish(up + suffix, json.dumps(ack), qos=1)

        client.on_message = reject
        rejected = subprocess.run(
            [reader, str(port), down, up, "--ship", ship, "--timeout", "5"],
            capture_output=True,
            text=True,
            timeout=8,
            check=False,
        )
        assert rejected.returncode != 0 and "CLS.EEDI error 4" in rejected.stderr, (
            rejected
        )
        check_trust_scope(client, port, reader, inspector, down, up, ship, suffix)
        # A subscribed peer that provides no response must respect the single deadline.
        client.on_message = lambda *_: None
        silent = subprocess.run(
            [reader, str(port), down, up, "--ship", ship, "--timeout", "1"],
            capture_output=True,
            text=True,
            timeout=4,
            check=False,
        )
        assert silent.returncode != 0 and "timed out" in silent.stderr, silent
    finally:
        client.disconnect()
        client.loop_stop()
    print(
        "Correlated SHIP state projected two exact meter readings. Silent-peer timeout passed."
    )


def check_trust_scope(client, port, reader, inspector, down, up, ship, suffix):
    ski, fingerprint, secret = "a1" * 20, "b2" * 32, "c3" * 16
    pairing = {"forId": "private-peer-id", "forPar": fingerprint, "secret": secret}
    credentials = [
        {"ski": ski},
        {"shipPairingService": pairing},
        {"shipPairingService": {**pairing, "ski": ski}},
    ]
    timestamp = 1800000087
    sent = []

    def reader_run(*args):
        result = subprocess.run(
            [reader, str(port), down, up, "--timeout", "3", *args],
            capture_output=True,
            text=True,
            timeout=6,
            check=False,
        )
        for sensitive in (ski, fingerprint, secret, "private-peer-id"):
            assert sensitive not in result.stdout and sensitive not in result.stderr
        return result

    def reply(request_id, data, topic):
        body = {
            "type": "de.keo-connectivity.clseedi.state",
            "source": "probe",
            "id": str(uuid.uuid4()),
            "specversion": "1.0",
            "data": {"protocol": "1.2.0", "timestamp": timestamp, **data},
        }
        if request_id is not None:
            body["relation"] = request_id
        sent.append(body)
        client.publish(topic, json.dumps(body), qos=1)

    def check_target(target, options):
        expected_topic = down + target
        topic = up + target
        other = up if target else up + suffix

        def noisy_read(client, userdata, publication):
            assert publication.topic == expected_topic
            request = json.loads(publication.payload)
            assert request["data"]["parameters"] == ["limits"]
            reply(None, {"trust": credentials}, topic)
            reply(request["id"], {"trust": credentials}, other)
            # Unknown future fields on the selected target must disappear on re-encode.
            reply(request["id"], {"privateDiagnostic": credentials}, topic)

        client.on_message = noisy_read
        accepted = reader_run(*options)
        assert accepted.returncode == 0, accepted.stderr
        assert json.loads(accepted.stdout)["data"] == {
            "protocol": "1.2.0",
            "timestamp": timestamp,
        }

        def overbroad_read(client, userdata, publication):
            request = json.loads(publication.payload)
            reply(request["id"], {"trust": credentials}, topic)

        client.on_message = overbroad_read
        rejected = reader_run(*options)
        assert rejected.returncode != 0 and not rejected.stdout
        assert "outside the requested field selection" in rejected.stderr

    check_target("", [])
    check_target(suffix, ["--ship", ship])

    def check_summary(trust, expected):
        def summarize(client, userdata, publication):
            assert publication.topic == down
            request = json.loads(publication.payload)
            assert request["data"]["parameters"] == ["trust"]
            reply(request["id"], trust, up)

        client.on_message = summarize
        result = reader_run("--trust-summary")
        assert result.returncode == 0, result.stderr
        projection = subprocess.check_output(
            [inspector, "trust-summary"],
            input=json.dumps(sent[-1]),
            text=True,
            timeout=5,
        )
        assert result.stdout == projection
        assert json.loads(result.stdout) == {"timestamp": timestamp, "trust": expected}

    for trust, expected in [
        ({}, None),
        ({"trust": []}, {"directSki": 0, "pairingPending": 0, "pairingEstablished": 0}),
        (
            {"trust": credentials},
            {"directSki": 1, "pairingPending": 1, "pairingEstablished": 1},
        ),
    ]:
        check_summary(trust, expected)

    def overbroad_summary(client, userdata, publication):
        request = json.loads(publication.payload)
        assert request["data"]["parameters"] == ["trust"]
        reply(request["id"], {"trust": credentials, "notify": {}}, up)

    client.on_message = overbroad_summary
    rejected = reader_run("--trust-summary")
    assert rejected.returncode != 0 and not rejected.stdout
    assert "outside the requested field selection" in rejected.stderr

    count = len(sent)
    for incompatible in [("--ship", ship), ("--field", "limits")]:
        result = reader_run("--trust-summary", *incompatible)
        assert result.returncode != 0 and not result.stdout
        assert "requires a local target" in result.stderr
    assert len(sent) == count
    print(
        "Selective local and SHIP reads isolated trust data; local trust summary matched CLI counts."
    )


def check_desired_settings(port, driver):
    """A Python device reads the Rust backend before and after confirming production."""
    subscribed = threading.Event()
    incoming = queue.Queue()
    peer = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, protocol=mqtt.MQTTv5)
    peer.on_connect = lambda client, *_: client.subscribe("desired/to", qos=1)
    peer.on_subscribe = lambda *_: subscribed.set()
    peer.on_message = lambda client, userdata, msg: incoming.put(
        json.loads(msg.payload)
    )

    def send(kind, ident, data):
        packet = {
            "id": ident,
            "type": f"de.keo-connectivity.clseedi.{kind}",
            "specversion": "1.0",
            "source": "device",
            "data": {"protocol": "1.2.0", **data},
        }
        peer.publish("desired/from", json.dumps(packet), qos=1).wait_for_publish(5)

    def expect(ident, kind):
        packet = incoming.get(timeout=5)
        assert packet["id"] == ident and packet["type"].endswith("." + kind), packet
        return packet

    peer.connect("127.0.0.1", port)
    peer.loop_start()
    try:
        assert subscribed.wait(5)
        process = subprocess.Popen(
            [driver, str(port)], stdout=subprocess.PIPE, text=True
        )
        try:
            first = expect("in", "control")
            assert first["data"]["limits"]["power"]["active"] == {
                "consumption": {"active": True, "value": 4200}
            }
            send("ack", "in", {"errorNumber": 0})
            send("read", "single", {"parameters": []})
            reply = expect("settings", "control")
            assert (
                reply["relation"] == "single"
                and reply["data"]["limits"] == first["data"]["limits"]
            )
            second = expect("out", "control")
            assert second["data"]["limits"]["power"]["active"] == {
                "production": {"active": True, "value": 1500}
            }
            send("ack", "out", {"errorNumber": 0})
            send("read", "mixed", {"parameters": []})
            try:
                unexpected = incoming.get(timeout=2)
            except queue.Empty:
                pass
            else:
                raise AssertionError(
                    f"unexpected mixed-settings response: {unexpected}"
                )
            send("read", "barrier", {"parameters": []})
            expect("still-connected", "read")
            output, _ = process.communicate(timeout=5)
            assert process.returncode == 0 and "returned DesiredState" in output, output
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    finally:
        peer.disconnect()
        peer.loop_stop()
    print(
        "Desired settings: single-direction read succeeded; mixed read preserved both limits, no wire reply in 2 s."
    )


def main():
    driver = binary("--manifest-path", str(ROOT / "interop/device-peer/Cargo.toml"))
    reader = binary("--features", "mqtt", "--example", "mqtt_read")
    inspector = binary("--bin", "cls-eedi")
    with tempfile.TemporaryDirectory() as name:
        checkout = Path(name)
        subprocess.run(["git", "init", "-q", checkout], check=True)
        subprocess.run(
            ["git", "-C", checkout, "fetch", "-q", "--depth=1", PEER, REVISION],
            check=True,
            timeout=60,
        )
        subprocess.run(
            ["git", "-C", checkout, "checkout", "--quiet", "--detach", REVISION],
            check=True,
        )
        container = subprocess.check_output(
            ["docker", "run", "-d", "-p", "127.0.0.1::1883", BROKER],
            text=True,
            timeout=120,
        ).strip()
        try:
            broker_has(container, "listeners", needle="tcp:default")
            address = subprocess.check_output(
                ["docker", "port", container, "1883/tcp"], text=True
            )
            port = int(address.strip().rsplit(":", 1)[1])
            desired = binary(
                "--manifest-path",
                str(ROOT / "interop/device-peer/Cargo.toml"),
                "--example",
                "desired_backend",
            )
            check_desired_settings(port, desired)
            check_virtual_device(container, port, driver, checkout)
            check_read_example(port, reader, inspector)
            subprocess.run(
                [sys.executable, str(ROOT / "scripts/check-peer.py"), str(port)],
                check=True,
            )
        finally:
            subprocess.run(
                ["docker", "rm", "-f", container], check=True, stdout=subprocess.DEVNULL
            )


if __name__ == "__main__":
    main()
