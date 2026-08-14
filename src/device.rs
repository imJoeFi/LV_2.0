use crate::link::{open_serial, Link, TraceSink};
use crate::protocol::{
    AdapterMessage, DisplayCharacterSet, DisplayDimensions, DisplayTime, ItemNumber, Level1Amount,
    ReaderCommand, VmcEvent,
};
use std::error::Error;
use std::fmt;
use std::future::pending;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{sleep, sleep_until, Instant};
use uuid::Uuid;

const ADAPTER_STARTUP_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_APPLICATION_RESPONSE_TIME: Duration = Duration::from_secs(7);
const ACTOR_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Serial and timing configuration for the PC-to-MDB adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdbConfig {
    port: PathBuf,
    baud: u32,
    application_response_time: Duration,
    display_fallback: Option<(DisplayDimensions, DisplayCharacterSet)>,
}

impl MdbConfig {
    /// Creates settings for a WAFER-style adapter.
    ///
    /// The default seven-second decision deadline matches the reader
    /// configuration captured from the current adapter. The adapter's stored
    /// MDB configuration remains authoritative and must be provisioned to the
    /// same value if this deadline is changed.
    pub fn new(port: PathBuf, baud: u32) -> Self {
        Self {
            port,
            baud,
            application_response_time: DEFAULT_APPLICATION_RESPONSE_TIME,
            display_fallback: None,
        }
    }

    #[must_use]
    /// Changes the actor's vend-decision deadline.
    ///
    /// This does not reprogram the adapter's stored MDB reader configuration.
    pub fn with_application_response_time(mut self, timeout: Duration) -> Self {
        self.application_response_time = timeout;
        self
    }

    /// Supplies display capabilities when the adapter completed MDB setup
    /// before this process connected.
    ///
    /// A later `SETUP/CONFIGURATION` message from the VMC overrides this hint.
    #[must_use]
    pub fn with_display_fallback(
        mut self,
        dimensions: DisplayDimensions,
        character_set: DisplayCharacterSet,
    ) -> Self {
        self.display_fallback = Some((dimensions, character_set));
        self
    }

    pub fn port(&self) -> &Path {
        &self.port
    }

    pub const fn baud(&self) -> u32 {
        self.baud
    }

    pub const fn application_response_time(&self) -> Duration {
        self.application_response_time
    }

    pub const fn display_fallback(&self) -> Option<(DisplayDimensions, DisplayCharacterSet)> {
        self.display_fallback
    }
}

/// Funds reported in the reader's Level 1 `BEGIN SESSION` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFunds {
    MachineMaximum,
    Known(Level1Amount),
    Unknown,
}

impl SessionFunds {
    fn resolve(self, machine_maximum: Option<Level1Amount>) -> Result<u16, MdbError> {
        match self {
            Self::MachineMaximum => machine_maximum
                .map(Level1Amount::raw)
                .ok_or(MdbError::MachineMaximumUnknown),
            Self::Known(amount) => Ok(amount.raw()),
            Self::Unknown => Ok(u16::MAX),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceStatus {
    Inactive,
    Disabled,
    Enabled,
    SessionActive,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    StatusChanged(DeviceStatus),
    Reinitialized,
    CashSale {
        price: Level1Amount,
        item: ItemNumber,
    },
    Fault(String),
}

pub struct DeviceEvents {
    receiver: broadcast::Receiver<DeviceEvent>,
}

impl DeviceEvents {
    pub async fn next_event(&mut self) -> Result<DeviceEvent, MdbError> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Ok(event),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(MdbError::Disconnected);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdbError {
    Transport(String),
    Disconnected,
    InvalidState {
        operation: &'static str,
        state: &'static str,
    },
    MachineMaximumUnknown,
    DisplayConfigurationUnknown,
    DisplayUnavailable,
    UnsupportedDisplayCharacterSet(u8),
    DisplayMessageTooLong {
        length: usize,
        capacity: usize,
    },
    UnsupportedDisplayCharacter(char),
}

impl fmt::Display for MdbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => formatter.write_str(message),
            Self::Disconnected => formatter.write_str("MDB actor disconnected"),
            Self::InvalidState { operation, state } => {
                write!(
                    formatter,
                    "cannot {operation} while the MDB device is {state}"
                )
            }
            Self::MachineMaximumUnknown => {
                formatter.write_str("the VMC has not supplied its maximum price")
            }
            Self::DisplayConfigurationUnknown => formatter.write_str(
                "the VMC display configuration was not observed and no fallback was configured",
            ),
            Self::DisplayUnavailable => {
                formatter.write_str("the VMC reports no MDB-accessible display")
            }
            Self::UnsupportedDisplayCharacterSet(value) => write!(
                formatter,
                "the VMC reports unsupported display character set {value}"
            ),
            Self::DisplayMessageTooLong { length, capacity } => write!(
                formatter,
                "display message is {length} bytes but the VMC display accepts {capacity}"
            ),
            Self::UnsupportedDisplayCharacter(character) => write!(
                formatter,
                "character {character:?} is not supported by the VMC display"
            ),
        }
    }
}

impl Error for MdbError {}

impl From<io::Error> for MdbError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error.to_string())
    }
}

impl From<MdbError> for io::Error {
    fn from(error: MdbError) -> Self {
        Self::other(error)
    }
}

struct DeviceHandle {
    commands: mpsc::UnboundedSender<ActorCommand>,
    status: watch::Receiver<DeviceStatus>,
    device_events: broadcast::Receiver<DeviceEvent>,
}

impl DeviceHandle {
    async fn display_message(
        &self,
        session_id: Option<SessionId>,
        message: &str,
        time: DisplayTime,
    ) -> Result<(), MdbError> {
        let (reply_sender, reply) = oneshot::channel();
        self.commands
            .send(ActorCommand::DisplayMessage {
                session_id,
                message: message.to_owned(),
                time,
                reply: reply_sender,
            })
            .map_err(|_| MdbError::Disconnected)?;
        reply.await.map_err(|_| MdbError::Disconnected)?
    }
}

/// An MDB reader connection with no application-owned session.
#[must_use = "dropping the device disconnects the MDB actor"]
pub struct MdbDevice {
    handle: DeviceHandle,
}

impl MdbDevice {
    pub async fn connect(config: &MdbConfig) -> Result<Self, MdbError> {
        Self::connect_with_trace(config, drop).await
    }

    pub async fn connect_with_trace(
        config: &MdbConfig,
        trace: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self, MdbError> {
        let stream = open_serial(config.port(), config.baud())?;
        sleep(ADAPTER_STARTUP_DELAY).await;
        Ok(spawn_actor(
            stream,
            config.application_response_time(),
            config.display_fallback(),
            Arc::new(trace),
        ))
    }

    pub fn status(&self) -> DeviceStatus {
        *self.handle.status.borrow()
    }

    pub fn subscribe(&self) -> DeviceEvents {
        DeviceEvents {
            receiver: self.handle.device_events.resubscribe(),
        }
    }

    /// Requests a message on the VMC display while no session is active.
    pub async fn display_message(&self, message: &str, time: DisplayTime) -> Result<(), MdbError> {
        self.handle.display_message(None, message, time).await
    }

    pub async fn begin_session(self, funds: SessionFunds) -> Result<MdbSession, BeginSessionError> {
        let id = SessionId::new();
        let (events_sender, events) = mpsc::unbounded_channel();
        let (reply_sender, reply) = oneshot::channel();
        let handle = self.handle;
        if handle
            .commands
            .send(ActorCommand::BeginSession {
                id,
                funds,
                events: events_sender,
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(BeginSessionError::new(MdbError::Disconnected, handle));
        }

        match reply.await {
            Ok(Ok(())) => Ok(MdbSession {
                handle: Some(handle),
                id,
                funds,
                events,
            }),
            Ok(Err(error)) => Err(BeginSessionError::new(error, handle)),
            Err(_) => Err(BeginSessionError::new(MdbError::Disconnected, handle)),
        }
    }

    pub async fn shutdown(self) -> Result<(), MdbError> {
        let (reply_sender, reply) = oneshot::channel();
        self.handle
            .commands
            .send(ActorCommand::Shutdown {
                reply: reply_sender,
            })
            .map_err(|_| MdbError::Disconnected)?;
        reply.await.map_err(|_| MdbError::Disconnected)?
    }
}

pub struct BeginSessionError {
    source: MdbError,
    device: MdbDevice,
}

impl BeginSessionError {
    fn new(source: MdbError, handle: DeviceHandle) -> Self {
        Self {
            source,
            device: MdbDevice { handle },
        }
    }

    pub fn into_parts(self) -> (MdbError, MdbDevice) {
        (self.source, self.device)
    }
}

impl fmt::Debug for BeginSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BeginSessionError")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for BeginSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for BeginSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(Uuid);

impl SessionId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VendId(Uuid);

impl VendId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for VendId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendSuccessEvidence {
    Confirmed,
    AssumedAfterReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndReason {
    Completed,
    ApplicationRequested,
    Reset,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    id: SessionId,
    reason: SessionEndReason,
    successful_vends: u32,
    failed_vends: u32,
    cancelled_vends: u32,
}

impl SessionSummary {
    pub const fn id(&self) -> SessionId {
        self.id
    }

    pub const fn reason(&self) -> SessionEndReason {
        self.reason
    }

    pub const fn successful_vends(&self) -> u32 {
        self.successful_vends
    }

    pub const fn failed_vends(&self) -> u32 {
        self.failed_vends
    }

    pub const fn cancelled_vends(&self) -> u32 {
        self.cancelled_vends
    }
}

/// Exclusive ownership of an active MDB session.
#[must_use = "finish or cancel the session to recover the MDB device"]
pub struct MdbSession {
    handle: Option<DeviceHandle>,
    id: SessionId,
    funds: SessionFunds,
    events: mpsc::UnboundedReceiver<SessionEvent>,
}

impl MdbSession {
    pub const fn id(&self) -> SessionId {
        self.id
    }

    pub const fn advertised_funds(&self) -> SessionFunds {
        self.funds
    }

    /// Requests a message on the VMC display while this session is idle.
    pub async fn display_message(&self, message: &str, time: DisplayTime) -> Result<(), MdbError> {
        self.handle
            .as_ref()
            .ok_or(MdbError::InvalidState {
                operation: "display a message",
                state: "the session is already finished",
            })?
            .display_message(Some(self.id), message, time)
            .await
    }

    pub fn try_event(&mut self) -> Result<Option<SessionEvent>, MdbError> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(MdbError::Disconnected),
        }
    }

    pub async fn next_event(&mut self) -> Result<SessionEvent, MdbError> {
        self.events.recv().await.ok_or(MdbError::Disconnected)
    }

    pub async fn receive_event(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<SessionEvent>, MdbError> {
        match tokio::time::timeout(timeout, self.events.recv()).await {
            Ok(Some(event)) => Ok(Some(event)),
            Ok(None) => Err(MdbError::Disconnected),
            Err(_) => Ok(None),
        }
    }

    pub async fn cancel(self) -> Result<EndedSession, MdbError> {
        self.finish().await
    }

    pub async fn finish(mut self) -> Result<EndedSession, MdbError> {
        let handle = self.handle.take().ok_or(MdbError::InvalidState {
            operation: "finish the session",
            state: "already finished",
        })?;
        let (reply_sender, reply) = oneshot::channel();
        handle
            .commands
            .send(ActorCommand::EndSession {
                id: self.id,
                reply: reply_sender,
            })
            .map_err(|_| MdbError::Disconnected)?;
        let summary = reply.await.map_err(|_| MdbError::Disconnected)??;
        Ok(EndedSession {
            device: MdbDevice { handle },
            summary,
        })
    }
}

impl Drop for MdbSession {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let (reply, _ignored) = oneshot::channel();
            let _ = handle
                .commands
                .send(ActorCommand::EndSession { id: self.id, reply });
        }
    }
}

pub struct EndedSession {
    device: MdbDevice,
    summary: SessionSummary,
}

impl EndedSession {
    pub const fn summary(&self) -> &SessionSummary {
        &self.summary
    }

    pub fn into_device(self) -> MdbDevice {
        self.device
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendDecisionError {
    Cancelled,
    TimedOut,
    SessionEnded,
    Abandoned,
    AlreadyDecided,
    Disconnected,
}

impl fmt::Display for VendDecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("the VMC cancelled this vend"),
            Self::TimedOut => formatter.write_str("the MDB decision deadline expired"),
            Self::SessionEnded => formatter.write_str("the MDB session ended"),
            Self::Abandoned => formatter.write_str("the vend decision capability was dropped"),
            Self::AlreadyDecided => {
                formatter.write_str("this vend is no longer awaiting a decision")
            }
            Self::Disconnected => formatter.write_str("the MDB actor disconnected"),
        }
    }
}

impl Error for VendDecisionError {}

/// A single-use capability to approve or deny one VMC vend request.
#[must_use = "a pending vend is denied automatically if this value is dropped"]
pub struct PendingVend {
    commands: mpsc::UnboundedSender<ActorCommand>,
    id: VendId,
    price: Level1Amount,
    item: ItemNumber,
    decided: bool,
}

impl fmt::Debug for PendingVend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingVend")
            .field("id", &self.id)
            .field("price", &self.price)
            .field("item", &self.item)
            .finish_non_exhaustive()
    }
}

impl PendingVend {
    pub const fn id(&self) -> VendId {
        self.id
    }

    pub const fn requested_price(&self) -> Level1Amount {
        self.price
    }

    pub const fn item_number(&self) -> ItemNumber {
        self.item
    }

    pub async fn approve(self) -> Result<(), VendDecisionError> {
        let price = self.price;
        self.decide(VendDecision::Approve(price)).await
    }

    pub async fn approve_for(self, amount: Level1Amount) -> Result<(), VendDecisionError> {
        self.decide(VendDecision::Approve(amount)).await
    }

    pub async fn deny(self) -> Result<(), VendDecisionError> {
        self.decide(VendDecision::Deny).await
    }

    async fn decide(mut self, decision: VendDecision) -> Result<(), VendDecisionError> {
        self.decided = true;
        let (reply_sender, reply) = oneshot::channel();
        self.commands
            .send(ActorCommand::DecideVend {
                id: self.id,
                decision,
                reply: reply_sender,
            })
            .map_err(|_| VendDecisionError::Disconnected)?;
        reply.await.unwrap_or(Err(VendDecisionError::Disconnected))
    }
}

impl Drop for PendingVend {
    fn drop(&mut self) {
        if !self.decided {
            let _ = self
                .commands
                .send(ActorCommand::AbandonVend { id: self.id });
        }
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    VendRequested(PendingVend),
    VendCancelled {
        vend_id: VendId,
    },
    VendDecisionExpired {
        vend_id: VendId,
        error: VendDecisionError,
    },
    VendSucceeded {
        vend_id: VendId,
        amount: Level1Amount,
        reported_item: Option<ItemNumber>,
        evidence: VendSuccessEvidence,
    },
    VendFailed {
        vend_id: VendId,
        amount: Level1Amount,
    },
    Ended {
        reason: SessionEndReason,
    },
}

impl SessionEvent {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::VendRequested(_) => "vend_request",
            Self::VendCancelled { .. } => "vend_cancel",
            Self::VendDecisionExpired { .. } => "vend_decision_expired",
            Self::VendSucceeded { .. } => "vend_success",
            Self::VendFailed { .. } => "vend_failure",
            Self::Ended { .. } => "session_ended",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum VendDecision {
    Approve(Level1Amount),
    Deny,
}

enum ActorCommand {
    BeginSession {
        id: SessionId,
        funds: SessionFunds,
        events: mpsc::UnboundedSender<SessionEvent>,
        reply: oneshot::Sender<Result<(), MdbError>>,
    },
    DisplayMessage {
        session_id: Option<SessionId>,
        message: String,
        time: DisplayTime,
        reply: oneshot::Sender<Result<(), MdbError>>,
    },
    DecideVend {
        id: VendId,
        decision: VendDecision,
        reply: oneshot::Sender<Result<(), VendDecisionError>>,
    },
    AbandonVend {
        id: VendId,
    },
    EndSession {
        id: SessionId,
        reply: oneshot::Sender<Result<SessionSummary, MdbError>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), MdbError>>,
    },
}

#[derive(Debug, Clone, Copy)]
struct VendRecord {
    id: VendId,
    requested_price: Level1Amount,
    approved_amount: Option<Level1Amount>,
}

enum SessionPhase {
    Idle,
    VendPending { vend: VendRecord, deadline: Instant },
    DenyingForEnd(VendRecord),
    AwaitingVendResult(VendRecord),
    AwaitingSessionComplete,
    AwaitingReset(Option<VendRecord>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayState {
    Unknown,
    Unavailable,
    Available {
        dimensions: DisplayDimensions,
        character_set: DisplayCharacterSet,
    },
    UnsupportedCharacterSet(u8),
}

impl DisplayState {
    fn from_fallback(fallback: Option<(DisplayDimensions, DisplayCharacterSet)>) -> Self {
        fallback.map_or(Self::Unknown, |(dimensions, character_set)| {
            Self::Available {
                dimensions,
                character_set,
            }
        })
    }

    fn from_setup(columns: u8, rows: u8, character_set: u8) -> Self {
        let Ok(dimensions) = DisplayDimensions::new(columns, rows) else {
            return Self::Unavailable;
        };
        let character_set = match character_set {
            0 => DisplayCharacterSet::Basic,
            1 => DisplayCharacterSet::FullAscii,
            value => return Self::UnsupportedCharacterSet(value),
        };
        Self::Available {
            dimensions,
            character_set,
        }
    }
}

struct ActiveSession {
    id: SessionId,
    events: mpsc::UnboundedSender<SessionEvent>,
    phase: SessionPhase,
    summary: SessionSummary,
    end_requested: bool,
    end_waiters: Vec<oneshot::Sender<Result<SessionSummary, MdbError>>>,
}

impl ActiveSession {
    fn new(id: SessionId, events: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self {
            id,
            events,
            phase: SessionPhase::Idle,
            summary: SessionSummary {
                id,
                reason: SessionEndReason::Completed,
                successful_vends: 0,
                failed_vends: 0,
                cancelled_vends: 0,
            },
            end_requested: false,
            end_waiters: Vec::new(),
        }
    }
}

fn spawn_actor<T>(
    io: T,
    application_response_time: Duration,
    display_fallback: Option<(DisplayDimensions, DisplayCharacterSet)>,
    trace: TraceSink,
) -> MdbDevice
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (commands, command_receiver) = mpsc::unbounded_channel();
    // The WAFER bridge owns MDB initialization and may have completed it
    // before this process connects. Treat the composite adapter as enabled
    // until an explicit RESET, DISABLE, or ENABLE is forwarded.
    let (status_sender, status) = watch::channel(DeviceStatus::Enabled);
    let (device_events, device_event_receiver) = broadcast::channel(32);
    let actor = Actor {
        link: Link::new(io, trace),
        command_sender: commands.downgrade(),
        commands: command_receiver,
        command_channel_closed: false,
        status: status_sender,
        device_events,
        application_response_time,
        machine_maximum: None,
        display_fallback,
        display: DisplayState::from_fallback(display_fallback),
        active: None,
        completed: None,
        invalid_vends: Vec::new(),
        shutdown_waiters: Vec::new(),
        exit_deadline: None,
    };
    tokio::spawn(actor.run());
    MdbDevice {
        handle: DeviceHandle {
            commands,
            status,
            device_events: device_event_receiver,
        },
    }
}

struct Actor<T> {
    link: Link<T>,
    command_sender: mpsc::WeakUnboundedSender<ActorCommand>,
    commands: mpsc::UnboundedReceiver<ActorCommand>,
    command_channel_closed: bool,
    status: watch::Sender<DeviceStatus>,
    device_events: broadcast::Sender<DeviceEvent>,
    application_response_time: Duration,
    machine_maximum: Option<Level1Amount>,
    display_fallback: Option<(DisplayDimensions, DisplayCharacterSet)>,
    display: DisplayState,
    active: Option<ActiveSession>,
    completed: Option<SessionSummary>,
    invalid_vends: Vec<(VendId, VendDecisionError)>,
    shutdown_waiters: Vec<oneshot::Sender<Result<(), MdbError>>>,
    exit_deadline: Option<Instant>,
}

impl<T> Actor<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    async fn run(mut self) {
        loop {
            if self.should_exit() {
                self.complete_shutdown(&Ok(()));
                break;
            }
            let deadline = self.next_deadline();
            tokio::select! {
                message = self.link.receive() => {
                    match message {
                        Ok(message) => {
                            if let Err(error) = self.handle_adapter_message(message).await {
                                self.fail(error);
                                break;
                            }
                        }
                        Err(error) => {
                            self.fail(MdbError::from(error));
                            break;
                        }
                    }
                }
                command = self.commands.recv(), if !self.command_channel_closed => {
                    if let Some(command) = command {
                        if let Err(error) = self.handle_command(command).await {
                            self.fail(error);
                            break;
                        }
                    } else {
                        self.command_channel_closed = true;
                        self.exit_deadline = Some(Instant::now() + ACTOR_SHUTDOWN_GRACE);
                        if self.active.is_some() {
                            if let Err(error) = self.request_end_without_waiter().await {
                                self.fail(error);
                                break;
                            }
                        }
                    }
                }
                () = wait_for_deadline(deadline) => {
                    if let Err(error) = self.handle_deadline().await {
                        self.fail(error);
                        break;
                    }
                }
            }
        }
    }

    fn should_exit(&self) -> bool {
        (self.command_channel_closed || !self.shutdown_waiters.is_empty()) && self.active.is_none()
    }

    fn next_deadline(&self) -> Option<Instant> {
        let vend_deadline = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::VendPending { deadline, .. } => Some(deadline),
                _ => None,
            });
        match (vend_deadline, self.exit_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    async fn handle_deadline(&mut self) -> Result<(), MdbError> {
        let now = Instant::now();
        if self.exit_deadline.is_some_and(|deadline| deadline <= now) {
            self.complete_shutdown(&Err(MdbError::Transport(
                "timed out waiting for the MDB session to close".into(),
            )));
            self.active = None;
            return Ok(());
        }

        let expired = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::VendPending { vend, deadline } if deadline <= now => Some(vend),
                _ => None,
            });
        if let Some(vend) = expired {
            self.invalidate_vend(vend.id, VendDecisionError::TimedOut);
            self.link.send(ReaderCommand::Deny).await?;
            if let Some(session) = &mut self.active {
                session.phase = SessionPhase::Idle;
                let _ = session.events.send(SessionEvent::VendDecisionExpired {
                    vend_id: vend.id,
                    error: VendDecisionError::TimedOut,
                });
            }
        }
        Ok(())
    }

    async fn handle_command(&mut self, command: ActorCommand) -> Result<(), MdbError> {
        match command {
            ActorCommand::BeginSession {
                id,
                funds,
                events,
                reply,
            } => {
                let result = self.begin_session(id, funds, events).await;
                let fatal = result.as_ref().err().is_some_and(is_transport_error);
                let _ = reply.send(result.clone());
                if fatal {
                    return Err(result.expect_err("checked above"));
                }
            }
            ActorCommand::DisplayMessage {
                session_id,
                message,
                time,
                reply,
            } => {
                let result = self.display_message(session_id, &message, time).await;
                let fatal = result.as_ref().err().is_some_and(is_transport_error);
                let _ = reply.send(result.clone());
                if fatal {
                    return Err(result.expect_err("checked above"));
                }
            }
            ActorCommand::DecideVend {
                id,
                decision,
                reply,
            } => {
                let result = self.decide_vend(id, decision).await;
                let fatal = matches!(result, Err(VendDecisionError::Disconnected));
                let _ = reply.send(result);
                if fatal {
                    return Err(MdbError::Disconnected);
                }
            }
            ActorCommand::AbandonVend { id } => self.abandon_vend(id).await?,
            ActorCommand::EndSession { id, reply } => self.request_end(id, reply).await?,
            ActorCommand::Shutdown { reply } => {
                self.shutdown_waiters.push(reply);
                self.exit_deadline = Some(Instant::now() + ACTOR_SHUTDOWN_GRACE);
                self.request_end_without_waiter().await?;
            }
        }
        Ok(())
    }

    async fn display_message(
        &mut self,
        session_id: Option<SessionId>,
        message: &str,
        time: DisplayTime,
    ) -> Result<(), MdbError> {
        match session_id {
            None if self.active.is_some() => {
                return Err(MdbError::InvalidState {
                    operation: "display a message without a session",
                    state: "a session is active",
                });
            }
            Some(id) => {
                let Some(session) = &self.active else {
                    return Err(MdbError::InvalidState {
                        operation: "display a session message",
                        state: "no session is active",
                    });
                };
                if session.id != id {
                    return Err(MdbError::InvalidState {
                        operation: "display a session message",
                        state: "a different session is active",
                    });
                }
                if !matches!(session.phase, SessionPhase::Idle) {
                    return Err(MdbError::InvalidState {
                        operation: "display a session message",
                        state: "the session is processing or ending a vend",
                    });
                }
            }
            None => {}
        }

        if matches!(*self.status.borrow(), DeviceStatus::Inactive) {
            return Err(MdbError::InvalidState {
                operation: "display a message",
                state: "the reader has not completed MDB setup",
            });
        }

        let (dimensions, character_set) = match self.display {
            DisplayState::Unknown => return Err(MdbError::DisplayConfigurationUnknown),
            DisplayState::Unavailable => return Err(MdbError::DisplayUnavailable),
            DisplayState::UnsupportedCharacterSet(value) => {
                return Err(MdbError::UnsupportedDisplayCharacterSet(value));
            }
            DisplayState::Available {
                dimensions,
                character_set,
            } => (dimensions, character_set),
        };

        if let Some(character) = message.chars().find(|character| {
            !character.is_ascii()
                || !((' '..='~').contains(character))
                || (character_set == DisplayCharacterSet::Basic
                    && !matches!(character, '0'..='9' | 'A'..='Z' | ' ' | '.'))
        }) {
            return Err(MdbError::UnsupportedDisplayCharacter(character));
        }

        let capacity = dimensions.capacity();
        if message.len() > capacity {
            return Err(MdbError::DisplayMessageTooLong {
                length: message.len(),
                capacity,
            });
        }

        let mut data = vec![b' '; capacity];
        data[..message.len()].copy_from_slice(message.as_bytes());
        self.link
            .send(ReaderCommand::DisplayRequest { time, data })
            .await?;
        Ok(())
    }

    async fn begin_session(
        &mut self,
        id: SessionId,
        funds: SessionFunds,
        events: mpsc::UnboundedSender<SessionEvent>,
    ) -> Result<(), MdbError> {
        if self.active.is_some() {
            return Err(MdbError::InvalidState {
                operation: "begin another session",
                state: "a session is already active",
            });
        }
        let status = *self.status.borrow();
        match status {
            DeviceStatus::Disabled | DeviceStatus::Inactive => {
                return Err(MdbError::InvalidState {
                    operation: "begin a session",
                    state: "the VMC has not enabled the reader",
                });
            }
            DeviceStatus::Disconnected => {
                return Err(MdbError::Disconnected);
            }
            DeviceStatus::Enabled | DeviceStatus::SessionActive => {}
        }
        let raw_funds = funds.resolve(self.machine_maximum)?;
        self.link
            .send(ReaderCommand::BeginSession(raw_funds))
            .await?;
        self.completed = None;
        self.active = Some(ActiveSession::new(id, events));
        self.publish_status(DeviceStatus::SessionActive);
        Ok(())
    }

    async fn decide_vend(
        &mut self,
        id: VendId,
        decision: VendDecision,
    ) -> Result<(), VendDecisionError> {
        if let Some(error) = self.take_invalid_vend(id) {
            return Err(error);
        }
        let vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::VendPending { vend, .. } if vend.id == id => Some(vend),
                _ => None,
            });
        let Some(mut vend) = vend else {
            return Err(VendDecisionError::AlreadyDecided);
        };

        match decision {
            VendDecision::Approve(amount) => {
                self.link
                    .send(ReaderCommand::Approve(amount))
                    .await
                    .map_err(|_| VendDecisionError::Disconnected)?;
                vend.approved_amount = Some(amount);
                if let Some(session) = &mut self.active {
                    session.phase = SessionPhase::AwaitingVendResult(vend);
                }
            }
            VendDecision::Deny => {
                self.link
                    .send(ReaderCommand::Deny)
                    .await
                    .map_err(|_| VendDecisionError::Disconnected)?;
                if let Some(session) = &mut self.active {
                    session.phase = SessionPhase::Idle;
                }
            }
        }
        Ok(())
    }

    async fn abandon_vend(&mut self, id: VendId) -> Result<(), MdbError> {
        let pending_vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::VendPending { vend, .. } if vend.id == id => Some(vend),
                _ => None,
            });
        if let Some(vend) = pending_vend {
            self.invalidate_vend(vend.id, VendDecisionError::Abandoned);
            self.link.send(ReaderCommand::Deny).await?;
            if let Some(session) = &mut self.active {
                session.phase = SessionPhase::Idle;
                let _ = session.events.send(SessionEvent::VendDecisionExpired {
                    vend_id: vend.id,
                    error: VendDecisionError::Abandoned,
                });
            }
        }
        Ok(())
    }

    async fn request_end(
        &mut self,
        id: SessionId,
        reply: oneshot::Sender<Result<SessionSummary, MdbError>>,
    ) -> Result<(), MdbError> {
        if let Some(summary) = &self.completed {
            if summary.id == id {
                let _ = reply.send(Ok(summary.clone()));
                return Ok(());
            }
        }
        let Some(session) = &mut self.active else {
            let _ = reply.send(Err(MdbError::InvalidState {
                operation: "finish the session",
                state: "no matching session is active",
            }));
            return Ok(());
        };
        if session.id != id {
            let _ = reply.send(Err(MdbError::InvalidState {
                operation: "finish the session",
                state: "a different session is active",
            }));
            return Ok(());
        }
        session.end_waiters.push(reply);
        self.initiate_end().await
    }

    async fn request_end_without_waiter(&mut self) -> Result<(), MdbError> {
        if self.active.is_some() {
            self.initiate_end().await?;
        }
        Ok(())
    }

    async fn initiate_end(&mut self) -> Result<(), MdbError> {
        let phase = self.active.as_ref().map(|session| match session.phase {
            SessionPhase::Idle => 0,
            SessionPhase::VendPending { .. } => 1,
            SessionPhase::DenyingForEnd(_) => 2,
            SessionPhase::AwaitingVendResult(_) => 3,
            SessionPhase::AwaitingSessionComplete => 4,
            SessionPhase::AwaitingReset(_) => 5,
        });
        if let Some(session) = &mut self.active {
            session.end_requested = true;
            session.summary.reason = SessionEndReason::ApplicationRequested;
        }
        match phase {
            Some(0) => {
                self.link.send(ReaderCommand::SessionCancel).await?;
                if let Some(session) = &mut self.active {
                    session.phase = SessionPhase::AwaitingSessionComplete;
                }
            }
            Some(1) => {
                let vend = self
                    .active
                    .as_ref()
                    .and_then(|session| match session.phase {
                        SessionPhase::VendPending { vend, .. } => Some(vend),
                        _ => None,
                    });
                if let Some(vend) = vend {
                    self.invalidate_vend(vend.id, VendDecisionError::SessionEnded);
                }
                self.link.send(ReaderCommand::Deny).await?;
                if let Some(session) = &mut self.active {
                    session.phase = SessionPhase::DenyingForEnd(
                        vend.expect("a pending phase always contains a vend"),
                    );
                }
            }
            Some(2..=5) | None => {}
            Some(_) => unreachable!(),
        }
        Ok(())
    }

    async fn handle_adapter_message(&mut self, message: AdapterMessage) -> Result<(), MdbError> {
        match message {
            AdapterMessage::Ack => {
                let denying_for_end = self
                    .active
                    .as_ref()
                    .is_some_and(|session| matches!(session.phase, SessionPhase::DenyingForEnd(_)));
                if denying_for_end {
                    self.link.send(ReaderCommand::SessionCancel).await?;
                    if let Some(session) = &mut self.active {
                        session.phase = SessionPhase::AwaitingSessionComplete;
                    }
                }
            }
            AdapterMessage::Nak | AdapterMessage::Retransmit => {
                // The WAFER bridge owns MDB retransmission. Retain these in the
                // trace until its exact host-side retry contract is qualified.
            }
            AdapterMessage::Vmc(event) => self.handle_vmc_event(event).await?,
        }
        Ok(())
    }

    async fn handle_vmc_event(&mut self, event: VmcEvent) -> Result<(), MdbError> {
        match event {
            VmcEvent::Reset => self.handle_reset(),
            VmcEvent::SetupConfiguration {
                feature_level,
                display_columns,
                display_rows,
                display_character_set,
            } => {
                let _ = feature_level;
                self.display =
                    DisplayState::from_setup(display_columns, display_rows, display_character_set);
                self.publish_status(DeviceStatus::Disabled);
            }
            VmcEvent::SetupPrices { maximum, minimum } => {
                self.machine_maximum = maximum;
                let _ = minimum;
            }
            VmcEvent::ReaderEnable => {
                if self.active.is_some() {
                    self.command_out_of_sequence().await?;
                } else {
                    self.publish_status(DeviceStatus::Enabled);
                }
            }
            VmcEvent::ReaderDisable => {
                if self.active.is_some() {
                    self.command_out_of_sequence().await?;
                } else {
                    self.publish_status(DeviceStatus::Disabled);
                }
            }
            VmcEvent::ReaderCancel => {
                if self.active.is_some() {
                    self.command_out_of_sequence().await?;
                } else {
                    self.link.send(ReaderCommand::Cancelled).await?;
                }
            }
            VmcEvent::VendRequest { price, item } => self.handle_vend_request(price, item).await?,
            VmcEvent::VendCancel => self.handle_vend_cancel().await?,
            VmcEvent::VendSuccess { item } => self.handle_vend_success(item).await?,
            VmcEvent::VendFailure => self.handle_vend_failure().await?,
            VmcEvent::SessionComplete => self.handle_session_complete().await?,
            VmcEvent::CashSale { price, item } => {
                let _ = self
                    .device_events
                    .send(DeviceEvent::CashSale { price, item });
            }
            VmcEvent::Other => {}
        }
        Ok(())
    }

    async fn handle_vend_request(
        &mut self,
        price: Level1Amount,
        item: ItemNumber,
    ) -> Result<(), MdbError> {
        let is_idle = self
            .active
            .as_ref()
            .is_some_and(|session| matches!(session.phase, SessionPhase::Idle));
        if !is_idle {
            self.command_out_of_sequence().await?;
            return Ok(());
        }
        let id = VendId::new();
        let vend = VendRecord {
            id,
            requested_price: price,
            approved_amount: None,
        };
        if let Some(session) = &mut self.active {
            session.phase = SessionPhase::VendPending {
                vend,
                deadline: Instant::now() + self.application_response_time,
            };
            let pending_vend = PendingVend {
                commands: self
                    .command_sender
                    .upgrade()
                    .ok_or(MdbError::Disconnected)?,
                id,
                price,
                item,
                decided: false,
            };
            if session
                .events
                .send(SessionEvent::VendRequested(pending_vend))
                .is_err()
            {
                self.abandon_vend(id).await?;
            }
        }
        Ok(())
    }

    async fn handle_vend_cancel(&mut self) -> Result<(), MdbError> {
        let pending_vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::VendPending { vend, .. } => Some(vend),
                _ => None,
            });
        let ending_vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::DenyingForEnd(vend) => Some(vend),
                _ => None,
            });
        let Some(vend) = pending_vend.or(ending_vend) else {
            self.command_out_of_sequence().await?;
            return Ok(());
        };
        self.invalidate_vend(vend.id, VendDecisionError::Cancelled);
        if pending_vend.is_some() {
            self.link.send(ReaderCommand::Deny).await?;
        }
        if let Some(session) = &mut self.active {
            if pending_vend.is_some() {
                session.phase = SessionPhase::Idle;
            }
            session.summary.cancelled_vends += 1;
            let _ = session
                .events
                .send(SessionEvent::VendCancelled { vend_id: vend.id });
        }
        Ok(())
    }

    async fn handle_session_complete(&mut self) -> Result<(), MdbError> {
        let valid = self.active.as_ref().is_none_or(|session| {
            matches!(
                session.phase,
                SessionPhase::Idle
                    | SessionPhase::DenyingForEnd(_)
                    | SessionPhase::AwaitingSessionComplete
            )
        });
        if !valid {
            self.command_out_of_sequence().await?;
            return Ok(());
        }

        self.link.send(ReaderCommand::EndSession).await?;
        let reason = self
            .active
            .as_ref()
            .map_or(SessionEndReason::Completed, |session| {
                session.summary.reason
            });
        self.complete_active(reason);
        Ok(())
    }

    async fn handle_vend_success(
        &mut self,
        reported_item: Option<ItemNumber>,
    ) -> Result<(), MdbError> {
        let vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::AwaitingVendResult(vend) => Some(vend),
                _ => None,
            });
        let Some(vend) = vend else {
            self.command_out_of_sequence().await?;
            return Ok(());
        };
        let amount = vend.approved_amount.unwrap_or(vend.requested_price);
        let end_requested = if let Some(session) = &mut self.active {
            session.phase = SessionPhase::Idle;
            session.summary.successful_vends += 1;
            let _ = session.events.send(SessionEvent::VendSucceeded {
                vend_id: vend.id,
                amount,
                reported_item,
                evidence: VendSuccessEvidence::Confirmed,
            });
            session.end_requested
        } else {
            false
        };
        if end_requested {
            self.initiate_end().await?;
        }
        Ok(())
    }

    async fn handle_vend_failure(&mut self) -> Result<(), MdbError> {
        let vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::AwaitingVendResult(vend) => Some(vend),
                _ => None,
            });
        let Some(vend) = vend else {
            self.command_out_of_sequence().await?;
            return Ok(());
        };
        let amount = vend.approved_amount.unwrap_or(vend.requested_price);
        let end_requested = if let Some(session) = &mut self.active {
            session.phase = SessionPhase::Idle;
            session.summary.failed_vends += 1;
            let _ = session.events.send(SessionEvent::VendFailed {
                vend_id: vend.id,
                amount,
            });
            session.end_requested
        } else {
            false
        };
        if end_requested {
            self.initiate_end().await?;
        }
        Ok(())
    }

    async fn command_out_of_sequence(&mut self) -> Result<(), MdbError> {
        let pending_vend = self.pending_vend();
        let approved_vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::AwaitingVendResult(vend)
                | SessionPhase::AwaitingReset(Some(vend)) => Some(vend),
                _ => None,
            });

        if let Some(vend) = pending_vend {
            self.invalidate_vend(vend.id, VendDecisionError::SessionEnded);
            if let Some(session) = &mut self.active {
                let _ = session.events.send(SessionEvent::VendDecisionExpired {
                    vend_id: vend.id,
                    error: VendDecisionError::SessionEnded,
                });
            }
        }
        if let Some(session) = &mut self.active {
            session.phase = SessionPhase::AwaitingReset(approved_vend);
        }
        self.link.send(ReaderCommand::CommandOutOfSequence).await?;
        Ok(())
    }

    fn handle_reset(&mut self) {
        let approved_vend = self
            .active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::AwaitingVendResult(vend)
                | SessionPhase::AwaitingReset(Some(vend)) => Some(vend),
                _ => None,
            });
        if let Some(vend) = approved_vend {
            let amount = vend.approved_amount.unwrap_or(vend.requested_price);
            if let Some(session) = &mut self.active {
                session.summary.successful_vends += 1;
                let _ = session.events.send(SessionEvent::VendSucceeded {
                    vend_id: vend.id,
                    amount,
                    reported_item: None,
                    evidence: VendSuccessEvidence::AssumedAfterReset,
                });
            }
        }
        if let Some(vend) = self.pending_vend() {
            self.invalidate_vend(vend.id, VendDecisionError::SessionEnded);
        }
        self.complete_active(SessionEndReason::Reset);
        self.display = DisplayState::from_fallback(self.display_fallback);
        self.publish_status(DeviceStatus::Inactive);
        let _ = self.device_events.send(DeviceEvent::Reinitialized);
    }

    fn pending_vend(&self) -> Option<VendRecord> {
        self.active
            .as_ref()
            .and_then(|session| match session.phase {
                SessionPhase::VendPending { vend, .. } => Some(vend),
                _ => None,
            })
    }

    fn invalidate_vend(&mut self, id: VendId, error: VendDecisionError) {
        if let Some((_, existing_error)) = self
            .invalid_vends
            .iter_mut()
            .find(|(vend_id, _)| *vend_id == id)
        {
            *existing_error = error;
            return;
        }
        self.invalid_vends.push((id, error));
        if self.invalid_vends.len() > 32 {
            self.invalid_vends.remove(0);
        }
    }

    fn take_invalid_vend(&mut self, id: VendId) -> Option<VendDecisionError> {
        let index = self
            .invalid_vends
            .iter()
            .position(|(vend_id, _)| *vend_id == id)?;
        Some(self.invalid_vends.remove(index).1)
    }

    fn complete_active(&mut self, reason: SessionEndReason) {
        let Some(mut session) = self.active.take() else {
            return;
        };
        session.summary.reason = reason;
        let _ = session.events.send(SessionEvent::Ended { reason });
        for waiter in session.end_waiters {
            let _ = waiter.send(Ok(session.summary.clone()));
        }
        self.completed = Some(session.summary);
        if matches!(
            reason,
            SessionEndReason::Completed | SessionEndReason::ApplicationRequested
        ) {
            self.publish_status(DeviceStatus::Enabled);
        }
    }

    fn publish_status(&self, status: DeviceStatus) {
        self.status.send_replace(status);
        let _ = self.device_events.send(DeviceEvent::StatusChanged(status));
    }

    fn fail(&mut self, error: MdbError) {
        if let Some(vend) = self.pending_vend() {
            self.invalidate_vend(vend.id, VendDecisionError::Disconnected);
        }
        if let Some(mut session) = self.active.take() {
            session.summary.reason = SessionEndReason::Disconnected;
            let _ = session.events.send(SessionEvent::Ended {
                reason: SessionEndReason::Disconnected,
            });
            for waiter in session.end_waiters {
                let _ = waiter.send(Err(error.clone()));
            }
        }
        self.publish_status(DeviceStatus::Disconnected);
        let _ = self
            .device_events
            .send(DeviceEvent::Fault(error.to_string()));
        self.complete_shutdown(&Err(error));
    }

    fn complete_shutdown(&mut self, result: &Result<(), MdbError>) {
        for waiter in self.shutdown_waiters.drain(..) {
            let _ = waiter.send(result.clone());
        }
    }
}

fn is_transport_error(error: &MdbError) -> bool {
    matches!(error, MdbError::Transport(_) | MdbError::Disconnected)
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        sleep_until(deadline).await;
    } else {
        pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::time::timeout;

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    fn fake_device(application_response_time: Duration) -> (MdbDevice, DuplexStream) {
        fake_device_with_display(application_response_time, None)
    }

    fn fake_device_with_display(
        application_response_time: Duration,
        display_fallback: Option<(DisplayDimensions, DisplayCharacterSet)>,
    ) -> (MdbDevice, DuplexStream) {
        let (driver, adapter) = duplex(4096);
        let device = spawn_actor(
            driver,
            application_response_time,
            display_fallback,
            Arc::new(drop),
        );
        (device, adapter)
    }

    fn known_funds(raw: u16) -> SessionFunds {
        SessionFunds::Known(Level1Amount::new(raw).expect("test amount should be valid"))
    }

    async fn send_vmc(adapter: &mut DuplexStream, payload: &[u8]) {
        let checksum = payload
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        let mut frame = vec![0x02];
        frame.extend(hex::encode(payload).bytes());
        frame.extend(format!("{checksum:02x}").bytes());
        frame.push(0x03);
        adapter.write_all(&frame).await.unwrap();
    }

    async fn send_adapter_ack(adapter: &mut DuplexStream) {
        adapter.write_all(b"\x0200\x03").await.unwrap();
    }

    async fn read_reader(adapter: &mut DuplexStream, length: usize) -> Vec<u8> {
        let mut frame = vec![0; length];
        timeout(TEST_TIMEOUT, adapter.read_exact(&mut frame))
            .await
            .expect("reader response timed out")
            .unwrap();
        frame
    }

    async fn next_vend(session: &mut MdbSession) -> PendingVend {
        match timeout(TEST_TIMEOUT, session.next_event())
            .await
            .unwrap()
            .unwrap()
        {
            SessionEvent::VendRequested(vend) => vend,
            event => panic!("expected vend request, got {event:?}"),
        }
    }

    fn four_character_display() -> (DisplayDimensions, DisplayCharacterSet) {
        (
            DisplayDimensions::new(4, 1).unwrap(),
            DisplayCharacterSet::FullAscii,
        )
    }

    #[tokio::test]
    async fn device_can_request_a_padded_vmc_display_message() {
        let (device, mut adapter) =
            fake_device_with_display(Duration::from_secs(1), Some(four_character_display()));

        device
            .display_message("PAY", DisplayTime::from_deciseconds(10))
            .await
            .unwrap();
        assert_eq!(
            read_reader(&mut adapter, 7).await,
            [0x02, 0x0a, 0x50, 0x41, 0x59, 0x20, 0x16]
        );
        device.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_display_does_not_change_the_vend_phase() {
        let dimensions = DisplayDimensions::new(16, 1).unwrap();
        let (device, mut adapter) = fake_device_with_display(
            Duration::from_secs(1),
            Some((dimensions, DisplayCharacterSet::FullAscii)),
        );
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_adapter_ack(&mut adapter).await;
        tokio::task::yield_now().await;

        session
            .display_message("MAKE A SELECTION", DisplayTime::MAX)
            .await
            .unwrap();
        let frame = read_reader(&mut adapter, 19).await;
        assert_eq!(&frame[..2], [0x02, 0xff]);
        assert_eq!(&frame[2..18], b"MAKE A SELECTION");
        assert_eq!(
            frame[18],
            frame[..18]
                .iter()
                .fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
        );

        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;
        vend.deny().await.unwrap();
        let _ = read_reader(&mut adapter, 2).await;
        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        let _ = read_reader(&mut adapter, 2).await;
        let _ = session.next_event().await.unwrap();
        let _ = session.finish().await.unwrap();
    }

    #[tokio::test]
    async fn observed_display_setup_overrides_the_fallback() {
        let (device, mut adapter) =
            fake_device_with_display(Duration::from_secs(1), Some(four_character_display()));
        send_vmc(&mut adapter, &[0x11, 0x00, 0x03, 0x10, 0x01, 0x01]).await;
        tokio::task::yield_now().await;

        device
            .display_message("MAKE A SELECTION", DisplayTime::MAX)
            .await
            .unwrap();
        let frame = read_reader(&mut adapter, 19).await;
        assert_eq!(&frame[2..18], b"MAKE A SELECTION");
        device.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn display_errors_are_specific_and_do_not_write_frames() {
        let (device, _adapter) = fake_device(Duration::from_secs(1));
        assert_eq!(
            device.display_message("PAY", DisplayTime::MAX).await,
            Err(MdbError::DisplayConfigurationUnknown)
        );
        device.shutdown().await.unwrap();

        let (device, _adapter) =
            fake_device_with_display(Duration::from_secs(1), Some(four_character_display()));
        assert_eq!(
            device.display_message("TOO LONG", DisplayTime::MAX).await,
            Err(MdbError::DisplayMessageTooLong {
                length: 8,
                capacity: 4,
            })
        );
        assert_eq!(
            device.display_message("PAY ☃", DisplayTime::MAX).await,
            Err(MdbError::UnsupportedDisplayCharacter('☃'))
        );
        device.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn observed_display_capabilities_control_text_validation() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        send_vmc(&mut adapter, &[0x11, 0x00, 0x03, 0x04, 0x01, 0x00]).await;
        tokio::task::yield_now().await;

        assert_eq!(
            device.display_message("pay", DisplayTime::MAX).await,
            Err(MdbError::UnsupportedDisplayCharacter('p'))
        );
        device
            .display_message("PAY", DisplayTime::MAX)
            .await
            .unwrap();
        assert_eq!(
            read_reader(&mut adapter, 7).await,
            [0x02, 0xff, 0x50, 0x41, 0x59, 0x20, 0x0b]
        );
        device.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn setup_can_report_an_unavailable_or_unsupported_display() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        send_vmc(&mut adapter, &[0x11, 0x00, 0x03, 0x00, 0x01, 0x01]).await;
        tokio::task::yield_now().await;
        assert_eq!(
            device.display_message("PAY", DisplayTime::MAX).await,
            Err(MdbError::DisplayUnavailable)
        );
        device.shutdown().await.unwrap();

        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        send_vmc(&mut adapter, &[0x11, 0x00, 0x03, 0x04, 0x01, 0x02]).await;
        tokio::task::yield_now().await;
        assert_eq!(
            device.display_message("PAY", DisplayTime::MAX).await,
            Err(MdbError::UnsupportedDisplayCharacterSet(2))
        );
        device.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reset_restores_the_display_fallback_but_blocks_until_reenabled() {
        let (device, mut adapter) =
            fake_device_with_display(Duration::from_secs(1), Some(four_character_display()));
        send_vmc(&mut adapter, &[0x11, 0x00, 0x03, 0x10, 0x01, 0x01]).await;
        timeout(TEST_TIMEOUT, async {
            while device.status() != DeviceStatus::Disabled {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        send_vmc(&mut adapter, &[0x10]).await;
        timeout(TEST_TIMEOUT, async {
            while device.status() != DeviceStatus::Inactive {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(
            device.display_message("PAY", DisplayTime::MAX).await,
            Err(MdbError::InvalidState {
                operation: "display a message",
                state: "the reader has not completed MDB setup",
            })
        );

        send_vmc(&mut adapter, &[0x14, 0x01]).await;
        timeout(TEST_TIMEOUT, async {
            while device.status() != DeviceStatus::Enabled {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        device
            .display_message("PAY", DisplayTime::MAX)
            .await
            .unwrap();
        assert_eq!(
            read_reader(&mut adapter, 7).await,
            [0x02, 0xff, 0x50, 0x41, 0x59, 0x20, 0x0b]
        );
        device.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_display_is_rejected_once_a_vend_is_underway() {
        let (device, mut adapter) =
            fake_device_with_display(Duration::from_secs(1), Some(four_character_display()));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;

        assert_eq!(
            session.display_message("PAY", DisplayTime::MAX).await,
            Err(MdbError::InvalidState {
                operation: "display a session message",
                state: "the session is processing or ending a vend",
            })
        );

        vend.deny().await.unwrap();
        let _ = read_reader(&mut adapter, 2).await;
        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        let _ = read_reader(&mut adapter, 2).await;
        let _ = session.next_event().await.unwrap();
        let _ = session.finish().await.unwrap();
    }

    #[tokio::test]
    async fn successful_vend_ends_with_end_session() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        assert_eq!(read_reader(&mut adapter, 4).await, [0x03, 0x05, 0x41, 0x49]);

        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;
        let vend_id = vend.id();
        vend.approve().await.unwrap();
        assert_eq!(read_reader(&mut adapter, 4).await, [0x05, 0x04, 0xe3, 0xec]);

        send_vmc(&mut adapter, &[0x13, 0x02]).await;
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::VendSucceeded {
                vend_id: id,
                evidence: VendSuccessEvidence::Confirmed,
                ..
            } if id == vend_id
        ));

        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x07, 0x07]);
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::Ended {
                reason: SessionEndReason::Completed
            }
        ));
        let ended = session.finish().await.unwrap();
        assert_eq!(ended.summary().successful_vends(), 1);
    }

    #[tokio::test]
    async fn vmc_cancel_denies_only_the_pending_vend() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;
        let vend_id = vend.id();

        send_vmc(&mut adapter, &[0x13, 0x01]).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x06, 0x06]);
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::VendCancelled { vend_id: id } if id == vend_id
        ));
        assert_eq!(vend.approve().await, Err(VendDecisionError::Cancelled));

        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x07, 0x07]);
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::Ended { .. }
        ));
        let ended = session.finish().await.unwrap();
        assert_eq!(ended.summary().cancelled_vends(), 1);
    }

    #[tokio::test]
    async fn finishing_with_a_pending_vend_denies_before_requesting_session_cancel() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;

        let finish = tokio::spawn(session.finish());
        assert_eq!(read_reader(&mut adapter, 2).await, [0x06, 0x06]);
        assert_eq!(vend.approve().await, Err(VendDecisionError::SessionEnded));

        send_adapter_ack(&mut adapter).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x04, 0x04]);
        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x07, 0x07]);
        let ended = finish.await.unwrap().unwrap();
        assert_eq!(
            ended.summary().reason(),
            SessionEndReason::ApplicationRequested
        );
    }

    #[tokio::test]
    async fn reset_after_approval_is_reported_as_an_assumed_success() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;
        let vend_id = vend.id();
        vend.approve().await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;

        send_vmc(&mut adapter, &[0x10]).await;
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::VendSucceeded {
                vend_id: id,
                evidence: VendSuccessEvidence::AssumedAfterReset,
                ..
            } if id == vend_id
        ));
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::Ended {
                reason: SessionEndReason::Reset
            }
        ));
        let ended = session.finish().await.unwrap();
        assert_eq!(ended.summary().successful_vends(), 1);
        assert_eq!(ended.summary().reason(), SessionEndReason::Reset);
    }

    #[tokio::test]
    async fn an_expired_decision_is_denied_and_the_capability_is_invalidated() {
        let (device, mut adapter) = fake_device(Duration::from_millis(20));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;

        assert_eq!(read_reader(&mut adapter, 2).await, [0x06, 0x06]);
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::VendDecisionExpired {
                error: VendDecisionError::TimedOut,
                ..
            }
        ));
        assert_eq!(vend.approve().await, Err(VendDecisionError::TimedOut));

        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        let _ = read_reader(&mut adapter, 2).await;
        let _ = session.next_event().await.unwrap();
        let _ = session.finish().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_a_pending_vend_denies_it() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;
        let vend_id = vend.id();

        drop(vend);
        assert_eq!(read_reader(&mut adapter, 2).await, [0x06, 0x06]);
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::VendDecisionExpired {
                vend_id: id,
                error: VendDecisionError::Abandoned,
            } if id == vend_id
        ));

        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        let _ = read_reader(&mut adapter, 2).await;
        let _ = session.next_event().await.unwrap();
        let _ = session.finish().await.unwrap();
    }

    #[tokio::test]
    async fn session_complete_cannot_skip_a_pending_vend_decision() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        let mut session = device.begin_session(known_funds(1345)).await.unwrap();
        let _ = read_reader(&mut adapter, 4).await;
        send_vmc(&mut adapter, &[0x13, 0x00, 0x04, 0xe3, 0x00, 0x01]).await;
        let vend = next_vend(&mut session).await;

        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x0b, 0x0b]);
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::VendDecisionExpired {
                error: VendDecisionError::SessionEnded,
                ..
            }
        ));
        assert_eq!(vend.deny().await, Err(VendDecisionError::SessionEnded));

        send_vmc(&mut adapter, &[0x10]).await;
        assert!(matches!(
            session.next_event().await.unwrap(),
            SessionEvent::Ended {
                reason: SessionEndReason::Reset
            }
        ));
        let _ = session.finish().await.unwrap();
    }

    #[tokio::test]
    async fn machine_maximum_comes_from_setup_prices() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        send_vmc(&mut adapter, &[0x11, 0x01, 0x05, 0x41, 0x00, 0x01]).await;
        tokio::task::yield_now().await;

        let session = device
            .begin_session(SessionFunds::MachineMaximum)
            .await
            .unwrap();
        assert_eq!(read_reader(&mut adapter, 4).await, [0x03, 0x05, 0x41, 0x49]);

        let finish = tokio::spawn(session.finish());
        assert_eq!(read_reader(&mut adapter, 2).await, [0x04, 0x04]);
        send_vmc(&mut adapter, &[0x13, 0x04]).await;
        let _ = read_reader(&mut adapter, 2).await;
        let _ = finish.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reader_cancel_while_enabled_is_answered_automatically() {
        let (device, mut adapter) = fake_device(Duration::from_secs(1));
        send_vmc(&mut adapter, &[0x14, 0x02]).await;
        assert_eq!(read_reader(&mut adapter, 2).await, [0x08, 0x08]);
        device.shutdown().await.unwrap();
    }
}
