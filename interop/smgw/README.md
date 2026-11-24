# CLS.EEDI over an authenticated SMGW channel

This standalone test runs the CLS.EEDI backend and local-device roles against an MQTT 5 broker across the SMGW simulator's TLS proxy. The backend reads the device's state, verifies the related reply and then observes its offline last will after the gateway channel is cut while the application remains alive.

| Path | What the test exercises |
| --- | --- |
| Local MQTT client → loopback bridge → HAN TLS endpoint | Reads, state replies and presence use the same client connection. |
| CLS → simulated SMGW → simulated active external market participant | Separate ephemeral test authorities issue each leg's client and server chains. Both legs verify the chain and provisioned leaf pin. The gateway checks the HAN SNI before opening the WAN connection. |
| WAN endpoint → loopback broker ← backend MQTT client | EMQX processes subscriptions, QoS 1 messages and the retained last will. The application-level read ID must match the returned state. |

The WAN bridge delays its first MQTT forward by 75 milliseconds while the application polls in 10-millisecond intervals. The one-session proxy makes a restarted connection observable. A successful exchange proves the original CONNECT survives multiple caller deadlines.

A separate test attempts a non-provisioned SNI and verifies that the gateway refuses the route before making any WAN connection.

## Reproduce

A Rust toolchain and the OpenSSL development libraries are required. Cargo fetches the SMGW simulator from crates.io. Each test generates its own HAN and WAN certificate chains in a temporary directory. The SNI rejection test runs without Docker.

```sh
cargo test --locked --manifest-path interop/smgw/Cargo.toml
```

For the full test, use an existing disposable broker on IPv4 loopback.

```sh
CLS_EEDI_BROKER_PORT=1883 cargo test --locked \
  --manifest-path interop/smgw/Cargo.toml -- --include-ignored
```

The shared CI fixture runs this exchange with `scripts/check-mqtt.sh --with-smgw`. For a dedicated broker with Docker, run this block from the repository root.

```sh
(
  set -eu
  broker=$(docker run --detach --publish 127.0.0.1::1883 \
    emqx/emqx@sha256:a1e3d10fa1dcc8c94325b815a93b5b5974253a97cb36470b44e47d5d3ef9cd13)
  trap 'docker rm -f "$broker" >/dev/null' EXIT
  for i in $(seq 1 60); do
    if docker exec "$broker" emqx ctl listeners 2>/dev/null | grep -q 'tcp:default'; then break; fi
    sleep 1
  done
  port=$(docker port "$broker" 1883/tcp)
  CLS_EEDI_BROKER_PORT="${port##*:}" cargo test --locked \
    --manifest-path interop/smgw/Cargo.toml -- --include-ignored
)
```

The interoperability baseline uses `smgw-cls` 0.5.0 and EMQX 5.8.6. The gateway simulator’s test transport uses TLS 1.2, Brainpool P256r1 and ECDHE-ECDSA-AES128-GCM-SHA256. It admits a single authorized session on loopback. The MQTT bridges terminate inside the test process. The test demonstrates software transport integration and correlated application state. Gateway administration, field PKI, equipment operation and vendor firmware need independent evidence. [Simulator transport](https://docs.rs/smgw-cls/0.5.0/smgw_cls/transport/)
