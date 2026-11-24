+++
title = "State and metering"
description = "Interpret selective replies, inspect signed meter readings and calculate a complete controllable fleet total."
weight = 3
+++

## Interpret state

Keep the original read's target and field selection until its response arrives. A full reply replaces the target snapshot. A selective reply clears only requested fields that the peer omitted. An unsolicited measurement update replaces the observations for each reported source. Preserve integer scaling and source identity when adapting measurements to a dashboard.

The cache compares timestamps within each property and meter. If meter A reports at second 20, a delayed reading for meter B from second 18 can still update B. A full meter inventory from second 21 prevents a delayed report from resurrecting a removed meter. The cache timestamp is the greatest accepted timestamp across the target. Use `Backend::meter_observed_at(id)` to label a meter reading with its device time, and `Backend::property_observed_at(field)` for a property update or authoritative clear. For a correlated read racing an observation in its scope within the same second, retry after a stale result.

## Inspect a meter read

From a checkout, try the bundled capture with `cargo run --quiet --locked -- meters < examples/meters.json`. For a running local device, request the measurements field and pass the correlated state directly to the CLI.

```sh
cargo build --locked --bin cls-eedi
cargo run --quiet --locked --features mqtt --example mqtt_read -- \
  1883 clseedi/to-localdevice/demo clseedi/from-localdevice/demo --field measurements \
  | target/debug/cls-eedi meters
```

The JSON object contains the state timestamp in Unix seconds, the number of recognized meters and the readings. Each row identifies the meter, source, property path and wire unit. A signed decimal string preserves all digits. Extreme scales use an exact exponent form. Keep the value as a string when passing it through systems with limited numeric precision.

An omitted measurements field yields `meterCount: null`. An explicit empty inventory yields `meterCount: 0`. A recognized meter may have no individual readings, so use the count to distinguish an empty inventory from a source with unavailable readings. For unsolicited updates, the count describes the current message. The backend engine maintains the accumulated inventory. [KEO measurement semantics](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#measurements-statemeasurements)

### Publish a controllable fleet reading

MGCP (Monitoring of Grid Connection Point) measures the site boundary. MPC (Monitoring of Power Consumption) measures an appliance or branch inside it. Combining the boundary reading with the branch readings would count the same load twice.

Use `eebus::meter_snapshot` with four complete, correlated replies from one authenticated equipment session. Read measurement descriptions, electrical connection descriptions, connection parameter descriptions and measurement values. Bind replies to the expected peer, feature and request counter. Refresh the full snapshot after a catalogue change or partial notification. The adapter accepts one electrical connection and at most 1,024 rows in each catalogue or value list. Under the consume-positive convention, exported watts and produced watt-hours carry a negative sign. [EEBUS monitoring profiles](https://www.eebus.org/specifications/)

Keep the grid connection point as its own meter. For the controllable source, pass the configured member IDs and each member’s current total active watts to `metering::controllable_total_power`. Provision up to 256 disjoint electrical branches and select a common sampling window appropriate to your equipment. A stale or missing reading makes the group total unavailable until every member reports again. A fully observed group can measure zero. Reused SPINE measurement IDs are local to each peer. [KEO measurement requirements](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#measurements-statemeasurements)

```sh
cargo run --quiet --locked --example fleet_power | cargo run --quiet --locked -- meters
cargo run --quiet --locked --example fleet_power -- --missing | cargo run --quiet --locked -- meters
cargo test --locked --manifest-path interop/eebus/Cargo.toml --test metering
```

The example reports 1,425.25 W with both loads and an empty reading list when one is missing. The peer test independently reads −1,275.25 W and −85.125 Wh at the grid connection point. Two MPC heat pumps contribute 125.125 W and 74.875 W, producing a 200.000 W controllable total in backend readback. Their phase currents and the grid energy counter stay separate.
