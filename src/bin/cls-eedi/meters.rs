//! Meter-only JSON export. KEO 1.2.0 uses the MPC/MGCP measurement units.
use cls_eedi::wire::{Measurement, Meter, State};
use serde::Serialize;
use std::io::{self, Write};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    timestamp: i64,
    meter_count: Option<usize>,
    rows: Vec<Row<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Row<'a> {
    timestamp: i64,
    meter_id: &'a str,
    source: &'a str,
    property: &'static str,
    value: String,
    unit: &'static str,
}

fn readings<'a>(rows: &mut Vec<Row<'a>>, timestamp: i64, meter: &'a Meter) {
    let mut add = |property, unit, sample: &Option<Measurement>| {
        if let Some(sample) = sample {
            rows.push(Row {
                timestamp,
                meter_id: &meter.id,
                source: &meter.source,
                property,
                value: sample.value.to_string(),
                unit,
            });
        }
    };
    if let Some(power) = &meter.power {
        add("power.total", "W", &power.total);
        if let Some(phases) = &power.phases {
            add("power.phases.a", "W", &phases.a);
            add("power.phases.b", "W", &phases.b);
            add("power.phases.c", "W", &phases.c);
        }
    }
    if let Some(energy) = &meter.energy {
        add("energy.consumed", "Wh", &energy.consumed);
        add("energy.produced", "Wh", &energy.produced);
    }
    if let Some(current) = &meter.current {
        add("current.a", "A", &current.a);
        add("current.b", "A", &current.b);
        add("current.c", "A", &current.c);
    }
    if let Some(voltage) = &meter.voltage {
        add("voltage.a-n", "V", &voltage.a_n);
        add("voltage.b-n", "V", &voltage.b_n);
        add("voltage.c-n", "V", &voltage.c_n);
    }
    add("frequency", "Hz", &meter.frequency);
}

/// serde_json already escapes quotes, backslashes and ASCII controls below DEL.
/// Encode the remaining non-printable and non-ASCII text to keep terminals free
/// of C1 escapes and bidi overrides while preserving exact identifier round trips.
struct AsciiJson;

impl serde_json::ser::Formatter for AsciiJson {
    fn write_string_fragment<W: Write + ?Sized>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if !ch.is_ascii() || ch == '\x7f' {
                writer.write_all(&fragment.as_bytes()[start..index])?;
                for code in ch.encode_utf16(&mut [0; 2]) {
                    write!(writer, "\\u{code:04x}")?;
                }
                start = index + ch.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

pub(super) fn write_report(state: &State, output: impl Write) -> io::Result<()> {
    let mut report = Report {
        timestamp: state.timestamp,
        meter_count: state.measurements.as_ref().map(Vec::len),
        rows: Vec::new(),
    };
    for meter in state.measurements.iter().flatten() {
        readings(&mut report.rows, state.timestamp, meter);
    }
    let mut output = io::BufWriter::new(output);
    report.serialize(&mut serde_json::Serializer::with_formatter(
        &mut output,
        AsciiJson,
    ))?;
    output.write_all(b"\n")?;
    output.flush()
}
