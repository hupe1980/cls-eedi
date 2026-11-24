# MQTT peer interoperability

Run `just peers` from the repository root with Cargo, Git, Docker and uv. The harness fetches pinned OpenEnergyTwin peers, provisions one synthetic load and starts an EMQX broker on loopback.

| Peer | Verification |
| --- | --- |
| Python virtual device | The Rust backend sends a 4,200 W limit for 60 seconds. The peer acknowledges it, applies 0.0042 MW and clears it at expiry. [Virtual device](https://github.com/OpenEnergyTwin/simulation-vSMGW/blob/6145e61d7b715aa639df1be95d236315556159a4/src/vsmgw_sim.py) |
| uMSB backend | A replacement arrives during a pending write. The device rejects the replacement, completes the original write and reports its value on a separate state read. |
| Python MQTT probes | Selective reads exercise correlation, escaped SHIP IDs, meter projection, timeouts and trust-summary redaction. Desired-settings reads cover both single and mixed power directions. |

To run only the uMSB checks against an existing disposable broker, use `python3 scripts/check-peer.py PORT`. For an interactive state read, follow the [MQTT guide](https://hupe1980.github.io/cls-eedi/guide/mqtt/).
