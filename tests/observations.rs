use cls_eedi::engine::{Backend, BackendEvent, Clock, EngineError};
use cls_eedi::wire::{Message, Meter, Payload, State};

fn state(at: i64, meters: &[(&str, &str)]) -> State {
    State {
        timestamp: at,
        measurements: Some(
            meters
                .iter()
                .map(|(id, scope)| Meter {
                    id: (*id).into(),
                    source: (*scope).into(),
                    ..Default::default()
                })
                .collect(),
        ),
        ..Default::default()
    }
}

fn deliver(
    backend: &mut Backend,
    state: State,
    relation: Option<&str>,
) -> Result<BackendEvent, EngineError> {
    let mut message = Message::new("observation", "device", Payload::State(state));
    message.relation = relation.map(String::from);
    backend.receive("site", &message, "unused", Clock::new(100, 0))
}

fn meters(backend: &Backend) -> Vec<(&str, &str)> {
    backend
        .state()
        .unwrap()
        .measurements
        .iter()
        .flatten()
        .map(|m| (m.id.as_str(), m.source.as_str()))
        .collect()
}

#[test]
fn disjoint_observations_survive_reordering_in_the_same_publication() {
    let mut backend = Backend::new("site", "backend", 4);
    deliver(&mut backend, state(20, &[("a", "gcp")]), None).unwrap();
    let mut older = state(10, &[("a", "controllable"), ("b", "gcp")]);
    older.supported_eebus_use_cases = Some(vec!["lpc".into()]);
    deliver(&mut backend, older, None).unwrap();
    let cached = backend.state().unwrap();
    assert_eq!(cached.timestamp, 20);
    assert_eq!(
        backend.property_observed_at("supportedEebusUseCases"),
        Some(10)
    );
    assert_eq!(backend.property_observed_at("measurements"), Some(20));
    assert_eq!(backend.meter_observed_at("a"), Some(20));
    assert_eq!(backend.meter_observed_at("b"), Some(10));
    assert_eq!(cached.supported_eebus_use_cases.as_ref().unwrap(), &["lpc"]);
    assert_eq!(meters(&backend), [("a", "gcp"), ("b", "gcp")]);
}

#[test]
fn complete_inventory_prevents_old_sources_from_reappearing() {
    let mut backend = Backend::new("site", "backend", 2);
    deliver(&mut backend, state(5, &[("retired", "gcp")]), None).unwrap();
    backend
        .read("inventory", vec!["measurements".into()])
        .unwrap();
    deliver(
        &mut backend,
        state(10, &[("new", "gcp")]),
        Some("inventory"),
    )
    .unwrap();
    assert!(matches!(
        deliver(&mut backend, state(8, &[("retired", "gcp")]), None).unwrap(),
        BackendEvent::Stale
    ));
    assert_eq!(meters(&backend), [("new", "gcp")]);
    assert_eq!(backend.meter_observed_at("new"), Some(10));
    assert_eq!(backend.meter_observed_at("retired"), None);
    deliver(
        &mut backend,
        state(11, &[("retired", "controllable")]),
        None,
    )
    .unwrap();
    assert_eq!(
        meters(&backend),
        [("new", "gcp"), ("retired", "controllable")]
    );
}

#[test]
fn telemetry_does_not_cancel_a_selective_read_of_another_property() {
    let mut backend = Backend::new("site", "backend", 2);
    backend
        .read("capabilities", vec!["supportedEebusUseCases".into()])
        .unwrap();
    deliver(&mut backend, state(80, &[("a", "gcp")]), None).unwrap();
    let response = State {
        timestamp: 75,
        supported_eebus_use_cases: Some(vec!["lpc".into()]),
        ..Default::default()
    };
    assert!(matches!(
        deliver(&mut backend, response, Some("capabilities")).unwrap(),
        BackendEvent::StateUpdated
    ));
    assert_eq!(backend.state().unwrap().timestamp, 80);
    assert_eq!(meters(&backend), [("a", "gcp")]);
}

#[test]
fn failed_admission_preserves_values_and_freshness_for_the_retry() {
    let mut backend = Backend::new("site", "backend", 1);
    deliver(&mut backend, state(1, &[("a", "gcp")]), None).unwrap();
    assert!(matches!(
        deliver(
            &mut backend,
            state(3, &[("a", "controllable"), ("b", "gcp")]),
            None
        ),
        Err(EngineError::Capacity)
    ));
    assert_eq!(meters(&backend), [("a", "gcp")]);
    deliver(&mut backend, state(2, &[("a", "controllable")]), None).unwrap();
    assert_eq!(meters(&backend), [("a", "controllable")]);
}
