#[cfg(not(unix))]
compile_error!("lv-mdb-tools currently requires a Unix-like operating system");

mod device;
mod harness;
pub mod kiosk;
mod link;
mod protocol;
mod terminal;

pub use device::{
    BeginSessionError, DeviceEvent, DeviceEvents, DeviceStatus, EndedSession, MdbConfig, MdbDevice,
    MdbError, MdbSession, PendingVend, SessionEndReason, SessionEvent, SessionFunds, SessionId,
    SessionSummary, VendDecisionError, VendId, VendSuccessEvidence,
};
pub use harness::{Harness, HarnessConfig};
pub use protocol::{
    DisplayCharacterSet, DisplayDimensions, DisplayTime, InvalidAmount, InvalidDisplayDimensions,
    InvalidDisplayTime, ItemNumber, Level1Amount,
};
