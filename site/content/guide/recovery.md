+++
title = "Recovery and deployment"
description = "Persist commands and equipment outcomes, reconcile interrupted writes and choose per-target policies."
weight = 5
+++

## Recover an uncertain write

When `DeviceEvent::Apply` returns a pending ID, checkpoint before dispatch. For a custom adapter, call `control_for_dispatch` once immediately before the equipment write. The EEBUS adapter performs this step inside `dispatch`. It returns the remaining lifetime, reports expiry, or identifies an operation awaiting reconciliation. Checkpoint again after recording the equipment result and before publishing the acknowledgement. Store checkpoint bytes atomically in application-controlled storage with access restricted to the service identity. Pairing credentials can be present in a checkpoint.

On restart, create a fresh engine with the configured route, source, target kind, capacity and replay window. Call its `restore` method with synchronized UTC and the new process's monotonic clock. Recovery validates that the saved policy matches the provisioned engine. Inspect each pending operation against the equipment. Mark a known result accepted or rejected, and leave an uncertain result pending. Configure the replay window to cover the deployment's delivery and retry horizon. Exhausting capacity stops admission, which keeps earlier outcomes available for reconciliation.

## Recover a backend with an outstanding control

Use one durable writer for each route. Register the outgoing command, then commit its encoded message and backend checkpoint together before publishing. Protect the store against disclosure, tampering and rollback. On startup, restore under the provisioned identity. Use `Backend::pending_controls` to inspect uncertain requests. Reconcile with the peer or equipment before repeating a write. A matching acknowledgement can complete the original request and should trigger another durable commit.

Use `Backend::confirmed_settings` to inspect both accepted power directions locally. A device asking for desired settings while a write is outstanding receives no control from the engine. Once the outcome is known, the usual single-control wire restrictions apply. Start a fresh read for observed telemetry after a restart.

Run `python3 interop/restart/check.py` from the checkout to exercise two process terminations with file-backed checkpoints and a separate simulated equipment receipt. At 25 seconds, the desired and observed readings each have 35 seconds left on the original one-minute control. The harness uses synchronized simulated time and one writer on a trusted POSIX filesystem.

## Choose a deployment policy

Use a separate engine per authorized target. For a disconnected or unsupported controllable device, the local engine uses error 4. It rejects a second concurrent command for the same property with error 3. The KEO overlap clause and the broader unavailable-device error table leave room for differing peer behaviour. Verify these cases with your counterpart before adopting its policy.

A backend can hold separate desired consumption and production limits. If a device asks for all settings and a single reply would violate the wire rule against combining both directions, the backend reports `EngineError::DesiredState`. Negotiate this case with the equipment integrator. State readback can still represent both directions.

For pairing, authorize trust changes separately from power controls. A request replaces the trust list as a unit. Return equipment-observed SKIs after certificate installation. Never log raw requests, checkpoints or pairing-state readbacks. Send changes in supported use cases promptly, and schedule `LocalDevice::notification` with the application's clock.
