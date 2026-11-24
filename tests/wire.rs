use cls_eedi::wire::*;
use serde_json::{json, Value};

fn envelope(kind: &str, mut data: Value) -> Value {
    data["protocol"] = json!("1.2.0");
    json!({"type":format!("de.keo-connectivity.clseedi.{kind}"),"source":"local","id":"m1","specversion":"1.0","data":data})
}

fn decode(value: Value) -> Result<Message, WireError> {
    Message::decode(&serde_json::to_vec(&value).unwrap())
}

#[test]
fn roundtrip_preserves_scaled_readings_and_optional_types() {
    let body = json!({"timestamp":0,"measurements":[{"id":"m","source":"gcp","current":{"b":{"value":{"number":-123,"scale":-2}}}}]});
    let expected = envelope("state", body);
    let message = decode(expected.clone()).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&message.encode().unwrap()).unwrap(),
        expected
    );
}

#[test]
fn future_minor_ignores_unknown_objects_and_open_vocabulary() {
    let mut v = envelope(
        "state",
        json!({"timestamp":12,"futureField":null,"supportedEebusUseCases":["lpc","new"],"measurements":[{"source":"future","current":false}]}),
    );
    v["data"]["protocol"] = json!("1.9.0");
    let m = decode(v).unwrap();
    let Payload::State(state) = m.payload else {
        panic!()
    };
    assert_eq!(state.supported_eebus_use_cases.unwrap(), ["lpc"]);
    assert!(state.measurements.unwrap().is_empty());
}

#[test]
fn incompatible_ack_retains_kind_for_loop_suppression() {
    let mut v = envelope("ack", json!({"errorNumber":3}));
    v["data"]["protocol"] = json!("2.0.0");
    assert_eq!(
        decode(v),
        Err(WireError::UnsupportedMajor {
            id: "m1".into(),
            kind: Kind::Ack
        })
    );
}

#[test]
fn duplicate_and_null_do_not_coalesce_into_valid_values() {
    // SR-001: enforce budgets and unambiguous input, including ignored extension objects.
    let raw = serde_json::to_string(&envelope("read", json!({"parameters":["limits"]}))).unwrap();
    let duplicate = raw.replace("\"parameters\":", "\"parameters\":[],\"parame\\u0074ers\":");
    assert_eq!(
        Message::decode(duplicate.as_bytes()),
        Err(WireError::InvalidJson)
    );
    let nested = raw.replace(
        "\"parameters\":",
        "\"future\":{\"x\":1,\"x\":2},\"parameters\":",
    );
    assert_eq!(
        Message::decode(nested.as_bytes()),
        Err(WireError::InvalidJson)
    );
    assert!(decode(envelope("control", json!({"trust":null}))).is_err());
    assert!(decode(envelope(
        "localDeviceStatus",
        json!({"connected":false,"timestamp":null})
    ))
    .is_err());
    let limits = DecodeLimits {
        bytes: raw.len() - 1,
        ..Default::default()
    };
    assert_eq!(
        Message::decode_with(raw.as_bytes(), limits),
        Err(WireError::TooLarge)
    );
    let limits = DecodeLimits {
        depth: 1,
        ..Default::default()
    };
    assert_eq!(
        Message::decode_with(raw.as_bytes(), limits),
        Err(WireError::InvalidJson)
    );
    let limits = DecodeLimits {
        entries_per_container: 1,
        ..Default::default()
    };
    assert_eq!(
        Message::decode_with(raw.as_bytes(), limits),
        Err(WireError::InvalidJson)
    );
}

#[test]
fn limit_failsafe_and_reply_have_distinct_rules() {
    let empty = envelope("control", json!({}));
    assert!(decode(empty.clone()).is_err());
    let mut empty_reply = empty;
    empty_reply["relation"] = json!("read1");
    assert!(decode(empty_reply).is_ok());
    let both = json!({"power":{"active":{"consumption":0,"production":1}}});
    assert!(decode(envelope("control", json!({"failsafes":both}))).is_err());
    assert!(decode(envelope("state", json!({"timestamp":10,"failsafes":both}))).is_ok());
    let unbounded = json!({"power":{"active":{"consumption":{"value":42,"active":true}}}});
    assert!(decode(envelope("control", json!({"limits":unbounded}))).is_ok());
    let mut invalid = unbounded;
    invalid["power"]["active"]["consumption"]["duration"] = json!(0);
    assert!(decode(envelope("control", json!({"limits":invalid}))).is_err());
}

#[test]
fn status_notify_and_measurement_semantics() {
    assert!(decode(envelope("localDeviceStatus", json!({"connected":true}))).is_err());
    assert!(decode(envelope(
        "localDeviceStatus",
        json!({"connected":false,"timestamp":12})
    ))
    .is_err());
    assert!(decode(envelope("control", json!({"notify":{}}))).is_ok());
    assert!(decode(envelope("control", json!({"notify":{"source":["gcp"]}}))).is_err());
    let mut v = envelope(
        "state",
        json!({"timestamp":10,"measurements":[{"id":"m","source":"gcp","power":{"total":{"value":{"number":1,"scale":32768}}}}]}),
    );
    assert!(decode(v.clone()).is_err());
    v["data"]["measurements"][0]["power"]["total"]["value"]["scale"] = json!(-32768);
    let m = v["data"]["measurements"][0].clone();
    v["data"]["measurements"].as_array_mut().unwrap().push(m);
    assert!(decode(v).is_err());
}

#[test]
fn trust_diagnostics_redact_pairing_secrets_and_invalid_fields() {
    // SR-004: all diagnostic representations reachable from wire decoding redact credentials.
    let secret = "b1".repeat(16);
    let props = json!({"trust":[{"shipPairingService":{"forId":"local","forPar":"a0".repeat(32),"secret":secret}}]});
    let m = decode(envelope("control", props.clone())).unwrap();
    let Payload::Control(c) = &m.payload else {
        panic!()
    };
    let entry = &c.trust.as_ref().unwrap()[0];
    let pairing = entry.ship_pairing_service.as_ref().unwrap();
    for text in [
        format!("{m:?}"),
        format!("{c:?}"),
        format!("{entry:?}"),
        format!("{pairing:?}"),
    ] {
        assert!(!text.contains(&secret));
    }
    let mut invalid = props;
    invalid["trust"][0]["shipPairingService"]["forPar"] = json!("not even hex");
    let err = decode(envelope("control", invalid)).unwrap_err();
    assert!(!format!("{err:?} {err}").contains(&secret));
}

#[test]
fn trust_selection_and_directions() {
    let mixed = json!({"trust":[{"ski":"ff".repeat(20),"certificate":"00ff"}]});
    assert!(decode(envelope("control", mixed)).is_err());
    assert!(decode(envelope("control", json!({"trust":[]}))).is_ok());
    assert!(decode(envelope(
        "state",
        json!({"timestamp":10,"trust":[{"certificate":"00ff"}]})
    ))
    .is_err());
}

#[test]
fn integers_use_decimal_lexemes_without_floating_point_rounding() {
    let sample = envelope(
        "control",
        json!({"limits":{"power":{"active":{"consumption":{"value":9876,"active":true}}}}}),
    )
    .to_string();
    for (token, expected) in [
        ("4200.0", 4200),
        ("9.007199254740993e15", 9007199254740993),
        ("9223372036854775807.00", i64::MAX),
        ("4.2e+000000003", 4200),
    ] {
        let msg = Message::decode(sample.replace("9876", token).as_bytes()).unwrap();
        let Payload::Control(c) = msg.payload else {
            panic!()
        };
        assert_eq!(c.limits.unwrap().consumption.unwrap().value, expected);
    }
    for bad in [
        "9.0000000000000000000001",
        "9223372036854775808.0",
        "4.2e-5",
        "00.25e2",
        "1.e3",
        "1e+",
        "1e--1",
    ] {
        assert!(
            Message::decode(sample.replace("9876", bad).as_bytes()).is_err(),
            "{bad}"
        );
    }
    let expected = sample.replace("true", "false").replace("9876", "0.0");
    assert!(Message::decode(expected.as_bytes()).is_ok());
    assert!(Message::decode(sample.replace("9876", "0.0").as_bytes()).is_err());
}

#[test]
fn error_acknowledgements_require_an_unambiguous_request() {
    for (input, expected) in [
        (envelope("control", json!({})), Some(2)),
        (envelope("control", json!({"limits":null})), Some(1)),
        (envelope("ack", json!({})), None),
        (envelope("state", json!({})), None),
    ] {
        let failure = Message::decode_for_reply(input.to_string().as_bytes()).unwrap_err();
        assert_eq!(failure.reply.as_ref().map(|h| h.error_number), expected);
        if let Some(h) = failure.reply {
            assert_eq!(h.id, "m1");
        }
    }
    let mut invalid = envelope("control", json!({"limits":null}));
    invalid["relation"] = json!("a-read");
    assert!(Message::decode_for_reply(invalid.to_string().as_bytes())
        .unwrap_err()
        .reply
        .is_none());
    let dup = invalid
        .to_string()
        .replace("\"id\":", "\"id\":\"another\",\"id\":");
    assert!(Message::decode_for_reply(dup.as_bytes())
        .unwrap_err()
        .reply
        .is_none());
}

#[test]
fn outbound_messages_obey_receiver_budgets() {
    let read = Read {
        parameters: vec!["limits".into(); DecodeLimits::default().entries_per_container + 1],
    };
    assert!(Message::new("r", "b", Payload::Read(read))
        .encode()
        .is_err());
    let read = Message::new(
        "x".repeat(MAX_MESSAGE_BYTES),
        "b",
        Payload::Read(Read::default()),
    );
    assert_eq!(read.encode(), Err(WireError::TooLarge));
}

fn failure_code(v: &Value) -> Option<u64> {
    Message::decode_for_reply(v.to_string().as_bytes())
        .unwrap_err()
        .reply
        .map(|r| r.error_number)
}

#[test]
fn classify_schema_failures_separately_from_empty_or_conflicting_commands() {
    for (body, code) in [
        (json!({"limits": {}}), 2),
        (json!({"failsafes":{"power":{}}}), 2),
        (
            json!({"limits":{"power":{"active":{"newDirection":42}}}}),
            2,
        ),
        (
            json!({"limits":{"power":{"active":{"consumption":{"value":5}}}}}),
            1,
        ),
        (json!({"limits":{"power":null}}), 1),
        (
            json!({"limits":{"power":{"active":{"consumption":{"value":-1,"active":true}}}}}),
            1,
        ),
        (
            json!({"limits":{"power":{"active":{"consumption":{"value":0,"active":true}}}}}),
            2,
        ),
        (json!({"notify":{"interval":20}}), 2),
        (
            json!({"notify":{"interval":30,"endTime":-1,"source":["gcp"]}}),
            1,
        ),
        (json!({"trust":[],"notify":{}}), 2),
        (json!({"unknownFutureControl":{}}), 2),
    ] {
        assert_eq!(failure_code(&envelope("control", body)), Some(code));
    }
    assert!(decode(envelope("state", json!({"timestamp":1,"limits":{}}))).is_ok());
    let mut v = envelope("control", json!({"trust":[]}));
    v.as_object_mut().unwrap().remove("source");
    assert_eq!(failure_code(&v), Some(1));
    v["source"] = json!("backend");
    v.as_object_mut().unwrap().remove("specversion");
    assert_eq!(failure_code(&v), Some(1));
    v.as_object_mut().unwrap().remove("id");
    assert_eq!(failure_code(&v), None);
}

#[test]
fn schema_objects_cannot_be_encoded_as_positional_arrays() {
    assert!(decode(json!([
        "de.keo-connectivity.clseedi.read",
        "id",
        "source",
        "1.0"
    ]))
    .is_err());
    for data in [
        json!({"limits":[[{"consumption":{"value":5,"active":true}}]]}),
        json!({"limits":{"power":{"active":{"consumption":[5,true]}}}}),
        json!({"trust":[["ab".repeat(20)]]}),
        json!({"notify":[]}),
    ] {
        assert_eq!(failure_code(&envelope("control", data)), Some(1));
    }
    let meter = json!({"id":"m","source":"gcp","frequency":{"value":[50,0]}});
    assert!(decode(envelope(
        "state",
        json!({"timestamp":100,"measurements":[meter]})
    ))
    .is_err());
}

#[test]
fn compatibility_gate_and_same_major_acknowledgement_policy() {
    for kind in ["read", "control", "state", "ack", "localDeviceStatus"] {
        for related in [false, true] {
            let mut v = envelope(kind, json!({}));
            v["data"]["protocol"] = json!("2.0.0");
            if related {
                v["relation"] = json!("read-1");
            }
            assert_eq!(failure_code(&v), (kind != "ack").then_some(2));
        }
    }
    let mut bad = envelope("control", json!({"notify":false}));
    bad["relation"] = json!("read-1");
    assert_eq!(failure_code(&bad), None);
    for kind in ["state", "ack", "localDeviceStatus"] {
        assert_eq!(failure_code(&envelope(kind, json!({}))), None);
    }
}

#[test]
fn extensions_can_use_unbounded_json_numbers_without_changing_known_types() {
    let read = envelope("read", json!({"extension":1}))
        .to_string()
        .replace("\"extension\":1", "\"extension\":1e100000");
    assert!(Message::decode(read.as_bytes()).is_ok());
    let command = envelope("control", json!({"notify":123})).to_string();
    for token in ["1e100000", "0.1", "-184467440737095516150"] {
        let err = Message::decode_for_reply(command.replace("123", token).as_bytes()).unwrap_err();
        assert_eq!(err.reply.unwrap().error_number, 1);
    }
    // A real object member matching serde_json's internal numeric representation
    // remains a member of an object on the wire.
    let forged = envelope(
        "state",
        json!({"timestamp":{"$serde_json::private::Number":"100"}}),
    )
    .to_string();
    assert!(Message::decode(forged.as_bytes()).is_err());
}

#[test]
fn an_unexpected_related_read_reports_a_protocol_error() {
    let mut read = envelope("read", json!({}));
    read["relation"] = json!("some-read");
    assert_eq!(failure_code(&read), Some(2));
    let mut malformed = read;
    malformed["data"]["parameters"] = json!(false);
    assert_eq!(failure_code(&malformed), Some(1));
}
