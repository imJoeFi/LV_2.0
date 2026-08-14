//! Bench harness for the full MDB machine-interaction flow, with payment faked.
//!
//! This is the Rust counterpart to `mdb/mdb_flow_test.py`. It drives the
//! WAFER RS232-MDB (PC2MDB) box, waits for a selection, shows a fake QR
//! placeholder, and asks a human to approve or deny the vend. Answering `y`
//! vends for real.

use clap::Parser;
use lv_mdb_tools::{Harness, HarnessConfig};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const AP113_DISPLAY_CELLS: usize = 16;

#[derive(Debug, Parser)]
#[command(
    name = "mdb-flow-test",
    about = "Exercise the AP 113 select-first MDB flow with a human standing in for payment",
    long_about = None
)]
struct Args {
    /// Serial port connected to the WAFER RS232-MDB box.
    #[arg(long, default_value = "/dev/ttyUSB0")]
    port: PathBuf,

    /// Serial baud rate. The PC2MDB box normally uses 9600.
    #[arg(long, default_value_t = 9600)]
    baud: u32,

    /// Payment window in seconds. Keep this below the VMC's 60-second limit.
    #[arg(long, default_value = "45", value_parser = parse_timeout)]
    timeout: Duration,

    /// Raw Begin Session funds (decimal, or prefixed with 0x/0o/0b).
    #[arg(long, default_value = "0xFFFF", value_parser = parse_int::parse::<u16>)]
    funds: u16,

    /// Override the AP 113's 16-character VMC display while awaiting a selection.
    #[arg(long, value_name = "TEXT", value_parser = parse_vmc_display_message)]
    vmc_display_message: Option<String>,
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| format!("expected a number of seconds, got {value:?}"))?;
    if seconds.is_finite() && seconds > 0.0 && seconds < 60.0 {
        Ok(Duration::from_secs_f64(seconds))
    } else {
        Err("timeout must be greater than 0 and below the VMC's 60-second limit".into())
    }
}

fn parse_vmc_display_message(value: &str) -> Result<String, String> {
    if value.len() > AP113_DISPLAY_CELLS {
        return Err(format!(
            "the AP 113 VMC display accepts at most {AP113_DISPLAY_CELLS} ASCII characters"
        ));
    }
    if !value.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err("the VMC display message must contain printable ASCII only".into());
    }
    Ok(value.to_owned())
}

async fn run(args: Args) -> io::Result<()> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "this needs an interactive terminal for the y/n prompt",
        ));
    }

    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::Relaxed))
        .map_err(|error| io::Error::other(format!("could not install Ctrl+C handler: {error}")))?;

    let mut config = HarnessConfig::new(args.port, args.baud, args.timeout, args.funds);
    if let Some(message) = args.vmc_display_message {
        config = config.with_vmc_display_message(message);
    }
    let mut harness = Harness::open(config, interrupted).await?;
    harness.run().await
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("mdb-flow-test: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_defaults_match_the_python_harness() {
        let args = Args::try_parse_from(["mdb-flow-test"]).unwrap();
        assert_eq!(args.port, PathBuf::from("/dev/ttyUSB0"));
        assert_eq!(args.baud, 9600);
        assert_eq!(args.timeout, Duration::from_secs(45));
        assert_eq!(args.funds, 0xffff);
        assert_eq!(args.vmc_display_message, None);
    }

    #[test]
    fn parses_prefixed_funds() {
        for (value, expected) in [("0x0541", 1345), ("0o12", 10), ("0b1010", 10)] {
            let args = Args::try_parse_from(["mdb-flow-test", "--funds", value]).unwrap();
            assert_eq!(args.funds, expected);
        }
    }

    #[test]
    fn rejects_a_payment_window_that_can_race_the_machine() {
        assert!(Args::try_parse_from(["mdb-flow-test", "--timeout", "60"]).is_err());
    }

    #[test]
    fn parses_a_vmc_display_override() {
        let args =
            Args::try_parse_from(["mdb-flow-test", "--vmc-display-message", "MAKE A SELECTION"])
                .unwrap();
        assert_eq!(
            args.vmc_display_message.as_deref(),
            Some("MAKE A SELECTION")
        );
    }

    #[test]
    fn rejects_invalid_vmc_display_text() {
        assert!(Args::try_parse_from([
            "mdb-flow-test",
            "--vmc-display-message",
            "THIS MESSAGE IS TOO LONG",
        ])
        .is_err());
        assert!(
            Args::try_parse_from(["mdb-flow-test", "--vmc-display-message", "SNOWMAN ☃",]).is_err()
        );
    }
}
