use std::fmt;

const BEGIN_SESSION: u8 = 0x03;
const VEND_APPROVED: u8 = 0x05;
const VEND_DENIED: u8 = 0x06;
const END_SESSION: u8 = 0x07;
const MAX_BUFFER_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Funds(u16);

impl Funds {
    pub(crate) const UNKNOWN: Self = Self(0xffff);

    pub(crate) const fn new(raw: u16) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Funds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(&format_money(self.0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price(u16);

impl Price {
    pub(crate) const fn new(raw: u16) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(&format_money(self.0))
    }
}

fn format_money(raw: u16) -> String {
    let dollars = raw / 10;
    let cents = (raw % 10) * 10;
    format!("${dollars}.{cents:02}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    row: u8,
    column: u8,
}

impl Selection {
    pub(crate) const fn new(row: u8, column: u8) -> Self {
        Self { row, column }
    }

    pub(crate) const fn row(self) -> u8 {
        self.row
    }

    pub(crate) const fn column(self) -> u8 {
        self.column
    }
}

impl fmt::Display for Selection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let selection = if self.row < 26 {
            format!("{}{}", char::from(b'A' + self.row), self.column)
        } else {
            format!("?{}/{}", self.row, self.column)
        };
        formatter.pad(&selection)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderCommand {
    BeginSession(Funds),
    Approve(Price),
    Deny,
    EndSession,
    AcknowledgeSessionComplete,
}

impl ReaderCommand {
    fn payload(self) -> Vec<u8> {
        match self {
            Self::BeginSession(funds) => {
                let [high, low] = funds.raw().to_be_bytes();
                vec![BEGIN_SESSION, high, low]
            }
            Self::Approve(price) => {
                let [high, low] = price.raw().to_be_bytes();
                vec![VEND_APPROVED, high, low]
            }
            Self::Deny => vec![VEND_DENIED],
            Self::EndSession => vec![END_SESSION],
            // Proven on the AP 113. Do not change this unusual payload without
            // testing against the physical machine.
            Self::AcknowledgeSessionComplete => vec![0x06, 0x06],
        }
    }

    pub(crate) fn frame(self) -> Vec<u8> {
        let mut payload = self.payload();
        payload.push(checksum(&payload));
        payload
    }
}

impl fmt::Display for ReaderCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeginSession(funds) if *funds == Funds::UNKNOWN => {
                write!(
                    formatter,
                    "BEGIN SESSION (0xFFFF -- displayed literally, NOT \"funds unknown\")"
                )
            }
            Self::BeginSession(funds) => write!(formatter, "BEGIN SESSION ({funds} credit)"),
            Self::Approve(price) => write!(formatter, "VEND APPROVED price={}", price.raw()),
            Self::Deny => formatter.write_str("VEND DENIED"),
            Self::EndSession => formatter.write_str("END SESSION"),
            Self::AcknowledgeSessionComplete => formatter.write_str("ack SESSION COMPLETE"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderStatus {
    Disabled,
    Enabled,
    Cancelled,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcEvent {
    VendRequest { price: Price, selection: Selection },
    VendCancel,
    VendSuccess { selection: Option<Selection> },
    VendFailure,
    SessionComplete,
    Reader(ReaderStatus),
    OtherVend,
}

impl VmcEvent {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::VendRequest { .. } => "vend_request",
            Self::VendCancel => "vend_cancel",
            Self::VendSuccess { .. } => "vend_success",
            Self::VendFailure => "vend_failure",
            Self::SessionComplete => "session_complete",
            Self::Reader(_) => "reader",
            Self::OtherVend => "other",
        }
    }

    pub(crate) const fn note(self) -> &'static str {
        match self {
            Self::VendCancel => "   <== VEND CANCEL",
            Self::VendSuccess { .. } => "   <== VEND SUCCESS",
            Self::VendFailure => "   <== VEND FAILURE",
            Self::SessionComplete => "   <== SESSION COMPLETE",
            Self::Reader(ReaderStatus::Disabled) => "   <== READER DISABLE",
            Self::Reader(ReaderStatus::Enabled) => "   <== READER ENABLE",
            Self::Reader(ReaderStatus::Cancelled) => "   <== READER CANCEL",
            _ => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnhandledFrame;

impl TryFrom<&[u8]> for VmcEvent {
    type Error = UnhandledFrame;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        if bytes.len() >= 6 && bytes[0] == 0x13 && bytes[1] == 0x00 {
            return Ok(Self::VendRequest {
                price: Price::new(u16::from_be_bytes([bytes[2], bytes[3]])),
                selection: Selection::new(bytes[4], bytes[5]),
            });
        }
        if bytes.len() >= 2 && bytes[0] == 0x13 {
            return Ok(match bytes[1] {
                0x01 => Self::VendCancel,
                0x02 => Self::VendSuccess {
                    selection: bytes.get(2..4).map(|item| Selection::new(item[0], item[1])),
                },
                0x03 => Self::VendFailure,
                0x04 => Self::SessionComplete,
                _ => Self::OtherVend,
            });
        }
        if bytes.len() >= 2 && bytes[0] == 0x14 {
            let status = match bytes[1] {
                0x00 => ReaderStatus::Disabled,
                0x01 => ReaderStatus::Enabled,
                0x02 => ReaderStatus::Cancelled,
                _ => ReaderStatus::Other,
            };
            return Ok(Self::Reader(status));
        }
        Err(UnhandledFrame)
    }
}

#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
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

pub fn frame_note(bytes: &[u8], event: Option<VmcEvent>) -> &'static str {
    if bytes == [0x00] {
        "   (ack)"
    } else {
        event.map_or("", VmcEvent::note)
    }
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
    fn commands_include_the_proven_checksums() {
        assert_eq!(
            ReaderCommand::BeginSession(Funds::new(100)).frame(),
            vec![0x03, 0x00, 0x64, 0x67]
        );
        assert_eq!(
            ReaderCommand::Approve(Price::new(1251)).frame(),
            vec![0x05, 0x04, 0xe3, 0xec]
        );
        assert_eq!(ReaderCommand::Deny.frame(), vec![0x06, 0x06]);
        assert_eq!(ReaderCommand::EndSession.frame(), vec![0x07, 0x07]);
        assert_eq!(
            ReaderCommand::AcknowledgeSessionComplete.frame(),
            vec![0x06, 0x06, 0x0c]
        );
    }

    #[test]
    fn formats_money_and_selections_like_the_machine() {
        assert_eq!(Price::new(1345).to_string(), "$134.50");
        assert_eq!(Funds::new(u16::MAX).to_string(), "$6553.50");
        assert_eq!(Selection::new(0, 1).to_string(), "A1");
        assert_eq!(format!("{:^5}", Selection::new(0, 1)), " A1  ");
        assert_eq!(format!("{:^9}", Price::new(1345)), " $134.50 ");
        assert_eq!(Selection::new(4, 5).to_string(), "E5");
        assert_eq!(Selection::new(26, 3).to_string(), "?26/3");
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
    fn parses_ascii_hex_with_spaces() {
        assert_eq!(
            parse_ascii_hex(b"13 00 04 E3 00 01"),
            Some(vec![0x13, 0x00, 0x04, 0xe3, 0x00, 0x01])
        );
        assert_eq!(parse_ascii_hex(b"firmware banner"), None);
    }

    #[test]
    fn extracts_typed_vend_request_fields() {
        let event = VmcEvent::try_from(&[0x13, 0x00, 0x04, 0xe3, 0x01, 0x02, 0xfb][..]);
        assert_eq!(
            event,
            Ok(VmcEvent::VendRequest {
                price: Price::new(1251),
                selection: Selection::new(1, 2),
            })
        );
    }

    #[test]
    fn recognizes_session_complete() {
        assert_eq!(
            VmcEvent::try_from(&[0x13, 0x04, 0x17][..]),
            Ok(VmcEvent::SessionComplete)
        );
    }
}
