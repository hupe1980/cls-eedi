//! A backend, simulated equipment acceptance and a selective read over encoded JSON.
use cls_eedi::{
    engine::{Backend, BackendEvent, Clock, DeviceEvent, LocalDevice, Outcome},
    wire::{Control, Limit, Message, Power},
};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let route = "example-device";
    let receipt = Clock::new(1_800_000_000, 0);
    let later = Clock::new(1_800_000_010, 10);
    let mut backend = Backend::new(route, "operator", 16);
    let mut device = LocalDevice::new(route, "meter-room", false, 16);
    device.capabilities("capabilities-1", vec!["lpc".into()], receipt)?;

    let control = Control {
        limits: Some(Power {
            consumption: Some(Limit {
                value: 4200,
                active: true,
                duration: Some(60),
            }),
            production: None,
        }),
        ..Control::default()
    };
    let request = backend.send_control("limit-1", control, receipt)?;
    let incoming = Message::decode(&request.encode()?)?;
    let DeviceEvent::Apply { request_id } = device.receive(route, &incoming, "reply-1", receipt)?
    else {
        return Err("expected an equipment operation".into());
    };
    let dispatch = device
        .control_for_dispatch(&request_id, receipt)?
        .ok_or("command expired before reaching equipment")?;
    assert_eq!(
        dispatch.limits.and_then(|p| p.consumption).map(|l| l.value),
        Some(4200)
    );
    // The simulated equipment has now accepted the ceiling.
    let ack = device
        .complete(&request_id, Outcome::Accepted, receipt)?
        .ok_or("expected an acknowledgement")?;
    let event = backend.receive(route, &Message::decode(&ack.encode()?)?, "reply-3", receipt)?;
    if !matches!(
        event,
        BackendEvent::Acknowledged {
            error_number: 0,
            ..
        }
    ) {
        return Err("equipment did not accept the command".into());
    }
    let duplicate = device.receive(route, &incoming, "reply-4", later)?;
    if !matches!(duplicate, DeviceEvent::Reply(_)) {
        return Err("expected a cached reply to the redelivery".into());
    }
    let read = backend.read("read-1", vec!["limits".into()])?;
    let DeviceEvent::Reply(state) = device.receive(route, &read, "state-1", later)? else {
        return Err("expected a state reply".into());
    };
    backend.receive(route, &Message::decode(&state.encode()?)?, "reply-5", later)?;
    let limit = backend
        .state()
        .and_then(|state| state.limits.as_ref())
        .and_then(|limits| limits.consumption.as_ref())
        .ok_or("missing consumption readback")?;
    assert_eq!(limit.duration, Some(50));
    println!(
        "Equipment accepted {} W. After redelivery, {} s remain.",
        limit.value,
        limit.duration.unwrap_or_default()
    );
    Ok(())
}
