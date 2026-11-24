+++
title = "MQTT and routing"
description = "Provision per-device topics, enforce broker permissions and read state over MQTT 5."
weight = 2
+++

## Configure MQTT

Provision a pair of directional base topics for each local device. Bind identities to those topics in the broker ACLs and supply the trusted broker certificate configuration to the client. Subsequent devices use an escaped SHIP ID in the final topic level. Assign backend and local-device roles consistently on both ends.

Retained online presence includes the connection timestamp. The offline will omits it because a broker cannot know the failure time when a device connects. A broker delivery receipt confirms transfer. Equipment acceptance is reported by a correlated CLS.EEDI acknowledgement.

## Enforce device permissions at the broker

The EMQX test profile maps a verified client certificate's common name to the MQTT principal. Device A can read its own inbound route and publish its own upstream traffic. A request using A's certificate and B's client ID still has A's permissions. The tests check the broker's denial codes for B's telemetry, a fleet wildcard and a control directed to B, then verify an authorized exchange still completes. Run `scripts/check-mqtt.sh` to reproduce the checks. The configuration files and certificate issuance assumptions are in `interop/mqtt-acl/`.

Drive the synchronous MQTT session by polling it regularly. A poll deadline can elapse while the broker is still establishing the connection. Continue polling the same session after an empty result. The handshake retains its progress across calls. This matters on a gateway channel that can accept only one TCP connection.

## Query a local device

With a subscriber on the provisioned device route, run the read-only example from a checkout.

```sh
cargo run --locked --features mqtt --example mqtt_read -- \
  1883 clseedi/to-localdevice/demo clseedi/from-localdevice/demo --field limits
```

The example connects to loopback and prints the correlated JSON state to standard output. Repeat `--field` to request more properties. Use `--ship ID` to address a provisioned subsequent device, and `--timeout SECONDS` to set the deadline.

The Rust backend also interoperates with OpenEnergyTwin's Python virtual device for a timed LPC control. The harness verifies the acknowledgement and inspects the device's own active limit immediately before and at expiry. A separate Python probe tests state reads. Run both with `uv run --locked --python 3.13 --script interop/device-peer/check.py`. [Virtual-device implementation](https://github.com/OpenEnergyTwin/simulation-vSMGW/blob/6145e61d7b715aa639df1be95d236315556159a4/src/vsmgw_sim.py)
