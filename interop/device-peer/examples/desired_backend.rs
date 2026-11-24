//! Exercise REQ-50 with one confirmed direction and then with both directions.
use cls_eedi::{
    engine::{Backend, BackendEvent, Clock, EngineError},
    transport::{
        mqtt::{Broker, Session, SessionEvent},
        Role, Routes, Target,
    },
    wire::{Control, Limit, Power},
};
use std::{
    error::Error,
    net::Ipv4Addr,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let port = std::env::args().nth(1).ok_or("expected port")?.parse()?;
    let routes = Routes::new("desired/to", "desired/from")?;
    let broker = Broker::loopback("desired-backend", Ipv4Addr::LOCALHOST.into(), port)?;
    let mut session = Session::new(broker, Role::Backend, routes, "operator")?;
    let mut backend = Backend::new("site", "operator", 4);
    let clock = Clock::new(1_800_000_000, 0);
    let limit = |value| {
        Some(Limit {
            value,
            active: true,
            duration: None,
        })
    };
    let consumption = Power {
        consumption: limit(4200),
        production: None,
    };
    let production = Power {
        consumption: None,
        production: limit(1500),
    };
    let control = |power| Control {
        limits: Some(power),
        ..Default::default()
    };
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        match session.poll(Duration::from_millis(100))? {
            Some(SessionEvent::Ready) => {
                let first = backend.send_control("in", control(consumption.clone()), clock)?;
                session.publish(&Target::Local, &first)?;
            }
            Some(SessionEvent::Message {
                target: Target::Local,
                message,
            }) => match backend.receive("site", &message, "settings", clock) {
                Ok(BackendEvent::Acknowledged {
                    error_number: 0, ..
                }) => (),
                Ok(BackendEvent::Reply(reply)) if message.id == "single" => {
                    session.publish(&Target::Local, &reply)?;
                    let second = backend.send_control("out", control(production.clone()), clock)?;
                    session.publish(&Target::Local, &second)?;
                }
                Err(EngineError::DesiredState) if message.id == "mixed" => {
                    let settings = backend.confirmed_settings(clock)?.limits.unwrap();
                    assert_eq!(settings.consumption, consumption.consumption);
                    assert_eq!(settings.production, production.production);
                    println!(
                        "Mixed read returned DesiredState and preserved both confirmed settings."
                    );
                }
                Err(EngineError::DesiredState) if message.id == "barrier" => {
                    let probe = backend.read("still-connected", vec!["limits".into()])?;
                    session.publish(&Target::Local, &probe)?;
                    session.disconnect(Duration::from_secs(2))?;
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
                Ok(_) => return Err("unexpected backend event".into()),
            },
            Some(SessionEvent::Invalid { error, .. }) => return Err(error.into()),
            _ => {}
        }
    }
    Err("timed out during desired-settings exchange".into())
}
