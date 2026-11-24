//! Loopback-only simulated equipment for testing an independent backend.
use cls_eedi::{
    engine::{Clock, DeviceEvent, LocalDevice, Outcome},
    transport::{
        mqtt::{Broker, Session, SessionEvent},
        Role, Routes, Target,
    },
};
use std::{
    error::Error,
    net::Ipv4Addr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

fn main() -> Result<(), Box<dyn Error>> {
    let port: u16 = std::env::args()
        .nth(1)
        .ok_or("usage: mqtt_device PORT")?
        .parse()?;
    let start = Instant::now();
    let now = || -> Result<Clock, Box<dyn Error>> {
        Ok(Clock::new(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_secs()
                .try_into()?,
            start.elapsed().as_secs(),
        ))
    };
    let id = || Uuid::new_v4().to_string();
    let route = "demo";
    let routes = Routes::new(
        "clseedi/to-localdevice/demo",
        "clseedi/from-localdevice/demo",
    )?;
    let broker = Broker::loopback(&id(), Ipv4Addr::LOCALHOST.into(), port)?;
    let mut session = Session::new(broker, Role::LocalDevice, routes, "simulated-equipment")?;
    let mut device = LocalDevice::new(route, "simulated-equipment", false, 64);
    let capabilities = device.capabilities(&id(), vec!["lpc".into()], now()?)?;
    loop {
        match session.poll(Duration::from_secs(1))? {
            Some(SessionEvent::Ready) => {
                session.publish(&Target::Local, &capabilities)?;
                println!("Ready for controls on clseedi/to-localdevice/demo");
            }
            Some(SessionEvent::Message {
                target: Target::Local,
                message,
            }) => {
                let response = match device.receive(route, &message, &id(), now()?)? {
                    DeviceEvent::Apply { request_id } => {
                        let current = device.control_for_dispatch(&request_id, now()?)?;
                        let outcome = match current {
                            Some(control)
                                if control.trust.is_none()
                                    && control.notify.is_none()
                                    && control.failsafes.is_none() =>
                            {
                                if let Some(limit) =
                                    control.limits.and_then(|limits| limits.consumption)
                                {
                                    println!(
                                        "Simulated equipment received {} W (active={})",
                                        limit.value, limit.active
                                    );
                                    Outcome::Accepted
                                } else {
                                    Outcome::Unsupported
                                }
                            }
                            Some(_) => Outcome::Unsupported,
                            None => Outcome::Rejected,
                        };
                        device.complete(&request_id, outcome, now()?)?
                    }
                    DeviceEvent::Reply(reply) => Some(*reply),
                    DeviceEvent::Silent => None,
                };
                if let Some(reply) = response {
                    session.publish(&Target::Local, &reply)?;
                }
            }
            Some(SessionEvent::Invalid { error, .. }) => eprintln!("Invalid envelope: {error}"),
            _ => {}
        }
    }
}
