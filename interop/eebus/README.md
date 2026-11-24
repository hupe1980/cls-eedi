# EEBUS application interoperability

## Consumption and production limits

The limit tests run the public `cls_eedi::eebus::LimitAdapter` over EEBUS 0.14.0 SPINE engines. The guard reads fresh description and current-limit lists from the equipment over SPINE JSON. The selected limit ID must be unique, measured in watts and explicitly changeable. The adapter chooses its advertised obligation, queues one write and completes the CLS.EEDI request when the equipment returns a result for that write. [Pinned EEBUS implementation](https://github.com/hupe1980/eebus/tree/32326543f7d01368f7e819357215855039344f96)

| Equipment | CLS.EEDI request | SPINE write | Peer evidence |
| --- | --- | --- | --- |
| Stock 0.14.0 heat pump | 5,300 W consumption | +5,300 W | Acceptance with a fresh heartbeat and refusal without one |
| Stock 0.14.0 PV inverter | 1,500 W production | −1,500 W | Acceptance with a fresh heartbeat and refusal without one |

LPP Technical Specification 1.0.0 §2.8.1 and its implementation guide §3.6 specify nonpositive active production limits. The stock actor accepts the signed write and tracks the production ceiling as a positive magnitude internally. These checks use software equipment. [EEBUS specifications](https://www.eebus.org/specifications/) · [EEBUS 0.14.0 release](https://github.com/hupe1980/eebus/releases/tag/v0.14.0)

Both successful tests receive a 30-second limit and dispatch after seven seconds. The outgoing write contains the remaining 23 seconds. The equipment and CLS.EEDI readback both show the limit at second 29 and release it at second 30. A refusal gives the backend error 3. Another test submits production to a consumption-only peer and verifies the adapter queues no write.

An open-ended replacement test first installs a 4,200 W limit for 20 seconds. Ten seconds later it requests 3,700 W indefinitely. It inspects the serialized write for a deletion scoped to the advertised limit ID and its end time, followed by a partial update in the same command. After the original deadline, both the equipment and backend still report the replacement.

## Metering

The metering tests read one MGCP grid-connection-point actor and two independently addressed MPC heat pumps. Each read crosses the SPINE JSON boundary. The collector verifies the responding feature, local destination, message class and initiating counter before handing the four complete replies to `cls_eedi::eebus::meter_snapshot`. The four reads cover measurement descriptions, connection descriptions, parameter descriptions and values. An additional test shows that replies from two devices with the same request counter cannot be interchanged. All three actors assign measurement ID 1, respectively, to produced energy, active power or phase-A current. [Pinned SPINE monitoring implementation](https://github.com/hupe1980/eebus/tree/32326543f7d01368f7e819357215855039344f96/src/usecases/monitoring)

| Measurement point | Verified observations |
| --- | --- |
| Grid connection point | −1,275.25 W total power and −85.125 Wh produced energy under the EEBUS load convention |
| East heat pump | 125.125 W active power and 3.75 A on phase A |
| West heat pump | 74.875 W active power with a separate peer-local descriptor catalogue |
| Provisioned controllable group | 200.000 W combined active power in backend readback |

The fixture provisions exactly two non-overlapping heat-pump branches behind the same grid meter and samples each in one observation window. It passes only their signed total active powers to `cls_eedi::metering::controllable_total_power`. Grid power, energy counters and phase currents remain separate. A missing pump or an invalid power reading withdraws the whole group total from backend readback. When both pumps report zero, readback restores a measured zero. Invalid grid power is omitted while its valid energy counter remains. Topology, group membership and sampling-window checks belong to the application. [KEO measurement and removal requirements](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#measurements-statemeasurements) · [EEBUS MGCP and MPC specifications](https://www.eebus.org/specifications/)

## Certificate and SKI commissioning

The commissioning test generates three ephemeral SHIP identities in memory. A CLS.EEDI control installs one DER certificate and a second peer’s SKI. The adapter uses the pinned SHIP implementation to extract the certificate’s SKI, records its SHA-256 fingerprint and installs both peers at commissioning trust level 32. A selective trust read compares the backend’s reported SKIs with the independent store’s inventory. [KEO trust replacement and readback](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#trust) · [SHIP certificate implementation](https://github.com/hupe1980/eebus/blob/32326543f7d01368f7e819357215855039344f96/src/cert.rs) · [SHIP trust store](https://github.com/hupe1980/eebus/blob/32326543f7d01368f7e819357215855039344f96/src/runtime/node.rs)

A second replacement contains a fresh SKI and a hex-encoded empty DER sequence. The peer’s X.509 parser rejects the certificate, the backend receives correlated error 3, and both original records remain. Subsequent successful replacements remove the certificate and then clear the entire store. Only the public certificate enters the control. Certificate and SKI entries return solely as SKIs in state. This certificate-replacement test covers the local SKI trust store. The pairing test below exercises a separate commissioning path.

## SHIP Pairing Service

Run `cargo test --locked --manifest-path interop/eebus/Cargo.toml --test pairing` to follow one CLS.EEDI trust replacement through authenticated SHIP readback. The local control unit configures the target ID, certificate fingerprint and secret from its dispatched control. Its signed announcement crosses IPv4 loopback between two DNS-SD daemons using a private multicast port. The advertised TCP service port is reserved without a listener. The subsequent SHIP connection uses a separate loopback endpoint.

The energy manager rejects a wrong secret and a substituted certificate fingerprint, accepts the correct HMAC once and rejects a replay. On the mutual TLS, WebSocket and SHIP connection, the control unit checks its target certificate pin before admitting the peer and records the observed SKI with that fingerprint. After the access-methods exchange supplies the SHIP ID, the public helper confirms the pairing. A selective backend read returns the established SKI alongside the original pairing fields. An identical callback is idempotent. A callback arriving before the SHIP ID, after session closure, with a superseded revision, or for a different configured ID or fingerprint cannot complete the pairing.

Both identities are generated for the run. The test advances the requester's fifteen-minute settlement timer using simulated elapsed time and verifies DNS-SD unregistration. The evidence covers software on loopback. Site multicast routing, operator PKI and vendor firmware require deployment checks. [KEO trust readback](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#trust-statetrust) · [EEBUS SHIP Pairing Service specification](https://www.eebus.org/specifications/)

## Running the tests

Run from the repository root. Cargo fetches EEBUS 0.14.0 from crates.io. Its packaged source identifies commit `32326543f7d01368f7e819357215855039344f96`.

```sh
cargo test --locked --manifest-path interop/eebus/Cargo.toml
```

The limit and metering tests exchange SPINE datagrams in memory. The pairing test runs DNS-SD and SHIP over loopback with ephemeral TLS credentials. Site multicast behavior, field credentials, equipment timing and vendor firmware require deployment checks.
