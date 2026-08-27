mod catalog;
mod engine;
mod manager_protocol;
mod money;
mod purchase;

pub use catalog::{
    Catalog, CatalogError, PaymentPolicy, Product, ProductId, Slot, SlotId, MIN_LIGHTNING_PRICE,
};
pub use engine::{
    ArmMode, CodeAccount, Entitlement, KioskEngine, KioskError, MachineSelection, PaymentKind,
    PersistentState, PromoCode, SelectionOutcome, SlotHealth, Transaction, TransactionId,
    TransactionStatus,
};
pub use manager_protocol::{
    AdminPin, AdminPinError, CommandEnvelope, CommandId, CommandResult, EventEnvelope,
    EventSequence, FreeVendScope, KioskSnapshot, ManagerCommand, ManagerEvent, ManagerRequest,
    ManagerResponse, PaymentSummary, PromoAccountRef, PurchaseSummary, PurchaseSummaryState,
    SlotSnapshot, StateRevision, VendAuthorizationSnapshot, WireRequest, WireResponse,
    MANAGER_ALPN, MANAGER_PROTOCOL_VERSION,
};
pub use money::Msats;
pub use purchase::{
    AssistanceReason, AssistanceResolution, LightningInvoice, LightningPurchase,
    LightningPurchaseState, PurchaseId,
};
