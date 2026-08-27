mod catalog;
mod engine;
mod manager_protocol;
mod money;
mod purchase;

pub use catalog::{
    Catalog, CatalogError, PaymentPolicy, Product, ProductId, Slot, SlotId, MIN_LIGHTNING_PRICE,
};
pub use engine::{
    ArmMode, CodeAccount, Entitlement, KioskEngine, KioskError, LightningLimit,
    LightningLimitReason, MachineSelection, PaymentKind, PersistentState, PromoCode,
    SelectionOutcome, SlotHealth, Transaction, TransactionId, TransactionStatus,
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
    LightningPurchaseState, PurchaseId, LIGHTNING_INVOICE_RATE_WINDOW_MILLIS,
    MAX_LIGHTNING_INVOICES_PER_WINDOW, MAX_PAYABLE_ABANDONED_KIOSK_WIDE,
    MAX_PAYABLE_ABANDONED_PER_PRODUCT,
};
