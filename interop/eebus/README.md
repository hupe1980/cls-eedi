# EEBUS equipment interoperability

Run the suite from the repository root. It uses EEBUS 0.14.0 software peers, in-memory SPINE sessions and ephemeral SHIP identities.

```sh
cargo test --locked --manifest-path interop/eebus/Cargo.toml
```

| Test | What crosses the interface |
| --- | --- |
| `limitation` | A heat pump receives a positive consumption limit and an inverter receives a negative production limit. Tests cover heartbeat refusal, expiry and replacement with an open-ended limit. |
| `metering` | A grid meter and two heat pumps return descriptions and measured values. The backend preserves sign, scale and source, and withdraws the group total when a member reading is missing. |
| `commissioning` | The device replaces a trust list containing a certificate and an SKI. A malformed certificate leaves both original records intact. |
| `pairing` | A signed DNS-SD announcement and authenticated loopback SHIP connection establish the peer SKI. A selective CLS.EEDI read returns the confirmed identity. |

Run a single suite with `--test limitation`, for example. Physical load response, site multicast routing and vendor firmware require validation on the installation. [Equipment guide](https://hupe1980.github.io/cls-eedi/guide/equipment/) · [EEBUS 0.14.0](https://github.com/hupe1980/eebus/releases/tag/v0.14.0)
