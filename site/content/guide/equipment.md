+++
title = "Equipment and commissioning"
description = "Connect EEBUS appliances, deliver consumption and production limits, and inspect SHIP trust state."
weight = 4
+++

## Connect the local device to EEBUS equipment

SHIP provides the authenticated connection to the appliance. SPINE carries the EEBUS commands and measurements over that connection. A Subject Key Identifier (SKI) identifies a peer when provisioning trust.

Enable the `eebus` Cargo feature on `cls-eedi`. Enable `mqtt` separately when this process also serves CLS.EEDI over a broker. The adapter operates on a provisioned SPINE session. The host application owns authenticated SHIP transport, feature discovery, binding, heartbeat delivery and persistence.

1. Establish the client-to-server binding and drain existing events. Issue full reads of the target LoadControl feature’s description and current-limit lists. Correlate each reply’s endpoints and initiating counter before feeding it to the guard engine. Leave both complete reply events queued for `LimitAdapter::new`. The adapter requires a unique, explicitly writable active-power obligation in watts for the requested direction.
2. When the local device accepts an incoming request for dispatch, persist its checkpoint. Dispatch with a fresh clock sample so discovery time is deducted from a timed limit.
3. Transmit the generated SPINE datagram on that session. Feed responses through the adapter. Equipment refusal produces a negative CLS.EEDI result. After definitive completion, persist the checkpoint before publishing any acknowledgement.
4. If the connection fails before the equipment result, discard the uncertain session, keep the original operation pending and reconcile with equipment. A missing response cannot establish whether the device applied the write.

Run the peer exercise from a checkout.

```sh
cargo test --locked --manifest-path interop/eebus/Cargo.toml --test limitation
```

`LimitAdapter::new` takes ownership of the SPINE engine with those two reply events queued and borrows the route's `LocalDevice`. Supply the fully qualified client and server feature addresses. Call `dispatch(request_id, clock)`, then `poll_transmit()`. Pass the matching session's response to `handle_datagram`; a returned `Completion` contains the equipment result and any CLS.EEDI acknowledgement. Use `into_engine()` after completion to resume ordinary SPINE traffic and release the local-device borrow for checkpointing. Poll the adapter's `deadline()` in the host event loop. Dropping a pending adapter discards the SPINE retry queue and releases the local engine for reconciliation.

The harness exchanges real SPINE messages with in-memory equipment actors. Deployment evidence for physical power, authenticated hardware and site timing still comes from the actual installation. [EEBUS peer revision](https://github.com/hupe1980/eebus/tree/32326543f7d01368f7e819357215855039344f96)

Send the write promptly after dispatch. The adapter deducts time already elapsed on the local clock. The equipment starts the transmitted relative duration on receipt, so subsequent transport delay can shift its physical expiry.

KEO represents a production ceiling as a nonnegative watt magnitude. EEBUS LPP 1.0.0 §2.8.1 uses the passive sign convention, so a 1,500 W production ceiling is −1,500 W in the SPINE write. The released EEBUS 0.14.0 inverter actor accepts this signed write when its heartbeat is fresh and rejects it when the heartbeat is absent. The integration suite checks both results and the original expiry deadline. [EEBUS specifications](https://www.eebus.org/specifications/)

## Check commissioning status

A valid local-device trust readback can contain the original SHIP pairing secret. Use the summary mode when checking installation progress. It requests only the local trust property and emits a timestamp and three counts. For a saved capture, use `cls-eedi trust-summary`.

```sh
cargo run --quiet --locked --features mqtt --example mqtt_read -- \
  1883 clseedi/to-localdevice/demo clseedi/from-localdevice/demo --trust-summary
```

| Field | What one entry means |
| --- | --- |
| `directSki` | The device reports a directly trusted SKI, including an SKI derived from an installed certificate. |
| `pairingPending` | The device retains the original pairing information and has yet to report the peer SKI. |
| `pairingEstablished` | The pairing-service entry includes the peer SKI after establishment. |

A report with `trust: null` means the state omitted trust. Three zero counts mean the device returned an empty list. Counts describe entries and can include multiple entries for a peer. Use the state timestamp to identify the observation time. The summary mode accepts a local-device route and cannot be combined with `--ship` or `--field`. [KEO 1.2.0 trust readback](https://github.com/KEO-cls-eedi/specification/blob/1.2.0/doc/clseedi.md#trust-statetrust)

### Confirm a replacement in an adapter

Stage the full trust list before changing equipment. For certificate entries, verify and install each certificate in the equipment trust store, then supply one `InstalledCertificate` with the submitted DER hex and observed SKI per distinct certificate to `Outcome::Trusted`. For a list containing only direct SKIs and pairing-service entries, use `Outcome::Accepted` after equipment acceptance. A rejected replacement preserves the previous inventory.

Capture `LocalDevice::trust_revision()` when starting pairing after equipment acceptance. Enable `eebus-pairing` to use the EEBUS runtime with AWS-LC. After the SHIP handshake and access-methods exchange, call `eebus::confirm_pairing(&mut device, revision, &connection)`. It matches the peer’s advertised SHIP ID and TLS leaf fingerprint to the accepted entry and records the SKI from that same connection. A signed discovery announcement alone is insufficient. A peer that supplies no SHIP ID leaves the entry pending.

A first confirmation returns `true`. Checkpoint the updated device before publishing trust readback. An identical callback returns `false`. A pending replacement requires reconciliation, and a superseded revision cannot change the active list. Applications with their own SHIP stack can pass their verified peer identity to `LocalDevice::pairing_established`.

Reproduce the commissioning exchange on loopback. The test provisions ephemeral identities, discovers the signed pairing request, and reads back the established SKI through CLS.EEDI.

```sh
cargo test --locked --manifest-path interop/eebus/Cargo.toml --test pairing
```

Use `initialize_trust` once to seed a new local inventory from verified equipment state.
