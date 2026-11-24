//! Integrate CLS.EEDI controls and metering with an authenticated EEBUS peer.
//!
//! Use [`meter_snapshot`] to project complete MGCP or MPC reads. Use
//! [`crate::metering::controllable_total_power`] to combine signed watt readings
//! from the provisioned fleet. [`LimitAdapter`] handles LPC and LPP writes.
//!
//! Establish the binding and drain earlier session events. Read the complete limit
//! descriptions and current limit list. The host correlates each incoming reply
//! by peer endpoints and read counter, leaving both reply events on the guard
//! engine for [`LimitAdapter::new`]. Forward the prepared datagram promptly. The SPINE time
//! period starts at equipment receipt. Retain unrelated incoming traffic for the
//! application and deliver only this session's responses here. After receiving
//! completion, reclaim the engine with `into_engine` and process buffered traffic
//! (including discovery and binding changes) before constructing the next adapter.
//!
//! ```
//! use cls_eedi::eebus::{AdapterError, LimitAdapter};
//! use cls_eedi::engine::Clock;
//! use eebus::model::Datagram;
//!
//! fn exchange(adapter: &mut LimitAdapter<'_>, id: &str, now: Clock,
//!             transport: impl FnOnce(Datagram) -> Datagram) -> Result<(), AdapterError> {
//!     adapter.dispatch(id, now)?;
//!     let packet = adapter.poll_transmit().expect("dispatched write");
//!     let response = transport(packet);
//!     let completion = adapter.handle_datagram(&response, now)?;
//!     // Persist completed state before publishing completion.acknowledgement.
//!     assert!(completion.is_some());
//!     Ok(())
//! }
//! ```

use std::time::Duration;

use ::eebus::codec::ElementTag;
use ::eebus::model::*;
use ::eebus::spine::{Engine, SpineEvent, DEFAULT_MAX_RESPONSE_DELAY};
use ::eebus::usecases::limitation::{resolve_writable_limit, Direction, LimitResolutionError};

use crate::engine::{Clock, EngineError, LocalDevice, Outcome};
use crate::wire::{Control, Limit, Message};

/// Reason a write or a session handoff could not proceed.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("fully qualified, distinct peer endpoints and a local client feature are required")]
    Endpoint,
    #[error("drain outstanding operations, datagrams and unrelated events before handing over the SPINE engine")]
    SessionBusy,
    #[error("the local client does not hold the required remote binding")]
    Binding,
    #[error("two fresh full limit-list replies from the selected feature are required")]
    Snapshot,
    #[error(transparent)]
    Limit(#[from] LimitResolutionError),
    #[error("one directional limit is required")]
    UnsupportedControl,
    #[error("a SPINE write is outstanding; reconcile the local operation if the session closes")]
    Outstanding,
    #[error("this exchange completed; return the engine and process session updates")]
    Completed,
    #[error("the queued limit expired before dispatch; complete it as rejected")]
    Expired,
    #[error(transparent)]
    Local(#[from] EngineError),
}

/// A verified result and the corresponding local protocol transition.
pub struct Completion {
    pub request_id: String,
    /// Absent for related controls.
    pub acknowledgement: Option<Message>,
    /// Original SPINE result for diagnostics.
    pub error: ErrorNumber,
}

struct Pending {
    id: String,
    counter: MsgCounter,
    sent: bool,
    result: Option<ErrorNumber>,
}

/// Exclusively owns the guard engine while a limit may be retried by that engine.
/// Drop on timeout or disconnect. This discards queued frames and retransmission
/// timers. The local journal retains the consumed dispatch for reconciliation.
pub struct LimitAdapter<'a> {
    local: &'a mut LocalDevice,
    engine: Engine,
    client: FeatureAddress,
    server: FeatureAddress,
    descriptions: LoadControlLimitDescriptionListData,
    current: LoadControlLimitListData,
    pending: Option<Pending>,
    completed: bool,
}

impl<'a> LimitAdapter<'a> {
    /// Bind a SPINE engine holding exactly two complete LoadControl read replies
    /// (descriptions and current limits) to one authenticated session and route. On
    /// construction failure the supplied SPINE engine is discarded.
    pub fn new(
        local: &'a mut LocalDevice,
        mut engine: Engine,
        client: FeatureAddress,
        server: FeatureAddress,
    ) -> Result<Self, AdapterError> {
        if client.device.as_ref() != Some(engine.device().address())
            || server.device.is_none()
            || server.device == client.device
            || client.entity.as_ref().is_none_or(Vec::is_empty)
            || server.entity.as_ref().is_none_or(Vec::is_empty)
            || server.feature.is_none()
            || !engine
                .device()
                .resolve(&client)
                .is_some_and(|f| f.role() == Role::Client)
        {
            return Err(AdapterError::Endpoint);
        }
        if engine.poll_timeout().is_some() || engine.poll_transmit().is_some() {
            return Err(AdapterError::SessionBusy);
        }
        let mut descriptions = None;
        let mut current = None;
        while let Some(event) = engine.poll_event() {
            let SpineEvent::ReplyReceived {
                feature,
                data,
                is_partial,
                ..
            } = event
            else {
                return Err(AdapterError::SessionBusy);
            };
            if feature != server || is_partial {
                return Err(AdapterError::Snapshot);
            }
            match data {
                CmdData::LoadControlLimitDescriptionListData(list) if descriptions.is_none() => {
                    descriptions = Some(list)
                }
                CmdData::LoadControlLimitListData(list) if current.is_none() => {
                    current = Some(list)
                }
                _ => return Err(AdapterError::Snapshot),
            }
        }
        Ok(Self {
            local,
            engine,
            client,
            server,
            descriptions: descriptions.ok_or(AdapterError::Snapshot)?,
            current: current.ok_or(AdapterError::Snapshot)?,
            pending: None,
            completed: false,
        })
    }

    /// Queue one authorized write with its current remaining duration. Discovery
    /// and binding errors leave the one-shot dispatch authorization available.
    pub fn dispatch(&mut self, request_id: &str, now: Clock) -> Result<MsgCounter, AdapterError> {
        if self.completed {
            return Err(AdapterError::Completed);
        }
        if self.pending.is_some() {
            return Err(AdapterError::Outstanding);
        }
        if !self
            .engine
            .relations()
            .holds_binding(&self.client, &self.server)
        {
            return Err(AdapterError::Binding);
        }
        let (_, control) = self
            .local
            .pending()
            .find(|(id, _)| *id == request_id)
            .ok_or(EngineError::Unexpected)?;
        let (direction, _) = directional(control)?;
        let limit_id = resolve_writable_limit(&self.descriptions, &self.current, direction)?;
        let elapsed = Duration::from_secs(now.elapsed_seconds);
        let delay = self
            .server
            .device
            .as_ref()
            .and_then(|d| self.engine.peer(d))
            .and_then(|p| p.feature_at(&self.server))
            .and_then(|f| f.max_response_delay)
            .unwrap_or(DEFAULT_MAX_RESPONSE_DELAY);
        elapsed.checked_add(delay).ok_or(EngineError::Clock)?;
        let control = self
            .local
            .control_for_dispatch(request_id, now)?
            .ok_or(AdapterError::Expired)?;
        let (_, limit) = directional(&control)?;
        let (data, filters) = write(limit_id, direction, limit);
        let counter =
            self.engine
                .write_filtered(&self.server, &self.client, data, filters, elapsed);
        self.pending = Some(Pending {
            id: request_id.into(),
            counter,
            sent: false,
            result: None,
        });
        Ok(counter)
    }

    /// Take the write for immediate delivery on this authenticated session.
    pub fn poll_transmit(&mut self) -> Option<Datagram> {
        let frame = self.engine.poll_transmit()?;
        self.pending.as_mut()?.sent = true;
        Some(frame)
    }

    /// First SPINE response deadline in the elapsed clock domain. On timeout,
    /// discard this adapter, then reconcile the outstanding CLS.EEDI operation.
    pub fn deadline(&self) -> Option<Duration> {
        self.engine.poll_timeout()
    }

    /// Accept a single result delivered by the authenticated, directly connected
    /// peer. Unrelated frames return `None`. A device component omitted in simple
    /// addressing resolves to its authenticated endpoint (SPINE §5.2.2.1).
    pub fn handle_datagram(
        &mut self,
        packet: &Datagram,
        now: Clock,
    ) -> Result<Option<Completion>, AdapterError> {
        let Some(pending) = &mut self.pending else {
            return Ok(None);
        };
        if !pending.sent || pending.result.is_some() {
            return Ok(None);
        }
        let Some(header) = &packet.header else {
            return Ok(None);
        };
        let Some(error) = result_code(packet) else {
            return Ok(None);
        };
        if header.cmd_classifier != Some(CmdClassifier::Result)
            || header.msg_counter.is_none()
            || header
                .address_originator
                .as_ref()
                .is_some_and(|origin| !matches_endpoint(Some(origin), &self.server))
            || header.msg_counter_reference != Some(pending.counter)
            || !matches_endpoint(header.address_source.as_ref(), &self.server)
            || !matches_endpoint(header.address_destination.as_ref(), &self.client)
        {
            return Ok(None);
        }
        let mut normalized;
        let frame = if header.address_source.as_ref() == Some(&self.server)
            && header.address_destination.as_ref() == Some(&self.client)
        {
            packet
        } else {
            normalized = packet.clone();
            let header = normalized.header.as_mut().expect("validated header");
            header.address_source = Some(self.server.clone());
            header.address_destination = Some(self.client.clone());
            &normalized
        };
        self.engine
            .handle_datagram(frame, Duration::from_secs(now.elapsed_seconds));
        if !matches!(self.engine.poll_event(), Some(SpineEvent::ResultReceived {request, error: observed}) if request == pending.counter && observed == error)
        {
            return Ok(None);
        }
        pending.result = Some(error);
        self.finish(now)
    }

    /// Retry the local journal transition if, for example, recording a definitive
    /// equipment result failed due to an invalid local clock.
    pub fn finish(&mut self, now: Clock) -> Result<Option<Completion>, AdapterError> {
        let Some((pending, error)) = self
            .pending
            .as_ref()
            .and_then(|p| p.result.map(|error| (p, error)))
        else {
            return Ok(None);
        };
        let outcome = match error {
            ErrorNumber::None => Outcome::Accepted,
            ErrorNumber::DestinationUnknown
            | ErrorNumber::DestinationUnreachable
            | ErrorNumber::CommandNotSupported => Outcome::Unsupported,
            _ => Outcome::Rejected,
        };
        let acknowledgement = self.local.complete(&pending.id, outcome, now)?;
        let request_id = self.pending.take().expect("verified pending result").id;
        self.completed = true;
        Ok(Some(Completion {
            request_id,
            acknowledgement,
            error,
        }))
    }

    /// Recover a clean engine. An unresolved write destroys the engine, including
    /// its SPINE retry buffer, and remains pending on the local device.
    pub fn into_engine(self) -> Result<Engine, AdapterError> {
        if self.pending.is_some() {
            return Err(AdapterError::Outstanding);
        }
        Ok(self.engine)
    }
}

fn directional(control: &Control) -> Result<(Direction, &Limit), AdapterError> {
    if control.trust.is_some() || control.failsafes.is_some() || control.notify.is_some() {
        return Err(AdapterError::UnsupportedControl);
    }
    let power = control
        .limits
        .as_ref()
        .ok_or(AdapterError::UnsupportedControl)?;
    match (&power.consumption, &power.production) {
        (Some(limit), None) => Ok((Direction::Consumption, limit)),
        (None, Some(limit)) => Ok((Direction::Production, limit)),
        _ => Err(AdapterError::UnsupportedControl),
    }
}

fn write(id: LoadControlLimitId, direction: Direction, limit: &Limit) -> (CmdData, Vec<Filter>) {
    let duration = limit.duration;
    let mut filters = Vec::new();
    if duration.is_none() {
        // LPC §3.4.1.4: partial writes preserve omitted fields. Withdraw this
        // limit's previous end time atomically before applying an open-ended value.
        filters.push(
            Filter::delete()
                .select(FilterSelectors::LoadControlLimitListDataSelectors(
                    LoadControlLimitListDataSelectors { limit_id: Some(id) },
                ))
                .covering(FilterElements::LoadControlLimitDataElements(
                    LoadControlLimitDataElements {
                        time_period: Some(TimePeriodElements {
                            end_time: Some(ElementTag),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
        );
    }
    filters.push(Filter::partial());
    // LPP §2.8.1 uses non-positive production watts. The wire validator
    // requires a nonnegative CLS.EEDI magnitude, so negation is representable.
    let watts = if direction == Direction::Production {
        -limit.value
    } else {
        limit.value
    };
    let data = LoadControlLimitData {
        limit_id: Some(id),
        is_limit_active: Some(limit.active),
        value: Some(ScaledNumber::whole(watts)),
        time_period: duration.map(|s| TimePeriod {
            end_time: Some(format_iso8601_duration(Duration::from_secs(s.into())).into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    (
        CmdData::LoadControlLimitListData(LoadControlLimitListData {
            load_control_limit_data: Some(vec![data]),
        }),
        filters,
    )
}

fn matches_endpoint(actual: Option<&FeatureAddress>, expected: &FeatureAddress) -> bool {
    actual.is_some_and(|a| {
        a.entity == expected.entity
            && a.feature == expected.feature
            && (a.device.is_none() || a.device == expected.device)
    })
}

fn result_code(packet: &Datagram) -> Option<ErrorNumber> {
    let [cmd] = packet.payload.as_ref()?.cmd.as_deref()? else {
        return None;
    };
    if cmd
        .function
        .as_ref()
        .is_some_and(|f| f != &Function::ResultData)
        || cmd.filter.is_some()
    {
        return None;
    }
    let CmdData::ResultData(result) = cmd.data.as_ref()? else {
        return None;
    };
    result.error_number
}

mod metering;
pub use metering::{meter_snapshot, MeterError, MeterProfile, MAX_METER_LIST_ROWS};

#[cfg(feature = "eebus-pairing")]
mod pairing;
#[cfg(feature = "eebus-pairing")]
pub use pairing::confirm_pairing;
