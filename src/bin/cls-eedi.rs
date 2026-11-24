//! Validate an envelope or project selected state into an operator report.
#[path = "cls-eedi/meters.rs"]
mod meters;
use cls_eedi::wire::{Message, Payload, State, TrustReport, MAX_MESSAGE_BYTES};
use std::{
    env,
    io::{self, Read, Write},
    process::ExitCode,
};

const USAGE: &str = "\
Usage: cls-eedi check < message.json
       cls-eedi meters < state.json
       cls-eedi trust-summary < state.json
check   Validate one CLS.EEDI envelope from standard input.
meters  Export meter readings from one state envelope as JSON.
trust-summary  Count direct, pending and established trust entries without printing credentials.";

fn read_envelope() -> Result<Message, String> {
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_MESSAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read standard input: {e}"))?;
    Message::decode(&bytes).map_err(|e| format!("invalid envelope: {e}"))
}

fn check() -> Result<(), String> {
    let message = read_envelope()?;
    let family = match message.payload {
        Payload::Read(_) => "read",
        Payload::State(_) => "state",
        Payload::Ack(_) => "ack",
        Payload::Control(_) => "control",
        Payload::LocalDeviceStatus(_) => "localDeviceStatus",
    };
    println!(
        "valid {family} (CLS.EEDI {}.{}.{})",
        message.protocol.major, message.protocol.minor, message.protocol.patch
    );
    Ok(())
}

fn read_state(command: &str) -> Result<State, String> {
    match read_envelope()?.payload {
        Payload::State(state) => Ok(state),
        _ => Err(format!("{command} expects a CLS.EEDI state envelope")),
    }
}

fn trust_summary() -> Result<(), String> {
    let report = TrustReport::from_state(&read_state("trust-summary")?);
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &report).map_err(|error| error.to_string())?;
    writeln!(out).map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let command = args.next();
    let result = match (command.as_deref().and_then(|s| s.to_str()), args.next()) {
        (Some("check"), None) => check(),
        (Some("trust-summary"), None) => trust_summary(),
        (Some("meters"), None) => read_state("meters").and_then(|state| {
            meters::write_report(&state, io::stdout().lock())
                .map_err(|error| format!("cannot write meter report: {error}"))
        }),
        (Some("--help" | "-h"), None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        (Some("--version" | "-V"), None) => {
            println!("cls-eedi {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
