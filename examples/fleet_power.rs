//! Publish a controllable meter from one synchronized sample of two disjoint loads.
use cls_eedi::{
    engine::{Clock, LocalDevice},
    metering::controllable_total_power,
    wire::{Measurement, Message, Meter, MeterPower, Payload, ScaledNumber},
};
use std::{error::Error, io::Write};

fn main() -> Result<(), Box<dyn Error>> {
    let missing = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--missing" => true,
        _ => return Err("usage: fleet_power [--missing]".into()),
    };
    let members = ["heat-pump", "inverter"];
    // Watts under the passive sign convention. Both sources refer to this interval.
    let pump = ScaledNumber {
        number: 18425,
        scale: -1,
    };
    let inverter = ScaledNumber {
        number: -41725,
        scale: -2,
    };
    let samples = [
        ("heat-pump", Some(pump)),
        ("inverter", (!missing).then_some(inverter)),
    ];
    let total = controllable_total_power(&members, &samples)?;
    let aggregate = Meter {
        id: "site-controllable".into(),
        source: "controllable".into(),
        power: total.map(|value| MeterPower {
            total: Some(Measurement { value }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut device = LocalDevice::new("site", "collector", false, 8);
    device.observe_meters(vec![aggregate])?;
    let state = device.snapshot(Clock::new(1_800_000_000, 0))?;
    let envelope = Message::new("fleet-snapshot", "collector", Payload::State(state));
    let mut output = std::io::stdout().lock();
    output.write_all(&envelope.encode()?)?;
    writeln!(output)?;
    Ok(())
}
