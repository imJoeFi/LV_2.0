use std::error::Error;
use std::fmt;
use std::time::Duration;

const MAX_BUFFER_SIZE: usize = 4096;
const MAX_DISPLAY_DATA_LENGTH: usize = 32;
const UNKNOWN_AMOUNT: u16 = u16::MAX;

/// A numeric MDB Level 1 monetary value.
///
/// `0xFFFF` is deliberately excluded because MDB reserves it for an unknown
/// value rather than the largest numeric amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Level1Amount(u16);

impl Level1Amount {
    pub const MAX: Self = Self(0xfffe);

    pub const fn new(raw: u16) -> Result<Self, InvalidAmount> {
        if raw == UNKNOWN_AMOUNT {
            Err(InvalidAmount)
        } else {
            Ok(Self(raw))
        }
    }

    pub const fn raw(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Level1Amount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} raw MDB units", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAmount;

impl fmt::Display for InvalidAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0xFFFF is MDB's reserved unknown amount")
    }
}

impl Error for InvalidAmount {}

/// A manufacturer-defined MDB item number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemNumber(u16);

impl ItemNumber {
    pub const UNKNOWN: Self = Self(u16::MAX);

    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u16 {
        self.0
    }

    pub const fn bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }
}

impl fmt::Display for ItemNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "0x{:04X}", self.0)
    }
}

/// Time requested for an MDB message on the VMC display, in 0.1-second units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayTime(u8);

impl DisplayTime {
    /// The longest display request representable by MDB: 25.5 seconds.
    pub const MAX: Self = Self(u8::MAX);

    pub const fn from_deciseconds(deciseconds: u8) -> Self {
        Self(deciseconds)
    }

    pub const fn as_deciseconds(self) -> u8 {
        self.0
    }

    pub const fn duration(self) -> Duration {
        Duration::from_millis(self.0 as u64 * 100)
    }
}

impl TryFrom<Duration> for DisplayTime {
    type Error = InvalidDisplayTime;

    fn try_from(duration: Duration) -> Result<Self, Self::Error> {
        const DECISECOND_NANOS: u128 = 100_000_000;

        let nanoseconds = duration.as_nanos();
        if !nanoseconds.is_multiple_of(DECISECOND_NANOS) {
            return Err(InvalidDisplayTime);
        }
        let deciseconds = nanoseconds / DECISECOND_NANOS;
        u8::try_from(deciseconds)
            .map(Self)
            .map_err(|_| InvalidDisplayTime)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDisplayTime;

impl fmt::Display for InvalidDisplayTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("display time must be an exact 0.1-second value from 0 to 25.5 seconds")
    }
}

impl Error for InvalidDisplayTime {}

/// Geometry reported by the VMC for its MDB-accessible display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayDimensions {
    columns: u8,
    rows: u8,
}

impl DisplayDimensions {
    pub const fn new(columns: u8, rows: u8) -> Result<Self, InvalidDisplayDimensions> {
        if columns == 0 || rows == 0 {
            Err(InvalidDisplayDimensions)
        } else {
            Ok(Self { columns, rows })
        }
    }

    pub const fn columns(self) -> u8 {
        self.columns
    }

    pub const fn rows(self) -> u8 {
        self.rows
    }

    pub fn capacity(self) -> usize {
        usize::from(self.columns)
            .saturating_mul(usize::from(self.rows))
            .min(MAX_DISPLAY_DATA_LENGTH)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDisplayDimensions;

impl fmt::Display for InvalidDisplayDimensions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MDB display dimensions must have at least one row and one column")
    }
}

impl Error for InvalidDisplayDimensions {}

/// Character repertoire advertised by the VMC for its display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayCharacterSet {
    /// Digits, uppercase letters, spaces, and decimal points.
    Basic,
    /// Printable ASCII.
    FullAscii,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderCommand {
    BeginSession(u16),
    DisplayRequest { time: DisplayTime, data: Vec<u8> },
    SessionCancel,
    Approve(Level1Amount),
    Deny,
    EndSession,
    Cancelled,
    CommandOutOfSequence,
}

impl ReaderCommand {
    fn payload(&self) -> Vec<u8> {
        match self {
            Self::BeginSession(funds) => {
                let [high, low] = funds.to_be_bytes();
                vec![0x03, high, low]
            }
            Self::DisplayRequest { time, data } => {
                let mut payload = Vec::with_capacity(data.len() + 2);
                payload.extend([0x02, time.as_deciseconds()]);
                payload.extend(data);
                payload
            }
            Self::SessionCancel => vec![0x04],
            Self::Approve(amount) => {
                let [high, low] = amount.raw().to_be_bytes();
                vec![0x05, high, low]
            }
            Self::Deny => vec![0x06],
            Self::EndSession => vec![0x07],
            Self::Cancelled => vec![0x08],
            Self::CommandOutOfSequence => vec![0x0b],
        }
    }

    pub fn frame(&self) -> Vec<u8> {
        let mut payload = self.payload();
        payload.push(checksum(&payload));
        payload
    }
}

impl fmt::Display for ReaderCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeginSession(UNKNOWN_AMOUNT) => {
                formatter.write_str("BEGIN SESSION (funds unknown)")
            }
            Self::BeginSession(funds) => write!(formatter, "BEGIN SESSION (funds={funds})"),
            Self::DisplayRequest { time, data } => write!(
                formatter,
                "DISPLAY REQUEST ({:.1}s, {:?})",
                time.duration().as_secs_f64(),
                String::from_utf8_lossy(data).trim_end()
            ),
            Self::SessionCancel => formatter.write_str("SESSION CANCEL REQUEST"),
            Self::Approve(amount) => write!(formatter, "VEND APPROVED (amount={})", amount.raw()),
            Self::Deny => formatter.write_str("VEND DENIED"),
            Self::EndSession => formatter.write_str("END SESSION"),
            Self::Cancelled => formatter.write_str("CANCELLED"),
            Self::CommandOutOfSequence => formatter.write_str("COMMAND OUT OF SEQUENCE"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcEvent {
    Reset,
    SetupConfiguration {
        feature_level: u8,
        display_columns: u8,
        display_rows: u8,
        display_character_set: u8,
    },
    SetupPrices {
        maximum: Option<Level1Amount>,
        minimum: Option<Level1Amount>,
    },
    VendRequest {
        price: Level1Amount,
        item: ItemNumber,
    },
    VendCancel,
    VendSuccess {
        item: Option<ItemNumber>,
    },
    VendFailure,
    SessionComplete,
    CashSale {
        price: Level1Amount,
        item: ItemNumber,
    },
    ReaderDisable,
    ReaderEnable,
    ReaderCancel,
    Other,
}

impl VmcEvent {
    pub const fn note(self) -> &'static str {
        match self {
            Self::Reset => "   <== RESET",
            Self::SetupConfiguration { .. } => "   <== SETUP CONFIGURATION",
            Self::SetupPrices { .. } => "   <== SETUP MAX/MIN PRICES",
            Self::VendRequest { .. } => "   <== VEND REQUEST",
            Self::VendCancel => "   <== VEND CANCEL",
            Self::VendSuccess { .. } => "   <== VEND SUCCESS",
            Self::VendFailure => "   <== VEND FAILURE",
            Self::SessionComplete => "   <== SESSION COMPLETE",
            Self::CashSale { .. } => "   <== CASH SALE",
            Self::ReaderDisable => "   <== READER DISABLE",
            Self::ReaderEnable => "   <== READER ENABLE",
            Self::ReaderCancel => "   <== READER CANCEL",
            Self::Other => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterMessage {
    Ack,
    Nak,
    Retransmit,
    Vmc(VmcEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    TooShort,
    BadChecksum,
    InvalidLength,
    ReservedAmount,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => formatter.write_str("MDB frame is too short"),
            Self::BadChecksum => formatter.write_str("MDB checksum does not match"),
            Self::InvalidLength => formatter.write_str("MDB frame has an invalid length"),
            Self::ReservedAmount => {
                formatter.write_str("MDB frame uses 0xFFFF as a numeric amount")
            }
        }
    }
}

impl Error for ProtocolError {}

pub fn parse_adapter_message(bytes: &[u8]) -> Result<AdapterMessage, ProtocolError> {
    match bytes {
        [0x00] => return Ok(AdapterMessage::Ack),
        // The WAFER PC2MDB forwards RESET as the single command byte rather
        // than the usual payload-plus-checksum representation.
        [0x10] => return Ok(AdapterMessage::Vmc(VmcEvent::Reset)),
        [0xff] => return Ok(AdapterMessage::Nak),
        [0xaa] => return Ok(AdapterMessage::Retransmit),
        _ => {}
    }

    validate_checksum(bytes)?;
    let payload = &bytes[..bytes.len() - 1];
    let event = match payload {
        [0x10] => VmcEvent::Reset,
        [0x11, 0x00, feature_level, display_columns, display_rows, display_info] => {
            VmcEvent::SetupConfiguration {
                feature_level: *feature_level,
                display_columns: *display_columns,
                display_rows: *display_rows,
                display_character_set: *display_info & 0b111,
            }
        }
        [0x11, 0x01, maximum_high, maximum_low, minimum_high, minimum_low] => {
            VmcEvent::SetupPrices {
                maximum: optional_amount(*maximum_high, *maximum_low),
                minimum: optional_amount(*minimum_high, *minimum_low),
            }
        }
        [0x13, 0x00, price_high, price_low, item_high, item_low] => VmcEvent::VendRequest {
            price: amount(*price_high, *price_low)?,
            item: ItemNumber::new(u16::from_be_bytes([*item_high, *item_low])),
        },
        [0x13, 0x01] => VmcEvent::VendCancel,
        [0x13, 0x02] => VmcEvent::VendSuccess { item: None },
        [0x13, 0x02, item_high, item_low] => VmcEvent::VendSuccess {
            item: Some(ItemNumber::new(u16::from_be_bytes([*item_high, *item_low]))),
        },
        [0x13, 0x03] => VmcEvent::VendFailure,
        [0x13, 0x04] => VmcEvent::SessionComplete,
        [0x13, 0x05, price_high, price_low, item_high, item_low] => VmcEvent::CashSale {
            price: amount(*price_high, *price_low)?,
            item: ItemNumber::new(u16::from_be_bytes([*item_high, *item_low])),
        },
        [0x14, 0x00] => VmcEvent::ReaderDisable,
        [0x14, 0x01] => VmcEvent::ReaderEnable,
        [0x14, 0x02] => VmcEvent::ReaderCancel,
        [0x10, ..]
        | [0x11 | 0x13 | 0x14, 0x00 | 0x01, ..]
        | [0x13 | 0x14, 0x02, ..]
        | [0x13, 0x03..=0x05, ..] => return Err(ProtocolError::InvalidLength),
        _ => VmcEvent::Other,
    };
    Ok(AdapterMessage::Vmc(event))
}

fn validate_checksum(bytes: &[u8]) -> Result<(), ProtocolError> {
    if bytes.len() < 2 {
        return Err(ProtocolError::TooShort);
    }
    let (&received, payload) = bytes.split_last().ok_or(ProtocolError::TooShort)?;
    if checksum(payload) == received {
        Ok(())
    } else {
        Err(ProtocolError::BadChecksum)
    }
}

fn amount(high: u8, low: u8) -> Result<Level1Amount, ProtocolError> {
    Level1Amount::new(u16::from_be_bytes([high, low])).map_err(|_| ProtocolError::ReservedAmount)
}

fn optional_amount(high: u8, low: u8) -> Option<Level1Amount> {
    Level1Amount::new(u16::from_be_bytes([high, low])).ok()
}

#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        loop {
            let Some(start) = self.buffer.iter().position(|byte| *byte == 0x02) else {
                if self.buffer.len() > MAX_BUFFER_SIZE {
                    self.buffer.clear();
                }
                break;
            };
            let Some(relative_end) = self.buffer[start + 1..]
                .iter()
                .position(|byte| *byte == 0x03)
            else {
                if start > 0 {
                    self.buffer.drain(..start);
                }
                if self.buffer.len() > MAX_BUFFER_SIZE {
                    self.buffer.clear();
                }
                break;
            };
            let end = start + 1 + relative_end;
            frames.push(self.buffer[start + 1..end].to_vec());
            self.buffer.drain(..=end);
        }

        frames
    }
}

pub fn parse_ascii_hex(input: &[u8]) -> Option<Vec<u8>> {
    let compact: Vec<u8> = input
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    hex::decode(compact).ok()
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn checksum(payload: &[u8]) -> u8 {
    payload
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_excludes_the_reserved_unknown_value() {
        assert_eq!(Level1Amount::new(0xfffe), Ok(Level1Amount::MAX));
        assert_eq!(Level1Amount::new(0xffff), Err(InvalidAmount));
    }

    #[test]
    fn display_time_uses_exact_deciseconds() {
        assert_eq!(
            DisplayTime::try_from(Duration::from_millis(100)),
            Ok(DisplayTime::from_deciseconds(1))
        );
        assert_eq!(
            DisplayTime::try_from(Duration::from_millis(25_500)),
            Ok(DisplayTime::MAX)
        );
        assert_eq!(
            DisplayTime::try_from(Duration::from_millis(150)),
            Err(InvalidDisplayTime)
        );
        assert_eq!(
            DisplayTime::try_from(Duration::from_millis(25_600)),
            Err(InvalidDisplayTime)
        );
    }

    #[test]
    fn display_dimensions_are_nonzero_and_cap_messages_at_32_bytes() {
        let dimensions = DisplayDimensions::new(20, 2).unwrap();
        assert_eq!(dimensions.columns(), 20);
        assert_eq!(dimensions.rows(), 2);
        assert_eq!(dimensions.capacity(), 32);
        assert_eq!(DisplayDimensions::new(0, 1), Err(InvalidDisplayDimensions));
    }

    #[test]
    fn level_one_responses_have_the_required_checksums() {
        assert_eq!(
            ReaderCommand::BeginSession(100).frame(),
            [0x03, 0x00, 0x64, 0x67]
        );
        assert_eq!(ReaderCommand::SessionCancel.frame(), [0x04, 0x04]);
        assert_eq!(
            ReaderCommand::Approve(Level1Amount::new(1251).unwrap()).frame(),
            [0x05, 0x04, 0xe3, 0xec]
        );
        assert_eq!(ReaderCommand::Deny.frame(), [0x06, 0x06]);
        assert_eq!(ReaderCommand::EndSession.frame(), [0x07, 0x07]);
        assert_eq!(ReaderCommand::Cancelled.frame(), [0x08, 0x08]);
        assert_eq!(
            ReaderCommand::DisplayRequest {
                time: DisplayTime::from_deciseconds(10),
                data: b"PAY ".to_vec(),
            }
            .frame(),
            [0x02, 0x0a, 0x50, 0x41, 0x59, 0x20, 0x16]
        );
    }

    #[test]
    fn display_request_supports_the_32_byte_mdb_maximum() {
        let frame = ReaderCommand::DisplayRequest {
            time: DisplayTime::MAX,
            data: vec![b'A'; 32],
        }
        .frame();
        assert_eq!(frame.len(), 35);
        assert_eq!(&frame[..2], [0x02, 0xff]);
        assert_eq!(&frame[2..34], [b'A'; 32]);
        assert_eq!(frame[34], checksum(&frame[..34]));
    }

    #[test]
    fn session_complete_requires_end_session_not_vend_denied() {
        assert_eq!(ReaderCommand::EndSession.frame(), [0x07, 0x07]);
        assert_ne!(
            ReaderCommand::EndSession.frame(),
            ReaderCommand::Deny.frame()
        );
    }

    #[test]
    fn decoder_handles_junk_and_split_frames() {
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(b"banner\x02130004").is_empty());
        assert_eq!(
            decoder.push(b"e30001fb\x03\x02130417\x03"),
            vec![b"130004e30001fb".to_vec(), b"130417".to_vec()]
        );
    }

    #[test]
    fn parses_and_validates_a_vend_request() {
        let event = parse_adapter_message(&[0x13, 0x00, 0x04, 0xe3, 0x01, 0x02, 0xfd]);
        assert_eq!(
            event,
            Ok(AdapterMessage::Vmc(VmcEvent::VendRequest {
                price: Level1Amount::new(1251).unwrap(),
                item: ItemNumber::new(0x0102),
            }))
        );
    }

    #[test]
    fn parses_vmc_display_capabilities() {
        assert_eq!(
            parse_adapter_message(&[0x11, 0x00, 0x03, 0x10, 0x01, 0x01, 0x26]),
            Ok(AdapterMessage::Vmc(VmcEvent::SetupConfiguration {
                feature_level: 3,
                display_columns: 16,
                display_rows: 1,
                display_character_set: 1,
            }))
        );
    }

    #[test]
    fn rejects_bad_checksums_and_extra_bytes() {
        assert_eq!(
            parse_adapter_message(&[0x13, 0x04, 0x00]),
            Err(ProtocolError::BadChecksum)
        );
        assert_eq!(
            parse_adapter_message(&[0x13, 0x04, 0x00, 0x17]),
            Err(ProtocolError::InvalidLength)
        );
    }

    #[test]
    fn accepts_the_wafer_adapters_single_byte_reset() {
        assert_eq!(
            parse_adapter_message(&[0x10]),
            Ok(AdapterMessage::Vmc(VmcEvent::Reset))
        );
        assert_eq!(
            parse_adapter_message(&[0x10, 0x10]),
            Ok(AdapterMessage::Vmc(VmcEvent::Reset))
        );
    }
}
