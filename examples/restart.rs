//! Crash-recovery worker for `python3 interop/restart/check.py` on Unix.
//! The driver supplies a private, single-writer directory and kills this worker
//! at two explicit boundaries. No network or physical equipment is involved.
use cls_eedi::{
    engine::{Backend, BackendEvent, Clock, DeviceEvent, EngineError, LocalDevice, Outcome},
    wire::{Control, Limit, Message, Payload, Power},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const ROUTE: &str = "simulated-site";
const ID: &str = "load-test-001";
const UTC: i64 = 1_800_000_000;

/// The engine's opaque checkpoint and its outstanding wire publication commit together.
#[derive(Serialize, Deserialize)]
struct Journal {
    engine: Vec<u8>,
    outbox: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    id: String,
    watts: i64,
    expires_at: i64,
}

fn save(directory: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staged = directory.join(format!(".{name}.{}.{nonce}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&staged)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, value)?;
        file.sync_all()?;
        fs::rename(&staged, directory.join(name))?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(staged);
    }
    result
}

fn load<T: DeserializeOwned>(directory: &Path, name: &str) -> Result<T> {
    Ok(serde_json::from_reader(File::open(directory.join(name))?)?)
}

fn backend() -> Backend {
    Backend::new(ROUTE, "backend", 8)
}

fn device() -> LocalDevice {
    LocalDevice::new(ROUTE, "local-device", false, 8)
}

fn boundary(name: &str) -> Result<()> {
    println!("{name}");
    io::stdout().flush()?;
    // The supervising test sends SIGKILL while the worker is parked.
    loop {
        std::thread::park();
    }
}

fn request() -> Control {
    Control {
        limits: Some(Power {
            production: None,
            consumption: Some(Limit {
                value: 4200,
                active: true,
                duration: Some(60),
            }),
        }),
        ..Control::default()
    }
}

// The synthetic equipment has its own independently persisted history. It
// records every equipment call, including any duplicate the driver might make.
fn apply(directory: &Path, id: String, control: Control, now: Clock) -> Result<()> {
    let limit = control
        .limits
        .and_then(|p| p.consumption)
        .ok_or("expected consumption limit")?;
    let receipt = Receipt {
        id,
        watts: limit.value,
        expires_at: now.unix_seconds + i64::from(limit.duration.ok_or("expected a timed limit")?),
    };
    let mut operations: Vec<Receipt> = load(directory, "equipment.json")?;
    operations.push(receipt);
    save(directory, "equipment.json", &operations)
}

fn submit(directory: &Path) -> Result<()> {
    let now = Clock::new(UTC, 0);
    let mut backend = backend();
    let control = backend.send_control(ID, request(), now)?;
    let bytes = control.encode()?;
    save(
        directory,
        "backend.json",
        &Journal {
            engine: backend.checkpoint(now)?,
            outbox: Some(bytes.clone()),
        },
    )?;

    let mut device = device();
    device.capabilities("equipment-capabilities", vec!["lpc".into()], now)?;
    let DeviceEvent::Apply { request_id } =
        device.receive(ROUTE, &Message::decode(&bytes)?, "unused", now)?
    else {
        return Err("expected first dispatch admission".into());
    };
    save(
        directory,
        "device.json",
        &Journal {
            engine: device.checkpoint(now)?,
            outbox: None,
        },
    )?;
    let dispatch = Clock::new(UTC + 2, 2);
    let control = device
        .control_for_dispatch(&request_id, dispatch)?
        .ok_or("command expired before dispatch")?;
    apply(directory, request_id, control, dispatch)?;
    boundary("equipment-accepted")
}

fn reconcile(directory: &Path) -> Result<()> {
    // A new process starts with a new monotonic origin and an independently advanced UTC.
    let now = Clock::new(UTC + 12, 0);
    let backend_store: Journal = load(directory, "backend.json")?;
    let control = Message::decode(
        backend_store
            .outbox
            .as_deref()
            .ok_or("missing durable control")?,
    )?;
    let local_store: Journal = load(directory, "device.json")?;
    let mut device = device().restore(&local_store.engine, now)?;
    assert!(matches!(
        device.receive(ROUTE, &control, "unused", now)?,
        DeviceEvent::Silent
    ));
    assert!(matches!(
        device.control_for_dispatch(ID, now),
        Err(EngineError::ReconciliationRequired)
    ));
    // Only the independently persisted equipment receipt establishes acceptance.
    let receipts: Vec<Receipt> = load(directory, "equipment.json")?;
    let [receipt] = receipts.as_slice() else {
        return Err("expected exactly one equipment write".into());
    };
    if receipt.id != ID || receipt.watts != 4200 || receipt.expires_at != UTC + 60 {
        return Err("unexpected equipment outcome".into());
    }
    let ack = device
        .complete(ID, Outcome::Accepted, now)?
        .ok_or("missing acknowledgement")?;
    save(
        directory,
        "device.json",
        &Journal {
            engine: device.checkpoint(now)?,
            outbox: Some(ack.encode()?),
        },
    )?;
    boundary("ack-durable")
}

fn recover(directory: &Path) -> Result<()> {
    let now = Clock::new(UTC + 25, 0);
    let local_store: Journal = load(directory, "device.json")?;
    let mut device = device().restore(&local_store.engine, now)?;
    let ack = Message::decode(local_store.outbox.as_deref().ok_or("missing durable ack")?)?;
    let backend_store: Journal = load(directory, "backend.json")?;
    let control = Message::decode(
        backend_store
            .outbox
            .as_deref()
            .ok_or("missing durable control")?,
    )?;
    let mut backend = backend().restore(&backend_store.engine, now)?;
    // A second delivery after completion must return the same ACK without dispatch.
    let DeviceEvent::Reply(cached) = device.receive(ROUTE, &control, "unused", now)? else {
        return Err("completed duplicate did not replay its acknowledgement".into());
    };
    assert_eq!(*cached, ack);
    let outcome = backend.receive(ROUTE, &ack, "unused", now)?;
    assert!(
        matches!(outcome, BackendEvent::Acknowledged { request_id, error_number: 0 } if request_id == ID)
    );
    save(
        directory,
        "backend.json",
        &Journal {
            engine: backend.checkpoint(now)?,
            outbox: None,
        },
    )?;

    let read = backend.read("readback", vec!["limits".into()])?;
    let DeviceEvent::Reply(state) = device.receive(ROUTE, &read, "readback-state", now)? else {
        return Err("device did not return state".into());
    };
    assert!(matches!(
        backend.receive(ROUTE, &state, "unused", now)?,
        BackendEvent::StateUpdated
    ));
    let observed = backend
        .state()
        .and_then(|s| s.limits.as_ref())
        .and_then(|p| p.consumption.as_ref())
        .ok_or("missing observed limit")?;
    assert_eq!(
        (observed.value, observed.active, observed.duration),
        (4200, true, Some(35))
    );

    // The backend also retains the original command deadline across delayed acceptance.
    let read = device.read_control("read-desired")?;
    let BackendEvent::Reply(desired) = backend.receive(ROUTE, &read, "desired", now)? else {
        return Err("backend did not return desired settings".into());
    };
    let Payload::Control(desired) = desired.payload else {
        return Err("expected related control".into());
    };
    assert_eq!(
        desired
            .limits
            .and_then(|p| p.consumption)
            .and_then(|l| l.duration),
        Some(35)
    );
    let receipts: Vec<Receipt> = load(directory, "equipment.json")?;
    assert_eq!(receipts.len(), 1);
    println!(
        "Recovered delayed ACK; 4200 W has 35 s remaining on both roles; one equipment write."
    );
    Ok(())
}

fn verify_expiry(directory: &Path) -> Result<()> {
    let now = Clock::new(UTC + 60, 0);
    let store: Journal = load(directory, "backend.json")?;
    assert!(store.outbox.is_none());
    let mut backend = backend().restore(&store.engine, now)?;
    let store: Journal = load(directory, "device.json")?;
    let mut device = device().restore(&store.engine, now)?;
    // Replaying the delayed ACK cannot create or extend a completed request.
    let ack = Message::decode(store.outbox.as_deref().ok_or("missing durable ack")?)?;
    assert!(matches!(
        backend.receive(ROUTE, &ack, "unused", now),
        Err(EngineError::Unexpected)
    ));
    let read = device.read_control("verify-expiry")?;
    let BackendEvent::Reply(reply) = backend.receive(ROUTE, &read, "expired-settings", now)? else {
        return Err("missing restored settings".into());
    };
    let Payload::Control(desired) = reply.payload else {
        return Err("expected accumulated control".into());
    };
    let limit = desired
        .limits
        .and_then(|p| p.consumption)
        .ok_or("missing restored limit")?;
    assert!(!limit.active && limit.duration.is_none());
    let observed = device
        .snapshot(now)?
        .limits
        .and_then(|p| p.consumption)
        .ok_or("missing restored observation")?;
    assert!(!observed.active && observed.duration.is_none());
    assert_eq!(load::<Vec<Receipt>>(directory, "equipment.json")?.len(), 1);
    println!("A fresh process confirmed expiry at the original 60-second deadline.");
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let phase = args.next().ok_or("run python3 interop/restart/check.py")?;
    let path = args.next().ok_or("expected private checkpoint directory")?;
    let directory = Path::new(&path);
    match phase.to_str() {
        Some("submit") => {
            // This phase is reserved for a fresh simulation.
            if directory.read_dir()?.next().is_some() {
                return Err("expected empty directory".into());
            }
            save(directory, "equipment.json", &Vec::<Receipt>::new())?;
            submit(directory)
        }
        Some("reconcile") => reconcile(directory),
        Some("recover") => recover(directory),
        Some("verify-expiry") => verify_expiry(directory),
        _ => Err("unknown phase".into()),
    }
}
