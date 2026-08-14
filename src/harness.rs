use crate::device::{
    MdbConfig, MdbDevice, MdbSession, PendingVend, SessionEndReason, SessionEvent, SessionFunds,
};
use crate::protocol::{ItemNumber, Level1Amount};
use crate::terminal::{flush_input, read_key, timestamp, Cbreak, Console};
use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SETTLE_WATCH: Duration = Duration::from_secs(10);

pub struct HarnessConfig {
    port: PathBuf,
    baud: u32,
    payment_window: Duration,
    funds: SessionFunds,
}

impl HarnessConfig {
    pub fn new(port: PathBuf, baud: u32, payment_window: Duration, funds: u16) -> Self {
        let funds = Level1Amount::new(funds).map_or(SessionFunds::Unknown, SessionFunds::Known);
        Self {
            port,
            baud,
            payment_window,
            funds,
        }
    }
}

pub struct Harness {
    config: HarnessConfig,
    device: Option<MdbDevice>,
    console: Console,
    interrupted: Arc<AtomicBool>,
}

impl Harness {
    pub async fn open(config: HarnessConfig, interrupted: Arc<AtomicBool>) -> io::Result<Self> {
        let console = Console::default();
        let trace_console = console.clone();
        let mdb_config = MdbConfig::new(config.port.clone(), config.baud)
            .with_application_response_time(config.payment_window + Duration::from_secs(1));
        let device = MdbDevice::connect_with_trace(&mdb_config, move |message| {
            trace_console.log(format!("{}  {message}", timestamp()));
        })
        .await?;
        Ok(Self {
            config,
            device: Some(device),
            console,
            interrupted,
        })
    }

    pub async fn run(&mut self) -> io::Result<()> {
        self.print_startup();
        let mut device = Some(self.take_device()?);
        let mut round_number = 0_u64;
        let loop_result = loop {
            if self.is_interrupted() {
                break Ok(());
            }
            round_number += 1;
            let Some(current_device) = device.take() else {
                break Err(io::Error::other("MDB device is unavailable"));
            };
            match self.run_round(current_device, round_number).await {
                Ok((next_device, RoundOutcome::Continue)) => device = Some(next_device),
                Ok((next_device, RoundOutcome::Interrupted)) => {
                    device = Some(next_device);
                    break Ok(());
                }
                Err(error) => break Err(error),
            }
        };

        self.console.set_countdown("");
        self.console.log("\nstopping");
        if let Some(device) = device {
            if let Err(error) = device.shutdown().await {
                self.console.log(format!(
                    "{}  could not stop MDB actor: {error}",
                    timestamp()
                ));
            }
        }
        loop_result
    }

    fn print_startup(&self) {
        self.console
            .log(format!("port     {}", self.config.port.display()));
        self.console.log(format!(
            "window   {:.0}s",
            self.config.payment_window.as_secs_f64()
        ));
        self.console.log(format!(
            "adapter  must advertise at least {:.0}s maximum response time",
            (self.config.payment_window + Duration::from_secs(1)).as_secs_f64()
        ));
        match self.config.funds {
            SessionFunds::Known(funds) => self.console.log(format!(
                "funds    0x{:04X} ({})",
                funds.raw(),
                format_money(funds)
            )),
            SessionFunds::Unknown => self.console.log("funds    0xFFFF (MDB unknown funds)"),
            SessionFunds::MachineMaximum => self.console.log("funds    machine maximum"),
        }
        self.console.log(format!(
            "scale    raw x 10 / 10^2 -- so raw 1345 = {}",
            format_money(Level1Amount::new(1345).expect("1345 is a valid MDB amount"))
        ));
        self.console
            .log("WARNING  answering \"y\" vends for real. B2 is your empty slot.");
        self.console.log("Ctrl+C to stop.");
    }

    async fn run_round(
        &self,
        device: MdbDevice,
        round_number: u64,
    ) -> io::Result<(MdbDevice, RoundOutcome)> {
        if self.is_interrupted() {
            return Ok((device, RoundOutcome::Interrupted));
        }
        self.console.log("");
        self.console.log("=".repeat(68));
        self.console.log(format!(
            "  ROUND {round_number}  --  press a selection on the machine"
        ));
        self.console.log("=".repeat(68));

        let session = device
            .begin_session(self.config.funds)
            .await
            .map_err(io::Error::other)?;
        let (mut session, vend) = self.wait_for_selection(session).await?;
        let Some(vend) = vend else {
            let ended = session.finish().await?;
            return Ok((ended.into_device(), RoundOutcome::Interrupted));
        };
        let price = vend.requested_price();
        let item = vend.item_number();
        self.console.log(format!(
            "{}  ==> SELECTION {}  price={} ({})  raw item bytes {:02X} {:02X}",
            timestamp(),
            format_ap113_item(item),
            price.raw(),
            format_money(price),
            item.bytes()[0],
            item.bytes()[1]
        ));
        show_fake_qr(&self.console, item, price);

        let payment_started = Instant::now();
        let result = self.wait_for_payment(&mut session)?;
        let elapsed = payment_started.elapsed();

        match result {
            PaymentResult::Interrupted => {
                let ended = session.finish().await?;
                return Ok((ended.into_device(), RoundOutcome::Interrupted));
            }
            PaymentResult::Cancelled => {
                self.console.log(format!(
                    "{}  round ended by the machine after {:.1}s",
                    timestamp(),
                    elapsed.as_secs_f64()
                ));
                let ended = session.finish().await?;
                return Ok((ended.into_device(), RoundOutcome::Continue));
            }
            PaymentResult::Paid | PaymentResult::Declined | PaymentResult::Timeout => {}
        }

        let expectation = if result == PaymentResult::Paid {
            self.console.log(format!(
                "{}  payment confirmed after {:.1}s -- approving",
                timestamp(),
                elapsed.as_secs_f64()
            ));
            vend.approve().await.map_err(io::Error::other)?;
            "expect VEND SUCCESS 13 02, or VEND FAILURE 13 03 if the slot is empty"
        } else {
            let reason = if result == PaymentResult::Declined {
                "declined".to_owned()
            } else {
                format!("timed out at {:.0}s", elapsed.as_secs_f64())
            };
            self.console
                .log(format!("{}  payment {reason} -- denying", timestamp()));
            vend.deny().await.map_err(io::Error::other)?;
            "expect SESSION COMPLETE and no vend"
        };

        self.console.log(format!(
            "    watching {}s -- {expectation}",
            SETTLE_WATCH.as_secs()
        ));
        let seen = self.watch(&mut session, SETTLE_WATCH).await?;
        self.report_result(result, &seen);

        let ended = session.finish().await?;
        Ok((ended.into_device(), RoundOutcome::Continue))
    }

    async fn wait_for_selection(
        &self,
        mut session: MdbSession,
    ) -> io::Result<(MdbSession, Option<PendingVend>)> {
        let mut waited = Duration::ZERO;
        loop {
            if self.is_interrupted() {
                return Ok((session, None));
            }
            let Some(event) = session.receive_event(Duration::from_secs(1)).await? else {
                waited += Duration::from_secs(1);
                if waited.as_secs().is_multiple_of(15) {
                    self.console.log(format!(
                        "{}  still waiting for a selection ({}s) -- Ctrl+C to stop",
                        timestamp(),
                        waited.as_secs()
                    ));
                }
                continue;
            };
            match event {
                SessionEvent::VendRequested(vend) => return Ok((session, Some(vend))),
                SessionEvent::Ended { reason } => {
                    self.console.log(format!(
                        "{}  machine ended the session before a selection ({reason:?}) -- re-arming",
                        timestamp()
                    ));
                    let device = session.finish().await?.into_device();
                    session = device
                        .begin_session(self.config.funds)
                        .await
                        .map_err(io::Error::other)?;
                }
                SessionEvent::VendCancelled { .. }
                | SessionEvent::VendDecisionExpired { .. }
                | SessionEvent::VendSucceeded { .. }
                | SessionEvent::VendFailed { .. } => {}
            }
        }
    }

    fn wait_for_payment(&self, session: &mut MdbSession) -> io::Result<PaymentResult> {
        let deadline = Instant::now() + self.config.payment_window;
        flush_input();
        self.console.log(
            "    >>> Payment received?   press  y = vend   n = decline   \
             (no Enter needed, no answer = timeout)",
        );
        let _cbreak = Cbreak::enter()?;

        loop {
            if self.is_interrupted() {
                self.console.set_countdown("");
                return Ok(PaymentResult::Interrupted);
            }

            let now = Instant::now();
            if now >= deadline {
                self.console.set_countdown("");
                return Ok(PaymentResult::Timeout);
            }
            let remaining = deadline.duration_since(now);
            self.console.set_countdown(format!(
                "    waiting for payment... {:4.0}s   press y or n ",
                remaining.as_secs_f64()
            ));

            if matches!(
                session.try_event()?,
                Some(
                    SessionEvent::VendCancelled { .. }
                        | SessionEvent::VendDecisionExpired { .. }
                        | SessionEvent::Ended { .. }
                        | SessionEvent::VendFailed { .. }
                        | SessionEvent::VendSucceeded { .. }
                )
            ) {
                self.console.set_countdown("");
                return Ok(PaymentResult::Cancelled);
            }

            let Some(answer) = read_key(Duration::from_millis(250).min(remaining))? else {
                continue;
            };
            match answer.to_ascii_lowercase() {
                b'y' => {
                    self.console.set_countdown("");
                    self.console
                        .log(format!("{}  >>> you pressed Y", timestamp()));
                    return Ok(PaymentResult::Paid);
                }
                b'n' => {
                    self.console.set_countdown("");
                    self.console
                        .log(format!("{}  >>> you pressed N", timestamp()));
                    return Ok(PaymentResult::Declined);
                }
                _ => {}
            }
        }
    }

    async fn watch(&self, session: &mut MdbSession, duration: Duration) -> io::Result<WatchResult> {
        let mut seen = WatchResult::default();
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline && !self.is_interrupted() {
            let Some(event) = session.receive_event(Duration::from_millis(200)).await? else {
                continue;
            };
            seen.record(&event);
            if seen.session_ended {
                break;
            }
        }
        Ok(seen)
    }

    fn report_result(&self, payment: PaymentResult, seen: &WatchResult) {
        if seen.vend_success {
            let item = seen
                .vended_item
                .map(|item| format!(" {}", format_ap113_item(item)))
                .unwrap_or_default();
            self.console
                .log(format!("{}  RESULT: vended{item}", timestamp()));
        } else if payment == PaymentResult::Paid {
            if seen.vend_failure {
                self.console.log(format!(
                    "{}  RESULT: approved but DID NOT VEND (empty slot, or motor/sensor fault)",
                    timestamp()
                ));
            } else {
                self.console.log(format!(
                    "{}  RESULT: approved, machine said {}",
                    timestamp(),
                    seen.description()
                ));
            }
        } else {
            let reason = if payment == PaymentResult::Declined {
                "declined"
            } else {
                "timed out"
            };
            self.console.log(format!(
                "{}  RESULT: {reason}, machine said {} -- no vend, nothing charged",
                timestamp(),
                seen.description()
            ));
        }
    }

    fn take_device(&mut self) -> io::Result<MdbDevice> {
        self.device
            .take()
            .ok_or_else(|| io::Error::other("MDB device is already in use"))
    }

    fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaymentResult {
    Paid,
    Declined,
    Timeout,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundOutcome {
    Continue,
    Interrupted,
}

#[derive(Default)]
struct WatchResult {
    vend_success: bool,
    vend_failure: bool,
    session_ended: bool,
    vended_item: Option<ItemNumber>,
    names: BTreeSet<&'static str>,
}

impl WatchResult {
    fn record(&mut self, event: &SessionEvent) {
        self.names.insert(event.name());
        match event {
            SessionEvent::VendSucceeded { reported_item, .. } => {
                self.vend_success = true;
                self.vended_item = *reported_item;
            }
            SessionEvent::VendFailed { .. } => self.vend_failure = true,
            SessionEvent::Ended { reason } => {
                self.session_ended = true;
                if *reason != SessionEndReason::Completed {
                    self.names.insert("abnormal_session_end");
                }
            }
            SessionEvent::VendRequested(_)
            | SessionEvent::VendCancelled { .. }
            | SessionEvent::VendDecisionExpired { .. } => {}
        }
    }

    fn description(&self) -> String {
        if self.names.is_empty() {
            "nothing".into()
        } else {
            format!("{:?}", self.names.iter().copied().collect::<Vec<_>>())
        }
    }
}

fn format_money(amount: Level1Amount) -> String {
    let raw = amount.raw();
    let dollars = raw / 10;
    let cents = (raw % 10) * 10;
    format!("${dollars}.{cents:02}")
}

fn format_ap113_item(item: ItemNumber) -> String {
    let [row, column] = item.bytes();
    if row < 26 {
        format!("{}{}", char::from(b'A' + row), column)
    } else {
        format!("{item}")
    }
}

fn show_fake_qr(console: &Console, item: ItemNumber, price: Level1Amount) {
    let selection = format_ap113_item(item);
    let formatted_price = format_money(price);
    let payload = format!("lnbc-FAKE-{selection}-{}", price.raw());
    console.log(format!(
        concat!(
            "\n",
            "        +-------------------------------------+\n",
            "        |                                     |\n",
            "        |     [ QR CODE WOULD APPEAR HERE ]   |\n",
            "        |                                     |\n",
            "        |{selection:^37}|\n",
            "        |{formatted_price:^37}|\n",
            "        |                                     |\n",
            "        +-------------------------------------+\n",
            "        payload: {payload}\n",
        ),
        selection = selection,
        formatted_price = formatted_price,
        payload = payload,
    ));
}
