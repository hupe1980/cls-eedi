+++
title = "Interoperability and verification"
description = "Reproduce software peer exchanges, validate reference messages and measure codec performance."
weight = 6
+++

## Verify an integration

```sh
cargo test --locked --all-features
cargo doc --locked --no-deps --all-features --open
```

Record the device firmware, route assignments, MQTT settings, command and reply IDs, and elapsed time at readback when testing a peer. Redact meter identities and pairing credentials before sharing traces. Keep cross-vendor results attributable to the actual equipment tested. The stable wire profile is pinned to KEO 1.2.0. Experimental tariffs and schedules belong to a separate upstream revision. [KEO release](https://github.com/KEO-cls-eedi/specification/tree/1.2.0)

## Reproduce the peer check

Run `just peers` with Cargo, Git, Docker and uv. It starts a disposable loopback broker and fetches pinned versions of OpenEnergyTwin’s backend and virtual device.

The uMSB backend issues a 4,200 W consumption ceiling. While that equipment write is pending, it sends a 3,100 W replacement. The local device rejects the replacement with error 3, and uMSB reports `communicationFailure`. Once the first write completes, a state read returns 4,200 W. The first operation receives a successful wire acknowledgement, but the superseded uMSB HTTP transaction has no terminal callback through the subsequent reset. Read device state before retrying an uncertain write. [uMSB scheduler](https://github.com/OpenEnergyTwin/simulation-umsb/blob/6de729c3e1b7a1407892d3f0de9eac4ea449730a/src/scheduler.rs)

The same suite uses a Python device fixture to request desired controls from the Rust backend. A single confirmed direction returns a correlated reply. With both directions confirmed, Rust reports `EngineError::DesiredState` and retains both limits. The device observes no wire reply within two seconds. The pinned uMSB supports consumption commands only, so it cannot establish how another backend would handle the mixed case.

## Exercise the equipment and gateway adapters

The EEBUS harness checks limit acceptance, refusal and expiry on software peers. The smart-meter gateway harness carries MQTT across two authenticated TLS connections and verifies the broker’s offline notification after a channel cut.

```sh
cargo test --locked --manifest-path interop/eebus/Cargo.toml
scripts/check-mqtt.sh --with-smgw
```

## Measure the codec

Run `cargo bench --bench codec` on the target machine. The harness decodes a state envelope with 1, 32 and 128 meters, warms the parser, then reports the median and 99th percentile of 2,000 calls per size. It excludes broker latency and equipment response time. The default decoder admits up to 256 KiB, 32 levels of nesting and 256 entries per container. Applications can lower the limits using `Message::decode_with`.

For broker verification, run `scripts/check-mqtt.sh` with Docker and OpenSSL available. It creates an ephemeral CA and client credentials, checks trusted mutual TLS as well as rejection of an unrelated CA and a missing client identity, then removes the fixture. The integration uses clean MQTT 5 sessions with QoS 1 and Retain As Published so a device can identify a retained command even on a live subscription. [MQTT 5 standard](https://docs.oasis-open.org/mqtt/mqtt/v5.0/os/mqtt-v5.0-os.html)

## Check the reference examples

Validate the upstream examples against both the JSON Schema and the Rust codec. The script requires Git and uv.

```sh
git clone --depth 1 --branch 1.2.0 https://github.com/KEO-cls-eedi/specification.git /tmp/keo-cls-eedi-1.2.0
uv run --script scripts/check-reference.py /tmp/keo-cls-eedi-1.2.0
```
