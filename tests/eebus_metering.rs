#![cfg(feature = "eebus")]

use cls_eedi::{
    eebus::{meter_snapshot, MeterError, MeterProfile},
    wire::{Meter, ScaledNumber},
};
use eebus::{
    model::{
        self, CmdData, ElectricalConnectionPhaseName as Phase, MeasurementValueState, ScopeType,
        UnitOfMeasurement,
    },
    usecases::{
        mgcp,
        monitoring::{Measurand, MonitoredUnit, Quantity},
        mpc,
    },
};

struct Snapshot {
    descriptions: CmdData,
    connection: CmdData,
    parameters: CmdData,
    values: CmdData,
}
impl Snapshot {
    fn new(grid: bool, quantities: &[Measurand]) -> Self {
        let mut peer = MonitoredUnit::new(83).naming(if grid { mgcp::NAMING } else { mpc::NAMING });
        for (i, m) in quantities.iter().enumerate() {
            peer.add(m.clone());
            peer.set(
                m,
                if m.quantity == Quantity::EnergyProduced {
                    -17.
                } else {
                    (i + 1) as f64
                },
            );
        }
        Self {
            descriptions: peer.measurement_descriptions(),
            connection: peer.connection_description(),
            parameters: peer.parameter_descriptions(),
            values: peer.measurements(),
        }
    }
    fn parameters(&mut self) -> &mut Vec<model::ElectricalConnectionParameterDescriptionData> {
        let CmdData::ElectricalConnectionParameterDescriptionListData(l) = &mut self.parameters
        else {
            unreachable!()
        };
        l.electrical_connection_parameter_description_data
            .as_mut()
            .unwrap()
    }
    fn descriptions(&mut self) -> &mut Vec<model::MeasurementDescriptionData> {
        let CmdData::MeasurementDescriptionListData(l) = &mut self.descriptions else {
            unreachable!()
        };
        l.measurement_description_data.as_mut().unwrap()
    }
    fn values(&mut self) -> &mut Vec<model::MeasurementData> {
        let CmdData::MeasurementListData(l) = &mut self.values else {
            unreachable!()
        };
        l.measurement_data.as_mut().unwrap()
    }
    fn map(&self, p: MeterProfile) -> Result<Meter, MeterError> {
        meter_snapshot(
            p,
            "meter",
            Some("vendor"),
            &self.descriptions,
            &self.connection,
            &self.parameters,
            &self.values,
        )
    }
}

#[test]
fn all_thirteen_paths_use_scope_unit_and_phase_instead_of_row_position() {
    use Quantity::*;
    let mut channels = vec![
        Measurand::total(Power),
        Measurand::total(EnergyConsumed),
        Measurand::total(EnergyProduced),
        Measurand::total(Frequency),
    ];
    for phase in [Phase::A, Phase::B, Phase::C] {
        for q in [Power, Current, Voltage] {
            channels.push(Measurand::on(q, phase.clone()));
        }
    }
    for (grid, profile) in [(true, MeterProfile::Mgcp), (false, MeterProfile::Mpc)] {
        let mut data = Snapshot::new(grid, &channels);
        for p in data.parameters() {
            if p.scope_type == Some(ScopeType::AcVoltage) {
                p.ac_measured_in_reference_to = Some(Phase::Neutral);
            }
        }
        data.descriptions().reverse();
        data.parameters().rotate_left(5);
        data.values().reverse();
        let meter = data.map(profile).unwrap();
        assert_eq!(meter.id_type.as_deref(), Some("vendor"));
        assert_eq!(meter.source, if grid { "gcp" } else { "controllable" });
        let p = meter.power.unwrap();
        assert_eq!(p.total.unwrap().value.to_string(), "1");
        if grid {
            assert!(p.phases.is_none());
        } else {
            let m = p.phases.unwrap();
            for (v, n) in [(m.a, 5), (m.b, 8), (m.c, 11)] {
                assert_eq!(v.unwrap().value.to_string(), n.to_string());
            }
        }
        let e = meter.energy.unwrap();
        assert_eq!(e.consumed.unwrap().value.to_string(), "2");
        assert_eq!(e.produced.unwrap().value.to_string(), "-17");
        let c = meter.current.unwrap();
        let v = meter.voltage.unwrap();
        for (m, n) in [
            (c.a, 6),
            (c.b, 9),
            (c.c, 12),
            (v.a_n, 7),
            (v.b_n, 10),
            (v.c_n, 13),
            (meter.frequency, 4),
        ] {
            assert_eq!(m.unwrap().value.to_string(), n.to_string());
        }
    }
}

#[test]
fn preserves_extreme_wire_values_and_measured_zero_without_floats() {
    let mut data = Snapshot::new(true, &[Measurand::total_power()]);
    for (n, s) in [
        (i64::MIN, Some(i16::MIN)),
        (i64::MAX, Some(i16::MAX)),
        (0, Some(-7)),
        (341, None),
    ] {
        data.values()[0].value = Some(model::ScaledNumber {
            number: Some(model::Number(n)),
            scale: s.map(model::Scale),
        });
        let v = data
            .map(MeterProfile::Mgcp)
            .unwrap()
            .power
            .unwrap()
            .total
            .unwrap()
            .value;
        assert_eq!(
            v,
            ScaledNumber {
                number: n,
                scale: s.unwrap_or(0)
            }
        );
    }
}

#[test]
fn invalid_unknown_or_absent_samples_withdraw_only_the_affected_reading() {
    let mut data = Snapshot::new(
        false,
        &[
            Measurand::total_power(),
            Measurand::total(Quantity::EnergyProduced),
        ],
    );
    let original = data.values()[0].clone();
    for case in 0..7 {
        let mut changed = original.clone();
        match case {
            0 => changed.value_state = Some(MeasurementValueState::Error),
            1 => changed.value_state = Some(MeasurementValueState::OutOfRange),
            2 => changed.value_state = Some(MeasurementValueState::Other("_i:1_unknown".into())),
            3 => changed.value = None,
            4 => changed.value.as_mut().unwrap().number = None,
            5 => changed.value_type = Some(model::MeasurementValueType::AverageValue),
            _ => changed.value_type = None,
        }
        data.values()[0] = changed;
        let meter = data.map(MeterProfile::Mpc).unwrap();
        assert!(meter.power.is_none());
        assert_eq!(
            meter.energy.unwrap().produced.unwrap().value.to_string(),
            "-17"
        );
    }
    data.values().remove(0);
    data.values()[0].value = Some(model::ScaledNumber::whole(17));
    let empty = data.map(MeterProfile::Mpc).unwrap();
    assert!(empty.energy.is_none() && empty.power.is_none());
}

#[test]
fn inconsistent_units_directions_and_reused_ids_cannot_publish_a_partial_meter() {
    for case in 0..10 {
        let mut data = Snapshot::new(
            false,
            &[
                Measurand::total_power(),
                Measurand::total(Quantity::EnergyConsumed),
            ],
        );
        match case {
            0 => data.descriptions()[0].unit = Some(UnitOfMeasurement::Wh),
            1 => data.descriptions()[0].measurement_type = Some(model::MeasurementType::Energy),
            2 => data.parameters()[0].scope_type = Some(ScopeType::AcCurrent),
            3 => {
                data.parameters()[0].ac_measurement_type =
                    Some(model::ElectricalConnectionAcMeasurementType::Reactive)
            }
            4 => {
                let other = data.descriptions()[0].clone();
                data.descriptions().push(other);
            }
            5 => {
                let other = data.values()[0].clone();
                data.values().push(other);
            }
            6 => {
                let other = data.parameters()[0].clone();
                data.parameters().push(other);
            }
            7 => {
                let id = data.parameters()[0].measurement_id;
                data.parameters()[1].measurement_id = id;
            }
            8 => {
                let CmdData::ElectricalConnectionDescriptionListData(c) = &mut data.connection
                else {
                    unreachable!()
                };
                c.electrical_connection_description_data.as_mut().unwrap()[0]
                    .positive_energy_direction = Some(model::EnergyDirection::Produce);
            }
            _ => data.values()[0].measurement_id = Some(model::MeasurementId(999)),
        }
        assert!(data.map(MeterProfile::Mpc).is_err(), "case {case}");
    }
    let mut duplicate = Snapshot::new(
        false,
        &[
            Measurand::total_power(),
            Measurand::total(Quantity::EnergyConsumed),
        ],
    );
    let id = duplicate.descriptions()[1].measurement_id;
    let mut second = duplicate.descriptions()[0].clone();
    second.measurement_id = id;
    duplicate.descriptions()[1] = second;
    duplicate.parameters()[1].scope_type = Some(ScopeType::AcPowerTotal);
    assert_eq!(
        duplicate.map(MeterProfile::Mpc),
        Err(MeterError::AmbiguousField)
    );
}

#[test]
fn neutral_is_explicit_and_other_profiles_energy_never_maps() {
    let mut data = Snapshot::new(
        true,
        &[
            Measurand::on(Quantity::Voltage, Phase::A),
            Measurand::total(Quantity::EnergyProduced),
        ],
    );
    for reference in [None, Some(Phase::B), Some(Phase::Ground)] {
        data.parameters()[0].ac_measured_in_reference_to = reference;
        let projected = data.map(MeterProfile::Mpc).unwrap();
        assert!(projected.energy.is_none() && projected.voltage.is_none());
    }
    data.parameters()[0].ac_measured_in_reference_to = Some(Phase::Neutral);
    let m = data.map(MeterProfile::Mgcp).unwrap();
    assert_eq!(m.voltage.unwrap().a_n.unwrap().value.to_string(), "1");
    assert_eq!(m.energy.unwrap().produced.unwrap().value.to_string(), "-17");
}

#[test]
fn one_ac_boundary_and_four_correct_function_types_are_required() {
    let mut data = Snapshot::new(false, &[Measurand::total_power()]);
    assert!(matches!(
        meter_snapshot(
            MeterProfile::Mpc,
            "m",
            None,
            &data.values,
            &data.connection,
            &data.parameters,
            &data.values
        ),
        Err(MeterError::Function(_))
    ));
    assert!(matches!(
        meter_snapshot(
            MeterProfile::Mpc,
            &"界".repeat(257),
            None,
            &data.descriptions,
            &data.connection,
            &data.parameters,
            &data.values
        ),
        Err(MeterError::Identity)
    ));
    let CmdData::ElectricalConnectionDescriptionListData(list) = &mut data.connection else {
        unreachable!()
    };
    let entries = list
        .electrical_connection_description_data
        .as_mut()
        .unwrap();
    let other = entries[0].clone();
    entries.push(other);
    assert_eq!(data.map(MeterProfile::Mpc), Err(MeterError::Connection));
}

#[test]
fn total_power_can_cover_two_phases_of_a_two_phase_connection() {
    let mut data = Snapshot::new(false, &[Measurand::total_power()]);
    let CmdData::ElectricalConnectionDescriptionListData(list) = &mut data.connection else {
        unreachable!()
    };
    list.electrical_connection_description_data
        .as_mut()
        .unwrap()[0]
        .ac_connected_phases = Some(2);
    data.parameters()[0].ac_measured_phases = Some(Phase::Ab);
    assert!(data
        .map(MeterProfile::Mpc)
        .unwrap()
        .power
        .unwrap()
        .total
        .is_some());
    data.parameters()[0].ac_measured_phases = Some(Phase::A);
    assert!(matches!(
        data.map(MeterProfile::Mpc),
        Err(MeterError::Metadata(_))
    ));
}

#[test]
fn excessive_catalogues_are_rejected_before_indexing() {
    let count = cls_eedi::eebus::MAX_METER_LIST_ROWS + 1;
    for index in 0..3 {
        let mut data = Snapshot::new(true, &[Measurand::total_power()]);
        match index {
            0 => data.descriptions().resize_with(count, Default::default),
            1 => data.parameters().resize_with(count, Default::default),
            _ => data.values().resize_with(count, Default::default),
        }
        assert_eq!(data.map(MeterProfile::Mgcp), Err(MeterError::Capacity));
    }
}
