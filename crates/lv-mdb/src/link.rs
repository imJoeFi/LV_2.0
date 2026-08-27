use crate::protocol::{
    hex_bytes, parse_adapter_message, parse_ascii_hex, AdapterMessage, FrameDecoder, ReaderCommand,
};
use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_serial::{DataBits, FlowControl, Parity, SerialPort, SerialPortBuilderExt, StopBits};

pub type TraceSink = Arc<dyn Fn(String) + Send + Sync>;

pub struct Link<T> {
    io: T,
    decoder: FrameDecoder,
    decoded_frames: VecDeque<Vec<u8>>,
    trace: TraceSink,
}

impl<T> Link<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(io: T, trace: TraceSink) -> Self {
        Self {
            io,
            decoder: FrameDecoder::default(),
            decoded_frames: VecDeque::new(),
            trace,
        }
    }

    pub async fn send(&mut self, command: ReaderCommand) -> io::Result<()> {
        let frame = command.frame();
        self.io.write_all(&frame).await?;
        self.io.flush().await?;
        (self.trace)(format!("TX  {}   {command}", hex_bytes(&frame)));
        Ok(())
    }

    pub async fn receive(&mut self) -> io::Result<AdapterMessage> {
        let mut chunk = [0_u8; 256];
        loop {
            if let Some(payload_ascii) = self.decoded_frames.pop_front() {
                let Some(bytes) = parse_ascii_hex(&payload_ascii) else {
                    (self.trace)(format!(
                        "RX  {:?}   (text)",
                        String::from_utf8_lossy(&payload_ascii)
                    ));
                    continue;
                };

                match parse_adapter_message(&bytes) {
                    Ok(message) => {
                        let note = match message {
                            AdapterMessage::Vmc(event) => event.note(),
                            AdapterMessage::Ack => "   (ack)",
                            AdapterMessage::Nak => "   (nak)",
                            AdapterMessage::Retransmit => "   (retransmit)",
                        };
                        (self.trace)(format!("RX  {}{note}", hex_bytes(&bytes)));
                        return Ok(message);
                    }
                    Err(error) => {
                        (self.trace)(format!("RX  {}   (ignored: {error})", hex_bytes(&bytes)));
                        continue;
                    }
                }
            }

            let count = self.io.read(&mut chunk).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "MDB adapter disconnected",
                ));
            }
            self.decoded_frames
                .extend(self.decoder.push(&chunk[..count]));
        }
    }
}

pub fn open_serial(port: &Path, baud: u32) -> io::Result<tokio_serial::SerialStream> {
    let port = port.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "serial port path must be valid UTF-8",
        )
    })?;

    let mut stream = tokio_serial::new(port, baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .open_native_async()
        .map_err(io::Error::other)?;
    stream
        .write_data_terminal_ready(true)
        .map_err(io::Error::other)?;
    Ok(stream)
}
