# Certificate-bound broker authorization

The profile provisions three principals on EMQX 5.8.6. The broker verifies client certificates against the operator's private CA and maps their common names to both MQTT username and client ID. A client can replace its own session by reconnecting. It cannot claim another device's identity in CONNECT. This profile allows one active session per certificate CN. The fixture connects the backend to one route at a time. Concurrent backend sessions need distinct provisioned CNs and matching ACL entries. Keep the private CA's issuance policy under operator control. [EMQX certificate mapping](https://docs.emqx.com/en/emqx/latest/guides/access-control/authn/x509.html)

| Certificate CN | Can subscribe | Can publish |
| --- | --- | --- |
| device-a | Exact filter `grid/down/A/#` | `grid/up/A` and descendants |
| device-b | Exact filter `grid/down/B/#` | `grid/up/B` and descendants |
| backend | Exact filters `grid/up/A/#` and `grid/up/B/#` | Both downstream device trees |

The terminal deny rule covers additional devices, broad filters and unauthorized writes. EMQX returns MQTT 5 authorization reason codes on this profile. The rules cover MQTT operations and topics. Applications decide who may issue sensitive operations within one authorized command topic.

## Run the checks

```sh
scripts/check-mqtt.sh
```

The script requires Cargo, Docker and OpenSSL. It creates ephemeral identities, mounts the ACL and server credentials into a pinned broker, and exposes only its TLS MQTT listener on a random loopback port. A separate anonymous broker runs the protocol-only tests. The ACL test supplies device A's valid certificate while claiming device B's client ID and the backend's username. It asserts that EMQX rejects both a B-specific and a fleet-wide telemetry subscription with SUBACK `NotAuthorized` and rejects a valid CLS.EEDI control to B with PUBACK `NotAuthorized`. In the same run, B receives its authorized backend read and returns a correlated meter observation. A receives its own subsequent message.

## Configure an equivalent listener

Apply the settings in [emqx.conf](emqx.conf) to the broker's existing configuration and install [acl.conf](acl.conf) at the configured file-authorizer path. Point the listener's CA, certificate and key settings at provisioned files. The test paths under `/test-listener` are disposable mount points. Update the device CNs and topic trees together. The `eq` entries permit exactly the subscription filters used by the Rust session.

Keep `verify_peer`, `fail_if_no_peer_cert` and the certificate identity mappings enabled together. Disable other MQTT listeners on this broker or enforce equivalent authenticated identity mapping there. A plaintext listener accepting caller-supplied usernames would undermine the ACL. [EMQX file authorization](https://docs.emqx.com/en/emqx/latest/guides/access-control/authz/file.html)
