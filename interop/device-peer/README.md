# Independent virtual device

Run the Rust backend against OpenEnergyTwin's Python VSMGW local-device implementation. The check fetches its pinned revision into a temporary directory, starts an ephemeral EMQX broker on an OS-assigned loopback port and provisions one synthetic load. It requires Cargo, Git, uv, Python 3.13 and Docker. [Upstream peer source](https://github.com/OpenEnergyTwin/simulation-vSMGW/blob/6145e61d7b715aa639df1be95d236315556159a4/src/vsmgw_sim.py)

```sh
uv run --locked --python 3.13 --script interop/device-peer/check.py
```

The Rust backend requests 4,200 W for 60 seconds at simulated second 10. The upstream device publishes a successful acknowledgement with the command ID in its envelope ID. The check verifies that the backend correlates that reply and that the upstream simulator stores 0.0042 MW, marks the limit active at second 69 and expires it at second 70. The device uses Paho's MQTT 3.1.1 client. Rust uses MQTT 5 on its own connection to the same broker.

Upstream implements an LPC control callback. A second check uses a separately authored Python read responder to exercise the Rust read example. It verifies a selective request returning two meters, the installed CLI projection format (49.98 Hz and −125.5 W), an error acknowledgement, a SHIP ID containing slash and percent, correlation in the presence of an unrelated reply, and bounded termination with a silent subscriber. This second result is local test evidence for read behaviour. Commercial equipment and other control families require separate peer runs.

For a manual read, substitute the loopback broker port and provisioned base topics. Standard output contains the matching JSON state envelope. Use `--field` more than once to select additional fields, `--ship ID` for an authorized subsequent device, and `--timeout SECONDS` to change the 10-second deadline.

```sh
cargo run --locked --features mqtt --example mqtt_read -- \
  1883 clseedi/to-localdevice/demo clseedi/from-localdevice/demo --field limits
```

For local commissioning status, add `--trust-summary` in place of field and SHIP selections. The request selects only `trust`. Output is the same aggregate JSON as `cls-eedi trust-summary`: the state timestamp and counts of direct SKI entries, pending pairings and established pairings. Missing trust produces `null`; an empty list produces zero counts. No raw state envelope is written in this mode.

```sh
cargo run --locked --features mqtt --example mqtt_read -- \
  1883 clseedi/to-localdevice/demo clseedi/from-localdevice/demo --trust-summary
```

The live probe sends trust entries in unsolicited state, on a different route, and in an overbroad correlated reply. It checks that unsolicited and wrong-route data are ignored, that a correlated reply containing unrequested trust returns a failure with empty stdout, and that pairing secrets, fingerprints, SKIs and peer IDs stay out of stdout and stderr. It repeats the checks for local and SHIP field reads.

## Overlap and desired-settings reads

Run `just peers` from the repository root for both OpenEnergyTwin peers and the Python MQTT probes. To run only the uMSB checks against a disposable loopback broker, use `python3 scripts/check-peer.py PORT`. An optional pinned checkout before the port lets you reuse a local build.

The uMSB overlap test waits for the first simulated equipment write before submitting the replacement. It checks both ACK IDs and codes, the negative HTTP callback for the replacement, the number of equipment writes, and a separate MQTT state read. A subsequent reset confirms that the same route is still usable. A local request for accumulated control gets no reply within the two-second observation window.

A separate Python device confirms consumption and production controls from the Rust backend. It checks a successful single-direction read first, then observes the mixed-direction timeout and subsequent traffic on the same connection. The Rust fixture asserts that both settings remain intact. This tests the Rust backend’s policy. The pinned uMSB has no production-limit input.
