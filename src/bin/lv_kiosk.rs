#[path = "lv_kiosk/mdb.rs"]
mod mdb;

use clap::Parser;
use iced::widget::{
    button, column, container, image, mouse_area, operation, rich_text, row, scrollable, span,
    text, Column, Id, Row,
};
use iced::{time, window, Element, Length, Size, Subscription, Task, Theme};
use lv_mdb_tools::kiosk::{
    ArmMode, Catalog, KioskEngine, MachineSelection, PaymentKind, PaymentPolicy, Product,
    PromoCode, SelectionOutcome, SlotHealth, SlotId, StateStore, TransactionId, TransactionStatus,
};
use lv_mdb_tools::{
    ItemNumber, Level1Amount, MdbConfig, SessionEndReason, VendDecisionError, VendId,
};
use mdb::{ControllerEvent, DecisionFailure, MdbController};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const CUSTOMER_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const LIGHTNING_TIMEOUT: Duration = Duration::from_secs(45);
const RATE_LIMIT_RESET: Duration = Duration::from_secs(60);
const RATE_LIMIT_DELAY: Duration = Duration::from_secs(3);
const ADMIN_LOCKOUT: Duration = Duration::from_secs(30);
const MDB_APPLICATION_RESPONSE_TIME: Duration = Duration::from_secs(46);
const MDB_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Parser)]
#[command(about = "Portrait LightningVEND kiosk UI")]
struct Args {
    /// Serial port connected to the WAFER RS232-MDB adapter. Omit for simulation mode.
    #[arg(long, value_name = "PATH")]
    port: Option<PathBuf>,

    /// Serial baud rate. The PC2MDB adapter normally uses 9600.
    #[arg(long, default_value_t = 9600)]
    baud: u32,

    /// Product, slot, payment policy, and price configuration.
    #[arg(long, default_value = "config/kiosk.toml")]
    catalog: PathBuf,

    /// Persistent redb state file.
    #[arg(long, default_value = "lv-kiosk.redb")]
    database: PathBuf,

    /// Promo entitlements to load when --seed-demo is used.
    #[arg(long, default_value = "config/promo_codes.csv")]
    promo_codes: PathBuf,

    /// Replace promo codes and stock each configured slot with three demo items.
    #[arg(long)]
    seed_demo: bool,

    /// Start as a borderless fullscreen kiosk instead of a 480x800 development window.
    #[arg(long)]
    fullscreen: bool,

    /// Shared admin PIN.
    #[arg(long, default_value = "2468")]
    admin_pin: String,
}

fn main() -> iced::Result {
    let args = Args::parse();
    let fullscreen = args.fullscreen;
    let boot_args = args;
    iced::application(
        move || ApplicationState::boot(&boot_args),
        ApplicationState::update,
        ApplicationState::view,
    )
    .subscription(ApplicationState::subscription)
    .theme(Theme::Dark)
    .title("LightningVEND kiosk")
    .window(window::Settings {
        size: Size::new(480.0, 800.0),
        fullscreen,
        decorations: !fullscreen,
        resizable: false,
        ..window::Settings::default()
    })
    .run()
}

enum ApplicationState {
    Running(Box<KioskApp>),
    Failed(String),
}

impl ApplicationState {
    fn boot(args: &Args) -> Self {
        match KioskApp::boot(args) {
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
                    text("Kiosk could not start").size(32),
                    text(error).size(18),
                    text("Correct the configuration and restart the application.")
                ]
                .spacing(20),
            )
            .padding(30)
            .center(Length::Fill)
            .into(),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        match self {
            Self::Running(app) => {
                let clock = time::every(Duration::from_secs(1)).map(Message::Tick);
                if app.is_hardware() {
                    Subscription::batch([
                        clock,
                        time::every(MDB_POLL_INTERVAL).map(|_| Message::PollMdb),
                    ])
                } else {
                    clock
                }
            }
            Self::Failed(_) => Subscription::none(),
        }
    }
}

struct KioskApp {
    engine: KioskEngine,
    store: StateStore,
    backend: Backend,
    machine_status: MachineStatus,
    active_mdb_vend: Option<ActiveMdbVend>,
    admin_pin: String,
    page: Page,
    promo_input: String,
    admin_input: String,
    notice: Option<String>,
    now: Instant,
    last_customer_activity: Instant,
    invalid_code_attempts: u32,
    last_invalid_code: Option<Instant>,
    next_code_attempt: Option<Instant>,
    invalid_admin_attempts: u32,
    admin_locked_until: Option<Instant>,
    catalog_scroll_id: Id,
    catalog_drag: PointerDrag,
}

#[derive(Default)]
struct PointerDrag {
    cursor_y: Option<f32>,
    active: bool,
}

impl PointerDrag {
    fn move_to(&mut self, cursor_y: f32) -> Option<f32> {
        let delta = self
            .active
            .then(|| self.cursor_y.map(|previous_y| previous_y - cursor_y))
            .flatten();
        self.cursor_y = Some(cursor_y);
        delta.filter(|delta| delta.abs() > f32::EPSILON)
    }

    fn press(&mut self) {
        self.active = self.cursor_y.is_some();
    }

    const fn release(&mut self) {
        self.active = false;
    }
}

enum Backend {
    Simulator,
    Hardware(MdbController),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MachineStatus {
    Simulator,
    Connecting,
    Ready,
    Unavailable(String),
}

#[derive(Debug, Clone, Copy)]
struct ActiveMdbVend {
    vend_id: VendId,
    transaction_id: TransactionId,
    approval_sent: bool,
}

#[derive(Debug, Clone, Copy)]
struct MdbVendRequest {
    vend_id: VendId,
    requested_price: Level1Amount,
}

#[derive(Clone)]
enum Page {
    Ready,
    PromoEntry,
    Promo,
    Lightning {
        transaction_id: TransactionId,
        selection: MachineSelection,
        price_cents: u32,
        started: Instant,
    },
    Dispensing {
        transaction_id: TransactionId,
        selection: MachineSelection,
        payment: PaymentKind,
    },
    Result {
        title: String,
        body: String,
        destination: Destination,
    },
    AdminPin,
    Admin,
}

#[derive(Debug, Clone, Copy)]
enum Destination {
    Ready,
    Promo,
    Admin,
}

#[derive(Debug, Clone, Copy)]
enum SimulatedVendResult {
    Success,
    Failure,
    Uncertain,
}

#[derive(Debug, Clone)]
enum Message {
    Tick(Instant),
    PollMdb,
    OpenPromo,
    PromoDigit(char),
    PromoBackspace,
    SubmitPromo,
    Done,
    OpenAdmin,
    AdminDigit(char),
    AdminBackspace,
    SubmitAdmin,
    CloseAdmin,
    MachineSelected(SlotId),
    LightningAccepted(TransactionId),
    LightningCancelled(TransactionId),
    SimulatedVend(TransactionId, SimulatedVendResult),
    DismissResult(Destination),
    ArmFreeVend,
    Disarm,
    ArmMaintenance(SlotId),
    InventoryChange(SlotId, i32),
    MarkSlotResolved(SlotId),
    ResolveUncertain(TransactionId, bool),
    CatalogPointerMoved(f32),
    CatalogPointerPressed,
    CatalogPointerReleased,
}

impl KioskApp {
    fn boot(args: &Args) -> Result<Self, String> {
        if args.admin_pin.len() < 4 || !args.admin_pin.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("--admin-pin must contain at least four digits".to_owned());
        }
        let catalog = Catalog::load(&args.catalog).map_err(|error| error.to_string())?;
        let store = StateStore::open(&args.database).map_err(|error| error.to_string())?;
        let mut state = store.load().map_err(|error| error.to_string())?;
        if args.seed_demo {
            let file = File::open(&args.promo_codes).map_err(|error| {
                format!(
                    "could not open promo CSV {}: {error}",
                    args.promo_codes.display()
                )
            })?;
            state = store
                .replace_codes_from_csv(&state, &catalog, file)
                .map_err(|error| error.to_string())?;
            for slot in catalog.slots() {
                state.set_inventory(slot.id().clone(), 3);
                state.set_health(slot.id().clone(), SlotHealth::Ready);
            }
        }
        let engine = KioskEngine::new(catalog, state);
        store
            .save(engine.state())
            .map_err(|error| error.to_string())?;
        let (backend, machine_status) = match &args.port {
            Some(port) => {
                let config = MdbConfig::new(port.clone(), args.baud)
                    .with_application_response_time(MDB_APPLICATION_RESPONSE_TIME);
                let controller = MdbController::spawn(config)
                    .map_err(|error| format!("could not start the MDB controller: {error}"))?;
                (Backend::Hardware(controller), MachineStatus::Connecting)
            }
            None => (Backend::Simulator, MachineStatus::Simulator),
        };
        let now = Instant::now();
        Ok(Self {
            engine,
            store,
            backend,
            machine_status,
            active_mdb_vend: None,
            admin_pin: args.admin_pin.clone(),
            page: Page::Ready,
            promo_input: String::new(),
            admin_input: String::new(),
            notice: args.seed_demo.then(|| {
                "Demo data loaded: codes 123456 and 654321; all slots stocked with 3 items."
                    .to_owned()
            }),
            now,
            last_customer_activity: now,
            invalid_code_attempts: 0,
            last_invalid_code: None,
            next_code_attempt: None,
            invalid_admin_attempts: 0,
            admin_locked_until: None,
            catalog_scroll_id: Id::unique(),
            catalog_drag: PointerDrag::default(),
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if let Some(task) = self.update_catalog_drag(&message) {
            return task;
        }

        match message {
            Message::Tick(now) => self.tick(now),
            Message::PollMdb => self.poll_mdb(),
            Message::OpenPromo => {
                self.promo_input.clear();
                self.notice = None;
                self.page = Page::PromoEntry;
                self.record_customer_activity();
            }
            Message::PromoDigit(digit) => {
                if self.promo_input.len() < 6 {
                    self.promo_input.push(digit);
                    self.notice = None;
                    self.record_customer_activity();
                }
            }
            Message::PromoBackspace => {
                self.promo_input.pop();
                self.notice = None;
                self.record_customer_activity();
            }
            Message::SubmitPromo => self.submit_promo_code(),
            Message::Done => self.finish_customer_session(),
            Message::OpenAdmin => {
                self.admin_input.clear();
                self.notice = None;
                self.page = Page::AdminPin;
            }
            Message::AdminDigit(digit) => {
                if self.admin_input.len() < 12 {
                    self.admin_input.push(digit);
                    self.notice = None;
                }
            }
            Message::AdminBackspace => {
                self.admin_input.pop();
                self.notice = None;
            }
            Message::SubmitAdmin => self.submit_admin_pin(),
            Message::CloseAdmin => {
                self.engine.disarm();
                self.page = Page::Ready;
                self.notice = None;
            }
            Message::MachineSelected(slot) => self.machine_selected(&slot, None),
            Message::LightningAccepted(id) => self.lightning_accepted(id),
            Message::LightningCancelled(id) => self.lightning_cancelled(id),
            Message::SimulatedVend(id, result) => self.simulated_vend(id, result),
            Message::DismissResult(destination) => {
                self.page = match destination {
                    Destination::Ready => Page::Ready,
                    Destination::Promo => Page::Promo,
                    Destination::Admin => Page::Admin,
                };
                self.notice = None;
                self.record_customer_activity();
            }
            Message::ArmFreeVend => {
                self.engine.arm_free_vend();
                self.notice = Some(
                    "Free vend armed for the next configured, available selection.".to_owned(),
                );
            }
            Message::Disarm => {
                self.engine.disarm();
                self.notice = Some("Vend authorization disarmed.".to_owned());
            }
            Message::ArmMaintenance(slot) => {
                if let Err(error) = self.engine.arm_maintenance_test(slot.clone()) {
                    self.notice = Some(error.to_string());
                } else {
                    self.notice = Some(format!(
                        "Maintenance test armed. Enter {slot} on the vending machine."
                    ));
                }
            }
            Message::InventoryChange(slot, change) => self.change_inventory(slot, change),
            Message::MarkSlotResolved(slot) => {
                match self.engine.set_slot_health(slot, SlotHealth::Ready) {
                    Ok(()) => {
                        self.notice = Some("Slot marked ready.".to_owned());
                        self.persist();
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
            Message::ResolveUncertain(id, dispensed) => {
                self.resolve_uncertain(id, dispensed);
            }
            Message::CatalogPointerMoved(_)
            | Message::CatalogPointerPressed
            | Message::CatalogPointerReleased => unreachable!("handled before the main update"),
        }

        Task::none()
    }

    fn update_catalog_drag(&mut self, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::CatalogPointerMoved(cursor_y) => Some(
                self.catalog_drag
                    .move_to(*cursor_y)
                    .map_or_else(Task::none, |delta_y| {
                        operation::scroll_by(
                            self.catalog_scroll_id.clone(),
                            scrollable::AbsoluteOffset { x: 0.0, y: delta_y },
                        )
                    }),
            ),
            Message::CatalogPointerPressed => {
                self.catalog_drag.press();
                Some(Task::none())
            }
            Message::CatalogPointerReleased => {
                self.catalog_drag.release();
                Some(Task::none())
            }
            _ => None,
        }
    }

    const fn is_hardware(&self) -> bool {
        matches!(self.backend, Backend::Hardware(_))
    }

    fn poll_mdb(&mut self) {
        let mut events = Vec::new();
        if let Backend::Hardware(controller) = &mut self.backend {
            while let Some(event) = controller.try_event() {
                events.push(event);
            }
        }
        for event in events {
            self.handle_mdb_event(event);
        }
    }

    fn handle_mdb_event(&mut self, event: ControllerEvent) {
        match event {
            ControllerEvent::Connecting => self.machine_status = MachineStatus::Connecting,
            ControllerEvent::SessionReady => {
                let reconnected = matches!(self.machine_status, MachineStatus::Unavailable(_));
                self.machine_status = MachineStatus::Ready;
                if reconnected {
                    self.notice = Some("Vending machine reconnected and ready.".to_owned());
                }
            }
            ControllerEvent::VendRequested {
                vend_id,
                item,
                requested_price,
            } => self.mdb_vend_requested(
                MdbVendRequest {
                    vend_id,
                    requested_price,
                },
                item,
            ),
            ControllerEvent::VendCancelled { vend_id } => {
                self.cancel_mdb_transaction(
                    vend_id,
                    "The vending machine cancelled the selection.",
                );
            }
            ControllerEvent::VendDecisionExpired { vend_id, error } => {
                self.cancel_mdb_transaction(
                    vend_id,
                    &format!("The vending machine selection expired: {error}"),
                );
            }
            ControllerEvent::DecisionAccepted { vend_id } => {
                if self
                    .active_mdb_vend
                    .is_some_and(|active| active.vend_id == vend_id)
                {
                    eprintln!("MDB vend decision accepted by the local device actor: {vend_id}");
                }
            }
            ControllerEvent::DecisionFailed { vend_id, error } => {
                let uncertain =
                    matches!(error, DecisionFailure::Mdb(VendDecisionError::Disconnected));
                if uncertain
                    && self
                        .active_mdb_vend
                        .is_some_and(|active| active.vend_id == vend_id && active.approval_sent)
                {
                    self.finish_mdb_vend(
                        vend_id,
                        SimulatedVendResult::Uncertain,
                        "The MDB connection was lost while approving the vend.",
                    );
                } else {
                    self.cancel_mdb_transaction(
                        vend_id,
                        &format!("The vend could not be authorized: {error:?}"),
                    );
                }
            }
            ControllerEvent::VendSucceeded {
                vend_id,
                reported_item,
            } => {
                if let Some(item) = reported_item {
                    eprintln!("MDB reported successful item {item}");
                }
                self.finish_mdb_vend(
                    vend_id,
                    SimulatedVendResult::Success,
                    "The vending machine reported a successful dispense.",
                );
            }
            ControllerEvent::VendFailed { vend_id } => self.finish_mdb_vend(
                vend_id,
                SimulatedVendResult::Failure,
                "The vending machine could not dispense the item.",
            ),
            ControllerEvent::SessionEnded { reason } => self.mdb_session_ended(reason),
            ControllerEvent::Unavailable(error) => {
                self.machine_status = MachineStatus::Unavailable(error);
                if let Some(active) = self.active_mdb_vend {
                    if active.approval_sent {
                        self.finish_mdb_vend(
                            active.vend_id,
                            SimulatedVendResult::Uncertain,
                            "The MDB connection was lost after vend approval.",
                        );
                    } else {
                        self.cancel_mdb_transaction(
                            active.vend_id,
                            "The MDB connection was lost before authorization.",
                        );
                    }
                }
            }
            ControllerEvent::Fault(error) => self.notice = Some(format!("MDB warning: {error}")),
        }
    }

    fn mdb_vend_requested(&mut self, request: MdbVendRequest, item: ItemNumber) {
        if self.active_mdb_vend.is_some() {
            self.deny_mdb(request.vend_id);
            self.notice =
                Some("The machine requested another vend while one was active.".to_owned());
            return;
        }
        let [row, column] = item.bytes();
        let slot = match SlotId::from_ap113(row, column) {
            Ok(slot) => slot,
            Err(error) => {
                self.deny_mdb(request.vend_id);
                self.notice = Some(format!("Unsupported machine selection {item}: {error}"));
                return;
            }
        };
        eprintln!(
            "MDB selected {slot}; ignoring requested price {} in favor of kiosk policy",
            request.requested_price.raw()
        );
        self.machine_selected(&slot, Some(request));
    }

    fn mdb_session_ended(&mut self, reason: SessionEndReason) {
        let Some(active) = self.active_mdb_vend else {
            return;
        };
        if active.approval_sent {
            self.finish_mdb_vend(
                active.vend_id,
                SimulatedVendResult::Uncertain,
                &format!("The MDB session ended without a vend result ({reason:?})."),
            );
        } else {
            self.cancel_mdb_transaction(
                active.vend_id,
                &format!("The vending machine ended the selection ({reason:?})."),
            );
        }
    }

    fn resolve_uncertain(&mut self, id: TransactionId, dispensed: bool) {
        match self.engine.resolve_uncertain(id, dispensed) {
            Ok(()) => {
                self.notice = Some(if dispensed {
                    "Transaction marked dispensed; inventory and entitlement updated.".to_owned()
                } else {
                    "Transaction marked not dispensed; reservation released.".to_owned()
                });
                self.persist();
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        if self
            .last_invalid_code
            .is_some_and(|last| now.saturating_duration_since(last) >= RATE_LIMIT_RESET)
        {
            self.invalid_code_attempts = 0;
            self.last_invalid_code = None;
            self.next_code_attempt = None;
        }

        if self.engine.active_code().is_some()
            && !matches!(self.page, Page::Dispensing { .. })
            && now.saturating_duration_since(self.last_customer_activity) >= CUSTOMER_IDLE_TIMEOUT
        {
            self.finish_customer_session();
            self.notice = Some("Promo session ended after 20 seconds of inactivity.".to_owned());
            return;
        }

        let expired_lightning = match &self.page {
            Page::Lightning {
                transaction_id,
                started,
                ..
            } if now.saturating_duration_since(*started) >= LIGHTNING_TIMEOUT => {
                Some(*transaction_id)
            }
            _ => None,
        };
        if let Some(id) = expired_lightning {
            self.cancel_lightning_with_notice(
                id,
                "Lightning payment timed out. No payment was taken.",
            );
        }
    }

    fn submit_promo_code(&mut self) {
        self.record_customer_activity();
        if self.next_code_attempt.is_some_and(|next| self.now < next) {
            self.notice = Some("Please wait a moment before trying another code.".to_owned());
            return;
        }

        let result = PromoCode::parse(self.promo_input.clone())
            .and_then(|code| self.engine.authenticate_code(code));
        if result.is_ok() {
            self.invalid_code_attempts = 0;
            self.last_invalid_code = None;
            self.next_code_attempt = None;
            self.promo_input.clear();
            self.notice = Some("Code accepted. Choose any included item.".to_owned());
            self.page = Page::Promo;
        } else {
            self.invalid_code_attempts += 1;
            self.last_invalid_code = Some(self.now);
            if self.invalid_code_attempts >= 5 {
                self.next_code_attempt = Some(self.now + RATE_LIMIT_DELAY);
            }
            self.notice = Some("That code was not recognized.".to_owned());
            self.promo_input.clear();
        }
    }

    fn submit_admin_pin(&mut self) {
        if self
            .admin_locked_until
            .is_some_and(|until| self.now < until)
        {
            self.notice = Some("Admin access is temporarily locked.".to_owned());
            return;
        }
        if self.admin_input == self.admin_pin {
            self.invalid_admin_attempts = 0;
            self.admin_locked_until = None;
            self.admin_input.clear();
            self.notice = None;
            self.page = Page::Admin;
        } else {
            self.invalid_admin_attempts += 1;
            self.admin_input.clear();
            if self.invalid_admin_attempts >= 5 {
                self.invalid_admin_attempts = 0;
                self.admin_locked_until = Some(self.now + ADMIN_LOCKOUT);
                self.notice =
                    Some("Too many attempts. Admin access locked for 30 seconds.".to_owned());
            } else {
                self.notice = Some("Incorrect admin PIN.".to_owned());
            }
        }
    }

    fn finish_customer_session(&mut self) {
        if let Err(error) = self.engine.end_customer_session() {
            self.notice = Some(error.to_string());
        } else {
            self.persist();
            self.page = Page::Ready;
            self.promo_input.clear();
        }
    }

    fn machine_selected(&mut self, slot: &SlotId, mdb_request: Option<MdbVendRequest>) {
        self.record_customer_activity();
        match self.engine.machine_selected(slot) {
            Ok(SelectionOutcome::LightningPaymentRequired {
                transaction_id,
                selection,
                price_cents,
            }) => {
                if !self.persist() {
                    let _ = self.engine.vend_cancelled(transaction_id);
                    self.persist();
                    if let Some(request) = mdb_request {
                        self.deny_mdb(request.vend_id);
                    }
                    self.page = Page::Ready;
                    return;
                }
                if let Some(request) = mdb_request {
                    self.active_mdb_vend = Some(ActiveMdbVend {
                        vend_id: request.vend_id,
                        transaction_id,
                        approval_sent: false,
                    });
                }
                self.notice = None;
                self.page = Page::Lightning {
                    transaction_id,
                    selection,
                    price_cents,
                    started: self.now,
                };
            }
            Ok(SelectionOutcome::VendApproved {
                transaction_id,
                selection,
                payment,
            }) => {
                if !self.persist() {
                    let _ = self.engine.vend_cancelled(transaction_id);
                    self.persist();
                    if let Some(request) = mdb_request {
                        self.deny_mdb(request.vend_id);
                    }
                    self.page = Self::destination_page_for(&payment);
                    return;
                }
                if let Some(request) = mdb_request {
                    self.active_mdb_vend = Some(ActiveMdbVend {
                        vend_id: request.vend_id,
                        transaction_id,
                        approval_sent: false,
                    });
                }
                self.notice = None;
                self.page = Page::Dispensing {
                    transaction_id,
                    selection,
                    payment: payment.clone(),
                };
                if mdb_request.is_some() {
                    self.approve_active_mdb_vend(&payment);
                }
            }
            Ok(SelectionOutcome::Denied(message)) => {
                if let Some(request) = mdb_request {
                    self.deny_mdb(request.vend_id);
                }
                self.notice = Some(message);
            }
            Err(error) => {
                if let Some(request) = mdb_request {
                    self.deny_mdb(request.vend_id);
                }
                self.notice = Some(error.to_string());
            }
        }
    }

    fn lightning_accepted(&mut self, id: TransactionId) {
        match self.engine.lightning_payment_accepted(id) {
            Ok(SelectionOutcome::VendApproved {
                transaction_id,
                selection,
                payment,
            }) => {
                if !self.persist() {
                    let _ = self.engine.vend_cancelled(transaction_id);
                    self.persist();
                    if let Some(active) = self.active_mdb_vend.take() {
                        self.deny_mdb(active.vend_id);
                    }
                    self.page = Page::Ready;
                    return;
                }
                self.page = Page::Dispensing {
                    transaction_id,
                    selection,
                    payment: payment.clone(),
                };
                self.notice = None;
                if self.is_hardware() {
                    self.approve_active_mdb_vend(&payment);
                }
            }
            Ok(_) => self.notice = Some("Unexpected Lightning transition.".to_owned()),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn lightning_cancelled(&mut self, id: TransactionId) {
        self.cancel_lightning_with_notice(id, "Lightning payment cancelled.");
    }

    fn cancel_lightning_with_notice(&mut self, id: TransactionId, notice: &str) {
        match self.engine.cancel_lightning(id) {
            Ok(()) => {
                self.persist();
                if let Some(active) = self
                    .active_mdb_vend
                    .filter(|active| active.transaction_id == id)
                {
                    self.active_mdb_vend = None;
                    self.deny_mdb(active.vend_id);
                }
                self.page = Page::Ready;
                self.notice = Some(notice.to_owned());
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn simulated_vend(&mut self, id: TransactionId, result: SimulatedVendResult) {
        self.complete_vend(id, result, None);
    }

    fn complete_vend(
        &mut self,
        id: TransactionId,
        result: SimulatedVendResult,
        body_override: Option<&str>,
    ) {
        let state_before_result = self.engine.clone();
        let payment = self
            .engine
            .state()
            .transactions()
            .iter()
            .find(|transaction| transaction.id() == id)
            .map(|transaction| transaction.payment().clone());
        let outcome = match result {
            SimulatedVendResult::Success => self.engine.vend_succeeded(id),
            SimulatedVendResult::Failure => self.engine.vend_failed(id),
            SimulatedVendResult::Uncertain => self.engine.vend_uncertain(id),
        };
        match outcome {
            Ok(()) => {
                let persistence_failed = !self.persist();
                let result = if persistence_failed {
                    self.engine = state_before_result;
                    let _ = self.engine.vend_uncertain(id);
                    self.persist();
                    SimulatedVendResult::Uncertain
                } else {
                    result
                };
                self.record_customer_activity();
                let destination = match payment {
                    Some(PaymentKind::Promo { .. }) => Destination::Promo,
                    Some(PaymentKind::FreeVend | PaymentKind::MaintenanceTest) => {
                        Destination::Admin
                    }
                    _ => Destination::Ready,
                };
                let (title, default_body) = match result {
                    SimulatedVendResult::Success => (
                        "Enjoy!",
                        "The vending machine reported a successful dispense.",
                    ),
                    SimulatedVendResult::Failure => (
                        "Could not dispense",
                        "Nothing was claimed or charged. The slot now needs attention.",
                    ),
                    SimulatedVendResult::Uncertain => (
                        "Result needs review",
                        "The entitlement is reserved and an administrator must reconcile it.",
                    ),
                };
                self.page = Page::Result {
                    title: title.to_owned(),
                    body: if persistence_failed {
                        "The machine reported a result, but the kiosk could not save it. An administrator must reconcile this transaction.".to_owned()
                    } else {
                        body_override.unwrap_or(default_body).to_owned()
                    },
                    destination,
                };
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn finish_mdb_vend(&mut self, vend_id: VendId, result: SimulatedVendResult, body: &str) {
        let Some(active) = self
            .active_mdb_vend
            .filter(|active| active.vend_id == vend_id)
        else {
            return;
        };
        self.active_mdb_vend = None;
        self.complete_vend(active.transaction_id, result, Some(body));
    }

    fn cancel_mdb_transaction(&mut self, vend_id: VendId, notice: &str) {
        let Some(active) = self
            .active_mdb_vend
            .filter(|active| active.vend_id == vend_id)
        else {
            return;
        };
        self.active_mdb_vend = None;
        let payment = self
            .engine
            .state()
            .transactions()
            .iter()
            .find(|transaction| transaction.id() == active.transaction_id)
            .map(|transaction| transaction.payment().clone());
        match self.engine.vend_cancelled(active.transaction_id) {
            Ok(()) => {
                self.persist();
                self.page = payment
                    .as_ref()
                    .map_or(Page::Ready, Self::destination_page_for);
                self.notice = Some(notice.to_owned());
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn approve_active_mdb_vend(&mut self, payment: &PaymentKind) {
        let Some(active) = self.active_mdb_vend else {
            self.notice = Some("The MDB selection is no longer active.".to_owned());
            return;
        };
        let amount = match approval_amount(payment) {
            Ok(amount) => amount,
            Err(error) => {
                self.deny_mdb(active.vend_id);
                self.cancel_mdb_transaction(active.vend_id, &error);
                return;
            }
        };
        let result = match &self.backend {
            Backend::Hardware(controller) => controller.approve(active.vend_id, amount),
            Backend::Simulator => return,
        };
        match result {
            Ok(()) => {
                if let Some(active) = self.active_mdb_vend.as_mut() {
                    active.approval_sent = true;
                }
            }
            Err(error) => {
                self.machine_status = MachineStatus::Unavailable(error.to_string());
                self.cancel_mdb_transaction(
                    active.vend_id,
                    "The MDB controller stopped before vend approval.",
                );
            }
        }
    }

    fn deny_mdb(&mut self, vend_id: VendId) {
        let result = match &self.backend {
            Backend::Hardware(controller) => controller.deny(vend_id),
            Backend::Simulator => return,
        };
        if let Err(error) = result {
            self.machine_status = MachineStatus::Unavailable(error.to_string());
        }
    }

    const fn destination_page_for(payment: &PaymentKind) -> Page {
        match payment {
            PaymentKind::Promo { .. } => Page::Promo,
            PaymentKind::FreeVend | PaymentKind::MaintenanceTest => Page::Admin,
            PaymentKind::Lightning { .. } => Page::Ready,
        }
    }

    fn change_inventory(&mut self, slot: SlotId, change: i32) {
        let current = self.engine.state().inventory(&slot);
        let quantity = if change.is_negative() {
            current.saturating_sub(change.unsigned_abs())
        } else {
            current.saturating_add(change.unsigned_abs())
        };
        match self.engine.set_inventory(slot, quantity) {
            Ok(()) => {
                self.notice = Some(format!("Inventory updated to {quantity}."));
                self.persist();
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn persist(&mut self) -> bool {
        match self.store.save(self.engine.state()) {
            Ok(()) => true,
            Err(error) => {
                self.notice = Some(format!("Could not save kiosk state: {error}"));
                false
            }
        }
    }

    fn record_customer_activity(&mut self) {
        self.last_customer_activity = self.now;
    }

    fn view(&self) -> Element<'_, Message> {
        let machine_unavailable = self.is_hardware()
            && !matches!(self.machine_status, MachineStatus::Ready)
            && !matches!(self.page, Page::AdminPin | Page::Admin);
        let page = if machine_unavailable {
            self.view_machine_unavailable()
        } else {
            match &self.page {
                Page::Ready => self.view_ready(),
                Page::PromoEntry => self.view_promo_entry(),
                Page::Promo => self.view_promo(),
                Page::Lightning {
                    transaction_id,
                    selection,
                    price_cents,
                    started,
                } => self.view_lightning(*transaction_id, selection, *price_cents, *started),
                Page::Dispensing {
                    transaction_id,
                    selection,
                    payment,
                } => self.view_dispensing(*transaction_id, selection, payment),
                Page::Result {
                    title,
                    body,
                    destination,
                } => Self::view_result(title, body, *destination),
                Page::AdminPin => self.view_admin_pin(),
                Page::Admin => self.view_admin(),
            }
        };
        container(page)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(18)
            .into()
    }

    fn view_ready(&self) -> Element<'_, Message> {
        let header = row![
            column![
                text("LightningVEND").size(32),
                text("Select an item to begin").size(18)
            ]
            .width(Length::Fill),
            button("⚙")
                .style(button::subtle)
                .on_press(Message::OpenAdmin)
        ]
        .align_y(iced::Alignment::Center);

        let promo = button(
            column![
                text("Have an event code?").size(20),
                text("Enter your six-digit code").size(15)
            ]
            .spacing(3),
        )
        .padding(16)
        .width(Length::Fill)
        .style(button::primary)
        .on_press(Message::OpenPromo);

        let catalog = scrollable(self.catalog_grid())
            .id(self.catalog_scroll_id.clone())
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(24)
                    .scroller_width(16)
                    .margin(4)
                    .spacing(8),
            ))
            .width(Length::Fill)
            .height(Length::Fill);
        let draggable_catalog = mouse_area(catalog)
            .on_move(|position| Message::CatalogPointerMoved(position.y))
            .on_press(Message::CatalogPointerPressed)
            .on_release(Message::CatalogPointerReleased)
            .on_exit(Message::CatalogPointerReleased);

        let content = column![
            header,
            self.notice_view(),
            promo,
            text("Paying with Lightning?").size(20),
            text("Enter a Lightning item on the vending machine keypad."),
            text("Swipe the item list to browse.").size(13),
            draggable_catalog,
            self.machine_controls()
        ]
        .spacing(14);
        content.into()
    }

    fn view_promo_entry(&self) -> Element<'_, Message> {
        let locked = self.next_code_attempt.is_some_and(|next| self.now < next);
        let keypad = numeric_keypad(
            Message::PromoDigit,
            Message::PromoBackspace,
            Message::SubmitPromo,
            self.promo_input.len() == 6 && !locked,
        );
        column![
            text("Enter event code").size(30),
            text("Use the touchscreen keypad below."),
            self.notice_view(),
            container(text("●".repeat(self.promo_input.len())).size(34))
                .height(70)
                .width(Length::Fill)
                .center(Length::Fill)
                .style(container::rounded_box),
            keypad,
            button("Cancel")
                .width(Length::Fill)
                .style(button::secondary)
                .on_press(Message::Done)
        ]
        .spacing(16)
        .into()
    }

    fn view_promo(&self) -> Element<'_, Message> {
        let code = self
            .engine
            .active_code()
            .map_or_else(|| "unknown".to_owned(), PromoCode::masked);
        let idle = CUSTOMER_IDLE_TIMEOUT
            .saturating_sub(
                self.now
                    .saturating_duration_since(self.last_customer_activity),
            )
            .as_secs();
        let mut entitlements = Column::new().spacing(10);
        if let Some(account) = self.engine.active_account() {
            for (product_id, entitlement) in account.entitlements() {
                let Some(product) = self.engine.catalog().product(product_id) else {
                    continue;
                };
                let matching = self.engine.available_promo_slots(product_id);
                let locations = matching
                    .iter()
                    .map(SlotId::as_str)
                    .collect::<Vec<_>>()
                    .join(" or ");
                let completed = entitlement.claimed() == entitlement.granted();
                let status = if completed {
                    format!(
                        "{} of {} claimed",
                        entitlement.claimed(),
                        entitlement.granted()
                    )
                } else if entitlement.reserved() > 0 {
                    "Pending administrator review".to_owned()
                } else if matching.is_empty() {
                    "Sold out or temporarily unavailable".to_owned()
                } else {
                    format!(
                        "Included · {} remaining · enter {locations}",
                        entitlement.remaining()
                    )
                };
                let name = rich_text![span(product.name()).size(20).strikethrough(completed)]
                    .on_link_click(iced::never);
                entitlements = entitlements.push(
                    container(column![name, text(status).size(14)].spacing(4))
                        .padding(12)
                        .width(Length::Fill)
                        .style(if completed {
                            container::secondary
                        } else {
                            container::rounded_box
                        }),
                );
            }
        }

        column![
            row![
                column![
                    text("Your included items").size(28),
                    text(format!("Code {code} · signs out in {idle}s"))
                ]
                .width(Length::Fill),
                button("Done")
                    .style(button::secondary)
                    .on_press(Message::Done)
            ]
            .align_y(iced::Alignment::Center),
            self.notice_view(),
            text("Enter any eligible selection on the vending machine keypad."),
            scrollable(entitlements).height(Length::Fill),
            self.machine_controls()
        ]
        .spacing(12)
        .into()
    }

    fn view_lightning(
        &self,
        transaction_id: TransactionId,
        selection: &MachineSelection,
        price_cents: u32,
        started: Instant,
    ) -> Element<'_, Message> {
        let remaining = LIGHTNING_TIMEOUT
            .saturating_sub(self.now.saturating_duration_since(started))
            .as_secs();
        column![
            text(selection.product_name().to_owned()).size(30),
            text(format!(
                "Selection {} · {}",
                selection.slot(),
                dollars(price_cents)
            ))
            .size(19),
            container(
                column![
                    text("LIGHTNING QR").size(28),
                    text("coming in the payment integration milestone")
                ]
                .align_x(iced::Alignment::Center)
                .spacing(8)
            )
            .width(Length::Fill)
            .height(300)
            .center(Length::Fill)
            .style(container::rounded_box),
            text(format!("{remaining} seconds remaining")).size(18),
            button(if self.is_hardware() {
                "Vend"
            } else {
                "Vend — simulate accepted hold invoice"
            })
            .padding(16)
            .width(Length::Fill)
            .style(button::success)
            .on_press(Message::LightningAccepted(transaction_id)),
            button("Cancel")
                .padding(14)
                .width(Length::Fill)
                .style(button::danger)
                .on_press(Message::LightningCancelled(transaction_id))
        ]
        .align_x(iced::Alignment::Center)
        .spacing(18)
        .into()
    }

    fn view_dispensing(
        &self,
        transaction_id: TransactionId,
        selection: &MachineSelection,
        payment: &PaymentKind,
    ) -> Element<'_, Message> {
        let result_controls: Element<'_, Message> = if self.is_hardware() {
            container(text("Waiting for the vending machine to report the result…").size(18))
                .padding(16)
                .width(Length::Fill)
                .style(container::rounded_box)
                .into()
        } else {
            container(
                column![
                    text("Simulator result").size(18),
                    button("VEND SUCCESS")
                        .width(Length::Fill)
                        .style(button::success)
                        .on_press(Message::SimulatedVend(
                            transaction_id,
                            SimulatedVendResult::Success
                        )),
                    button("VEND FAILURE")
                        .width(Length::Fill)
                        .style(button::danger)
                        .on_press(Message::SimulatedVend(
                            transaction_id,
                            SimulatedVendResult::Failure
                        )),
                    button("Process interrupted — uncertain")
                        .width(Length::Fill)
                        .style(button::warning)
                        .on_press(Message::SimulatedVend(
                            transaction_id,
                            SimulatedVendResult::Uncertain
                        ))
                ]
                .spacing(10),
            )
            .padding(16)
            .width(Length::Fill)
            .style(container::rounded_box)
            .into()
        };
        column![
            text("Dispensing…").size(38),
            text(selection.product_name().to_owned()).size(26),
            text(format!(
                "Selection {} · {}",
                selection.slot(),
                payment.name()
            ))
            .size(18),
            result_controls
        ]
        .align_x(iced::Alignment::Center)
        .spacing(22)
        .into()
    }

    fn view_result(title: &str, body: &str, destination: Destination) -> Element<'static, Message> {
        column![
            text(title.to_owned()).size(40),
            text(body.to_owned()).size(20),
            button("Continue")
                .padding(16)
                .width(Length::Fill)
                .style(button::primary)
                .on_press(Message::DismissResult(destination))
        ]
        .align_x(iced::Alignment::Center)
        .spacing(28)
        .into()
    }

    fn view_admin_pin(&self) -> Element<'_, Message> {
        let locked = self
            .admin_locked_until
            .is_some_and(|until| self.now < until);
        column![
            text("Administrator access").size(30),
            self.notice_view(),
            container(text("●".repeat(self.admin_input.len())).size(34))
                .height(70)
                .width(Length::Fill)
                .center(Length::Fill)
                .style(container::rounded_box),
            numeric_keypad(
                Message::AdminDigit,
                Message::AdminBackspace,
                Message::SubmitAdmin,
                !self.admin_input.is_empty() && !locked,
            ),
            button("Cancel")
                .width(Length::Fill)
                .style(button::secondary)
                .on_press(Message::CloseAdmin)
        ]
        .spacing(16)
        .into()
    }

    fn view_admin(&self) -> Element<'_, Message> {
        let arm_status = match self.engine.arm_mode() {
            ArmMode::None => "No vend armed".to_owned(),
            ArmMode::FreeNext => "FREE VEND ARMED — next available selection".to_owned(),
            ArmMode::MaintenanceTest(slot) => format!("TEST ARMED — enter {slot}"),
        };
        let arm_button = button("Arm one free vend")
            .padding(12)
            .style(button::warning)
            .on_press(Message::ArmFreeVend);
        let disarm = button("Disarm")
            .padding(12)
            .style(button::secondary)
            .on_press(Message::Disarm);

        let slot_list = self.admin_slot_list();
        let history = self.admin_history();
        let body = column![
            row![
                text("Admin").size(30).width(Length::Fill),
                button("Exit").on_press(Message::CloseAdmin)
            ],
            self.notice_view(),
            text(arm_status),
            row![arm_button, disarm].spacing(8),
            self.machine_controls(),
            text("Inventory and slot health").size(22),
            slot_list,
            text("Recent transactions").size(22),
            history
        ]
        .spacing(12);
        scrollable(body).height(Length::Fill).into()
    }

    fn admin_slot_list(&self) -> Column<'_, Message> {
        let mut slot_list = Column::new().spacing(10);
        for slot in self.engine.catalog().slots() {
            let count = self.engine.state().inventory(slot.id());
            let health = self.engine.state().health(slot.id());
            let product = self
                .engine
                .catalog()
                .product(slot.product_id())
                .map_or("Unknown product", Product::name);
            let status = match health {
                SlotHealth::Ready if count == 0 => "Sold out",
                SlotHealth::Ready => "Ready",
                SlotHealth::NeedsAttention => "Needs attention",
            };
            let mut actions = Row::new().spacing(5);
            actions = actions
                .push(button("−").on_press(Message::InventoryChange(slot.id().clone(), -1)))
                .push(text(count).size(18))
                .push(button("+").on_press(Message::InventoryChange(slot.id().clone(), 1)))
                .push(button("+5").on_press(Message::InventoryChange(slot.id().clone(), 5)));
            if count > 0 {
                actions = actions.push(
                    button("Test")
                        .style(button::warning)
                        .on_press(Message::ArmMaintenance(slot.id().clone())),
                );
            }
            if health == SlotHealth::NeedsAttention {
                actions = actions.push(
                    button("Resolve")
                        .style(button::success)
                        .on_press(Message::MarkSlotResolved(slot.id().clone())),
                );
            }
            slot_list = slot_list.push(
                container(
                    column![
                        row![
                            text(format!("{} · {product}", slot.id())).size(18),
                            text(status).size(14)
                        ]
                        .spacing(10),
                        actions.align_y(iced::Alignment::Center)
                    ]
                    .spacing(8),
                )
                .padding(10)
                .width(Length::Fill)
                .style(if health == SlotHealth::NeedsAttention {
                    container::danger
                } else {
                    container::rounded_box
                }),
            );
        }
        slot_list
    }

    fn admin_history(&self) -> Column<'_, Message> {
        let mut history = Column::new().spacing(8);
        for transaction in self.engine.state().transactions().iter().rev().take(20) {
            let mut entry = Column::new().spacing(4).push(text(format!(
                "#{} · {} · {}",
                transaction.id(),
                transaction.slot(),
                transaction.product_name()
            )));
            entry = entry.push(
                text(format!(
                    "{} · {:?} · {}",
                    transaction.payment().name(),
                    transaction.status(),
                    transaction.started_at()
                ))
                .size(13),
            );
            if transaction.status() == TransactionStatus::Uncertain {
                entry = entry.push(
                    row![
                        button("Mark dispensed")
                            .style(button::warning)
                            .on_press(Message::ResolveUncertain(transaction.id(), true)),
                        button("Not dispensed")
                            .style(button::secondary)
                            .on_press(Message::ResolveUncertain(transaction.id(), false))
                    ]
                    .spacing(8),
                );
            }
            history = history.push(
                container(entry)
                    .padding(9)
                    .width(Length::Fill)
                    .style(container::secondary),
            );
        }
        history
    }

    fn catalog_grid(&self) -> Element<'_, Message> {
        let mut grid = Column::new().spacing(10);
        let products = self.engine.catalog().products().collect::<Vec<_>>();
        for pair in products.chunks(2) {
            let mut product_row = Row::new().spacing(10);
            for product in pair {
                product_row = product_row.push(self.product_card(product));
            }
            if pair.len() == 1 {
                product_row = product_row.push(container("").width(Length::FillPortion(1)));
            }
            grid = grid.push(product_row);
        }
        grid.into()
    }

    fn product_card<'a>(&'a self, product: &'a Product) -> Element<'a, Message> {
        let slots = self
            .engine
            .catalog()
            .slots_for_product(product.id())
            .collect::<Vec<_>>();
        let available = slots.iter().any(|slot| {
            slot.enabled()
                && self.engine.state().inventory(slot.id()) > 0
                && self.engine.state().health(slot.id()) == SlotHealth::Ready
        });
        let labels = slots
            .iter()
            .map(|slot| match slot.payment() {
                PaymentPolicy::Promo => format!("{} · Included with event code", slot.id()),
                PaymentPolicy::Lightning { price_cents } => {
                    format!("{} · {}", slot.id(), dollars(price_cents))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let availability = if available {
            labels
        } else {
            "Sold out or unavailable".to_owned()
        };
        container(
            column![
                Self::product_image(product),
                text(product.name()).size(17),
                text(availability).size(13)
            ]
            .spacing(6),
        )
        .padding(9)
        .width(Length::FillPortion(1))
        .height(185)
        .style(if available {
            container::rounded_box
        } else {
            container::secondary
        })
        .into()
    }

    fn product_image(product: &Product) -> Element<'_, Message> {
        product.image().filter(|path| path.exists()).map_or_else(
            || {
                let initials = product
                    .name()
                    .split_whitespace()
                    .filter_map(|word| word.chars().next())
                    .take(2)
                    .collect::<String>();
                container(text(initials).size(30))
                    .width(Length::Fill)
                    .height(95)
                    .center(Length::Fill)
                    .style(container::secondary)
                    .into()
            },
            |path| {
                image(image::Handle::from_path(path.to_owned()))
                    .width(Length::Fill)
                    .height(95)
                    .content_fit(iced::ContentFit::Contain)
                    .into()
            },
        )
    }

    fn view_machine_unavailable(&self) -> Element<'_, Message> {
        let (title, detail) = match &self.machine_status {
            MachineStatus::Connecting => (
                "Connecting to vending machine…",
                "Selections will be available as soon as the MDB session is ready.".to_owned(),
            ),
            MachineStatus::Unavailable(error) => (
                "Vending machine unavailable",
                format!("{error}\n\nThe kiosk will retry automatically."),
            ),
            MachineStatus::Ready | MachineStatus::Simulator => (
                "Vending machine unavailable",
                "Waiting for MDB hardware.".to_owned(),
            ),
        };
        column![
            text(title).size(34),
            text(detail).size(18),
            button("Administrator access")
                .padding(14)
                .style(button::secondary)
                .on_press(Message::OpenAdmin)
        ]
        .align_x(iced::Alignment::Center)
        .spacing(24)
        .into()
    }

    fn machine_controls(&self) -> Element<'_, Message> {
        match &self.machine_status {
            MachineStatus::Simulator => self.simulator_keypad(),
            MachineStatus::Connecting => container(text("Connecting to MDB machine…").size(15))
                .padding(10)
                .width(Length::Fill)
                .style(container::warning)
                .into(),
            MachineStatus::Ready => {
                container(text("MDB machine ready · enter a selection on its keypad").size(15))
                    .padding(10)
                    .width(Length::Fill)
                    .style(container::rounded_box)
                    .into()
            }
            MachineStatus::Unavailable(error) => container(
                text(format!(
                    "MDB unavailable · {error} · retrying automatically"
                ))
                .size(15),
            )
            .padding(10)
            .width(Length::Fill)
            .style(container::danger)
            .into(),
        }
    }

    fn simulator_keypad(&self) -> Element<'_, Message> {
        let mut keys = Column::new().spacing(6);
        let slots = self.engine.catalog().slots().collect::<Vec<_>>();
        for chunk in slots.chunks(4) {
            let mut key_row = Row::new().spacing(6);
            for slot in chunk {
                key_row = key_row.push(
                    button(text(slot.id().as_str()).size(16))
                        .width(Length::FillPortion(1))
                        .style(button::secondary)
                        .on_press(Message::MachineSelected(slot.id().clone())),
                );
            }
            keys = keys.push(key_row);
        }
        container(column![text("SIMULATED VENDING KEYPAD").size(12), keys].spacing(6))
            .padding(8)
            .width(Length::Fill)
            .style(container::dark)
            .into()
    }

    fn notice_view(&self) -> Element<'_, Message> {
        self.notice.as_ref().map_or_else(
            || container("").height(0).into(),
            |notice| {
                container(text(notice).size(15))
                    .padding(8)
                    .width(Length::Fill)
                    .style(container::warning)
                    .into()
            },
        )
    }
}

fn numeric_keypad(
    digit_message: fn(char) -> Message,
    backspace: Message,
    submit: Message,
    submit_enabled: bool,
) -> Element<'static, Message> {
    let mut keypad = Column::new().spacing(8);
    for digits in [['1', '2', '3'], ['4', '5', '6'], ['7', '8', '9']] {
        let mut digit_row = Row::new().spacing(8);
        for digit in digits {
            digit_row = digit_row.push(
                button(text(digit).size(26))
                    .height(64)
                    .width(Length::FillPortion(1))
                    .on_press(digit_message(digit)),
            );
        }
        keypad = keypad.push(digit_row);
    }
    keypad = keypad.push(
        row![
            button("⌫")
                .height(64)
                .width(Length::FillPortion(1))
                .on_press(backspace),
            button(text('0').size(26))
                .height(64)
                .width(Length::FillPortion(1))
                .on_press(digit_message('0')),
            if submit_enabled {
                button("Enter")
                    .height(64)
                    .width(Length::FillPortion(1))
                    .style(button::success)
                    .on_press(submit)
            } else {
                button("Enter")
                    .height(64)
                    .width(Length::FillPortion(1))
                    .style(button::secondary)
            }
        ]
        .spacing(8),
    );
    keypad.into()
}

fn dollars(cents: u32) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

fn approval_amount(payment: &PaymentKind) -> Result<Level1Amount, String> {
    let raw = match payment {
        PaymentKind::Lightning { price_cents } => u16::try_from(price_cents / 10)
            .map_err(|_| format!("configured Lightning price {price_cents} cents exceeds MDB"))?,
        PaymentKind::Promo { .. } | PaymentKind::FreeVend | PaymentKind::MaintenanceTest => 0,
    };
    Level1Amount::new(raw).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_catalog_prices() {
        assert_eq!(dollars(250), "$2.50");
        assert_eq!(dollars(400), "$4.00");
    }

    #[test]
    fn converts_kiosk_policy_to_mdb_approval_amounts() {
        assert_eq!(
            approval_amount(&PaymentKind::Lightning { price_cents: 250 })
                .unwrap()
                .raw(),
            25
        );
        assert_eq!(approval_amount(&PaymentKind::FreeVend).unwrap().raw(), 0);
    }

    #[test]
    fn hardware_mode_requires_an_explicit_port() {
        let simulator = Args::try_parse_from(["lv-kiosk"]).unwrap();
        assert_eq!(simulator.port, None);
        assert_eq!(simulator.baud, 9600);

        let hardware = Args::try_parse_from(["lv-kiosk", "--port", "/dev/ttyUSB0"]).unwrap();
        assert_eq!(hardware.port, Some(PathBuf::from("/dev/ttyUSB0")));
        assert_eq!(hardware.baud, 9600);
    }

    #[test]
    fn pointer_drag_tracks_content_in_both_directions() {
        let mut drag = PointerDrag::default();
        assert_eq!(drag.move_to(200.0), None);

        drag.press();
        assert_eq!(drag.move_to(175.0), Some(25.0));
        assert_eq!(drag.move_to(190.0), Some(-15.0));

        drag.release();
        assert_eq!(drag.move_to(150.0), None);
    }
}
