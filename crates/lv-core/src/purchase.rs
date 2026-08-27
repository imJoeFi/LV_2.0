use crate::{Msats, ProductId, SlotId};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PurchaseId(Uuid);

impl PurchaseId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for PurchaseId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PurchaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightningInvoice {
    pub bolt11: String,
    pub operation_id: [u8; 32],
    pub payment_hash: [u8; 32],
    pub expires_at_unix_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistanceReason {
    VendFailed,
    PaidAfterAbandonment,
    PaidAfterMdbDeadline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssistanceResolution {
    RefundedOutOfBand,
    ProductProvided,
    DeterminedDispensed,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LightningPurchaseState {
    InventoryReserved,
    InvoiceCreating {
        latest_expiration_unix_seconds: u64,
    },
    /// The customer left while invoice creation was in flight. Any invoice
    /// returned by that operation must be recorded as abandoned and monitored,
    /// but can never authorize a vend.
    AbandonedCreating {
        latest_expiration_unix_seconds: u64,
    },
    InvoiceDisplayed {
        invoice: LightningInvoice,
    },
    /// The invoice remains payable, but this purchase can never vend.
    /// Inventory has already been released for another customer.
    AbandonedAwaitingFinal {
        invoice: LightningInvoice,
    },
    CancelledBeforeInvoice {
        cancelled_at_unix_millis: u64,
    },
    InvoiceCreationFailed {
        failed_at_unix_millis: u64,
        message: String,
    },
    AwaitingVend {
        invoice: LightningInvoice,
        funded_at_unix_millis: u64,
    },
    Dispensed {
        completed_at_unix_millis: u64,
    },
    Expired {
        expired_at_unix_millis: u64,
    },
    AssistanceRequired {
        invoice: LightningInvoice,
        reason: AssistanceReason,
        reference: String,
    },
    VendUncertain {
        invoice: LightningInvoice,
        reference: String,
    },
    Resolved {
        resolution: AssistanceResolution,
        note: Option<String>,
        resolved_at_unix_millis: u64,
    },
}

impl LightningPurchaseState {
    /// Whether this state still owns one unit of the selected slot's inventory.
    pub const fn holds_inventory_reservation(&self) -> bool {
        match self {
            Self::InventoryReserved
            | Self::InvoiceCreating { .. }
            | Self::InvoiceDisplayed { .. }
            | Self::AwaitingVend { .. }
            | Self::VendUncertain { .. } => true,
            Self::AssistanceRequired { reason, .. } => {
                matches!(reason, AssistanceReason::VendFailed)
            }
            _ => false,
        }
    }

    /// Whether receiving payment is still allowed to advance this purchase to a vend.
    pub const fn may_advance_to_vend(&self) -> bool {
        matches!(self, Self::InvoiceDisplayed { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightningPurchase {
    pub(crate) id: PurchaseId,
    pub(crate) slot: SlotId,
    pub(crate) product: ProductId,
    pub(crate) amount: Msats,
    pub(crate) created_at_unix_millis: u64,
    pub(crate) state: LightningPurchaseState,
}

impl LightningPurchase {
    pub const fn id(&self) -> PurchaseId {
        self.id
    }

    pub const fn slot(&self) -> &SlotId {
        &self.slot
    }

    pub const fn product(&self) -> &ProductId {
        &self.product
    }

    pub const fn amount(&self) -> Msats {
        self.amount
    }

    pub const fn created_at_unix_millis(&self) -> u64 {
        self.created_at_unix_millis
    }

    pub const fn state(&self) -> &LightningPurchaseState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn invoice() -> LightningInvoice {
        LightningInvoice {
            bolt11: "lnbc-test".to_owned(),
            operation_id: [1; 32],
            payment_hash: [2; 32],
            expires_at_unix_seconds: 42,
        }
    }

    #[test]
    fn abandoning_permanently_releases_inventory_and_vending_authority() {
        let displayed = LightningPurchaseState::InvoiceDisplayed { invoice: invoice() };
        assert!(displayed.holds_inventory_reservation());
        assert!(displayed.may_advance_to_vend());

        let abandoned = LightningPurchaseState::AbandonedAwaitingFinal { invoice: invoice() };
        assert!(!abandoned.holds_inventory_reservation());
        assert!(!abandoned.may_advance_to_vend());
    }

    #[test]
    fn purchase_round_trips_through_json() {
        let purchase = LightningPurchase {
            id: PurchaseId::new(),
            slot: SlotId::from_str("B1").unwrap(),
            product: ProductId::parse("trail-mix").unwrap(),
            amount: Msats::from_msats(110_000),
            created_at_unix_millis: 1,
            state: LightningPurchaseState::InvoiceDisplayed { invoice: invoice() },
        };
        let encoded = serde_json::to_vec(&purchase).unwrap();
        let decoded: LightningPurchase = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, purchase);
    }
}
