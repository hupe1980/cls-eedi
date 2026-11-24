# Process recovery after equipment acceptance

Run the local simulation on Linux or macOS with Cargo and Python 3. It creates a fresh private directory, launches four worker processes, and removes the directory when it finishes.

```sh
python3 interop/restart/check.py
```

| Simulated time | Durable event | Observation |
| --- | --- | --- |
| 0 s | Backend commits its pending command and outgoing bytes together. Local device commits its pending operation before dispatch. | A 4,200 W consumption ceiling is valid for 60 seconds. |
| 2 s | Equipment records one accepted write with the original expiry. | The driver sends SIGKILL after the receipt is durable, before the local device processes acceptance. |
| 12 s | Fresh local engine restores its pending operation. Original request redelivery produces no actuation. After consulting the equipment receipt, it commits its completed checkpoint and outbound ACK together. | The driver sends SIGKILL again before ACK delivery. |
| 25 s | Fresh backend and local engines restore from disk. The local device replays the ACK on duplicate input. The backend correlates the delayed ACK and commits its checkpoint with the outstanding command removed. | A selective state read and a desired-control read each report 35 seconds remaining. The equipment history contains one write. |
| 60 s | Another process loads the terminal backend checkpoint. | The outstanding command is absent. Replaying the ACK leaves the original lease expired on both roles and the write count remains one. |

The worker keeps separate files for backend state, device state and simulated equipment receipts. Each commit creates a private temporary file in the destination directory, flushes it with `sync_all`, renames it over the old snapshot, and syncs the directory. This example assumes one writer per file and a trusted POSIX filesystem. It supplies deterministic synchronized UTC and a new monotonic origin for each process. The synthetic equipment exposes a durable, independently queryable receipt keyed by the operation ID.

The test exercises actual process termination with file-backed state. Broker delivery, physical hardware, power-loss durability, storage rollback and multi-writer coordination need deployment-specific evidence. Equipment that cannot establish a previous write's outcome must leave the operation pending for reconciliation. [Rust worker](../../examples/restart.rs)
