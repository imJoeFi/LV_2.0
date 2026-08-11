use crate::protocol::{
    frame_note, hex_bytes, parse_ascii_hex, FrameDecoder, ReaderCommand, VmcEvent,
};
use crate::terminal::{timestamp, Console};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const SERIAL_READ_TIMEOUT: Duration = Duration::from_millis(200);
type LinkEvent = Result<VmcEvent, String>;

pub struct Link {
    writer: Box<dyn SerialPort>,
    events: Receiver<LinkEvent>,
    stop: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
    console: Console,
}

impl Link {
    pub(crate) fn open(port: &Path, baud: u32, console: Console) -> io::Result<Self> {
        let writer = open_serial(port, baud)?;
        let mut reader = writer.try_clone().map_err(io::Error::other)?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader_console = console.clone();
        let (sender, events) = mpsc::channel();

        let reader_thread =
            thread::Builder::new()
                .name("mdb-serial-rx".into())
                .spawn(move || {
                    receive_loop(
                        reader.as_mut(),
                        &reader_console,
                        &sender,
                        reader_stop.as_ref(),
                    );
                })?;

        Ok(Self {
            writer,
            events,
            stop,
            reader_thread: Some(reader_thread),
            console,
        })
    }

    pub(crate) fn send(&mut self, command: ReaderCommand) -> io::Result<()> {
        let frame = command.frame();
        self.writer.write_all(&frame)?;
        self.writer.flush()?;
        self.console.log(format!(
            "{}  TX  {}   {command}",
            timestamp(),
            hex_bytes(&frame)
        ));
        Ok(())
    }

    pub(crate) fn try_event(&self) -> io::Result<Option<VmcEvent>> {
        match self.events.try_recv() {
            Ok(event) => event.map(Some).map_err(link_failure),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(reader_stopped()),
        }
    }

    pub(crate) fn receive_event(&self, timeout: Duration) -> io::Result<Option<VmcEvent>> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => event.map(Some).map_err(link_failure),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(reader_stopped()),
        }
    }

    pub(crate) fn drain_events(&self) -> io::Result<()> {
        while self.try_event()?.is_some() {}
        Ok(())
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop();
    }
}

fn receive_loop(
    reader: &mut dyn SerialPort,
    console: &Console,
    sender: &Sender<LinkEvent>,
    stop: &AtomicBool,
) {
    let mut decoder = FrameDecoder::default();
    let mut chunk = [0_u8; 256];

    while !stop.load(Ordering::Relaxed) {
        match reader.read(&mut chunk) {
            Ok(0) => {}
            Ok(count) => {
                for payload in decoder.push(&chunk[..count]) {
                    handle_received_frame(&payload, console, sender);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) =>
            {
                // Expected every 200 ms so this thread can notice `stop`.
            }
            Err(error) => {
                let message = format!("{}  read failed: {error}", timestamp());
                console.log(&message);
                let _ = sender.send(Err(message));
                return;
            }
        }
    }
}

fn handle_received_frame(payload_ascii: &[u8], console: &Console, sender: &Sender<LinkEvent>) {
    let Some(bytes) = parse_ascii_hex(payload_ascii) else {
        console.log(format!(
            "{}  RX  {:?}   (text)",
            timestamp(),
            String::from_utf8_lossy(payload_ascii)
        ));
        return;
    };

    let event = VmcEvent::try_from(bytes.as_slice()).ok();
    console.log(format!(
        "{}  RX  {}{}",
        timestamp(),
        hex_bytes(&bytes),
        frame_note(&bytes, event)
    ));
    if let Some(event) = event {
        let _ = sender.send(Ok(event));
    }
}

fn open_serial(port: &Path, baud: u32) -> io::Result<Box<dyn SerialPort>> {
    let port = port.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "serial port path must be valid UTF-8",
        )
    })?;

    serialport::new(port, baud)
        .timeout(SERIAL_READ_TIMEOUT)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .dtr_on_open(true)
        .open()
        .map_err(io::Error::other)
}

fn link_failure(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, message)
}

fn reader_stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "serial reader stopped")
}
