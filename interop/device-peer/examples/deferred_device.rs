//! A loopback equipment fixture. The test driver releases each write through stdin.
use cls_eedi::{
    engine::{Clock, DeviceEvent, LocalDevice, Outcome},
    transport::{
        mqtt::{Broker, Session, SessionEvent},
        Role, Routes, Target,
    },
    wire::{Message, Payload},
};
use std::{
    error::Error,
    io::{self, BufRead},
    net::Ipv4Addr,
    sync::mpsc,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let port = std::env::args().nth(1).ok_or("expected port")?.parse()?;
    let broker = Broker::loopback("overlap-fixture", Ipv4Addr::LOCALHOST.into(), port)?;
    let routes = Routes::new(
        "clseedi/to-localdevice/overlap",
        "clseedi/from-localdevice/overlap",
    )?;
    let mut link = Session::new(broker, Role::LocalDevice, routes, "equipment")?;
    let mut device = LocalDevice::new("overlap", "equipment", false, 8);
    let clock = Clock::new(1_800_000_000, 0);
    let capabilities = device.capabilities("caps", vec!["lpc".into()], clock)?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let publish = |link: &Session, reply: &Message| -> Result<(), Box<dyn Error>> {
        link.publish(&Target::Local, reply)?;
        if let Payload::Ack(a) = &reply.payload {
            println!("ACK {} {}", reply.id, a.error_number);
        }
        Ok(())
    };
    let mut deadline = None;
    let mut sequence = 0;
    loop {
        if let Some(command) = rx.try_iter().next() {
            if command == "read" {
                link.publish(&Target::Local, &device.read_control("desired-probe")?)?;
                deadline = Some(Instant::now() + Duration::from_secs(2));
            } else {
                let reply = device
                    .complete(&command, Outcome::Accepted, clock)?
                    .ok_or("expected ACK")?;
                publish(&link, &reply)?;
            }
        }
        match link.poll(Duration::from_millis(20))? {
            Some(SessionEvent::Ready) => {
                link.publish(&Target::Local, &capabilities)?;
                println!("READY");
            }
            Some(SessionEvent::Message {
                target: Target::Local,
                message,
            }) => {
                if deadline.is_some()
                    && matches!(message.payload, Payload::Control(_) | Payload::Ack(_))
                {
                    return Err("unexpected response during desired-settings probe".into());
                }
                sequence += 1;
                match device.receive("overlap", &message, &format!("response-{sequence}"), clock)? {
                    DeviceEvent::Apply { request_id } => {
                        let limit = device
                            .control_for_dispatch(&request_id, clock)?
                            .and_then(|c| c.limits?.consumption)
                            .ok_or("expected consumption limit")?;
                        println!("WRITE {request_id} {}", limit.value);
                    }
                    DeviceEvent::Reply(reply) => publish(&link, &reply)?,
                    DeviceEvent::Silent => {}
                }
            }
            Some(SessionEvent::Invalid { error, .. }) => return Err(error.into()),
            _ => {}
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            device.cancel_read();
            deadline = None;
            println!("SETTINGS TIMEOUT");
        }
    }
}
