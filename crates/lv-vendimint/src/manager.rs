use crate::request;
use bitcoin::Network;
use fedimint_core::invite_code::InviteCode;
use iroh::{EndpointAddr, EndpointId};
use lv_core::{
    CommandEnvelope, CommandId, CommandResult, EventEnvelope, EventSequence, KioskSnapshot,
    ManagerCommand, ManagerRequest, ManagerResponse,
};
use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use vendimint::Manager;

const STATE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CLAIM_TIMEOUT: Duration = Duration::from_secs(30);
const MANAGER_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const COMMAND_ATTEMPTS: usize = 3;
const COMMAND_RETRY_DELAY: Duration = Duration::from_millis(500);
const ECASH_EXPORT_RECLAIM_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct ManagerControllerConfig {
    storage_path: PathBuf,
    network: Network,
}

impl ManagerControllerConfig {
    pub fn mainnet(storage_path: impl Into<PathBuf>) -> Self {
        Self {
            storage_path: storage_path.into(),
            network: Network::Bitcoin,
        }
    }

    pub fn new(storage_path: impl Into<PathBuf>, network: Network) -> Self {
        Self {
            storage_path: storage_path.into(),
            network,
        }
    }

    pub fn storage_path(&self) -> &Path {
        &self.storage_path
    }

    pub const fn network(&self) -> Network {
        self.network
    }
}

pub struct ManagerController {
    commands: mpsc::UnboundedSender<ManagerControllerCommand>,
    events: mpsc::UnboundedReceiver<ManagerControllerEvent>,
}

impl ManagerController {
    pub fn spawn(config: ManagerControllerConfig) -> io::Result<Self> {
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (event_sender, events) = mpsc::unbounded_channel();
        thread::Builder::new()
            .name("lv-manager-vendimint".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        runtime.block_on(run(config, command_receiver, event_sender));
                    }
                    Err(error) => {
                        let _ = event_sender.send(ManagerControllerEvent::Unavailable(format!(
                            "could not start the Vendimint runtime: {error}"
                        )));
                    }
                }
            })?;
        Ok(Self { commands, events })
    }

    pub fn try_event(&mut self) -> Option<ManagerControllerEvent> {
        self.events.try_recv().ok()
    }

    pub fn begin_claim(
        &self,
        pairing_payload: impl Into<String>,
    ) -> Result<(), ManagerControllerStopped> {
        self.commands
            .send(ManagerControllerCommand::BeginClaim(pairing_payload.into()))
            .map_err(|_| ManagerControllerStopped)
    }

    pub fn update_federation(
        &self,
        invite_code: impl Into<String>,
    ) -> Result<(), ManagerControllerStopped> {
        self.commands
            .send(ManagerControllerCommand::UpdateFederation(
                invite_code.into(),
            ))
            .map_err(|_| ManagerControllerStopped)
    }

    pub fn send_command(
        &self,
        machine_id: EndpointId,
        command: ManagerCommand,
    ) -> Result<CommandId, ManagerControllerStopped> {
        let command_id = CommandId::new();
        self.commands
            .send(ManagerControllerCommand::SendCommand {
                machine_id,
                envelope: CommandEnvelope {
                    id: command_id,
                    command,
                },
            })
            .map_err(|_| ManagerControllerStopped)?;
        Ok(command_id)
    }

    pub fn export_funds(&self) -> Result<(), ManagerControllerStopped> {
        self.commands
            .send(ManagerControllerCommand::ExportFunds)
            .map_err(|_| ManagerControllerStopped)
    }
}

impl Drop for ManagerController {
    fn drop(&mut self) {
        let _ = self.commands.send(ManagerControllerCommand::Shutdown);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerControllerStopped;

impl fmt::Display for ManagerControllerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Vendimint manager controller stopped")
    }
}

impl std::error::Error for ManagerControllerStopped {}

pub struct ManagerClaim {
    machine_id: EndpointId,
    pin: String,
    response: Option<oneshot::Sender<bool>>,
}

impl ManagerClaim {
    pub const fn machine_id(&self) -> EndpointId {
        self.machine_id
    }

    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn respond(mut self, accepted: bool) -> Result<(), bool> {
        self.response.take().ok_or(accepted)?.send(accepted)
    }
}

impl fmt::Debug for ManagerClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagerClaim")
            .field("machine_id", &self.machine_id)
            .field("pin", &self.pin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum ManagerControllerEvent {
    Ready,
    ClaimPrepared(ManagerClaim),
    ClaimFailed(String),
    FederationUpdated,
    FederationUpdateFailed(String),
    BalanceUpdated {
        msats: u64,
    },
    FederationStatusUpdated {
        federation_ids: Vec<String>,
    },
    FundsExported(Vec<EcashExport>),
    FundsExportFailed(String),
    CommandCompleted {
        machine_id: EndpointId,
        command_id: CommandId,
        result: CommandResult,
    },
    CommandFailed {
        machine_id: EndpointId,
        error: String,
    },
    MachinesChanged(Vec<EndpointId>),
    SnapshotUpdated {
        machine_id: EndpointId,
        snapshot: KioskSnapshot,
    },
    EventsReceived {
        machine_id: EndpointId,
        events: Vec<EventEnvelope>,
    },
    MachineUnavailable {
        machine_id: EndpointId,
        error: String,
    },
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcashExport {
    pub federation_id: String,
    pub amount_msats: u64,
    pub token: String,
}

enum ManagerControllerCommand {
    BeginClaim(String),
    UpdateFederation(String),
    SendCommand {
        machine_id: EndpointId,
        envelope: CommandEnvelope,
    },
    ExportFunds,
    Shutdown,
}

struct ObservedManagerState {
    machines: Vec<EndpointId>,
    snapshots: HashMap<EndpointId, KioskSnapshot>,
    errors: HashMap<EndpointId, String>,
    event_sequences: HashMap<EndpointId, EventSequence>,
    balance_msats: Option<u64>,
    federation_ids: Vec<String>,
}

impl ObservedManagerState {
    fn new() -> Self {
        Self {
            machines: Vec::new(),
            snapshots: HashMap::new(),
            errors: HashMap::new(),
            event_sequences: HashMap::new(),
            balance_msats: None,
            federation_ids: Vec::new(),
        }
    }
}

async fn run(
    config: ManagerControllerConfig,
    mut commands: mpsc::UnboundedReceiver<ManagerControllerCommand>,
    events: mpsc::UnboundedSender<ManagerControllerEvent>,
) {
    let mut manager = match Manager::new(config.storage_path(), config.network()).await {
        Ok(manager) => manager,
        Err(error) => {
            let _ = events.send(ManagerControllerEvent::Unavailable(error.to_string()));
            return;
        }
    };
    let _ = events.send(ManagerControllerEvent::Ready);
    let mut observed = ObservedManagerState::new();
    refresh_manager_state(&manager, &events, &mut observed).await;
    let mut poll = tokio::time::interval(STATE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(ManagerControllerCommand::BeginClaim(payload)) => {
                    begin_claim(&manager, &events, &payload).await;
                }
                Some(ManagerControllerCommand::UpdateFederation(invite)) => {
                    update_federation(&manager, &events, &invite).await;
                }
                Some(ManagerControllerCommand::SendCommand { machine_id, envelope }) => {
                    send_command(&manager, &events, machine_id, envelope).await;
                }
                Some(ManagerControllerCommand::ExportFunds) => {
                    export_funds(&manager, &events).await;
                }
                Some(ManagerControllerCommand::Shutdown) | None => break,
            },
            _ = poll.tick() => {
                refresh_manager_state(&manager, &events, &mut observed).await;
            }
        }
    }
    let _ = manager.shutdown().await;
}

async fn begin_claim(
    manager: &Manager,
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    payload: &str,
) {
    let result = async {
        let endpoint: EndpointAddr = serde_json::from_str(payload.trim())?;
        let machine_id = endpoint.id;
        let (pin, response) = tokio::time::timeout(CLAIM_TIMEOUT, manager.claim_machine(endpoint))
            .await
            .map_err(|_| anyhow::anyhow!("timed out connecting to the kiosk"))??;
        Ok::<ManagerClaim, anyhow::Error>(ManagerClaim {
            machine_id,
            pin: pin.to_string(),
            response: Some(response),
        })
    }
    .await;
    let event = match result {
        Ok(claim) => ManagerControllerEvent::ClaimPrepared(claim),
        Err(error) => ManagerControllerEvent::ClaimFailed(error.to_string()),
    };
    let _ = events.send(event);
}

async fn update_federation(
    manager: &Manager,
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    invite: &str,
) {
    let result = match invite.trim().parse::<InviteCode>() {
        Ok(invite) => manager.update_federation(invite).await,
        Err(error) => Err(error),
    };
    let event = match result {
        Ok(()) => ManagerControllerEvent::FederationUpdated,
        Err(error) => ManagerControllerEvent::FederationUpdateFailed(error.to_string()),
    };
    let _ = events.send(event);
}

async fn send_command(
    manager: &Manager,
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    machine_id: EndpointId,
    envelope: CommandEnvelope,
) {
    let command_id = envelope.id;
    let response = request_command_with_retries(manager, &machine_id, &envelope).await;
    let event = match response {
        Ok(ManagerResponse::CommandResult {
            command_id: returned_id,
            result,
        }) if returned_id == command_id => ManagerControllerEvent::CommandCompleted {
            machine_id,
            command_id,
            result,
        },
        Ok(response) => ManagerControllerEvent::CommandFailed {
            machine_id,
            error: format!("kiosk returned an unexpected response: {response:?}"),
        },
        Err(error) => ManagerControllerEvent::CommandFailed {
            machine_id,
            error: error.to_string(),
        },
    };
    let _ = events.send(event);
}

async fn request_command_with_retries(
    manager: &Manager,
    machine_id: &EndpointId,
    envelope: &CommandEnvelope,
) -> anyhow::Result<ManagerResponse> {
    let mut last_error = "manager command was not attempted".to_owned();
    for attempt in 0..COMMAND_ATTEMPTS {
        match tokio::time::timeout(
            MANAGER_REQUEST_TIMEOUT,
            request(
                manager,
                machine_id,
                ManagerRequest::Command(envelope.clone()),
            ),
        )
        .await
        {
            Ok(Ok(response)) => return Ok(response),
            Ok(Err(error)) => last_error = error.to_string(),
            Err(_) => "timed out waiting for the kiosk".clone_into(&mut last_error),
        }
        if attempt + 1 < COMMAND_ATTEMPTS {
            tokio::time::sleep(COMMAND_RETRY_DELAY).await;
        }
    }
    anyhow::bail!("manager command failed after {COMMAND_ATTEMPTS} attempts: {last_error}")
}

async fn refresh_manager_state(
    manager: &Manager,
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    observed: &mut ObservedManagerState,
) {
    let balance_msats = manager.get_local_balance().await.msats;
    if observed.balance_msats != Some(balance_msats) {
        observed.balance_msats = Some(balance_msats);
        let _ = events.send(ManagerControllerEvent::BalanceUpdated {
            msats: balance_msats,
        });
    }
    let Ok(mut machines) = manager.list_machine_ids().await else {
        return;
    };
    machines.sort_unstable_by_key(ToString::to_string);
    if machines != observed.machines {
        observed.machines.clone_from(&machines);
        observed
            .snapshots
            .retain(|machine_id, _| machines.contains(machine_id));
        observed
            .errors
            .retain(|machine_id, _| machines.contains(machine_id));
        observed
            .event_sequences
            .retain(|machine_id, _| machines.contains(machine_id));
        let _ = events.send(ManagerControllerEvent::MachinesChanged(machines.clone()));
    }
    for machine_id in machines {
        if refresh_snapshot(manager, events, observed, machine_id).await {
            refresh_events(manager, events, observed, machine_id).await;
        }
    }
    refresh_federation_status(manager, events, observed).await;
}

async fn configured_federations(
    manager: &Manager,
) -> anyhow::Result<Vec<fedimint_core::invite_code::InviteCode>> {
    let mut seen = HashSet::new();
    let mut federations = Vec::new();
    for machine_id in manager.list_machine_ids().await? {
        let Some(config) = manager.get_machine_config(&machine_id).await? else {
            continue;
        };
        let federation = config.federation_invite_code;
        if seen.insert(federation.federation_id()) {
            federations.push(federation);
        }
    }
    Ok(federations)
}

async fn refresh_federation_status(
    manager: &Manager,
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    observed: &mut ObservedManagerState,
) {
    let Ok(federations) = configured_federations(manager).await else {
        return;
    };
    let mut federation_ids = federations
        .iter()
        .map(|invite| invite.federation_id().to_string())
        .collect::<Vec<_>>();
    federation_ids.sort_unstable();
    if federation_ids != observed.federation_ids {
        observed.federation_ids.clone_from(&federation_ids);
        let _ = events.send(ManagerControllerEvent::FederationStatusUpdated { federation_ids });
    }
}

async fn export_funds(manager: &Manager, events: &mpsc::UnboundedSender<ManagerControllerEvent>) {
    let result = async {
        let federations = configured_federations(manager).await?;
        if federations.is_empty() {
            anyhow::bail!("configure a federation on at least one paired kiosk first");
        }
        let mut exports = Vec::new();
        for invite in federations {
            let federation_id = invite.federation_id();
            if let Some(notes) = manager
                .sweep_all_ecash_notes(federation_id, ECASH_EXPORT_RECLAIM_AFTER, true, None::<()>)
                .await?
            {
                exports.push(EcashExport {
                    federation_id: federation_id.to_string(),
                    amount_msats: notes.total_amount().msats,
                    token: notes.to_string(),
                });
            }
        }
        if exports.is_empty() {
            anyhow::bail!("the manager wallet has no funds available to export");
        }
        Ok::<_, anyhow::Error>(exports)
    }
    .await;
    let event = match result {
        Ok(exports) => ManagerControllerEvent::FundsExported(exports),
        Err(error) => ManagerControllerEvent::FundsExportFailed(error.to_string()),
    };
    let _ = events.send(event);
}

async fn refresh_snapshot(
    manager: &Manager,
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    observed: &mut ObservedManagerState,
    machine_id: EndpointId,
) -> bool {
    let result = tokio::time::timeout(
        MANAGER_REQUEST_TIMEOUT,
        request(manager, &machine_id, ManagerRequest::GetSnapshot),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out waiting for the kiosk")));
    match result {
        Ok(ManagerResponse::Snapshot(snapshot)) => {
            let observed_sequence = observed.event_sequences.entry(machine_id).or_default();
            if snapshot.through_sequence < *observed_sequence {
                *observed_sequence = EventSequence::default();
            }
            let recovered = observed.errors.remove(&machine_id).is_some();
            if recovered || observed.snapshots.get(&machine_id) != Some(&snapshot) {
                observed.snapshots.insert(machine_id, snapshot.clone());
                let _ = events.send(ManagerControllerEvent::SnapshotUpdated {
                    machine_id,
                    snapshot,
                });
            }
            true
        }
        Ok(response) => {
            publish_machine_error(
                events,
                observed,
                machine_id,
                format!("kiosk returned an unexpected response: {response:?}"),
            );
            false
        }
        Err(error) => {
            publish_machine_error(events, observed, machine_id, error.to_string());
            false
        }
    }
}

async fn refresh_events(
    manager: &Manager,
    event_sender: &mpsc::UnboundedSender<ManagerControllerEvent>,
    observed: &mut ObservedManagerState,
    machine_id: EndpointId,
) {
    let after = observed
        .event_sequences
        .get(&machine_id)
        .copied()
        .unwrap_or_default();
    let response = tokio::time::timeout(
        MANAGER_REQUEST_TIMEOUT,
        request(
            manager,
            &machine_id,
            ManagerRequest::SubscribeEvents { after },
        ),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out waiting for kiosk events")));
    let Ok(ManagerResponse::Events(events)) = response else {
        return;
    };
    if events.is_empty() {
        return;
    }
    if let Some(sequence) = events.iter().map(|event| event.sequence).max() {
        observed.event_sequences.insert(machine_id, sequence);
    }
    let _ = event_sender.send(ManagerControllerEvent::EventsReceived { machine_id, events });
}

fn publish_machine_error(
    events: &mpsc::UnboundedSender<ManagerControllerEvent>,
    observed: &mut ObservedManagerState,
    machine_id: EndpointId,
    error: String,
) {
    if observed.errors.get(&machine_id) == Some(&error) {
        return;
    }
    observed.errors.insert(machine_id, error.clone());
    let _ = events.send(ManagerControllerEvent::MachineUnavailable { machine_id, error });
}
