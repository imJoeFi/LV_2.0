use crate::link::Link;
use crate::protocol::{Funds, Price, ReaderCommand, Selection, VmcEvent};
use crate::terminal::{flush_input, read_key, timestamp, Cbreak, Console};
use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const SETTLE_WATCH: Duration = Duration::from_secs(10);

pub struct HarnessConfig {
    port: PathBuf,
    baud: u32,
    payment_window: Duration,
    funds: Funds,
}

impl HarnessConfig {
    pub fn new(port: PathBuf, baud: u32, payment_window: Duration, funds: u16) -> Self {
        Self {
            port,
            baud,
            payment_window,
            funds: Funds::new(funds),
        }
    }
}

pub struct Harness {
    config: HarnessConfig,
    link: Link,
    console: Console,
    interrupted: Arc<AtomicBool>,
}

impl Harness {
    pub fn open(config: HarnessConfig, interrupted: Arc<AtomicBool>) -> io::Result<Self> {
        let console = Console::default();
        let link = Link::open(&config.port, config.baud, console.clone())?;
        thread::sleep(Duration::from_millis(500));
        Ok(Self {
            config,
            link,
            console,
            interrupted,
        })
    }

    pub fn clear_session(&mut self) -> io::Result<()> {
        self.console.log("clearing any open session...");
        self.link.send(ReaderCommand::EndSession)?;
        thread::sleep(Duration::from_millis(1500));
        self.link.stop();
        self.console
            .log("done -- if the display is still stuck, power-cycle the machine");
        Ok(())
    }

    pub fn run(&mut self) -> io::Result<()> {
        self.print_startup();
        let mut round_number = 0_u64;
        let loop_result = loop {
            if self.is_interrupted() {
                break Ok(());
            }
            round_number += 1;
            match self.run_round(round_number) {
                Ok(RoundOutcome::Continue) => {}
                Ok(RoundOutcome::Interrupted) => break Ok(()),
                Err(error) => break Err(error),
            }
        };

        self.console.set_countdown("");
        self.console.log("\nstopping -- closing the session");
        if let Err(error) = self.link.send(ReaderCommand::EndSession) {
            self.console
                .log(format!("{}  could not close session: {error}", timestamp()));
        }
        thread::sleep(Duration::from_millis(500));
        self.link.stop();
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
            "funds    0x{:04X} ({})",
            self.config.funds.raw(),
            self.config.funds
        ));
        self.console.log(format!(
            "scale    raw x 10 / 10^2 -- so raw 1345 = {}",
            Price::new(1345)
        ));
        self.console
            .log("WARNING  answering \"y\" vends for real. B2 is your empty slot.");
        self.console.log("Ctrl+C to stop.");
    }

    fn run_round(&mut self, round_number: u64) -> io::Result<RoundOutcome> {
        if self.is_interrupted() {
            return Ok(RoundOutcome::Interrupted);
        }
        self.console.log("");
        self.console.log("=".repeat(68));
        self.console.log(format!(
            "  ROUND {round_number}  --  press a selection on the machine"
        ));
        self.console.log("=".repeat(68));

        self.link.drain_events()?;
        self.link.send(ReaderCommand::EndSession)?;
        thread::sleep(Duration::from_millis(500));
        if self.is_interrupted() {
            return Ok(RoundOutcome::Interrupted);
        }
        self.link.drain_events()?;
        self.link
            .send(ReaderCommand::BeginSession(self.config.funds))?;

        let Some((price, selection)) = self.wait_for_selection()? else {
            return Ok(RoundOutcome::Interrupted);
        };
        self.console.log(format!(
            "{}  ==> SELECTION {selection}  price={} ({price})  raw item bytes \
             {:02X} {:02X}",
            timestamp(),
            price.raw(),
            selection.row(),
            selection.column()
        ));
        show_fake_qr(&self.console, selection, price);

        let payment_started = Instant::now();
        let result = self.wait_for_payment()?;
        let elapsed = payment_started.elapsed();

        match result {
            PaymentResult::Interrupted => return Ok(RoundOutcome::Interrupted),
            PaymentResult::Cancelled => {
                self.console.log(format!(
                    "{}  round ended by the machine after {:.1}s",
                    timestamp(),
                    elapsed.as_secs_f64()
                ));
                return Ok(RoundOutcome::Continue);
            }
            _ => {}
        }

        let expectation = if result == PaymentResult::Paid {
            self.console.log(format!(
                "{}  payment confirmed after {:.1}s -- approving",
                timestamp(),
                elapsed.as_secs_f64()
            ));
            self.link.send(ReaderCommand::Approve(price))?;
            "expect VEND SUCCESS 13 02, or VEND FAILURE 13 03 if the slot is empty"
        } else {
            let reason = if result == PaymentResult::Declined {
                "declined".to_owned()
            } else {
                format!("timed out at {:.0}s", elapsed.as_secs_f64())
            };
            self.console.log(format!(
                "{}  payment {reason} -- denying so the machine resets",
                timestamp()
            ));
            self.link.send(ReaderCommand::Deny)?;
            "expect the machine to reset and release the selection"
        };

        self.console.log(format!(
            "    watching {}s -- {expectation}",
            SETTLE_WATCH.as_secs()
        ));
        let seen = self.watch(SETTLE_WATCH)?;
        self.report_result(result, &seen);

        if !seen.session_complete {
            self.console.log(format!(
                "{}  no SESSION COMPLETE -- closing the session ourselves",
                timestamp()
            ));
            self.link.send(ReaderCommand::EndSession)?;
            thread::sleep(Duration::from_secs(1));
        }
        Ok(RoundOutcome::Continue)
    }

    fn wait_for_selection(&mut self) -> io::Result<Option<(Price, Selection)>> {
        let mut waited = Duration::ZERO;
        loop {
            if self.is_interrupted() {
                return Ok(None);
            }
            let Some(event) = self.link.receive_event(Duration::from_secs(1))? else {
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
                VmcEvent::VendRequest { price, selection } => {
                    return Ok(Some((price, selection)));
                }
                VmcEvent::SessionComplete => {
                    self.console.log(format!(
                        "{}  machine closed the session before a selection -- re-arming",
                        timestamp()
                    ));
                    self.link.send(ReaderCommand::AcknowledgeSessionComplete)?;
                    thread::sleep(Duration::from_millis(500));
                    self.link
                        .send(ReaderCommand::BeginSession(self.config.funds))?;
                }
                _ => {}
            }
        }
    }

    fn wait_for_payment(&self) -> io::Result<PaymentResult> {
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
                self.link.try_event()?,
                Some(VmcEvent::VendCancel | VmcEvent::SessionComplete | VmcEvent::VendFailure)
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

    fn watch(&mut self, duration: Duration) -> io::Result<WatchResult> {
        let mut seen = WatchResult::default();
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline && !self.is_interrupted() {
            let Some(event) = self.link.receive_event(Duration::from_millis(200))? else {
                continue;
            };
            seen.record(event);
            if seen.session_complete {
                self.link.send(ReaderCommand::AcknowledgeSessionComplete)?;
                break;
            }
        }
        Ok(seen)
    }

    fn report_result(&self, payment: PaymentResult, seen: &WatchResult) {
        if seen.vend_success {
            let selection = seen
                .vended_selection
                .map(|selection| format!(" {selection}"))
                .unwrap_or_default();
            self.console
                .log(format!("{}  RESULT: vended{selection}", timestamp()));
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
    session_complete: bool,
    vended_selection: Option<Selection>,
    names: BTreeSet<&'static str>,
}

impl WatchResult {
    fn record(&mut self, event: VmcEvent) {
        self.names.insert(event.name());
        match event {
            VmcEvent::VendSuccess { selection } => {
                self.vend_success = true;
                self.vended_selection = selection;
            }
            VmcEvent::VendFailure => self.vend_failure = true,
            VmcEvent::SessionComplete => self.session_complete = true,
            _ => {}
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

fn show_fake_qr(console: &Console, selection: Selection, price: Price) {
    let payload = format!("lnbc-FAKE-{selection}-{}", price.raw());
    console.log(format!(
        concat!(
            "\n",
            "        +-------------------------------------+\n",
            "        |                                     |\n",
            "        |     [ QR CODE WOULD APPEAR HERE ]   |\n",
            "        |                                     |\n",
            "        |{selection:^37}|\n",
            "        |{price:^37}|\n",
            "        |                                     |\n",
            "        +-------------------------------------+\n",
            "        payload: {payload}\n",
        ),
        selection = selection,
        price = price,
        payload = payload,
    ));
}
