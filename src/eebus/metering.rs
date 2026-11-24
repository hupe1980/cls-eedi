use std::collections::{BTreeMap, BTreeSet};

use ::eebus::model::{
    CmdData, CommodityType, ElectricalConnectionAcMeasurementType as AcKind,
    ElectricalConnectionMeasurandVariant as Variant,
    ElectricalConnectionParameterDescriptionData as Parameter,
    ElectricalConnectionPhaseName as Phase, ElectricalConnectionVoltageType as Voltage,
    EnergyDirection, MeasurementData, MeasurementDescriptionData, MeasurementType as Quantity,
    MeasurementValueState, MeasurementValueType, ScopeType as Scope, UnitOfMeasurement as Unit,
};

use crate::wire::{Measurement, Meter, Phases, ScaledNumber};

/// Maximum rows in each measurement, parameter or value list accepted by the adapter.
pub const MAX_METER_LIST_ROWS: usize = 1024;

/// The use case provisioned on this peer. An MPC meter represents a CLS.EEDI
/// controllable source directly only when it covers the entire configured fleet.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MeterProfile {
    /// Monitoring of Grid Connection Point.
    Mgcp,
    /// Monitoring of Power Consumption.
    Mpc,
}

/// Inconsistent configuration or metadata. No partial meter is returned on error.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum MeterError {
    #[error("expected a complete {0} list")]
    Function(&'static str),
    #[error("meter list exceeds the {MAX_METER_LIST_ROWS}-row limit")]
    Capacity,
    #[error("invalid meter identity")]
    Identity,
    #[error("a unique AC connection with positive consumption direction is required")]
    Connection,
    #[error("missing or duplicate {0} identifier")]
    Identifier(&'static str),
    #[error("measurement {0} has incomplete or incompatible metadata")]
    Metadata(u32),
    #[error("multiple measurements describe the same CLS.EEDI field")]
    AmbiguousField,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Line {
    A,
    B,
    C,
}

impl Line {
    fn select(phase: Option<&Phase>) -> Option<Self> {
        match phase {
            Some(Phase::A) => Some(Self::A),
            Some(Phase::B) => Some(Self::B),
            Some(Phase::C) => Some(Self::C),
            _ => None,
        }
    }
    fn insert(self, phases: &mut Phases, value: Measurement) {
        *match self {
            Self::A => &mut phases.a,
            Self::B => &mut phases.b,
            Self::C => &mut phases.c,
        } = Some(value);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Field {
    Power,
    PhasePower(Line),
    Consumed,
    Produced,
    Current(Line),
    Voltage(Line),
    Frequency,
}

impl Field {
    fn insert(self, meter: &mut Meter, value: Measurement) {
        match self {
            Self::Power => meter.power.get_or_insert_with(Default::default).total = Some(value),
            Self::PhasePower(line) => line.insert(
                meter
                    .power
                    .get_or_insert_with(Default::default)
                    .phases
                    .get_or_insert_with(Default::default),
                value,
            ),
            Self::Consumed => {
                meter.energy.get_or_insert_with(Default::default).consumed = Some(value)
            }
            Self::Produced => {
                meter.energy.get_or_insert_with(Default::default).produced = Some(value)
            }
            Self::Current(line) => {
                line.insert(meter.current.get_or_insert_with(Default::default), value)
            }
            Self::Voltage(line) => {
                let v = meter.voltage.get_or_insert_with(Default::default);
                *match line {
                    Line::A => &mut v.a_n,
                    Line::B => &mut v.b_n,
                    Line::C => &mut v.c_n,
                } = Some(value);
            }
            Self::Frequency => meter.frequency = Some(value),
        }
    }
    fn reading(self, sample: &MeasurementData) -> Option<Measurement> {
        if sample.value_type != Some(MeasurementValueType::Value)
            || !matches!(
                sample.value_state,
                None | Some(MeasurementValueState::Normal)
            )
        {
            return None;
        }
        let v = sample.value?;
        let number = v.number?.get();
        // MGCP §2.6.1 and MPC §2.5.1 apply the passive sign convention to
        // energy counters as well as power and current. Voltage is unsigned RMS.
        match self {
            Self::Produced if number > 0 => return None,
            Self::Consumed | Self::Voltage(_) | Self::Frequency if number < 0 => return None,
            _ => {}
        }
        Some(Measurement {
            value: ScaledNumber {
                number,
                scale: v.scale.map_or(0, |s| s.get()),
            },
        })
    }
}

/// Project complete SPINE snapshots for one electrical boundary into a fresh meter.
///
/// The caller authenticates the peer, correlates the four full reads to its paired
/// Measurement and ElectricalConnection features, and checks freshness. Supply
/// updated full snapshots after any partial notification or catalogue change.
/// The connection must use the load convention, with consumption positive.
/// Only MPC maps per-phase active power. Both profiles retain signed produced Wh.
/// Phase-to-neutral voltage requires an explicit neutral reference.
///
/// The released SPINE monitoring publisher omits supplementary AC measurement
/// type and variant. This projection accepts omissions for a recognized scope,
/// commodity, unit and AC connection, and checks explicit qualifiers. Unknown
/// scopes and unsupported channels such as line-to-line voltage are skipped.
/// Missing values, non-instantaneous value types, unknown or invalid quality, and
/// directionally invalid magnitudes yield absent readings. A meter with no
/// readings can replace an earlier observation to withdraw all its values.
///
/// ```
/// use cls_eedi::eebus::{meter_snapshot, MeterProfile};
/// use eebus::usecases::{mgcp, monitoring::{Measurand, MonitoredUnit}};
/// let mut unit = MonitoredUnit::new(1).naming(mgcp::NAMING);
/// unit.add(Measurand::total_power());
/// unit.set(&Measurand::total_power(), -450.);
/// let meter = meter_snapshot(MeterProfile::Mgcp, "house", None,
///     &unit.measurement_descriptions(), &unit.connection_description(),
///     &unit.parameter_descriptions(), &unit.measurements())?;
/// assert_eq!(meter.power.unwrap().total.unwrap().value.to_string(), "-450");
/// # Ok::<(), cls_eedi::eebus::MeterError>(())
/// ```
pub fn meter_snapshot(
    profile: MeterProfile,
    id: &str,
    id_type: Option<&str>,
    descriptions: &CmdData,
    connection: &CmdData,
    parameters: &CmdData,
    values: &CmdData,
) -> Result<Meter, MeterError> {
    if id.is_empty()
        || id.chars().nth(256).is_some()
        || id_type.is_some_and(|s| s.chars().nth(64).is_some())
    {
        return Err(MeterError::Identity);
    }
    let CmdData::MeasurementDescriptionListData(desc) = descriptions else {
        return Err(MeterError::Function("measurement descriptions"));
    };
    let CmdData::ElectricalConnectionDescriptionListData(conn) = connection else {
        return Err(MeterError::Function("electrical connections"));
    };
    let CmdData::ElectricalConnectionParameterDescriptionListData(params) = parameters else {
        return Err(MeterError::Function("connection parameters"));
    };
    let CmdData::MeasurementListData(samples) = values else {
        return Err(MeterError::Function("measurement values"));
    };
    let descriptions = desc
        .measurement_description_data
        .as_deref()
        .unwrap_or_default();
    let parameters = params
        .electrical_connection_parameter_description_data
        .as_deref()
        .unwrap_or_default();
    let samples = samples.measurement_data.as_deref().unwrap_or_default();
    if [descriptions.len(), parameters.len(), samples.len()]
        .into_iter()
        .any(|n| n > MAX_METER_LIST_ROWS)
    {
        return Err(MeterError::Capacity);
    }
    // SPINE encodes empty complete lists as an empty container; the caller
    // distinguishes them from partial reads and filtered notifications.
    let [connection] = conn
        .electrical_connection_description_data
        .as_deref()
        .unwrap_or_default()
    else {
        return Err(MeterError::Connection);
    };
    let connection_id = connection
        .electrical_connection_id
        .ok_or(MeterError::Connection)?;
    if connection.power_supply_type != Some(Voltage::Ac)
        || connection.positive_energy_direction != Some(EnergyDirection::Consume)
        || connection
            .ac_connected_phases
            .is_some_and(|n| !(1..=3).contains(&n))
    {
        return Err(MeterError::Connection);
    }

    let mut parameters_by_measurement = BTreeMap::new();
    let mut parameter_ids = BTreeSet::new();
    for p in parameters {
        if p.electrical_connection_id != Some(connection_id) {
            return Err(MeterError::Connection);
        }
        if !parameter_ids.insert(p.parameter_id.ok_or(MeterError::Identifier("parameter"))?) {
            return Err(MeterError::Identifier("parameter"));
        }
        if let Some(mid) = p.measurement_id {
            if parameters_by_measurement.insert(mid, p).is_some() {
                return Err(MeterError::Identifier("parameter measurement"));
            }
        }
    }
    let mut paths = BTreeMap::new();
    let mut assigned = BTreeSet::new();
    for d in descriptions {
        let mid = d
            .measurement_id
            .ok_or(MeterError::Identifier("description"))?;
        let path = field(
            profile,
            d,
            parameters_by_measurement.get(&mid).copied(),
            mid.get(),
            connection.ac_connected_phases,
        )?;
        if paths.insert(mid, path).is_some() {
            return Err(MeterError::Identifier("description"));
        }
        if path.is_some_and(|p| !assigned.insert(p)) {
            return Err(MeterError::AmbiguousField);
        }
    }
    if let Some(orphan) = parameters_by_measurement
        .keys()
        .find(|mid| !paths.contains_key(mid))
    {
        return Err(MeterError::Metadata(orphan.get()));
    }
    let mut meter = Meter {
        id: id.to_owned(),
        id_type: id_type.map(str::to_owned),
        source: match profile {
            MeterProfile::Mgcp => "gcp",
            MeterProfile::Mpc => "controllable",
        }
        .into(),
        ..Default::default()
    };
    let mut seen = BTreeSet::new();
    for sample in samples {
        let mid = sample
            .measurement_id
            .ok_or(MeterError::Identifier("value"))?;
        if !seen.insert(mid) {
            return Err(MeterError::Identifier("value"));
        }
        let Some(path) = paths.get(&mid) else {
            return Err(MeterError::Metadata(mid.get()));
        };
        if let Some(path) = path {
            if let Some(value) = path.reading(sample) {
                path.insert(&mut meter, value);
            }
        }
    }
    Ok(meter)
}

fn phase_count(p: &Phase) -> Option<u32> {
    match p {
        Phase::A | Phase::B | Phase::C => Some(1),
        Phase::Ab | Phase::Bc | Phase::Ac => Some(2),
        Phase::Abc => Some(3),
        _ => None,
    }
}

fn field(
    profile: MeterProfile,
    d: &MeasurementDescriptionData,
    parameter: Option<&Parameter>,
    mid: u32,
    connected: Option<u32>,
) -> Result<Option<Field>, MeterError> {
    let error = MeterError::Metadata(mid);
    let (kind, unit) = match (&d.scope_type, profile) {
        (Some(Scope::AcPowerTotal), _) | (Some(Scope::AcPower), MeterProfile::Mpc) => {
            (Quantity::Power, Unit::W)
        }
        (Some(Scope::GridFeedIn | Scope::GridConsumption), MeterProfile::Mgcp)
        | (Some(Scope::AcEnergyProduced | Scope::AcEnergyConsumed), MeterProfile::Mpc) => {
            (Quantity::Energy, Unit::Wh)
        }
        (Some(Scope::AcCurrent), _) => (Quantity::Current, Unit::A),
        (Some(Scope::AcVoltage), _) => (Quantity::Voltage, Unit::V),
        (Some(Scope::AcFrequency), _) => (Quantity::Frequency, Unit::Hz),
        _ => return Ok(None),
    };
    let p = parameter.ok_or(MeterError::Metadata(mid))?;
    if d.commodity_type != Some(CommodityType::Electricity)
        || d.measurement_type.as_ref() != Some(&kind)
        || d.unit != Some(unit)
        || p.voltage_type != Some(Voltage::Ac)
        || p.scope_type
            .as_ref()
            .is_some_and(|s| Some(s) != d.scope_type.as_ref())
    {
        return Err(error);
    }
    let expected_ac = match kind {
        Quantity::Voltage => Some(AcKind::Apparent),
        Quantity::Frequency => None,
        _ => Some(AcKind::Real),
    };
    if p.ac_measurement_type
        .as_ref()
        .is_some_and(|t| Some(t) != expected_ac.as_ref())
        || p.ac_measurement_variant
            .as_ref()
            .is_some_and(|v| v != &Variant::Rms)
        || p.ac_measured_harmonic.is_some()
    {
        return Err(error);
    }
    let phase = p.ac_measured_phases.as_ref();
    let line = Line::select(phase);
    let reference = p.ac_measured_in_reference_to.as_ref();
    if kind != Quantity::Voltage && reference.is_some_and(|r| r != &Phase::Neutral) {
        return Err(error);
    }
    let path = match d.scope_type.as_ref() {
        Some(Scope::AcPowerTotal) => {
            if phase
                .is_some_and(|p| phase_count(p).is_none_or(|n| connected.is_some_and(|c| c != n)))
            {
                return Err(error);
            }
            Some(Field::Power)
        }
        Some(Scope::AcPower) => Some(Field::PhasePower(line.ok_or(error)?)),
        Some(Scope::AcCurrent) => Some(Field::Current(line.ok_or(error)?)),
        Some(Scope::AcVoltage) => {
            if reference == Some(&Phase::Neutral) {
                line.map(Field::Voltage)
            } else {
                None
            }
        }
        Some(
            Scope::AcFrequency
            | Scope::AcEnergyConsumed
            | Scope::GridConsumption
            | Scope::AcEnergyProduced
            | Scope::GridFeedIn,
        ) => {
            if phase.is_some_and(|p| {
                phase_count(p).is_none_or(|n| {
                    if let Some(connected) = connected {
                        n != connected
                    } else {
                        p != &Phase::Abc
                    }
                })
            }) {
                return Err(error);
            }
            Some(match d.scope_type {
                Some(Scope::AcFrequency) => Field::Frequency,
                Some(Scope::AcEnergyConsumed | Scope::GridConsumption) => Field::Consumed,
                _ => Field::Produced,
            })
        }
        _ => None,
    };
    Ok(path)
}
