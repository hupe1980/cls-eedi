use std::{
    io::Write,
    process::{Command, Output, Stdio},
};

fn run(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cls-eedi"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn validates_public_example_and_explains_usage() {
    let output = run(&["check"], include_bytes!("../examples/read.json"));
    assert!(output.status.success());
    assert_eq!(output.stdout, b"valid read (CLS.EEDI 1.2.0)\n");
    assert_eq!(run(&["--help"], b"").status.code(), Some(0));
    assert_eq!(run(&["check", "other"], b"").status.code(), Some(2));
}

#[test]
fn diagnostics_do_not_echo_invalid_input() {
    let output = run(&["check"], br#"{"type":"private-pairing-value"}"#);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-pairing-value"));
}

fn telemetry(extra: serde_json::Value) -> Vec<u8> {
    let mut data = serde_json::json!({"protocol":"1.2.0","timestamp":87});
    data.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::to_vec(&serde_json::json!({
        "type":"de.keo-connectivity.clseedi.state", "source":"private-envelope-source",
        "id":"private-envelope-id","specversion":"1.0","data": data
    }))
    .unwrap()
}

#[test]
fn meters_projects_all_wire_properties_with_units_and_safe_identifiers() {
    use serde_json::{json, Value};
    let id = "meter\\\"\n\x1b\u{7f}\u{9b}\u{202e}🚗";
    let sample = json!({"value":{"number":2305,"scale":-1}});
    let phases = json!({"a":sample, "b":sample, "c":sample});
    let secret = "a7".repeat(16);
    let payload = telemetry(json!({
        "measurements":[
            {"id":id,"source":"gcp","frequency":sample,
             "power":{"total":sample,"phases":phases},
             "energy":{"consumed":sample,"produced":sample},
             "voltage":{"a-n":sample,"b-n":sample,"c-n":sample},"current":phases},
            {"id":"other","source":"controllable","frequency":{"value":{"number":-9223372036854775808i64,"scale":-32768}}}
        ],
        "trust":[{"shipPairingService":{"forId":"private-for-id","forPar":"ab".repeat(32),"secret":secret}}],
        "futureField":"private-extension"
    }));
    let output = run(&["meters"], &payload);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::str::from_utf8(&output.stdout).unwrap();
    assert!(text
        .trim_end_matches('\n')
        .bytes()
        .all(|b| (0x20..=0x7e).contains(&b)));
    assert!(!text.contains(&secret) && !text.contains("private-"));
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_str(text).unwrap();
    assert_eq!(report["timestamp"], 87);
    assert_eq!(report["meterCount"], 2);
    let rows = report["rows"].as_array().unwrap();
    let expected = [
        ("power.total", "W"),
        ("power.phases.a", "W"),
        ("power.phases.b", "W"),
        ("power.phases.c", "W"),
        ("energy.consumed", "Wh"),
        ("energy.produced", "Wh"),
        ("current.a", "A"),
        ("current.b", "A"),
        ("current.c", "A"),
        ("voltage.a-n", "V"),
        ("voltage.b-n", "V"),
        ("voltage.c-n", "V"),
        ("frequency", "Hz"),
    ];
    assert_eq!(rows.len(), expected.len() + 1);
    for (row, (property, unit)) in rows.iter().zip(expected) {
        assert_eq!(
            *row,
            json!({"timestamp":87,"meterId":id,"source":"gcp",
            "property":property,"value":"230.5","unit":unit})
        );
    }
    assert_eq!(
        rows.last().unwrap(),
        &json!({"timestamp":87, "meterId":"other","source":"controllable",
        "property":"frequency","value":"-9223372036854775808e-32768","unit":"Hz"})
    );
}

#[test]
fn meters_distinguishes_omission_empty_and_unavailable_values() {
    use serde_json::{json, Value};
    for (data, count) in [
        (json!({}), Value::Null),
        (json!({"measurements":[]}), json!(0)),
        (
            json!({"measurements":[{"id":"foreign","source":"future"}]}),
            json!(0),
        ),
        (
            json!({"measurements":[{"id":"missing","source":"gcp"}]}),
            json!(1),
        ),
    ] {
        let output = run(&["meters"], &telemetry(data));
        assert!(output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report, json!({"timestamp":87,"meterCount":count,"rows":[]}));
    }
}

#[test]
fn meters_rejects_non_state_invalid_and_oversized_input_without_partial_rows() {
    let mut oversized = vec![b' '; cls_eedi::wire::MAX_MESSAGE_BYTES];
    oversized.push(b' ');
    for input in [
        include_bytes!("../examples/read.json").as_slice(),
        &oversized,
        b"private-invalid-input",
    ] {
        let output = run(&["meters"], input);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-invalid-input"));
    }
}

#[test]
fn bundled_meter_capture_matches_the_quickstart() {
    let output = run(&["meters"], include_bytes!("../examples/meters.json"));
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["meterCount"], 1);
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .any(|r| r["value"] == "-125.5" && r["unit"] == "W"));
    assert!(rows
        .iter()
        .any(|r| r["value"] == "49.98" && r["unit"] == "Hz"));
}

#[test]
fn trust_summary_projects_only_time_and_counts() {
    let output = run(
        &["trust-summary"],
        include_bytes!("../examples/trust-state.json"),
    );
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, b"{\"timestamp\":1800000000,\"trust\":{\"directSki\":1,\"pairingPending\":1,\"pairingEstablished\":1}}\n");
}

#[test]
fn trust_summary_handles_absent_empty_and_invalid_state() {
    use serde_json::{json, Value};
    for (input, expected) in [
        (json!({}), Value::Null),
        (
            json!({"trust":[]}),
            json!({"directSki":0,"pairingPending":0,"pairingEstablished":0}),
        ),
    ] {
        let result = run(&["trust-summary"], &telemetry(input));
        assert!(result.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap(),
            json!({"timestamp":87,"trust":expected})
        );
    }
    for bytes in [
        telemetry(json!({"trust":[{"ski":"private-invalid-material"}]})),
        include_bytes!("../examples/read.json").to_vec(),
    ] {
        let output = run(&["trust-summary"], &bytes);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-invalid-material"));
    }
}
