//! Read selected state from a local MQTT 5 broker. This example sends one read.
use cls_eedi::{
    engine::{Backend, BackendEvent, Clock, EngineError},
    transport::{
        mqtt::{Broker, Error as MqttError, Session, SessionEvent},
        Role, Routes, Target,
    },
    wire::{DecodeLimits, Payload, TrustReport},
};
use std::{
    env,
    error::Error,
    net::Ipv4Addr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const USAGE: &str = "\
Usage: mqtt_read PORT TO_DEVICE FROM_DEVICE [--ship ID] [--timeout SECONDS] [--field NAME]...
       mqtt_read PORT TO_DEVICE FROM_DEVICE --trust-summary [--timeout SECONDS]
Connects to 127.0.0.1. A field read prints the correlated state envelope.
--trust-summary reads local trust and prints only aggregate counts and the timestamp.
Fields: limits (default), failsafes, measurements, notify, supportedEebusUseCases.
Timeout: 10 seconds by default, 1–300 seconds allowed.";

struct Query {
    port: u16,
    routes: Routes,
    target: Target,
    fields: Vec<String>,
    trust_summary: bool,
    timeout: Duration,
}

impl Query {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let port = args.next().ok_or(USAGE)?.parse()?;
        let mut routes = Routes::new(args.next().ok_or(USAGE)?, args.next().ok_or(USAGE)?)?;
        let mut target = Target::Local;
        let mut fields = Vec::new();
        let mut timeout = 10;
        let mut trust_summary = false;
        while let Some(flag) = args.next() {
            if flag == "--trust-summary" {
                if trust_summary {
                    return Err("--trust-summary may be specified only once".into());
                }
                trust_summary = true;
                continue;
            }
            let value = args.next().ok_or(USAGE)?;
            match flag.as_str() {
                "--ship" if target == Target::Local => {
                    routes = routes.with_ship(value.clone())?;
                    target = Target::Ship(value);
                }
                "--field"
                    if matches!(
                        value.as_str(),
                        "limits"
                            | "failsafes"
                            | "measurements"
                            | "notify"
                            | "supportedEebusUseCases"
                    ) =>
                {
                    if !fields.contains(&value) {
                        fields.push(value);
                    }
                }
                "--timeout" => {
                    timeout = value.parse()?;
                    if !(1..=300).contains(&timeout) {
                        return Err(USAGE.into());
                    }
                }
                _ => return Err(USAGE.into()),
            }
        }
        if trust_summary {
            if target != Target::Local || !fields.is_empty() {
                return Err("--trust-summary requires a local target and cannot be combined with --ship or --field".into());
            }
            fields.push("trust".into());
        } else if fields.is_empty() {
            fields.push("limits".into());
        }
        Ok(Self {
            port,
            routes,
            target,
            fields,
            trust_summary,
            timeout: Duration::from_secs(timeout),
        })
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1).peekable();
    if matches!(args.peek().map(String::as_str), Some("--help" | "-h")) {
        println!("{USAGE}");
        return Ok(());
    }
    let query = Query::parse(args)?;
    let id = Uuid::new_v4().to_string();
    let route = query.routes.topic(Role::Backend, &query.target)?;
    let mut backend = Backend::new(
        &route,
        "mqtt-read",
        DecodeLimits::default().entries_per_container,
    );
    let request = backend.read(&id, query.fields)?;
    let broker = Broker::loopback(
        &format!("read-{id}"),
        Ipv4Addr::LOCALHOST.into(),
        query.port,
    )?;
    let mut session = Session::new(broker, Role::Backend, query.routes, "mqtt-read")?;
    let start = Instant::now();
    let remaining = || {
        query
            .timeout
            .checked_sub(start.elapsed())
            .ok_or("timed out waiting for a correlated state reply")
    };
    loop {
        let event = match session.poll(remaining()?) {
            Err(MqttError::Route(_)) => continue,
            result => result?,
        };
        match event {
            Some(SessionEvent::Ready) => session.publish(&query.target, &request)?,
            Some(SessionEvent::Message { target, message })
                if target == query.target
                    && (message.relation.as_deref() == Some(&id)
                        || (matches!(message.payload, Payload::Ack(_))
                            && message.relation.is_none()
                            && message.id == id)) =>
            {
                let unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_secs()
                    .try_into()?;
                let clock = Clock::new(unix, start.elapsed().as_secs());
                let event = match backend.receive(
                    &route,
                    &message,
                    &Uuid::new_v4().to_string(),
                    clock,
                ) {
                    Err(EngineError::Unexpected)
                        if matches!(message.payload, Payload::State(_)) =>
                    {
                        return Err(
                            "device returned a state reply outside the requested field selection"
                                .into(),
                        )
                    }
                    result => result?,
                };
                match event {
                    BackendEvent::StateUpdated if message.relation.as_deref() == Some(&id) => {
                        let result = if query.trust_summary {
                            let Payload::State(state) = &message.payload else {
                                return Err("expected a correlated trust state".into());
                            };
                            serde_json::to_vec(&TrustReport::from_state(state))?
                        } else {
                            message.encode()?
                        };
                        session.disconnect(remaining()?)?;
                        println!("{}", std::str::from_utf8(&result)?);
                        return Ok(());
                    }
                    BackendEvent::Acknowledged { error_number, .. } => {
                        return Err(format!(
                            "device rejected read with CLS.EEDI error {error_number}"
                        )
                        .into());
                    }
                    _ => return Err("unexpected response to read".into()),
                }
            }
            Some(SessionEvent::NoSubscribers { .. }) => {
                return Err("broker reported no subscriber on the selected route".into())
            }
            _ => {}
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mqtt_read: {error}");
        std::process::exit(1);
    }
}
