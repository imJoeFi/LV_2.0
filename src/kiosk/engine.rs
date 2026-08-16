use super::{Catalog, PaymentPolicy, ProductId, SlotId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PromoCode(String);

impl PromoCode {
    pub fn parse(value: impl Into<String>) -> Result<Self, KioskError> {
        let value = value.into();
        if value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_digit()) {
            Ok(Self(value))
        } else {
            Err(KioskError::InvalidPromoCode)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn masked(&self) -> String {
        format!("••••{}", &self.0[4..])
    }
}

impl fmt::Display for PromoCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotHealth {
    Ready,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlement {
    granted: u32,
    claimed: u32,
    reserved: u32,
}

impl Entitlement {
    pub const fn new(granted: u32) -> Self {
        Self {
            granted,
            claimed: 0,
            reserved: 0,
        }
    }

    pub const fn granted(self) -> u32 {
        self.granted
    }

    pub const fn claimed(self) -> u32 {
        self.claimed
    }

    pub const fn reserved(self) -> u32 {
        self.reserved
    }

    pub const fn remaining(self) -> u32 {
        self.granted
            .saturating_sub(self.claimed.saturating_add(self.reserved))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CodeAccount {
    entitlements: BTreeMap<ProductId, Entitlement>,
}

impl CodeAccount {
    pub fn entitlements(&self) -> impl Iterator<Item = (&ProductId, Entitlement)> {
        self.entitlements
            .iter()
            .map(|(product, entitlement)| (product, *entitlement))
    }

    pub fn entitlement(&self, product: &ProductId) -> Option<Entitlement> {
        self.entitlements.get(product).copied()
    }

    fn grant(&mut self, product: ProductId, quantity: u32) -> Result<(), KioskError> {
        if quantity == 0 {
            return Err(KioskError::ZeroGrant);
        }
        let entitlement = self
            .entitlements
            .entry(product)
            .or_insert(Entitlement::new(0));
        entitlement.granted = entitlement
            .granted
            .checked_add(quantity)
            .ok_or(KioskError::QuantityOverflow)?;
        Ok(())
    }

    fn reserve(&mut self, product: &ProductId) -> Result<(), KioskError> {
        let entitlement = self
            .entitlements
            .get_mut(product)
            .ok_or(KioskError::ProductNotGranted)?;
        if entitlement.remaining() == 0 {
            return Err(KioskError::EntitlementExhausted);
        }
        entitlement.reserved += 1;
        Ok(())
    }

    fn claim_reserved(&mut self, product: &ProductId) -> Result<(), KioskError> {
        let entitlement = self
            .entitlements
            .get_mut(product)
            .ok_or(KioskError::ProductNotGranted)?;
        if entitlement.reserved == 0 {
            return Err(KioskError::NoReservation);
        }
        entitlement.reserved -= 1;
        entitlement.claimed += 1;
        Ok(())
    }

    fn release_reserved(&mut self, product: &ProductId) -> Result<(), KioskError> {
        let entitlement = self
            .entitlements
            .get_mut(product)
            .ok_or(KioskError::ProductNotGranted)?;
        if entitlement.reserved == 0 {
            return Err(KioskError::NoReservation);
        }
        entitlement.reserved -= 1;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(u64);

impl TransactionId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentKind {
    Promo { code: PromoCode },
    Lightning { price_cents: u32 },
    FreeVend,
    MaintenanceTest,
}

impl PaymentKind {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Promo { .. } => "Promo code",
            Self::Lightning { .. } => "Lightning",
            Self::FreeVend => "Free vend",
            Self::MaintenanceTest => "Maintenance test",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionStatus {
    AwaitingPayment,
    AwaitingVend,
    Succeeded,
    Failed,
    Cancelled,
    Uncertain,
    ResolvedDispensed,
    ResolvedNotDispensed,
}

impl TransactionStatus {
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::AwaitingPayment | Self::AwaitingVend)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    id: TransactionId,
    slot: SlotId,
    product: ProductId,
    product_name: String,
    payment: PaymentKind,
    status: TransactionStatus,
    started_at: String,
}

impl Transaction {
    pub const fn id(&self) -> TransactionId {
        self.id
    }

    pub fn slot(&self) -> &SlotId {
        &self.slot
    }

    pub fn product(&self) -> &ProductId {
        &self.product
    }

    pub fn product_name(&self) -> &str {
        &self.product_name
    }

    pub const fn payment(&self) -> &PaymentKind {
        &self.payment
    }

    pub const fn status(&self) -> TransactionStatus {
        self.status
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistentState {
    inventory: BTreeMap<SlotId, u32>,
    health: BTreeMap<SlotId, SlotHealth>,
    codes: BTreeMap<PromoCode, CodeAccount>,
    transactions: Vec<Transaction>,
    next_transaction_id: u64,
}

impl PersistentState {
    pub fn inventory(&self, slot: &SlotId) -> u32 {
        self.inventory.get(slot).copied().unwrap_or_default()
    }

    pub fn set_inventory(&mut self, slot: SlotId, quantity: u32) {
        self.inventory.insert(slot, quantity);
    }

    pub fn health(&self, slot: &SlotId) -> SlotHealth {
        self.health.get(slot).copied().unwrap_or(SlotHealth::Ready)
    }

    pub fn set_health(&mut self, slot: SlotId, health: SlotHealth) {
        self.health.insert(slot, health);
    }

    pub fn code(&self, code: &PromoCode) -> Option<&CodeAccount> {
        self.codes.get(code)
    }

    pub fn grant(
        &mut self,
        code: PromoCode,
        product: ProductId,
        quantity: u32,
    ) -> Result<(), KioskError> {
        self.codes.entry(code).or_default().grant(product, quantity)
    }

    pub fn clear_codes(&mut self) {
        self.codes.clear();
    }

    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    fn transaction(&self, id: TransactionId) -> Result<&Transaction, KioskError> {
        self.transactions
            .iter()
            .find(|transaction| transaction.id == id)
            .ok_or(KioskError::UnknownTransaction(id))
    }

    fn transaction_mut(&mut self, id: TransactionId) -> Result<&mut Transaction, KioskError> {
        self.transactions
            .iter_mut()
            .find(|transaction| transaction.id == id)
            .ok_or(KioskError::UnknownTransaction(id))
    }

    pub fn recover_after_restart(&mut self) -> bool {
        let mut changed = false;
        let mut needs_attention = Vec::new();
        for transaction in &mut self.transactions {
            match transaction.status {
                TransactionStatus::AwaitingPayment => {
                    transaction.status = TransactionStatus::Cancelled;
                    changed = true;
                }
                TransactionStatus::AwaitingVend => {
                    transaction.status = TransactionStatus::Uncertain;
                    needs_attention.push(transaction.slot.clone());
                    changed = true;
                }
                _ => {}
            }
        }
        for slot in needs_attention {
            self.set_health(slot, SlotHealth::NeedsAttention);
        }
        changed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmMode {
    None,
    FreeNext,
    MaintenanceTest(SlotId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSelection {
    slot: SlotId,
    product: ProductId,
    product_name: String,
    payment: PaymentPolicy,
}

impl MachineSelection {
    pub fn slot(&self) -> &SlotId {
        &self.slot
    }

    pub fn product(&self) -> &ProductId {
        &self.product
    }

    pub fn product_name(&self) -> &str {
        &self.product_name
    }

    pub const fn payment(&self) -> PaymentPolicy {
        self.payment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    PromoCodeRequired {
        selection: MachineSelection,
    },
    LightningPaymentRequired {
        transaction_id: TransactionId,
        selection: MachineSelection,
        price_cents: u32,
    },
    VendApproved {
        transaction_id: TransactionId,
        selection: MachineSelection,
        payment: PaymentKind,
    },
    Denied(String),
}

#[derive(Debug, Clone)]
pub struct KioskEngine {
    catalog: Catalog,
    state: PersistentState,
    active_code: Option<PromoCode>,
    arm_mode: ArmMode,
    active_transaction: Option<TransactionId>,
}

impl KioskEngine {
    pub fn new(catalog: Catalog, mut state: PersistentState) -> Self {
        state.recover_after_restart();
        Self {
            catalog,
            state,
            active_code: None,
            arm_mode: ArmMode::None,
            active_transaction: None,
        }
    }

    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub const fn state(&self) -> &PersistentState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut PersistentState {
        &mut self.state
    }

    pub const fn active_code(&self) -> Option<&PromoCode> {
        self.active_code.as_ref()
    }

    pub fn active_account(&self) -> Option<&CodeAccount> {
        self.active_code
            .as_ref()
            .and_then(|code| self.state.code(code))
    }

    pub const fn arm_mode(&self) -> &ArmMode {
        &self.arm_mode
    }

    pub fn authenticate_code(&mut self, code: PromoCode) -> Result<(), KioskError> {
        if self.state.code(&code).is_none() {
            return Err(KioskError::UnknownPromoCode);
        }
        self.active_code = Some(code);
        Ok(())
    }

    pub fn end_customer_session(&mut self) -> Result<(), KioskError> {
        if let Some(id) = self.active_transaction {
            let status = self.state.transaction(id)?.status;
            if status == TransactionStatus::AwaitingPayment {
                self.state.transaction_mut(id)?.status = TransactionStatus::Cancelled;
                self.active_transaction = None;
            }
        }
        self.active_code = None;
        Ok(())
    }

    pub fn arm_free_vend(&mut self) {
        self.arm_mode = ArmMode::FreeNext;
    }

    pub fn arm_maintenance_test(&mut self, slot: SlotId) -> Result<(), KioskError> {
        self.catalog
            .slot(&slot)
            .ok_or_else(|| KioskError::UnknownSlot(slot.clone()))?;
        if self.has_uncertain_transaction(&slot) {
            return Err(KioskError::UnresolvedTransaction(slot));
        }
        self.arm_mode = ArmMode::MaintenanceTest(slot);
        Ok(())
    }

    pub fn disarm(&mut self) {
        self.arm_mode = ArmMode::None;
    }

    pub fn set_inventory(&mut self, slot: SlotId, quantity: u32) -> Result<(), KioskError> {
        self.catalog
            .slot(&slot)
            .ok_or_else(|| KioskError::UnknownSlot(slot.clone()))?;
        self.state.set_inventory(slot, quantity);
        Ok(())
    }

    pub fn set_slot_health(&mut self, slot: SlotId, health: SlotHealth) -> Result<(), KioskError> {
        self.catalog
            .slot(&slot)
            .ok_or_else(|| KioskError::UnknownSlot(slot.clone()))?;
        if health == SlotHealth::Ready && self.has_uncertain_transaction(&slot) {
            return Err(KioskError::UnresolvedTransaction(slot));
        }
        self.state.set_health(slot, health);
        Ok(())
    }

    pub fn available_promo_slots(&self, product: &ProductId) -> Vec<SlotId> {
        self.catalog
            .slots_for_product(product)
            .filter(|slot| {
                slot.enabled()
                    && slot.payment() == PaymentPolicy::Promo
                    && self.state.inventory(slot.id()) > 0
                    && self.state.health(slot.id()) == SlotHealth::Ready
            })
            .map(|slot| slot.id().clone())
            .collect()
    }

    pub fn machine_selected(&mut self, slot_id: &SlotId) -> Result<SelectionOutcome, KioskError> {
        if self.active_transaction.is_some() {
            return Ok(SelectionOutcome::Denied(
                "Another vend is already in progress.".to_owned(),
            ));
        }
        let selection = self.selection(slot_id)?;

        match self.arm_mode.clone() {
            ArmMode::FreeNext => {
                if !self.slot_is_available(slot_id)? {
                    return Ok(SelectionOutcome::Denied(
                        "That selection is sold out or temporarily unavailable.".to_owned(),
                    ));
                }
                self.arm_mode = ArmMode::None;
                self.approve(selection, PaymentKind::FreeVend)
            }
            ArmMode::MaintenanceTest(expected) if expected == *slot_id => {
                if !self.slot_is_stocked(slot_id)? {
                    return Ok(SelectionOutcome::Denied(
                        "Stock this configured slot before running a test vend.".to_owned(),
                    ));
                }
                self.arm_mode = ArmMode::None;
                self.approve(selection, PaymentKind::MaintenanceTest)
            }
            ArmMode::MaintenanceTest(expected) => Ok(SelectionOutcome::Denied(format!(
                "Maintenance test is armed for {expected}, not {slot_id}."
            ))),
            ArmMode::None => {
                if !self.slot_is_available(slot_id)? {
                    return Ok(SelectionOutcome::Denied(
                        "That selection is sold out or temporarily unavailable.".to_owned(),
                    ));
                }
                self.customer_selection(selection)
            }
        }
    }

    fn customer_selection(
        &mut self,
        selection: MachineSelection,
    ) -> Result<SelectionOutcome, KioskError> {
        match (&self.active_code, selection.payment) {
            (Some(_), PaymentPolicy::Lightning { .. }) => Ok(SelectionOutcome::Denied(
                "That item requires Lightning. Choose an included item or press Done.".to_owned(),
            )),
            (Some(code), PaymentPolicy::Promo) => {
                let code = code.clone();
                let entitlement = self
                    .state
                    .code(&code)
                    .and_then(|account| account.entitlement(&selection.product));
                if entitlement.is_none_or(|entitlement| entitlement.remaining() == 0) {
                    return Ok(SelectionOutcome::Denied(
                        "That product is not included or has already been claimed.".to_owned(),
                    ));
                }
                self.approve(selection, PaymentKind::Promo { code })
            }
            (None, PaymentPolicy::Promo) => Ok(SelectionOutcome::PromoCodeRequired { selection }),
            (None, PaymentPolicy::Lightning { price_cents }) => {
                let id = self.create_transaction(
                    &selection,
                    PaymentKind::Lightning { price_cents },
                    TransactionStatus::AwaitingPayment,
                );
                self.active_transaction = Some(id);
                Ok(SelectionOutcome::LightningPaymentRequired {
                    transaction_id: id,
                    selection,
                    price_cents,
                })
            }
        }
    }

    fn approve(
        &mut self,
        selection: MachineSelection,
        payment: PaymentKind,
    ) -> Result<SelectionOutcome, KioskError> {
        if let PaymentKind::Promo { code } = &payment {
            self.state
                .codes
                .get_mut(code)
                .ok_or(KioskError::UnknownPromoCode)?
                .reserve(&selection.product)?;
        }
        let id =
            self.create_transaction(&selection, payment.clone(), TransactionStatus::AwaitingVend);
        self.active_transaction = Some(id);
        Ok(SelectionOutcome::VendApproved {
            transaction_id: id,
            selection,
            payment,
        })
    }

    pub fn lightning_payment_accepted(
        &mut self,
        id: TransactionId,
    ) -> Result<SelectionOutcome, KioskError> {
        if self.active_transaction != Some(id) {
            return Err(KioskError::TransactionNotActive(id));
        }
        let (slot, payment) = {
            let transaction = self.state.transaction_mut(id)?;
            if transaction.status != TransactionStatus::AwaitingPayment {
                return Err(KioskError::InvalidTransactionState(id));
            }
            transaction.status = TransactionStatus::AwaitingVend;
            (transaction.slot.clone(), transaction.payment.clone())
        };
        let selection = self.selection(&slot)?;
        Ok(SelectionOutcome::VendApproved {
            transaction_id: id,
            selection,
            payment,
        })
    }

    pub fn cancel_lightning(&mut self, id: TransactionId) -> Result<(), KioskError> {
        if self.active_transaction != Some(id) {
            return Err(KioskError::TransactionNotActive(id));
        }
        let transaction = self.state.transaction_mut(id)?;
        if transaction.status != TransactionStatus::AwaitingPayment {
            return Err(KioskError::InvalidTransactionState(id));
        }
        transaction.status = TransactionStatus::Cancelled;
        self.active_transaction = None;
        Ok(())
    }

    /// Records that the VMC cancelled a selection before a successful vend.
    ///
    /// Unlike a vend failure, cancellation does not mark the slot as needing
    /// attention. Any promo entitlement reserved before approval is released.
    pub fn vend_cancelled(&mut self, id: TransactionId) -> Result<(), KioskError> {
        if self.active_transaction != Some(id) {
            return Err(KioskError::TransactionNotActive(id));
        }
        let transaction = self.state.transaction(id)?.clone();
        match transaction.status {
            TransactionStatus::AwaitingPayment => {}
            TransactionStatus::AwaitingVend => self.resolve_promo(&transaction, false)?,
            _ => return Err(KioskError::InvalidTransactionState(id)),
        }
        self.state.transaction_mut(id)?.status = TransactionStatus::Cancelled;
        self.active_transaction = None;
        Ok(())
    }

    pub fn vend_succeeded(&mut self, id: TransactionId) -> Result<(), KioskError> {
        self.finish_vend(id, VendResult::Succeeded)
    }

    pub fn vend_failed(&mut self, id: TransactionId) -> Result<(), KioskError> {
        self.finish_vend(id, VendResult::Failed)
    }

    pub fn vend_uncertain(&mut self, id: TransactionId) -> Result<(), KioskError> {
        self.finish_vend(id, VendResult::Uncertain)
    }

    fn finish_vend(&mut self, id: TransactionId, result: VendResult) -> Result<(), KioskError> {
        if self.active_transaction != Some(id) {
            return Err(KioskError::TransactionNotActive(id));
        }
        let transaction = self.state.transaction(id)?.clone();
        if transaction.status != TransactionStatus::AwaitingVend {
            return Err(KioskError::InvalidTransactionState(id));
        }

        match result {
            VendResult::Succeeded => {
                self.decrement_inventory(&transaction.slot)?;
                self.resolve_promo(&transaction, true)?;
                if transaction.payment == PaymentKind::MaintenanceTest {
                    self.state
                        .set_health(transaction.slot.clone(), SlotHealth::Ready);
                }
                self.state.transaction_mut(id)?.status = TransactionStatus::Succeeded;
            }
            VendResult::Failed => {
                self.resolve_promo(&transaction, false)?;
                self.state
                    .set_health(transaction.slot.clone(), SlotHealth::NeedsAttention);
                self.state.transaction_mut(id)?.status = TransactionStatus::Failed;
            }
            VendResult::Uncertain => {
                self.state
                    .set_health(transaction.slot.clone(), SlotHealth::NeedsAttention);
                self.state.transaction_mut(id)?.status = TransactionStatus::Uncertain;
            }
        }
        self.active_transaction = None;
        Ok(())
    }

    pub fn resolve_uncertain(
        &mut self,
        id: TransactionId,
        dispensed: bool,
    ) -> Result<(), KioskError> {
        let transaction = self.state.transaction(id)?.clone();
        if transaction.status != TransactionStatus::Uncertain {
            return Err(KioskError::InvalidTransactionState(id));
        }
        if dispensed {
            self.decrement_inventory(&transaction.slot)?;
            self.resolve_promo(&transaction, true)?;
            self.state.transaction_mut(id)?.status = TransactionStatus::ResolvedDispensed;
        } else {
            self.resolve_promo(&transaction, false)?;
            self.state.transaction_mut(id)?.status = TransactionStatus::ResolvedNotDispensed;
        }
        Ok(())
    }

    fn resolve_promo(
        &mut self,
        transaction: &Transaction,
        dispensed: bool,
    ) -> Result<(), KioskError> {
        let PaymentKind::Promo { code } = &transaction.payment else {
            return Ok(());
        };
        let account = self
            .state
            .codes
            .get_mut(code)
            .ok_or(KioskError::UnknownPromoCode)?;
        if dispensed {
            account.claim_reserved(&transaction.product)
        } else {
            account.release_reserved(&transaction.product)
        }
    }

    fn decrement_inventory(&mut self, slot: &SlotId) -> Result<(), KioskError> {
        let quantity = self.state.inventory(slot);
        if quantity == 0 {
            return Err(KioskError::InventoryAlreadyZero(slot.clone()));
        }
        self.state.set_inventory(slot.clone(), quantity - 1);
        Ok(())
    }

    fn selection(&self, slot_id: &SlotId) -> Result<MachineSelection, KioskError> {
        let slot = self
            .catalog
            .slot(slot_id)
            .ok_or_else(|| KioskError::UnknownSlot(slot_id.clone()))?;
        let product = self
            .catalog
            .product(slot.product_id())
            .ok_or_else(|| KioskError::UnknownProduct(slot.product_id().clone()))?;
        Ok(MachineSelection {
            slot: slot_id.clone(),
            product: product.id().clone(),
            product_name: product.name().to_owned(),
            payment: slot.payment(),
        })
    }

    fn slot_is_available(&self, slot_id: &SlotId) -> Result<bool, KioskError> {
        Ok(self.slot_is_stocked(slot_id)? && self.state.health(slot_id) == SlotHealth::Ready)
    }

    fn slot_is_stocked(&self, slot_id: &SlotId) -> Result<bool, KioskError> {
        let slot = self
            .catalog
            .slot(slot_id)
            .ok_or_else(|| KioskError::UnknownSlot(slot_id.clone()))?;
        Ok(slot.enabled() && self.state.inventory(slot_id) > 0)
    }

    fn create_transaction(
        &mut self,
        selection: &MachineSelection,
        payment: PaymentKind,
        status: TransactionStatus,
    ) -> TransactionId {
        self.state.next_transaction_id += 1;
        let id = TransactionId(self.state.next_transaction_id);
        self.state.transactions.push(Transaction {
            id,
            slot: selection.slot.clone(),
            product: selection.product.clone(),
            product_name: selection.product_name.clone(),
            payment,
            status,
            started_at: chrono::Utc::now().to_rfc3339(),
        });
        id
    }

    fn has_uncertain_transaction(&self, slot: &SlotId) -> bool {
        self.state.transactions.iter().any(|transaction| {
            transaction.slot == *slot && transaction.status == TransactionStatus::Uncertain
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum VendResult {
    Succeeded,
    Failed,
    Uncertain,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum KioskError {
    #[error("promo codes must contain exactly six digits")]
    InvalidPromoCode,
    #[error("promo code not found")]
    UnknownPromoCode,
    #[error("promo grant quantity must be greater than zero")]
    ZeroGrant,
    #[error("promo quantity overflow")]
    QuantityOverflow,
    #[error("the promo code does not grant this product")]
    ProductNotGranted,
    #[error("the product entitlement has been fully claimed")]
    EntitlementExhausted,
    #[error("the product entitlement has no pending reservation")]
    NoReservation,
    #[error("unknown slot {0}")]
    UnknownSlot(SlotId),
    #[error("unknown product {0}")]
    UnknownProduct(ProductId),
    #[error("unknown transaction {0}")]
    UnknownTransaction(TransactionId),
    #[error("transaction {0} is not active")]
    TransactionNotActive(TransactionId),
    #[error("transaction {0} is in the wrong state")]
    InvalidTransactionState(TransactionId),
    #[error("slot {0} inventory is already zero")]
    InventoryAlreadyZero(SlotId),
    #[error("slot {0} has an uncertain transaction that must be reconciled first")]
    UnresolvedTransaction(SlotId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::str::FromStr;

    const CATALOG: &str = r#"
        [[products]]
        id = "water"
        name = "Sparkling Water"

        [[products]]
        id = "snack"
        name = "Trail Mix"

        [[slots]]
        id = "A1"
        product = "water"
        payment = "promo"

        [[slots]]
        id = "A2"
        product = "water"
        payment = "promo"

        [[slots]]
        id = "B1"
        product = "snack"
        payment = "lightning"
        price_cents = 250
    "#;

    fn engine() -> KioskEngine {
        let catalog = Catalog::parse(CATALOG, Path::new(".")).unwrap();
        let mut state = PersistentState::default();
        let code = PromoCode::parse("123456").unwrap();
        state
            .grant(code, ProductId::parse("water").unwrap(), 2)
            .unwrap();
        for slot in ["A1", "A2", "B1"] {
            state.set_inventory(SlotId::from_str(slot).unwrap(), 2);
        }
        KioskEngine::new(catalog, state)
    }

    fn transaction_id(outcome: SelectionOutcome) -> TransactionId {
        match outcome {
            SelectionOutcome::VendApproved { transaction_id, .. }
            | SelectionOutcome::LightningPaymentRequired { transaction_id, .. } => transaction_id,
            SelectionOutcome::PromoCodeRequired { selection } => {
                panic!("promo code required for {}", selection.slot())
            }
            SelectionOutcome::Denied(message) => panic!("selection denied: {message}"),
        }
    }

    #[test]
    fn promo_selection_without_a_code_requests_authentication_without_a_transaction() {
        let mut engine = engine();
        let slot = SlotId::from_str("A2").unwrap();

        let outcome = engine.machine_selected(&slot).unwrap();

        assert!(matches!(
            outcome,
            SelectionOutcome::PromoCodeRequired { selection }
                if selection.slot() == &slot && selection.product_name() == "Sparkling Water"
        ));
        assert_eq!(engine.active_transaction, None);
        assert!(engine.state().transactions().is_empty());
    }

    #[test]
    fn promo_entitlement_accepts_any_matching_slot() {
        let mut engine = engine();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();

        let first = transaction_id(
            engine
                .machine_selected(&SlotId::from_str("A2").unwrap())
                .unwrap(),
        );
        engine.vend_succeeded(first).unwrap();

        let account = engine.active_account().unwrap();
        let entitlement = account
            .entitlement(&ProductId::parse("water").unwrap())
            .unwrap();
        assert_eq!(entitlement.claimed(), 1);
        assert_eq!(
            engine.state().inventory(&SlotId::from_str("A2").unwrap()),
            1
        );
        assert_eq!(
            engine.state().inventory(&SlotId::from_str("A1").unwrap()),
            2
        );
    }

    #[test]
    fn zero_inventory_denies_without_consuming_entitlement() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine.set_inventory(slot.clone(), 0).unwrap();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();
        assert!(matches!(
            engine.machine_selected(&slot).unwrap(),
            SelectionOutcome::Denied(_)
        ));
        let entitlement = engine
            .active_account()
            .unwrap()
            .entitlement(&ProductId::parse("water").unwrap())
            .unwrap();
        assert_eq!(entitlement.remaining(), 2);
    }

    #[test]
    fn vend_failure_releases_promo_and_marks_slot_for_attention() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());
        engine.vend_failed(transaction).unwrap();
        assert_eq!(engine.state().inventory(&slot), 2);
        assert_eq!(engine.state().health(&slot), SlotHealth::NeedsAttention);
        assert_eq!(
            engine
                .active_account()
                .unwrap()
                .entitlement(&ProductId::parse("water").unwrap())
                .unwrap()
                .remaining(),
            2
        );
    }

    #[test]
    fn vmc_cancellation_releases_promo_without_faulting_the_slot() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());

        engine.vend_cancelled(transaction).unwrap();

        assert_eq!(engine.state().health(&slot), SlotHealth::Ready);
        assert_eq!(engine.state().inventory(&slot), 2);
        assert_eq!(
            engine
                .active_account()
                .unwrap()
                .entitlement(&ProductId::parse("water").unwrap())
                .unwrap()
                .remaining(),
            2
        );
        assert_eq!(
            engine.state().transactions().last().unwrap().status(),
            TransactionStatus::Cancelled
        );
    }

    #[test]
    fn uncertain_promo_stays_reserved_until_admin_resolution() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());
        engine.vend_uncertain(transaction).unwrap();
        let entitlement = engine
            .active_account()
            .unwrap()
            .entitlement(&ProductId::parse("water").unwrap())
            .unwrap();
        assert_eq!(entitlement.reserved(), 1);
        engine.resolve_uncertain(transaction, false).unwrap();
        let entitlement = engine
            .active_account()
            .unwrap()
            .entitlement(&ProductId::parse("water").unwrap())
            .unwrap();
        assert_eq!(entitlement.reserved(), 0);
        assert_eq!(entitlement.remaining(), 2);
    }

    #[test]
    fn lightning_is_machine_first() {
        let mut engine = engine();
        let outcome = engine
            .machine_selected(&SlotId::from_str("B1").unwrap())
            .unwrap();
        let id = transaction_id(outcome);
        let approved = engine.lightning_payment_accepted(id).unwrap();
        assert!(matches!(approved, SelectionOutcome::VendApproved { .. }));
    }

    #[test]
    fn free_vend_is_one_shot_and_respects_inventory() {
        let mut engine = engine();
        let slot = SlotId::from_str("B1").unwrap();
        engine.arm_free_vend();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());
        assert_eq!(engine.arm_mode(), &ArmMode::None);
        engine.vend_succeeded(transaction).unwrap();
        assert_eq!(engine.state().inventory(&slot), 1);
    }

    #[test]
    fn maintenance_test_can_resolve_a_slot_that_needs_attention() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .set_slot_health(slot.clone(), SlotHealth::NeedsAttention)
            .unwrap();
        engine.arm_maintenance_test(slot.clone()).unwrap();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());
        engine.vend_succeeded(transaction).unwrap();
        assert_eq!(engine.state().health(&slot), SlotHealth::Ready);
        assert_eq!(engine.state().inventory(&slot), 1);
    }

    #[test]
    fn restart_turns_an_in_flight_approval_into_admin_review() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());
        let state = engine.state().clone();

        let recovered = KioskEngine::new(engine.catalog().clone(), state);
        let transaction = recovered
            .state()
            .transactions()
            .iter()
            .find(|candidate| candidate.id() == transaction)
            .unwrap();
        assert_eq!(transaction.status(), TransactionStatus::Uncertain);
        assert_eq!(recovered.state().health(&slot), SlotHealth::NeedsAttention);
        let code = PromoCode::parse("123456").unwrap();
        assert_eq!(
            recovered
                .state()
                .code(&code)
                .unwrap()
                .entitlement(&ProductId::parse("water").unwrap())
                .unwrap()
                .reserved(),
            1
        );
    }

    #[test]
    fn uncertain_transaction_must_be_reconciled_before_slot_resolution() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .authenticate_code(PromoCode::parse("123456").unwrap())
            .unwrap();
        let transaction = transaction_id(engine.machine_selected(&slot).unwrap());
        engine.vend_uncertain(transaction).unwrap();
        assert_eq!(
            engine.set_slot_health(slot.clone(), SlotHealth::Ready),
            Err(KioskError::UnresolvedTransaction(slot.clone()))
        );
        engine.resolve_uncertain(transaction, false).unwrap();
        engine
            .set_slot_health(slot.clone(), SlotHealth::Ready)
            .unwrap();
        assert_eq!(engine.state().health(&slot), SlotHealth::Ready);
    }
}
