use clap::Parser;
use iced::widget::{
    button, column, container, image, rich_text, row, scrollable, span, text, Column, Row,
};
use iced::{time, window, Element, Length, Size, Subscription, Task, Theme};
use lv_mdb_tools::kiosk::{
    ArmMode, Catalog, KioskEngine, MachineSelection, PaymentKind, PaymentPolicy, Product,
    PromoCode, SelectionOutcome, SlotHealth, SlotId, StateStore, TransactionId, TransactionStatus,
};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const CUSTOMER_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const LIGHTNING_TIMEOUT: Duration = Duration::from_secs(45);
const RATE_LIMIT_RESET: Duration = Duration::from_secs(60);
const RATE_LIMIT_DELAY: Duration = Duration::from_secs(3);
const ADMIN_LOCKOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Parser)]
#[command(about = "Portrait LightningVEND kiosk UI (simulated MDB backend)")]
struct Args {
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

    /// Shared admin PIN for this simulator milestone.
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
    .title("LightningVEND kiosk simulator")
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
        if let Self::Running(app) = self {
            app.update(message);
        }
        Task::none()
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
            Self::Running(_) => time::every(Duration::from_secs(1)).map(Message::Tick),
            Self::Failed(_) => Subscription::none(),
        }
    }
}

struct KioskApp {
    engine: KioskEngine,
    store: StateStore,
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
        let now = Instant::now();
        Ok(Self {
            engine,
            store,
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
        })
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Tick(now) => self.tick(now),
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
            Message::MachineSelected(slot) => self.machine_selected(&slot),
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
                match self.engine.resolve_uncertain(id, dispensed) {
                    Ok(()) => {
                        self.notice = Some(if dispensed {
                            "Transaction marked dispensed; inventory and entitlement updated."
                                .to_owned()
                        } else {
                            "Transaction marked not dispensed; reservation released.".to_owned()
                        });
                        self.persist();
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
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
            if self.engine.cancel_lightning(id).is_ok() {
                self.persist();
            }
            self.page = Page::Ready;
            self.notice = Some("Lightning payment timed out. No payment was taken.".to_owned());
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

    fn machine_selected(&mut self, slot: &SlotId) {
        self.record_customer_activity();
        match self.engine.machine_selected(slot) {
            Ok(outcome) => {
                self.persist();
                match outcome {
                    SelectionOutcome::LightningPaymentRequired {
                        transaction_id,
                        selection,
                        price_cents,
                    } => {
                        self.notice = None;
                        self.page = Page::Lightning {
                            transaction_id,
                            selection,
                            price_cents,
                            started: self.now,
                        };
                    }
                    SelectionOutcome::VendApproved {
                        transaction_id,
                        selection,
                        payment,
                    } => {
                        self.notice = None;
                        self.page = Page::Dispensing {
                            transaction_id,
                            selection,
                            payment,
                        };
                    }
                    SelectionOutcome::Denied(message) => self.notice = Some(message),
                }
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn lightning_accepted(&mut self, id: TransactionId) {
        match self.engine.lightning_payment_accepted(id) {
            Ok(SelectionOutcome::VendApproved {
                transaction_id,
                selection,
                payment,
            }) => {
                self.persist();
                self.page = Page::Dispensing {
                    transaction_id,
                    selection,
                    payment,
                };
                self.notice = None;
            }
            Ok(_) => self.notice = Some("Unexpected Lightning transition.".to_owned()),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn lightning_cancelled(&mut self, id: TransactionId) {
        match self.engine.cancel_lightning(id) {
            Ok(()) => {
                self.persist();
                self.page = Page::Ready;
                self.notice = Some("Lightning payment cancelled.".to_owned());
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn simulated_vend(&mut self, id: TransactionId, result: SimulatedVendResult) {
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
                self.persist();
                self.record_customer_activity();
                let destination = match payment {
                    Some(PaymentKind::Promo { .. }) => Destination::Promo,
                    Some(PaymentKind::FreeVend | PaymentKind::MaintenanceTest) => {
                        Destination::Admin
                    }
                    _ => Destination::Ready,
                };
                let (title, body) = match result {
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
                    body: body.to_owned(),
                    destination,
                };
            }
            Err(error) => self.notice = Some(error.to_string()),
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

    fn persist(&mut self) {
        if let Err(error) = self.store.save(self.engine.state()) {
            self.notice = Some(format!("Could not save kiosk state: {error}"));
        }
    }

    fn record_customer_activity(&mut self) {
        self.last_customer_activity = self.now;
    }

    fn view(&self) -> Element<'_, Message> {
        let page = match &self.page {
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
            } => Self::view_dispensing(*transaction_id, selection, payment),
            Page::Result {
                title,
                body,
                destination,
            } => Self::view_result(title, body, *destination),
            Page::AdminPin => self.view_admin_pin(),
            Page::Admin => self.view_admin(),
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

        let content = column![
            header,
            self.notice_view(),
            promo,
            text("Paying with Lightning?").size(20),
            text("Enter a Lightning item on the vending machine keypad."),
            scrollable(self.catalog_grid()).height(Length::Fill),
            self.simulator_keypad()
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
            self.simulator_keypad()
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
            button("Vend — simulate accepted hold invoice")
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
        transaction_id: TransactionId,
        selection: &MachineSelection,
        payment: &PaymentKind,
    ) -> Element<'static, Message> {
        column![
            text("Dispensing…").size(38),
            text(selection.product_name().to_owned()).size(26),
            text(format!(
                "Selection {} · {}",
                selection.slot(),
                payment.name()
            ))
            .size(18),
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
                .spacing(10)
            )
            .padding(16)
            .width(Length::Fill)
            .style(container::rounded_box)
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
            self.simulator_keypad(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_catalog_prices() {
        assert_eq!(dollars(250), "$2.50");
        assert_eq!(dollars(400), "$4.00");
    }
}
