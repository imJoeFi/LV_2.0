use crate::{AssistanceResolution, Msats, ProductId, PurchaseId, SlotHealth, SlotId};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub const MANAGER_ALPN: &[u8] = b"lightningvend/manager/2";
pub const MANAGER_PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSequence(pub u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateRevision(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(Uuid);

impl CommandId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CommandId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdminPin(String);

impl AdminPin {
    pub fn parse(pin: impl Into<String>) -> Result<Self, AdminPinError> {
        let pin = pin.into();
        if (4..=12).contains(&pin.len()) && pin.bytes().all(|byte| byte.is_ascii_digit()) {
            Ok(Self(pin))
        } else {
            Err(AdminPinError)
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AdminPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AdminPin([REDACTED])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminPinError;

impl fmt::Display for AdminPinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("admin PIN must contain between four and twelve digits")
    }
}

impl std::error::Error for AdminPinError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreeVendScope {
    AnyConfiguredSlot,
    SpecificSlot(SlotId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerCommand {
    SetKioskName {
        name: String,
    },
    SetInitialAdminPin {
        pin: AdminPin,
    },
    ChangeAdminPin {
        pin: AdminPin,
    },
    SetInventory {
        slot: SlotId,
        quantity: u32,
        expected_revision: StateRevision,
    },
    ResolveSlotAttention {
        slot: SlotId,
        expected_revision: StateRevision,
    },
    ResolveUncertainVend {
        purchase_id: PurchaseId,
        dispensed: bool,
    },
    ResolveAssistance {
        purchase_id: PurchaseId,
        resolution: AssistanceResolution,
        note: Option<String>,
    },
    ArmFreeVend {
        scope: FreeVendScope,
        expires_in_seconds: u32,
    },
    ArmMaintenanceVend {
        slot: SlotId,
        expires_in_seconds: u32,
    },
    DisarmVendAuthorization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub id: CommandId,
    pub command: ManagerCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerRequest {
    GetSnapshot,
    SubscribeEvents { after: EventSequence },
    Command(CommandEnvelope),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRequest {
    pub version: u16,
    pub request: ManagerRequest,
}

impl WireRequest {
    pub const fn new(request: ManagerRequest) -> Self {
        Self {
            version: MANAGER_PROTOCOL_VERSION,
            request,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoAccountRef {
    pub fingerprint: String,
    pub masked: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentSummary {
    Promo { account: PromoAccountRef },
    Lightning { amount: Msats },
    FreeVend,
    MaintenanceTest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PurchaseSummaryState {
    AwaitingPayment,
    AwaitingVend,
    Dispensed,
    Failed,
    Cancelled,
    Expired,
    AssistanceRequired,
    Uncertain,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurchaseSummary {
    pub id: PurchaseId,
    pub slot: SlotId,
    pub product: ProductId,
    pub payment: PaymentSummary,
    pub state: PurchaseSummaryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotSnapshot {
    pub slot: SlotId,
    pub inventory: u32,
    pub reserved: u32,
    pub health: SlotHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VendAuthorizationSnapshot {
    FreeVend {
        scope: FreeVendScope,
        expires_at_unix_millis: Option<u64>,
    },
    MaintenanceVend {
        slot: SlotId,
        expires_at_unix_millis: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KioskSnapshot {
    pub name: Option<String>,
    pub admin_pin_configured: bool,
    pub revision: StateRevision,
    pub through_sequence: EventSequence,
    pub slots: Vec<SlotSnapshot>,
    pub unresolved_purchases: Vec<PurchaseSummary>,
    pub vend_authorization: Option<VendAuthorizationSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerEvent {
    KioskNameChanged {
        name: String,
    },
    AdminPinChanged,
    InventorySet {
        slot: SlotId,
        quantity: u32,
    },
    SlotHealthChanged {
        slot: SlotId,
        health: SlotHealth,
    },
    VendAuthorizationArmed,
    VendAuthorizationDisarmed,
    PurchaseChanged(PurchaseSummary),
    AssistanceResolved {
        purchase_id: PurchaseId,
        resolution: AssistanceResolution,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub sequence: EventSequence,
    pub occurred_at_unix_millis: u64,
    pub event: ManagerEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandResult {
    Applied {
        revision: StateRevision,
    },
    Rejected {
        code: String,
        message: String,
        current_revision: StateRevision,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagerResponse {
    Snapshot(KioskSnapshot),
    Events(Vec<EventEnvelope>),
    CommandResult {
        command_id: CommandId,
        result: CommandResult,
    },
    ProtocolError {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireResponse {
    pub version: u16,
    pub response: ManagerResponse,
}

impl WireResponse {
    pub const fn new(response: ManagerResponse) -> Self {
        Self {
            version: MANAGER_PROTOCOL_VERSION,
            response,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_pin_debug_output_is_redacted() {
        let pin = AdminPin::parse("1234").unwrap();
        assert_eq!(format!("{pin:?}"), "AdminPin([REDACTED])");
        assert!(!format!("{pin:?}").contains("1234"));
    }

    #[test]
    fn admin_pin_length_matches_the_kiosk_keypad() {
        assert!(AdminPin::parse("1234").is_ok());
        assert!(AdminPin::parse("123456789012").is_ok());
        assert!(AdminPin::parse("123").is_err());
        assert!(AdminPin::parse("1234567890123").is_err());
    }

    #[test]
    fn wire_messages_are_versioned_and_round_trip() {
        let request = WireRequest::new(ManagerRequest::Command(CommandEnvelope {
            id: CommandId::new(),
            command: ManagerCommand::ArmFreeVend {
                scope: FreeVendScope::AnyConfiguredSlot,
                expires_in_seconds: 30,
            },
        }));
        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: WireRequest = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.version, MANAGER_PROTOCOL_VERSION);
    }
}
