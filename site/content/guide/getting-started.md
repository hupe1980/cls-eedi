+++
title = "Getting started"
description = "Validate a message and follow a timed power limit from the backend to simulated equipment."
weight = 1
+++

CLS.EEDI carries JSON messages between an energy-management backend and a local device at the grid connection point. A live deployment uses MQTT and synchronized clocks. The core library can exercise the same exchange in memory. [KEO protocol overview](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md)

A grid backend sends a control to a local device. The local device asks the equipment to apply it and sends an acknowledgement when it knows the outcome. A subsequent state read reports what the device currently observes.

Add the library to a Rust project with `cargo add cls-eedi`. To run the examples below, clone the repository:

```sh
git clone https://github.com/hupe1980/cls-eedi.git
cd cls-eedi
```

## Validate a message

From a checkout, check the supplied selective-read example. The command reads one JSON envelope from standard input. Its summary reports the message family and protocol version. Invalid input exits with status 1. Incorrect command-line usage exits with status 2.

```sh
cargo run --quiet --bin cls-eedi -- check < examples/read.json
```

Expected output is `valid read (CLS.EEDI 1.2.0)`. This validates the envelope without connecting to equipment. Device capabilities, current state and permissions determine whether a live operation can succeed.

## Follow a power limit

```sh
cargo run --example roundtrip
```

The example sends a consumption ceiling to a simulated local device, records equipment acceptance and reads back the remaining duration. In a real integration, authorize the target before passing a control to the device engine. Supply the equipment outcome only when the adapter knows it. Reconcile uncertain writes on restart before dispatching them again.

The simulated device accepts 4,200 W for 60 seconds. At second 10, a duplicate delivery reuses the acknowledgement and readback reports 50 seconds remaining.
