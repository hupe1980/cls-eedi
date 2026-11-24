//! Test driver for a single synthetic load inside OpenEnergyTwin's virtual device.
use cls_eedi::{
    engine::{Backend, BackendEvent, Clock},
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
    let port: u16 = std::env::args()
        .nth(1)
        .ok_or("missing test broker port")?
        .parse()?;
    let device = std::env::args()
        .nth(2)
        .ok_or("missing unique simulator ID")?;
    let routes = Routes::new(
        format!("clseedi/to-localdevice/{device}"),
        format!("clseedi/from-localdevice/{device}"),
    )?;
    let broker = Broker::loopback(
        &format!("test-backend-{device}"),
        Ipv4Addr::LOCALHOST.into(),
        port,
    )?;
    let mut session = Session::new(broker, Role::Backend, routes, "interop")?;
    let mut backend = Backend::new(&device, "interop", 1);
    let clock = Clock::new(1_800_000_000, 0);
    let request = backend.send_control(
        "simulated-limit",
        Control {
            limits: Some(Power {
                consumption: Some(Limit {
                    value: 4200,
                    duration: Some(60),
                    active: true,
                }),
                production: None,
            }),
            ..Control::default()
        },
        clock,
    )?;
    let start = Instant::now();
    loop {
        let timeout = Duration::from_secs(10)
            .checked_sub(start.elapsed())
            .ok_or("vSMGW acknowledgement timed out")?;
        match session.poll(timeout)? {
            Some(SessionEvent::Ready) => session.publish(&Target::Local, &request)?,
            Some(SessionEvent::Message {
                target: Target::Local,
                message,
            }) => {
                let event = backend.receive(&device, &message, "unused", clock)?;
                if !matches!(event, BackendEvent::Acknowledged { request_id, error_number: 0 } if request_id == request.id)
                {
                    return Err("unexpected vSMGW acknowledgement".into());
                }
                session.disconnect(Duration::from_secs(2))?;
                println!("Rust backend received a successful correlated LPC acknowledgement.");
                return Ok(());
            }
            Some(SessionEvent::Invalid { error, .. }) => return Err(error.into()),
            _ => {}
        }
    }
}
