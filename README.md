# cls-eedi

Rust components for KEO CLS.EEDI 1.2.0. A grid backend sends a power limit or a state read to a local device. The local device applies the request to energy equipment and reports the outcome. The library implements both roles and includes a CLI for inspecting captured messages.

## Install

Add the library with `cargo add cls-eedi`. Install the CLI with `cargo install --locked cls-eedi` or `brew install hupe1980/tap/cls-eedi`.

## Run a local exchange

From the repository root:

```sh
cargo run --locked --example roundtrip
cargo run --quiet --locked -- check < examples/read.json
cargo run --quiet --locked -- meters < examples/meters.json
```

The first command applies a 4,200 W consumption ceiling for one minute. A duplicate delivery reuses the acknowledgement. Ten seconds later, the backend reads 50 seconds remaining. The other commands validate a selective read and display the sample’s signed meter readings. [Getting started](https://hupe1980.github.io/cls-eedi/guide/getting-started/)

## Choose an integration

| Need | Use |
| --- | --- |
| Decode messages or implement a backend or local device | The default build provides typed envelopes, per-target state machines and exact fleet power aggregation. |
| Exchange messages through a broker | Enable `mqtt` for MQTT 5. Provision device identities and directional topic permissions. |
| Connect EEBUS equipment | Enable `eebus` for power limits and meter snapshots. The host supplies the authenticated SHIP connection and checks measurement freshness. |
| Confirm a SHIP pairing | Enable `eebus-pairing` and pass the open equipment connection to `confirm_pairing` after the peer advertises its SHIP ID. |
| Resume after a restart | Persist checkpoints before equipment dispatch and acknowledgement. Reconcile an uncertain operation against equipment before retrying. |

The MQTT transport and EEBUS adapter can be enabled independently. The pairing helper adds the EEBUS runtime with the AWS-LC cryptography provider. Equipment acceptance, MQTT delivery and observed state are distinct events. [Routing](https://hupe1980.github.io/cls-eedi/guide/mqtt/) · [Equipment](https://hupe1980.github.io/cls-eedi/guide/equipment/) · [Metering](https://hupe1980.github.io/cls-eedi/guide/metering/) · [Recovery](https://hupe1980.github.io/cls-eedi/guide/recovery/)

## Development

Run `just check` for lint and tests, `just broker` for Docker-backed gateway tests, or `just serve` to preview the guides and Rust API reference. [Verification guide](https://hupe1980.github.io/cls-eedi/guide/verification/)

Licensed under MIT or Apache-2.0 at your option.
