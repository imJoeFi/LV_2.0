use lv_mdb::{
    ItemNumber, Level1Amount, MdbConfig, MdbDevice, MdbSession, PendingVend, SessionEndReason,
    SessionEvent, SessionFunds, VendDecisionError, VendId,
};
use std::io;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub struct MdbController {
    commands: mpsc::UnboundedSender<ControllerCommand>,
    events: mpsc::UnboundedReceiver<ControllerEvent>,
}

impl MdbController {
    pub fn spawn(config: MdbConfig) -> io::Result<Self> {
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (event_sender, events) = mpsc::unbounded_channel();
        thread::Builder::new()
            .name("lv-kiosk-mdb".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(run(config, command_receiver, event_sender)),
                    Err(error) => {
                        let _ = event_sender.send(ControllerEvent::Unavailable(format!(
                            "could not start the MDB runtime: {error}"
                        )));
                    }
                }
            })?;
        Ok(Self { commands, events })
    }

    pub fn try_event(&mut self) -> Option<ControllerEvent> {
        self.events.try_recv().ok()
    }

    pub fn approve(&self, vend_id: VendId, amount: Level1Amount) -> Result<(), ControllerStopped> {
        self.commands
            .send(ControllerCommand::Approve { vend_id, amount })
            .map_err(|_| ControllerStopped)
    }

    pub fn deny(&self, vend_id: VendId) -> Result<(), ControllerStopped> {
        self.commands
            .send(ControllerCommand::Deny { vend_id })
            .map_err(|_| ControllerStopped)
    }
}

impl Drop for MdbController {
    fn drop(&mut self) {
        let _ = self.commands.send(ControllerCommand::Shutdown);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerStopped;

impl std::fmt::Display for ControllerStopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MDB controller stopped")
    }
}

#[derive(Debug)]
pub enum ControllerEvent {
    Connecting,
    SessionReady,
    VendRequested {
        vend_id: VendId,
        item: ItemNumber,
        requested_price: Level1Amount,
    },
    VendCancelled {
        vend_id: VendId,
    },
    VendDecisionExpired {
        vend_id: VendId,
        error: VendDecisionError,
    },
    DecisionAccepted {
        vend_id: VendId,
    },
    DecisionFailed {
        vend_id: VendId,
        error: DecisionFailure,
    },
    VendSucceeded {
        vend_id: VendId,
        reported_item: Option<ItemNumber>,
    },
    VendFailed {
        vend_id: VendId,
    },
    SessionEnded {
        reason: SessionEndReason,
    },
    Unavailable(String),
    Fault(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionFailure {
    Mdb(VendDecisionError),
    NoActiveVend,
}

enum ControllerCommand {
    Approve {
        vend_id: VendId,
        amount: Level1Amount,
    },
    Deny {
        vend_id: VendId,
    },
    Shutdown,
}

enum ConnectedExit {
    Reconnect(String),
    Shutdown,
}

enum SessionExit {
    Ended(MdbSession),
    Reconnect(String),
    Shutdown(MdbSession),
}

async fn run(
    config: MdbConfig,
    mut commands: mpsc::UnboundedReceiver<ControllerCommand>,
    events: mpsc::UnboundedSender<ControllerEvent>,
) {
    loop {
        let _ = events.send(ControllerEvent::Connecting);
        let trace = |line: String| {
            eprintln!(
                "{}  MDB  {line}",
                chrono::Local::now().format("%H:%M:%S%.3f")
            );
        };
        let device = match MdbDevice::connect_with_trace(&config, trace).await {
            Ok(device) => device,
            Err(error) => {
                let _ = events.send(ControllerEvent::Unavailable(error.to_string()));
                if wait_to_retry(&mut commands, &events).await {
                    return;
                }
                continue;
            }
        };

        match run_connected(device, &mut commands, &events).await {
            ConnectedExit::Reconnect(error) => {
                let _ = events.send(ControllerEvent::Unavailable(error));
                if wait_to_retry(&mut commands, &events).await {
                    return;
                }
            }
            ConnectedExit::Shutdown => return,
        }
    }
}

async fn run_connected(
    mut device: MdbDevice,
    commands: &mut mpsc::UnboundedReceiver<ControllerCommand>,
    events: &mpsc::UnboundedSender<ControllerEvent>,
) -> ConnectedExit {
    loop {
        let session = match device.begin_session(SessionFunds::Unknown).await {
            Ok(session) => session,
            Err(error) => {
                let (error, recovered_device) = error.into_parts();
                device = recovered_device;
                let _ = events.send(ControllerEvent::Unavailable(format!(
                    "could not arm an MDB session: {error}"
                )));
                if wait_to_retry(commands, events).await {
                    let _ = device.shutdown().await;
                    return ConnectedExit::Shutdown;
                }
                continue;
            }
        };
        let _ = events.send(ControllerEvent::SessionReady);

        match run_session(session, commands, events).await {
            SessionExit::Ended(session) => match session.finish().await {
                Ok(ended) => device = ended.into_device(),
                Err(error) => return ConnectedExit::Reconnect(error.to_string()),
            },
            SessionExit::Reconnect(error) => return ConnectedExit::Reconnect(error),
            SessionExit::Shutdown(session) => {
                if let Ok(ended) = session.finish().await {
                    let _ = ended.into_device().shutdown().await;
                }
                return ConnectedExit::Shutdown;
            }
        }
    }
}

async fn run_session(
    mut session: MdbSession,
    commands: &mut mpsc::UnboundedReceiver<ControllerCommand>,
    events: &mpsc::UnboundedSender<ControllerEvent>,
) -> SessionExit {
    let mut pending: Option<PendingVend> = None;
    loop {
        tokio::select! {
            biased;
            event = session.next_event() => {
                let event = match event {
                    Ok(event) => event,
                    Err(error) => return SessionExit::Reconnect(error.to_string()),
                };
                match event {
                    SessionEvent::VendRequested(vend) => {
                        if pending.is_some() {
                            let vend_id = vend.id();
                            let result = vend.deny().await;
                            let detail = result.map_or_else(
                                |error| format!("could not deny overlapping vend {vend_id}: {error}"),
                                |()| format!("denied overlapping vend {vend_id}"),
                            );
                            let _ = events.send(ControllerEvent::Fault(detail));
                        } else {
                            let event = ControllerEvent::VendRequested {
                                vend_id: vend.id(),
                                item: vend.item_number(),
                                requested_price: vend.requested_price(),
                            };
                            pending = Some(vend);
                            let _ = events.send(event);
                        }
                    }
                    SessionEvent::VendCancelled { vend_id } => {
                        clear_matching_pending(&mut pending, vend_id);
                        let _ = events.send(ControllerEvent::VendCancelled { vend_id });
                    }
                    SessionEvent::VendDecisionExpired { vend_id, error } => {
                        clear_matching_pending(&mut pending, vend_id);
                        let _ = events.send(ControllerEvent::VendDecisionExpired { vend_id, error });
                    }
                    SessionEvent::VendSucceeded { vend_id, reported_item, .. } => {
                        let _ = events.send(ControllerEvent::VendSucceeded {
                            vend_id,
                            reported_item,
                        });
                    }
                    SessionEvent::VendFailed { vend_id, .. } => {
                        let _ = events.send(ControllerEvent::VendFailed { vend_id });
                    }
                    SessionEvent::Ended { reason } => {
                        drop(pending.take());
                        let _ = events.send(ControllerEvent::SessionEnded { reason });
                        return SessionExit::Ended(session);
                    }
                }
            }
            command = commands.recv() => match command {
                Some(ControllerCommand::Approve { vend_id, amount }) => {
                    decide(&mut pending, vend_id, Some(amount), events).await;
                }
                Some(ControllerCommand::Deny { vend_id }) => {
                    decide(&mut pending, vend_id, None, events).await;
                }
                Some(ControllerCommand::Shutdown) | None => {
                    drop(pending.take());
                    return SessionExit::Shutdown(session);
                }
            }
        }
    }
}

async fn decide(
    pending: &mut Option<PendingVend>,
    vend_id: VendId,
    approval: Option<Level1Amount>,
    events: &mpsc::UnboundedSender<ControllerEvent>,
) {
    let Some(vend) = pending.take() else {
        let _ = events.send(ControllerEvent::DecisionFailed {
            vend_id,
            error: DecisionFailure::NoActiveVend,
        });
        return;
    };
    if vend.id() != vend_id {
        *pending = Some(vend);
        let _ = events.send(ControllerEvent::DecisionFailed {
            vend_id,
            error: DecisionFailure::NoActiveVend,
        });
        return;
    }

    let result = match approval {
        Some(amount) => vend.approve_for(amount).await,
        None => vend.deny().await,
    };
    let event = result.map_or_else(
        |error| ControllerEvent::DecisionFailed {
            vend_id,
            error: DecisionFailure::Mdb(error),
        },
        |()| ControllerEvent::DecisionAccepted { vend_id },
    );
    let _ = events.send(event);
}

fn clear_matching_pending(pending: &mut Option<PendingVend>, vend_id: VendId) {
    if pending.as_ref().is_some_and(|vend| vend.id() == vend_id) {
        drop(pending.take());
    }
}

async fn wait_to_retry(
    commands: &mut mpsc::UnboundedReceiver<ControllerCommand>,
    events: &mpsc::UnboundedSender<ControllerEvent>,
) -> bool {
    let delay = sleep(RECONNECT_DELAY);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            () = &mut delay => return false,
            command = commands.recv() => match command {
                Some(
                    ControllerCommand::Approve { vend_id, .. }
                    | ControllerCommand::Deny { vend_id },
                ) => {
                    let _ = events.send(ControllerEvent::DecisionFailed {
                        vend_id,
                        error: DecisionFailure::NoActiveVend,
                    });
                }
                Some(ControllerCommand::Shutdown) | None => return true,
            }
        }
    }
}
