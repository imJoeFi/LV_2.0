use crate::{IncomingManagerRequest, ManagerProtocolHandler};
use bitcoin::{hashes::Hash, Network};
use fedimint_core::{core::OperationId, Amount};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_remote_client::FinalRemoteReceiveOperationState;
use lv_core::{LightningInvoice, Msats, PurchaseId};
use std::{
    fmt, io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet};
use vendimint::{Machine, MachineState};

const STATE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MANAGER_REQUEST_CAPACITY: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// Persistent configuration for the Vendimint machine owned by the kiosk.
#[derive(Debug, Clone)]
pub struct PaymentControllerConfig {
    storage_path: PathBuf,
    network: Network,
}

impl PaymentControllerConfig {
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

/// A dedicated background owner for Vendimint's machine identity and wallet.
pub struct PaymentController {
    commands: mpsc::UnboundedSender<PaymentControllerCommand>,
    events: mpsc::UnboundedReceiver<PaymentControllerEvent>,
}

impl PaymentController {
    pub fn spawn(config: PaymentControllerConfig) -> io::Result<Self> {
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (event_sender, events) = mpsc::unbounded_channel();
        thread::Builder::new()
            .name("lv-kiosk-vendimint".to_owned())
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
                        let _ = event_sender.send(PaymentControllerEvent::Unavailable(format!(
                            "could not start the Vendimint runtime: {error}"
                        )));
                    }
                }
            })?;
        Ok(Self { commands, events })
    }

    pub fn try_event(&mut self) -> Option<PaymentControllerEvent> {
        self.events.try_recv().ok()
    }

    pub fn create_invoice(
        &self,
        purchase_id: PurchaseId,
        amount: Msats,
        description: impl Into<String>,
        expiry: Duration,
    ) -> Result<(), PaymentControllerStopped> {
        let expiry_secs = u32::try_from(expiry.as_secs()).map_err(|_| PaymentControllerStopped)?;
        self.commands
            .send(PaymentControllerCommand::CreateInvoice {
                purchase_id,
                amount,
                description: description.into(),
                expiry_secs,
            })
            .map_err(|_| PaymentControllerStopped)
    }

    /// Reattaches to an invoice operation restored from durable kiosk state.
    pub fn observe_invoice(
        &self,
        purchase_id: PurchaseId,
        operation_id: [u8; 32],
    ) -> Result<(), PaymentControllerStopped> {
        self.commands
            .send(PaymentControllerCommand::ObserveInvoice {
                purchase_id,
                operation_id,
            })
            .map_err(|_| PaymentControllerStopped)
    }
}

impl Drop for PaymentController {
    fn drop(&mut self) {
        let _ = self.commands.send(PaymentControllerCommand::Shutdown);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaymentControllerStopped;

impl fmt::Display for PaymentControllerStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Vendimint payment controller stopped")
    }
}

impl std::error::Error for PaymentControllerStopped {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentMachineState {
    Unclaimed { pairing_payload: String },
    ClaimedUnconfigured,
    Ready,
}

/// A single-use capability to accept or reject an incoming manager claim.
pub struct ClaimRequest {
    pin: String,
    response: Option<tokio::sync::oneshot::Sender<bool>>,
}

impl ClaimRequest {
    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn respond(mut self, accepted: bool) -> Result<(), bool> {
        self.response.take().ok_or(accepted)?.send(accepted)
    }
}

impl fmt::Debug for ClaimRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaimRequest")
            .field("pin", &self.pin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum PaymentControllerEvent {
    MachineStateChanged(PaymentMachineState),
    ClaimRequested(ClaimRequest),
    InvoiceCreated {
        purchase_id: PurchaseId,
        invoice: LightningInvoice,
    },
    InvoiceCreationFailed {
        purchase_id: PurchaseId,
        error: String,
    },
    InvoiceFunded {
        purchase_id: PurchaseId,
    },
    InvoiceExpired {
        purchase_id: PurchaseId,
        expired_at_unix_millis: u64,
    },
    ManagerRequest(IncomingManagerRequest),
    Unavailable(String),
}

enum PaymentControllerCommand {
    CreateInvoice {
        purchase_id: PurchaseId,
        amount: Msats,
        description: String,
        expiry_secs: u32,
    },
    ObserveInvoice {
        purchase_id: PurchaseId,
        operation_id: [u8; 32],
    },
    Shutdown,
}

async fn run(
    config: PaymentControllerConfig,
    mut commands: mpsc::UnboundedReceiver<PaymentControllerCommand>,
    events: mpsc::UnboundedSender<PaymentControllerEvent>,
) {
    let (manager_handler, mut manager_requests) =
        ManagerProtocolHandler::channel(MANAGER_REQUEST_CAPACITY);
    let builder =
        match manager_handler.register(Machine::builder(config.storage_path(), config.network())) {
            Ok(builder) => builder,
            Err(error) => {
                let _ = events.send(PaymentControllerEvent::Unavailable(error.to_string()));
                return;
            }
        };
    let machine = match builder.build().await {
        Ok(machine) => Arc::new(machine),
        Err(error) => {
            let _ = events.send(PaymentControllerEvent::Unavailable(error.to_string()));
            return;
        }
    };

    let mut last_state = None;
    publish_machine_state(&machine, &events, &mut last_state).await;
    let mut state_poll = tokio::time::interval(STATE_POLL_INTERVAL);
    state_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(PaymentControllerCommand::CreateInvoice {
                    purchase_id,
                    amount,
                    description,
                    expiry_secs,
                }) => spawn_invoice_creation(
                    &mut tasks,
                    machine.clone(),
                    events.clone(),
                    purchase_id,
                    amount,
                    description,
                    expiry_secs,
                ),
                Some(PaymentControllerCommand::ObserveInvoice {
                    purchase_id,
                    operation_id,
                }) => spawn_invoice_observer(
                    &mut tasks,
                    machine.clone(),
                    events.clone(),
                    purchase_id,
                    operation_id,
                ),
                Some(PaymentControllerCommand::Shutdown) | None => break,
            },
            claim = machine.await_next_incoming_claim_request() => {
                if let Some((pin, response)) = claim {
                    let _ = events.send(PaymentControllerEvent::ClaimRequested(ClaimRequest {
                        pin: pin.to_string(),
                        response: Some(response),
                    }));
                }
            }
            request = manager_requests.recv() => {
                if let Some(request) = request {
                    let _ = events.send(PaymentControllerEvent::ManagerRequest(request));
                }
            }
            _ = state_poll.tick() => {
                publish_machine_state(&machine, &events, &mut last_state).await;
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    if let Ok(mut machine) = Arc::try_unwrap(machine) {
        let _ = machine.shutdown().await;
    }
}

async fn publish_machine_state(
    machine: &Machine,
    events: &mpsc::UnboundedSender<PaymentControllerEvent>,
    previous: &mut Option<PaymentMachineState>,
) {
    let state = match machine.get_machine_state().await {
        Ok(MachineState::Unclaimed(endpoint)) => match serde_json::to_string(&endpoint) {
            Ok(pairing_payload) => PaymentMachineState::Unclaimed { pairing_payload },
            Err(error) => {
                let _ = events.send(PaymentControllerEvent::Unavailable(format!(
                    "could not encode the Vendimint pairing address: {error}"
                )));
                return;
            }
        },
        Ok(MachineState::Claimed(None)) => PaymentMachineState::ClaimedUnconfigured,
        Ok(MachineState::Claimed(Some(_))) => PaymentMachineState::Ready,
        Err(error) => {
            let _ = events.send(PaymentControllerEvent::Unavailable(error.to_string()));
            return;
        }
    };
    if previous.as_ref() != Some(&state) {
        *previous = Some(state.clone());
        let _ = events.send(PaymentControllerEvent::MachineStateChanged(state));
    }
}

fn spawn_invoice_creation(
    tasks: &mut JoinSet<()>,
    machine: Arc<Machine>,
    events: mpsc::UnboundedSender<PaymentControllerEvent>,
    purchase_id: PurchaseId,
    amount: Msats,
    description: String,
    expiry_secs: u32,
) {
    tasks.spawn(async move {
        let result = machine
            .receive_payment(
                Amount::from_msats(amount.as_u64()),
                expiry_secs,
                Bolt11InvoiceDescription::Direct(description),
                None,
            )
            .await;
        let (invoice, operation_id) = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = events.send(PaymentControllerEvent::InvoiceCreationFailed {
                    purchase_id,
                    error: error.to_string(),
                });
                return;
            }
        };
        let Some(expires_at) = invoice.expires_at() else {
            let _ = events.send(PaymentControllerEvent::InvoiceCreationFailed {
                purchase_id,
                error: "Vendimint returned an invoice with an invalid expiration".to_owned(),
            });
            return;
        };
        let invoice = LightningInvoice {
            bolt11: invoice.to_string(),
            operation_id: operation_id.0,
            payment_hash: invoice.payment_hash().to_byte_array(),
            expires_at_unix_seconds: expires_at.as_secs(),
        };
        let _ = events.send(PaymentControllerEvent::InvoiceCreated {
            purchase_id,
            invoice,
        });
        observe_final_state(machine, events, purchase_id, operation_id).await;
    });
}

fn spawn_invoice_observer(
    tasks: &mut JoinSet<()>,
    machine: Arc<Machine>,
    events: mpsc::UnboundedSender<PaymentControllerEvent>,
    purchase_id: PurchaseId,
    operation_id: [u8; 32],
) {
    tasks.spawn(observe_final_state(
        machine,
        events,
        purchase_id,
        OperationId(operation_id),
    ));
}

async fn observe_final_state(
    machine: Arc<Machine>,
    events: mpsc::UnboundedSender<PaymentControllerEvent>,
    purchase_id: PurchaseId,
    operation_id: OperationId,
) {
    match machine
        .await_receive_payment_final_state(operation_id)
        .await
    {
        Ok(FinalRemoteReceiveOperationState::Funded) => {
            let _ = events.send(PaymentControllerEvent::InvoiceFunded { purchase_id });
        }
        Ok(FinalRemoteReceiveOperationState::Expired) => {
            let _ = events.send(PaymentControllerEvent::InvoiceExpired {
                purchase_id,
                expired_at_unix_millis: unix_millis_now(),
            });
        }
        Err(error) => {
            let _ = events.send(PaymentControllerEvent::Unavailable(format!(
                "could not observe Lightning purchase {purchase_id}: {error}"
            )));
        }
    }
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
