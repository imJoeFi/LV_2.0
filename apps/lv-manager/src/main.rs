use clap::Parser;
use iced::widget::{button, column, container, row, scrollable, text, text_input, Column};
use iced::{time, window, Element, Length, Size, Subscription, Task, Theme};
use iroh::EndpointId;
use lv_core::{
    AdminPin, AssistanceResolution, CommandResult, EventEnvelope, FreeVendScope, KioskSnapshot,
    ManagerCommand, ManagerEvent, PaymentSummary, PurchaseId, PurchaseSummary,
    PurchaseSummaryState, SlotHealth, SlotId, SlotSnapshot, VendAuthorizationSnapshot,
};
use lv_vendimint::{
    ManagerClaim, ManagerController, ManagerControllerConfig, ManagerControllerEvent,
};
use std::{collections::HashMap, path::PathBuf, time::Duration};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(about = "LightningVEND manager UI")]
struct Args {
    /// Persistent Vendimint manager identity and wallet directory.
    #[arg(long, default_value = "lv-manager-data")]
    data: PathBuf,
}

fn main() -> iced::Result {
    let args = Args::parse();
    iced::application(
        move || ApplicationState::boot(&args),
        ApplicationState::update,
        ApplicationState::view,
    )
    .subscription(ApplicationState::subscription)
    .theme(Theme::Dark)
    .title("LightningVEND Manager")
    .window(window::Settings {
        size: Size::new(1200.0, 800.0),
        ..window::Settings::default()
    })
    .run()
}

enum ApplicationState {
    Running(Box<ManagerApp>),
    Failed(String),
}

impl ApplicationState {
    fn boot(args: &Args) -> Self {
        match ManagerApp::boot(args) {
            Ok(app) => Self::Running(Box::new(app)),
            Err(error) => Self::Failed(error),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match self {
            Self::Running(app) => app.update(message),
            Self::Failed(_) => Task::none(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        match self {
            Self::Running(app) => app.view(),
            Self::Failed(error) => container(
                column![
                    text("Manager could not start").size(34),
                    text(error).size(18),
                    text("Correct the problem and restart the application.")
                ]
                .spacing(18),
            )
            .padding(36)
            .center(Length::Fill)
            .into(),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        match self {
            Self::Running(_) => time::every(EVENT_POLL_INTERVAL).map(|_| Message::Poll),
            Self::Failed(_) => Subscription::none(),
        }
    }
}

struct ManagerApp {
    controller: ManagerController,
    status: ManagerStatus,
    pairing_payload: String,
    pending_claim: Option<ManagerClaim>,
    federation_invite: String,
    kiosk_name_input: String,
    admin_pin_input: String,
    notice: Option<String>,
    machines: Vec<EndpointId>,
    selected_machine: Option<EndpointId>,
    snapshots: HashMap<EndpointId, KioskSnapshot>,
    machine_errors: HashMap<EndpointId, String>,
    activity: HashMap<EndpointId, Vec<EventEnvelope>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagerStatus {
    Starting,
    Ready,
    Unavailable(String),
}

#[derive(Debug, Clone)]
enum Message {
    Poll,
    PairingPayloadChanged(String),
    BeginClaim,
    ConfirmClaim,
    RejectClaim,
    FederationInviteChanged(String),
    UpdateFederation,
    SelectMachine(EndpointId),
    KioskNameChanged(String),
    SaveKioskName,
    AdminPinChanged(String),
    SaveAdminPin,
    InventoryChange(SlotId, i32),
    ResolveSlotAttention(SlotId),
    ArmFreeVend,
    ArmMaintenanceVend(SlotId),
    DisarmVendAuthorization,
    ResolveUncertain(PurchaseId, bool),
    ResolveAssistance(PurchaseId, AssistanceResolution),
}

impl ManagerApp {
    fn boot(args: &Args) -> Result<Self, String> {
        let controller = ManagerController::spawn(ManagerControllerConfig::mainnet(&args.data))
            .map_err(|error| format!("could not start the manager controller: {error}"))?;
        Ok(Self {
            controller,
            status: ManagerStatus::Starting,
            pairing_payload: String::new(),
            pending_claim: None,
            federation_invite: String::new(),
            kiosk_name_input: String::new(),
            admin_pin_input: String::new(),
            notice: None,
            machines: Vec::new(),
            selected_machine: None,
            snapshots: HashMap::new(),
            machine_errors: HashMap::new(),
            activity: HashMap::new(),
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Poll => self.poll(),
            Message::PairingPayloadChanged(payload) => {
                self.pairing_payload = payload;
                self.notice = None;
            }
            Message::BeginClaim => self.begin_claim(),
            Message::ConfirmClaim => self.respond_to_claim(true),
            Message::RejectClaim => self.respond_to_claim(false),
            Message::FederationInviteChanged(invite) => {
                self.federation_invite = invite;
                self.notice = None;
            }
            Message::UpdateFederation => self.update_federation(),
            Message::SelectMachine(machine_id) => self.select_machine(machine_id),
            Message::KioskNameChanged(name) => {
                if name.chars().count() <= 80 {
                    self.kiosk_name_input = name;
                }
            }
            Message::SaveKioskName => self.save_kiosk_name(),
            Message::AdminPinChanged(pin) => {
                if pin.len() <= 12 && pin.bytes().all(|byte| byte.is_ascii_digit()) {
                    self.admin_pin_input = pin;
                }
            }
            Message::SaveAdminPin => self.save_admin_pin(),
            Message::InventoryChange(slot, change) => self.change_inventory(&slot, change),
            Message::ResolveSlotAttention(slot) => self.resolve_slot_attention(slot),
            Message::ArmFreeVend => self.send_selected_command(ManagerCommand::ArmFreeVend {
                scope: FreeVendScope::AnyConfiguredSlot,
                expires_in_seconds: 30,
            }),
            Message::ArmMaintenanceVend(slot) => {
                self.send_selected_command(ManagerCommand::ArmMaintenanceVend {
                    slot,
                    expires_in_seconds: 30,
                });
            }
            Message::DisarmVendAuthorization => {
                self.send_selected_command(ManagerCommand::DisarmVendAuthorization);
            }
            Message::ResolveUncertain(purchase_id, dispensed) => {
                self.send_selected_command(ManagerCommand::ResolveUncertainVend {
                    purchase_id,
                    dispensed,
                });
            }
            Message::ResolveAssistance(purchase_id, resolution) => {
                self.send_selected_command(ManagerCommand::ResolveAssistance {
                    purchase_id,
                    resolution,
                    note: None,
                });
            }
        }
        Task::none()
    }

    fn poll(&mut self) {
        let mut events = Vec::new();
        while let Some(event) = self.controller.try_event() {
            events.push(event);
        }
        for event in events {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: ManagerControllerEvent) {
        match event {
            ManagerControllerEvent::Ready => self.status = ManagerStatus::Ready,
            ManagerControllerEvent::ClaimPrepared(claim) => {
                self.pending_claim = Some(claim);
                self.notice = None;
            }
            ManagerControllerEvent::ClaimFailed(error) => {
                self.notice = Some(format!("Could not start pairing: {error}"));
            }
            ManagerControllerEvent::FederationUpdated => {
                self.federation_invite.clear();
                self.notice = Some("Federation configuration saved and syncing.".to_owned());
            }
            ManagerControllerEvent::FederationUpdateFailed(error) => {
                self.notice = Some(format!("Could not configure the federation: {error}"));
            }
            ManagerControllerEvent::CommandCompleted {
                result,
                command_id: _,
                machine_id: _,
            } => match result {
                CommandResult::Applied { revision } => {
                    self.notice =
                        Some(format!("Kiosk command applied at revision {}.", revision.0));
                    self.admin_pin_input.clear();
                }
                CommandResult::Rejected { message, .. } => {
                    self.notice = Some(format!("Kiosk rejected the command: {message}"));
                }
            },
            ManagerControllerEvent::CommandFailed {
                machine_id: _,
                error,
            } => self.notice = Some(format!("Could not reach the kiosk: {error}")),
            ManagerControllerEvent::MachinesChanged(machines) => {
                let machine_added = machines.len() > self.machines.len();
                self.machines = machines;
                self.snapshots
                    .retain(|machine_id, _| self.machines.contains(machine_id));
                self.machine_errors
                    .retain(|machine_id, _| self.machines.contains(machine_id));
                self.activity
                    .retain(|machine_id, _| self.machines.contains(machine_id));
                if self
                    .selected_machine
                    .is_none_or(|selected| !self.machines.contains(&selected))
                {
                    self.selected_machine = self.machines.first().copied();
                }
                if machine_added {
                    self.notice = Some("Kiosk paired successfully.".to_owned());
                }
            }
            ManagerControllerEvent::SnapshotUpdated {
                machine_id,
                snapshot,
            } => {
                self.machine_errors.remove(&machine_id);
                if self.selected_machine == Some(machine_id) && self.kiosk_name_input.is_empty() {
                    self.kiosk_name_input = snapshot.name.clone().unwrap_or_default();
                }
                self.snapshots.insert(machine_id, snapshot);
            }
            ManagerControllerEvent::EventsReceived { machine_id, events } => {
                let activity = self.activity.entry(machine_id).or_default();
                activity.extend(events);
                activity.sort_unstable_by_key(|event| event.sequence);
                activity.dedup_by_key(|event| event.sequence);
            }
            ManagerControllerEvent::MachineUnavailable { machine_id, error } => {
                self.machine_errors.insert(machine_id, error);
            }
            ManagerControllerEvent::Unavailable(error) => {
                self.status = ManagerStatus::Unavailable(error);
            }
        }
    }

    fn begin_claim(&mut self) {
        if self.pairing_payload.trim().is_empty() {
            self.notice = Some("Paste the kiosk pairing payload first.".to_owned());
            return;
        }
        match self.controller.begin_claim(self.pairing_payload.clone()) {
            Ok(()) => self.notice = Some("Connecting to the kiosk…".to_owned()),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn respond_to_claim(&mut self, accepted: bool) {
        let Some(claim) = self.pending_claim.take() else {
            return;
        };
        if claim.respond(accepted).is_err() {
            self.notice = Some("The pairing request expired. Scan the kiosk again.".to_owned());
        } else if accepted {
            self.pairing_payload.clear();
            self.notice = Some("Pairing approved. Waiting for the kiosk…".to_owned());
        } else {
            self.notice = Some("Pairing rejected.".to_owned());
        }
    }

    fn update_federation(&mut self) {
        if self.federation_invite.trim().is_empty() {
            self.notice = Some("Paste a federation invite code first.".to_owned());
            return;
        }
        match self
            .controller
            .update_federation(self.federation_invite.clone())
        {
            Ok(()) => self.notice = Some("Joining the federation…".to_owned()),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn select_machine(&mut self, machine_id: EndpointId) {
        self.selected_machine = Some(machine_id);
        self.kiosk_name_input = self
            .snapshots
            .get(&machine_id)
            .and_then(|snapshot| snapshot.name.clone())
            .unwrap_or_default();
        self.admin_pin_input.clear();
        self.notice = None;
    }

    fn save_kiosk_name(&mut self) {
        self.send_selected_command(ManagerCommand::SetKioskName {
            name: self.kiosk_name_input.clone(),
        });
    }

    fn save_admin_pin(&mut self) {
        let pin = match AdminPin::parse(self.admin_pin_input.clone()) {
            Ok(pin) => pin,
            Err(error) => {
                self.notice = Some(error.to_string());
                return;
            }
        };
        let configured = self
            .selected_snapshot()
            .is_some_and(|snapshot| snapshot.admin_pin_configured);
        let command = if configured {
            ManagerCommand::ChangeAdminPin { pin }
        } else {
            ManagerCommand::SetInitialAdminPin { pin }
        };
        self.send_selected_command(command);
    }

    fn change_inventory(&mut self, slot: &SlotId, change: i32) {
        let Some(snapshot) = self.selected_snapshot() else {
            return;
        };
        let Some(current) = snapshot.slots.iter().find(|item| item.slot == *slot) else {
            return;
        };
        let quantity = if change.is_negative() {
            current.inventory.saturating_sub(change.unsigned_abs())
        } else {
            current.inventory.saturating_add(change.unsigned_abs())
        };
        self.send_selected_command(ManagerCommand::SetInventory {
            slot: slot.clone(),
            quantity,
            expected_revision: snapshot.revision,
        });
    }

    fn resolve_slot_attention(&mut self, slot: SlotId) {
        let Some(snapshot) = self.selected_snapshot() else {
            return;
        };
        self.send_selected_command(ManagerCommand::ResolveSlotAttention {
            slot,
            expected_revision: snapshot.revision,
        });
    }

    fn send_selected_command(&mut self, command: ManagerCommand) {
        let Some(machine_id) = self.selected_machine else {
            self.notice = Some("Select a kiosk first.".to_owned());
            return;
        };
        match self.controller.send_command(machine_id, command) {
            Ok(_) => self.notice = Some("Sending command to the kiosk…".to_owned()),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn selected_snapshot(&self) -> Option<&KioskSnapshot> {
        self.selected_machine
            .and_then(|machine_id| self.snapshots.get(&machine_id))
    }

    fn view(&self) -> Element<'_, Message> {
        let header = row![
            column![
                text("LightningVEND Manager").size(34),
                text(self.status_text()).size(15)
            ]
            .spacing(4)
            .width(Length::Fill),
            text(format!("{} kiosk(s)", self.machines.len())).size(17)
        ]
        .align_y(iced::Alignment::Center);
        let content = row![self.sidebar(), self.detail()].spacing(24);
        let mut page = column![header].spacing(18);
        if let Some(claim) = &self.pending_claim {
            page = page.push(Self::claim_confirmation(claim));
        }
        if let Some(notice) = &self.notice {
            page = page.push(
                container(text(notice).size(16))
                    .padding(12)
                    .width(Length::Fill)
                    .style(container::secondary),
            );
        }
        page = page.push(content);
        container(page)
            .padding(28)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let mut machine_list = Column::new().spacing(8);
        for machine_id in &self.machines {
            let label = self
                .snapshots
                .get(machine_id)
                .and_then(|snapshot| snapshot.name.as_deref())
                .map_or_else(|| short_machine_id(machine_id), str::to_owned);
            let mut machine_button = button(text(label).size(17)).padding(12).width(Length::Fill);
            if self.selected_machine != Some(*machine_id) {
                machine_button = machine_button.on_press(Message::SelectMachine(*machine_id));
            }
            machine_list = machine_list.push(machine_button);
        }
        if self.machines.is_empty() {
            machine_list = machine_list.push(text("No kiosks paired yet.").size(16));
        }
        let pairing = column![
            text("Pair another kiosk").size(20),
            text("Paste the payload encoded by the QR shown on the kiosk.").size(14),
            text_input("Kiosk pairing payload", &self.pairing_payload)
                .on_input(Message::PairingPayloadChanged)
                .padding(11),
            button("Connect")
                .padding(11)
                .width(Length::Fill)
                .on_press(Message::BeginClaim)
        ]
        .spacing(9);
        container(column![machine_list, pairing].spacing(28))
            .padding(18)
            .width(320)
            .height(Length::Fill)
            .style(container::rounded_box)
            .into()
    }

    fn detail(&self) -> Element<'_, Message> {
        let Some(machine_id) = self.selected_machine else {
            return self.onboarding();
        };
        let Some(snapshot) = self.snapshots.get(&machine_id) else {
            let body = self.machine_errors.get(&machine_id).map_or_else(
                || "Loading kiosk state…".to_owned(),
                |error| format!("Kiosk is currently unreachable: {error}"),
            );
            return container(text(body).size(19))
                .padding(24)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(container::rounded_box)
                .into();
        };
        let name = snapshot.name.as_deref().unwrap_or("Unnamed kiosk");
        let connection = if self.machine_errors.contains_key(&machine_id) {
            "Offline"
        } else {
            "Online"
        };
        let slots = snapshot
            .slots
            .iter()
            .fold(Column::new().spacing(8), |slots, slot| {
                slots.push(Self::slot_row(slot))
            });
        let body = column![
            row![
                column![
                    text(name).size(30),
                    text(short_machine_id(&machine_id)).size(13)
                ]
                .spacing(4)
                .width(Length::Fill),
                text(connection).size(18)
            ],
            text(format!(
                "Admin PIN: {}",
                if snapshot.admin_pin_configured {
                    "Configured"
                } else {
                    "Not configured"
                }
            )),
            text(authorization_text(snapshot.vend_authorization.as_ref())),
            text(format!(
                "{} unresolved purchase(s)",
                snapshot.unresolved_purchases.len()
            )),
            Self::unresolved_purchases(snapshot),
            self.remote_controls(snapshot),
            text("Inventory and health").size(22),
            slots,
            self.recent_activity(machine_id),
            self.federation_form()
        ]
        .spacing(14);
        container(scrollable(body))
            .padding(22)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(container::rounded_box)
            .into()
    }

    fn onboarding(&self) -> Element<'_, Message> {
        container(
            column![
                text("Pair your first kiosk").size(30),
                text("Open the kiosk's pairing screen, then paste its QR payload on the left.")
                    .size(18),
                self.federation_form()
            ]
            .spacing(18),
        )
        .padding(28)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(container::rounded_box)
        .into()
    }

    fn federation_form(&self) -> Element<'_, Message> {
        column![
            text("Payment federation").size(20),
            text("This configuration is securely synced to every paired kiosk.").size(14),
            text_input("Fedimint invite code", &self.federation_invite)
                .on_input(Message::FederationInviteChanged)
                .padding(11),
            button("Save federation")
                .padding(11)
                .on_press(Message::UpdateFederation)
        ]
        .spacing(9)
        .into()
    }

    fn remote_controls(&self, snapshot: &KioskSnapshot) -> Element<'_, Message> {
        let pin_action = if snapshot.admin_pin_configured {
            "Change PIN"
        } else {
            "Set initial PIN"
        };
        column![
            text("Kiosk settings").size(22),
            row![
                text_input("Kiosk name", &self.kiosk_name_input)
                    .on_input(Message::KioskNameChanged)
                    .padding(10),
                button("Save name")
                    .padding(11)
                    .on_press(Message::SaveKioskName)
            ]
            .spacing(8),
            row![
                text_input("Admin PIN", &self.admin_pin_input)
                    .on_input(Message::AdminPinChanged)
                    .secure(true)
                    .padding(10),
                button(pin_action)
                    .padding(11)
                    .on_press(Message::SaveAdminPin)
            ]
            .spacing(8),
            row![
                button("Arm one free vend (30s)")
                    .padding(11)
                    .style(button::warning)
                    .on_press(Message::ArmFreeVend),
                button("Disarm")
                    .padding(11)
                    .on_press(Message::DisarmVendAuthorization)
            ]
            .spacing(8)
        ]
        .spacing(10)
        .into()
    }

    fn unresolved_purchases(snapshot: &KioskSnapshot) -> Element<'static, Message> {
        if snapshot.unresolved_purchases.is_empty() {
            return container("").height(0).into();
        }
        snapshot
            .unresolved_purchases
            .iter()
            .fold(
                Column::new()
                    .push(text("Purchases needing attention").size(22))
                    .spacing(8),
                |purchases, purchase| purchases.push(Self::purchase_row(purchase)),
            )
            .into()
    }

    fn purchase_row(purchase: &PurchaseSummary) -> Element<'static, Message> {
        let payment = payment_text(&purchase.payment);
        let mut actions = row![].spacing(5);
        match &purchase.state {
            PurchaseSummaryState::Uncertain => {
                actions = actions
                    .push(
                        button("Dispensed")
                            .style(button::success)
                            .on_press(Message::ResolveUncertain(purchase.id, true)),
                    )
                    .push(
                        button("Not dispensed")
                            .on_press(Message::ResolveUncertain(purchase.id, false)),
                    );
            }
            PurchaseSummaryState::AssistanceRequired => {
                actions = actions
                    .push(button("Product provided").style(button::success).on_press(
                        Message::ResolveAssistance(
                            purchase.id,
                            AssistanceResolution::ProductProvided,
                        ),
                    ))
                    .push(
                        button("Refunded out of band").on_press(Message::ResolveAssistance(
                            purchase.id,
                            AssistanceResolution::RefundedOutOfBand,
                        )),
                    );
            }
            _ => {}
        }
        container(
            row![
                column![
                    text(format!("{} · {payment}", purchase.slot)).size(17),
                    text(format!("{:?}", purchase.state)).size(13)
                ]
                .spacing(3)
                .width(Length::Fill),
                actions
            ]
            .align_y(iced::Alignment::Center),
        )
        .padding(11)
        .width(Length::Fill)
        .style(container::warning)
        .into()
    }

    fn recent_activity(&self, machine_id: EndpointId) -> Element<'_, Message> {
        let mut activity = Column::new()
            .push(text("Recent activity").size(22))
            .spacing(7);
        if let Some(events) = self.activity.get(&machine_id) {
            for event in events.iter().rev().take(12) {
                activity = activity.push(
                    row![
                        text(format!("#{}", event.sequence.0)).width(65),
                        text(manager_event_text(&event.event))
                    ]
                    .spacing(8),
                );
            }
        } else {
            activity = activity.push(text("No activity recorded yet."));
        }
        activity.into()
    }

    fn slot_row(slot: &SlotSnapshot) -> Element<'_, Message> {
        let health = match slot.health {
            SlotHealth::Ready => "Ready",
            SlotHealth::NeedsAttention => "Needs attention",
        };
        let mut actions = row![
            button("−").on_press(Message::InventoryChange(slot.slot.clone(), -1)),
            button("+").on_press(Message::InventoryChange(slot.slot.clone(), 1)),
            button("+5").on_press(Message::InventoryChange(slot.slot.clone(), 5)),
            button("Test")
                .style(button::warning)
                .on_press(Message::ArmMaintenanceVend(slot.slot.clone()))
        ]
        .spacing(5);
        if slot.health == SlotHealth::NeedsAttention {
            actions = actions.push(
                button("Resolve")
                    .style(button::success)
                    .on_press(Message::ResolveSlotAttention(slot.slot.clone())),
            );
        }
        container(
            row![
                text(slot.slot.to_string()).size(18).width(70),
                text(format!("{} in stock", slot.inventory)).width(110),
                text(format!("{} reserved", slot.reserved)).width(110),
                text(health).width(120),
                actions
            ]
            .align_y(iced::Alignment::Center),
        )
        .padding(11)
        .width(Length::Fill)
        .style(container::secondary)
        .into()
    }

    fn claim_confirmation(claim: &ManagerClaim) -> Element<'_, Message> {
        container(
            row![
                column![
                    text("Confirm kiosk pairing").size(22),
                    text("Verify this same code is visible on the physical kiosk."),
                    text(claim.pin()).size(36)
                ]
                .spacing(5)
                .width(Length::Fill),
                button("Reject")
                    .padding(14)
                    .style(button::danger)
                    .on_press(Message::RejectClaim),
                button("Confirm")
                    .padding(14)
                    .style(button::success)
                    .on_press(Message::ConfirmClaim)
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center),
        )
        .padding(16)
        .width(Length::Fill)
        .style(container::warning)
        .into()
    }

    fn status_text(&self) -> &str {
        match &self.status {
            ManagerStatus::Starting => "Starting secure manager services…",
            ManagerStatus::Ready => "Mainnet · secure manager services ready",
            ManagerStatus::Unavailable(error) => error,
        }
    }
}

fn short_machine_id(machine_id: &EndpointId) -> String {
    let full = machine_id.to_string();
    format!("{}…{}", &full[..8], &full[full.len() - 6..])
}

fn authorization_text(authorization: Option<&VendAuthorizationSnapshot>) -> String {
    match authorization {
        None => "Vend authorization: Not armed".to_owned(),
        Some(VendAuthorizationSnapshot::FreeVend { scope, .. }) => {
            format!("Vend authorization: Free vend armed ({scope:?})")
        }
        Some(VendAuthorizationSnapshot::MaintenanceVend { slot, .. }) => {
            format!("Vend authorization: Maintenance test armed for {slot}")
        }
    }
}

fn payment_text(payment: &PaymentSummary) -> String {
    match payment {
        PaymentSummary::Promo { account } => format!("Event code {}", account.masked),
        PaymentSummary::Lightning { amount } => {
            format!("{} msats", amount.as_u64())
        }
        PaymentSummary::FreeVend => "Free vend".to_owned(),
        PaymentSummary::MaintenanceTest => "Maintenance test".to_owned(),
    }
}

fn manager_event_text(event: &ManagerEvent) -> String {
    match event {
        ManagerEvent::KioskNameChanged { name } => format!("Kiosk renamed to {name}"),
        ManagerEvent::AdminPinChanged => "Admin PIN changed".to_owned(),
        ManagerEvent::InventorySet { slot, quantity } => {
            format!("{slot} inventory set to {quantity}")
        }
        ManagerEvent::SlotHealthChanged { slot, health } => {
            format!("{slot} health changed to {health:?}")
        }
        ManagerEvent::VendAuthorizationArmed => "Vend authorization armed".to_owned(),
        ManagerEvent::VendAuthorizationDisarmed => "Vend authorization disarmed".to_owned(),
        ManagerEvent::PurchaseChanged(purchase) => {
            format!("{} purchase changed to {:?}", purchase.slot, purchase.state)
        }
        ManagerEvent::AssistanceResolved {
            purchase_id,
            resolution,
        } => format!("Purchase {purchase_id} resolved as {resolution:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_uses_a_persistent_manager_directory_by_default() {
        let args = Args::try_parse_from(["lv-manager"]).unwrap();
        assert_eq!(args.data, PathBuf::from("lv-manager-data"));
    }
}
