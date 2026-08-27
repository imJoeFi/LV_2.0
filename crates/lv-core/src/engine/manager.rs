use super::{
    unix_millis_now, ArmMode, KioskEngine, KioskError, ManagerProjection, PaymentKind, PromoCode,
    Transaction, TransactionStatus,
};
use crate::{
    CommandEnvelope, CommandResult, EventEnvelope, FreeVendScope, KioskSnapshot, ManagerCommand,
    ManagerEvent, ManagerRequest, ManagerResponse, PaymentSummary, PromoAccountRef,
    PurchaseSummary, PurchaseSummaryState, SlotSnapshot, StateRevision, VendAuthorizationSnapshot,
};
use sha2::{Digest, Sha256};

const MAX_KIOSK_NAME_LENGTH: usize = 80;
const MAX_VEND_AUTHORIZATION_SECONDS: u32 = 5 * 60;

impl KioskEngine {
    pub fn handle_manager_request(&mut self, request: ManagerRequest) -> ManagerResponse {
        match request {
            ManagerRequest::GetSnapshot => ManagerResponse::Snapshot(self.manager_snapshot()),
            ManagerRequest::SubscribeEvents { after } => ManagerResponse::Events(
                self.state
                    .manager
                    .events
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned()
                    .collect(),
            ),
            ManagerRequest::Command(envelope) => self.execute_manager_command(envelope),
        }
    }

    pub fn synchronize_manager_events(&mut self) {
        let projection = self.manager_projection();
        if !self.state.manager.projection.initialized {
            self.state.manager.projection = projection;
            return;
        }
        let events = projection_events(&self.state.manager.projection, &projection);
        self.state.manager.projection = projection;
        self.append_manager_events(events);
    }

    pub fn expire_vend_authorization(&mut self, now_unix_millis: u64) -> bool {
        let expired = self
            .arm_expires_at_unix_millis
            .is_some_and(|deadline| now_unix_millis >= deadline)
            && self.arm_mode != ArmMode::None;
        if expired {
            self.disarm();
        }
        expired
    }

    pub fn manager_snapshot(&self) -> KioskSnapshot {
        KioskSnapshot {
            name: self.state.manager.name.clone(),
            admin_pin_configured: self.state.manager.admin_pin.is_some(),
            revision: self.state.manager.revision,
            through_sequence: self.state.manager.through_sequence,
            slots: self
                .catalog
                .slots()
                .map(|slot| SlotSnapshot {
                    slot: slot.id().clone(),
                    inventory: self.state.inventory(slot.id()),
                    reserved: self.state.reserved_inventory(slot.id()),
                    health: self.state.health(slot.id()),
                })
                .collect(),
            unresolved_purchases: self
                .purchase_summaries()
                .into_values()
                .filter(|purchase| purchase.state.is_unresolved())
                .collect(),
            vend_authorization: self.vend_authorization_snapshot(),
        }
    }

    fn execute_manager_command(&mut self, envelope: CommandEnvelope) -> ManagerResponse {
        if let Some(result) = self.state.manager.command_results.get(&envelope.id) {
            return ManagerResponse::CommandResult {
                command_id: envelope.id,
                result: result.clone(),
            };
        }

        self.synchronize_manager_events();
        let mut candidate = self.clone();
        let result = match candidate.apply_manager_command(envelope.command) {
            Ok(mut events) => {
                let projection = candidate.manager_projection();
                events.extend(projection_events(
                    &candidate.state.manager.projection,
                    &projection,
                ));
                candidate.state.manager.projection = projection;
                candidate.append_manager_events(events);
                let result = CommandResult::Applied {
                    revision: candidate.state.manager.revision,
                };
                candidate
                    .state
                    .manager
                    .command_results
                    .insert(envelope.id, result.clone());
                *self = candidate;
                result
            }
            Err(error) => {
                let result = CommandResult::Rejected {
                    code: error.code.to_owned(),
                    message: error.message,
                    current_revision: self.state.manager.revision,
                };
                self.state
                    .manager
                    .command_results
                    .insert(envelope.id, result.clone());
                result
            }
        };
        ManagerResponse::CommandResult {
            command_id: envelope.id,
            result,
        }
    }

    fn apply_manager_command(
        &mut self,
        command: ManagerCommand,
    ) -> Result<Vec<ManagerEvent>, ManagerCommandError> {
        let mut events = Vec::new();
        match command {
            ManagerCommand::SetKioskName { name } => {
                if let Some(event) = self.set_kiosk_name(&name)? {
                    events.push(event);
                }
            }
            ManagerCommand::SetInitialAdminPin { pin } => {
                if self.state.manager.admin_pin.is_some() {
                    return Err(ManagerCommandError::new(
                        "admin_pin_already_set",
                        "the initial admin PIN has already been configured",
                    ));
                }
                self.state.manager.admin_pin = Some(pin);
                events.push(ManagerEvent::AdminPinChanged);
            }
            ManagerCommand::ChangeAdminPin { pin } => {
                if self.state.manager.admin_pin.is_none() {
                    return Err(ManagerCommandError::new(
                        "admin_pin_not_set",
                        "set the initial admin PIN before changing it",
                    ));
                }
                self.state.manager.admin_pin = Some(pin);
                events.push(ManagerEvent::AdminPinChanged);
            }
            ManagerCommand::SetInventory {
                slot,
                quantity,
                expected_revision,
            } => {
                self.require_revision(expected_revision)?;
                self.set_inventory(slot, quantity)
                    .map_err(ManagerCommandError::from)?;
            }
            ManagerCommand::ResolveSlotAttention {
                slot,
                expected_revision,
            } => {
                self.require_revision(expected_revision)?;
                self.set_slot_health(slot, super::SlotHealth::Ready)
                    .map_err(ManagerCommandError::from)?;
            }
            ManagerCommand::ResolveUncertainVend {
                purchase_id,
                dispensed,
            } => self
                .resolve_uncertain(purchase_id, dispensed)
                .map_err(ManagerCommandError::from)?,
            ManagerCommand::ResolveAssistance {
                purchase_id,
                resolution,
                note,
            } => {
                self.resolve_assistance(purchase_id, resolution, note)
                    .map_err(ManagerCommandError::from)?;
                events.push(ManagerEvent::AssistanceResolved {
                    purchase_id,
                    resolution,
                });
            }
            ManagerCommand::ArmFreeVend {
                scope,
                expires_in_seconds,
            } => {
                self.require_no_active_vend()?;
                let expires_at = authorization_expiration(expires_in_seconds)?;
                if let FreeVendScope::SpecificSlot(slot) = &scope {
                    self.catalog.slot(slot).ok_or_else(|| {
                        ManagerCommandError::from(KioskError::UnknownSlot(slot.clone()))
                    })?;
                    if self.has_uncertain_transaction(slot) {
                        return Err(ManagerCommandError::from(
                            KioskError::UnresolvedTransaction(slot.clone()),
                        ));
                    }
                }
                self.arm_mode = ArmMode::FreeNext;
                self.free_vend_scope = scope;
                self.arm_expires_at_unix_millis = Some(expires_at);
            }
            ManagerCommand::ArmMaintenanceVend {
                slot,
                expires_in_seconds,
            } => {
                self.require_no_active_vend()?;
                let expires_at = authorization_expiration(expires_in_seconds)?;
                self.arm_maintenance_test(slot)
                    .map_err(ManagerCommandError::from)?;
                self.arm_expires_at_unix_millis = Some(expires_at);
            }
            ManagerCommand::DisarmVendAuthorization => self.disarm(),
        }
        Ok(events)
    }

    fn set_kiosk_name(&mut self, name: &str) -> Result<Option<ManagerEvent>, ManagerCommandError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_KIOSK_NAME_LENGTH {
            return Err(ManagerCommandError::new(
                "invalid_kiosk_name",
                format!("kiosk name must contain between 1 and {MAX_KIOSK_NAME_LENGTH} characters"),
            ));
        }
        if self.state.manager.name.as_deref() == Some(name) {
            return Ok(None);
        }
        self.state.manager.name = Some(name.to_owned());
        Ok(Some(ManagerEvent::KioskNameChanged {
            name: name.to_owned(),
        }))
    }

    fn require_revision(&self, expected: StateRevision) -> Result<(), ManagerCommandError> {
        if expected == self.state.manager.revision {
            Ok(())
        } else {
            Err(ManagerCommandError::new(
                "revision_conflict",
                format!(
                    "expected kiosk revision {}, but the current revision is {}",
                    expected.0, self.state.manager.revision.0
                ),
            ))
        }
    }

    fn require_no_active_vend(&self) -> Result<(), ManagerCommandError> {
        if self.active_transaction.is_none() {
            Ok(())
        } else {
            Err(ManagerCommandError::new(
                "vend_in_progress",
                "a vend is already in progress",
            ))
        }
    }

    fn manager_projection(&self) -> ManagerProjection {
        ManagerProjection {
            initialized: true,
            inventory: self
                .catalog
                .slots()
                .map(|slot| (slot.id().clone(), self.state.inventory(slot.id())))
                .collect(),
            health: self
                .catalog
                .slots()
                .map(|slot| (slot.id().clone(), self.state.health(slot.id())))
                .collect(),
            purchases: self.purchase_summaries(),
            vend_authorization_armed: self.arm_mode != ArmMode::None,
        }
    }

    fn purchase_summaries(&self) -> std::collections::BTreeMap<crate::PurchaseId, PurchaseSummary> {
        self.state
            .transactions
            .iter()
            .map(|transaction| (transaction.id(), purchase_summary(transaction)))
            .collect()
    }

    fn vend_authorization_snapshot(&self) -> Option<VendAuthorizationSnapshot> {
        match &self.arm_mode {
            ArmMode::None => None,
            ArmMode::FreeNext => Some(VendAuthorizationSnapshot::FreeVend {
                scope: self.free_vend_scope.clone(),
                expires_at_unix_millis: self.arm_expires_at_unix_millis,
            }),
            ArmMode::MaintenanceTest(slot) => Some(VendAuthorizationSnapshot::MaintenanceVend {
                slot: slot.clone(),
                expires_at_unix_millis: self.arm_expires_at_unix_millis,
            }),
        }
    }

    fn append_manager_events(&mut self, events: Vec<ManagerEvent>) {
        if events.is_empty() {
            return;
        }
        self.state.manager.revision.0 = self.state.manager.revision.0.saturating_add(1);
        let occurred_at_unix_millis = unix_millis_now();
        for event in events {
            self.state.manager.through_sequence.0 =
                self.state.manager.through_sequence.0.saturating_add(1);
            self.state.manager.events.push(EventEnvelope {
                sequence: self.state.manager.through_sequence,
                occurred_at_unix_millis,
                event,
            });
        }
    }
}

impl PurchaseSummaryState {
    const fn is_unresolved(&self) -> bool {
        matches!(
            self,
            Self::AwaitingPayment | Self::AwaitingVend | Self::AssistanceRequired | Self::Uncertain
        )
    }
}

fn projection_events(
    previous: &ManagerProjection,
    current: &ManagerProjection,
) -> Vec<ManagerEvent> {
    let mut events = Vec::new();
    for (slot, quantity) in &current.inventory {
        if previous.inventory.get(slot) != Some(quantity) {
            events.push(ManagerEvent::InventorySet {
                slot: slot.clone(),
                quantity: *quantity,
            });
        }
    }
    for (slot, health) in &current.health {
        if previous.health.get(slot) != Some(health) {
            events.push(ManagerEvent::SlotHealthChanged {
                slot: slot.clone(),
                health: *health,
            });
        }
    }
    for (id, purchase) in &current.purchases {
        if previous.purchases.get(id) != Some(purchase) {
            events.push(ManagerEvent::PurchaseChanged(purchase.clone()));
        }
    }
    match (
        previous.vend_authorization_armed,
        current.vend_authorization_armed,
    ) {
        (false, true) => events.push(ManagerEvent::VendAuthorizationArmed),
        (true, false) => events.push(ManagerEvent::VendAuthorizationDisarmed),
        _ => {}
    }
    events
}

fn purchase_summary(transaction: &Transaction) -> PurchaseSummary {
    PurchaseSummary {
        id: transaction.id(),
        slot: transaction.slot().clone(),
        product: transaction.product().clone(),
        payment: match transaction.payment() {
            PaymentKind::Promo { code } => PaymentSummary::Promo {
                account: promo_account_ref(code),
            },
            PaymentKind::Lightning { price } => PaymentSummary::Lightning { amount: *price },
            PaymentKind::FreeVend => PaymentSummary::FreeVend,
            PaymentKind::MaintenanceTest => PaymentSummary::MaintenanceTest,
        },
        state: match transaction.status() {
            TransactionStatus::AwaitingPayment => PurchaseSummaryState::AwaitingPayment,
            TransactionStatus::AwaitingVend => PurchaseSummaryState::AwaitingVend,
            TransactionStatus::Succeeded => PurchaseSummaryState::Dispensed,
            TransactionStatus::Failed => PurchaseSummaryState::Failed,
            TransactionStatus::Cancelled => PurchaseSummaryState::Cancelled,
            TransactionStatus::Expired => PurchaseSummaryState::Expired,
            TransactionStatus::AssistanceRequired => PurchaseSummaryState::AssistanceRequired,
            TransactionStatus::Uncertain => PurchaseSummaryState::Uncertain,
            TransactionStatus::ResolvedDispensed | TransactionStatus::ResolvedNotDispensed => {
                PurchaseSummaryState::Resolved
            }
        },
    }
}

fn promo_account_ref(code: &PromoCode) -> PromoAccountRef {
    PromoAccountRef {
        fingerprint: format!("{:x}", Sha256::digest(code.as_str().as_bytes())),
        masked: code.masked(),
    }
}

fn authorization_expiration(expires_in_seconds: u32) -> Result<u64, ManagerCommandError> {
    if !(1..=MAX_VEND_AUTHORIZATION_SECONDS).contains(&expires_in_seconds) {
        return Err(ManagerCommandError::new(
            "invalid_expiration",
            format!(
                "vend authorization must expire between 1 and {MAX_VEND_AUTHORIZATION_SECONDS} seconds from now"
            ),
        ));
    }
    Ok(unix_millis_now().saturating_add(u64::from(expires_in_seconds) * 1_000))
}

struct ManagerCommandError {
    code: &'static str,
    message: String,
}

impl ManagerCommandError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<KioskError> for ManagerCommandError {
    fn from(error: KioskError) -> Self {
        Self::new("invalid_command", error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AdminPin, Catalog, CommandId, ManagerCommand, PersistentState, SlotHealth, SlotId,
    };
    use std::path::Path;
    use std::str::FromStr;

    const CATALOG: &str = r#"
        [[products]]
        id = "water"
        name = "Sparkling Water"

        [[slots]]
        id = "A1"
        product = "water"
        payment = "promo"
    "#;

    fn engine() -> KioskEngine {
        let catalog = Catalog::parse(CATALOG, Path::new(".")).unwrap();
        let mut engine = KioskEngine::new(catalog, PersistentState::default());
        engine.synchronize_manager_events();
        engine
    }

    fn apply(engine: &mut KioskEngine, command: ManagerCommand) -> CommandResult {
        let ManagerResponse::CommandResult { result, .. } =
            engine.handle_manager_request(ManagerRequest::Command(CommandEnvelope {
                id: CommandId::new(),
                command,
            }))
        else {
            panic!("manager command returned the wrong response");
        };
        result
    }

    #[test]
    fn inventory_commands_are_revision_checked_and_emit_events() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        assert!(matches!(
            apply(
                &mut engine,
                ManagerCommand::SetInventory {
                    slot: slot.clone(),
                    quantity: 4,
                    expected_revision: StateRevision(0),
                }
            ),
            CommandResult::Applied {
                revision: StateRevision(1)
            }
        ));
        assert_eq!(engine.state.inventory(&slot), 4);
        assert!(matches!(
            engine.state.manager.events.last().map(|event| &event.event),
            Some(ManagerEvent::InventorySet { quantity: 4, .. })
        ));

        assert!(matches!(
            apply(
                &mut engine,
                ManagerCommand::SetInventory {
                    slot,
                    quantity: 9,
                    expected_revision: StateRevision(0),
                }
            ),
            CommandResult::Rejected { ref code, .. } if code == "revision_conflict"
        ));
    }

    #[test]
    fn command_ids_are_idempotent() {
        let mut engine = engine();
        let id = CommandId::new();
        let request = ManagerRequest::Command(CommandEnvelope {
            id,
            command: ManagerCommand::SetKioskName {
                name: "Lobby".to_owned(),
            },
        });
        let first = engine.handle_manager_request(request.clone());
        let event_count = engine.state.manager.events.len();
        let second = engine.handle_manager_request(request);
        assert_eq!(first, second);
        assert_eq!(engine.state.manager.events.len(), event_count);
    }

    #[test]
    fn initial_pin_can_only_be_set_once() {
        let mut engine = engine();
        assert!(matches!(
            apply(
                &mut engine,
                ManagerCommand::SetInitialAdminPin {
                    pin: AdminPin::parse("1234").unwrap(),
                }
            ),
            CommandResult::Applied { .. }
        ));
        assert!(matches!(
            apply(
                &mut engine,
                ManagerCommand::SetInitialAdminPin {
                    pin: AdminPin::parse("5678").unwrap(),
                }
            ),
            CommandResult::Rejected { ref code, .. } if code == "admin_pin_already_set"
        ));
        assert_eq!(engine.state.admin_pin().unwrap().expose(), "1234");
    }

    #[test]
    fn remote_authorization_expires_and_is_visible_in_snapshots() {
        let mut engine = engine();
        assert!(matches!(
            apply(
                &mut engine,
                ManagerCommand::ArmFreeVend {
                    scope: FreeVendScope::AnyConfiguredSlot,
                    expires_in_seconds: 30,
                }
            ),
            CommandResult::Applied { .. }
        ));
        let snapshot = engine.manager_snapshot();
        let Some(VendAuthorizationSnapshot::FreeVend {
            expires_at_unix_millis: Some(deadline),
            ..
        }) = snapshot.vend_authorization
        else {
            panic!("free vend authorization missing from snapshot");
        };
        assert!(engine.expire_vend_authorization(deadline));
        engine.synchronize_manager_events();
        assert_eq!(engine.arm_mode(), &ArmMode::None);
        assert!(matches!(
            engine.state.manager.events.last().map(|event| &event.event),
            Some(ManagerEvent::VendAuthorizationDisarmed)
        ));
    }

    #[test]
    fn snapshots_report_slot_health_and_pin_configuration() {
        let mut engine = engine();
        let slot = SlotId::from_str("A1").unwrap();
        engine
            .set_slot_health(slot.clone(), SlotHealth::NeedsAttention)
            .unwrap();
        engine.synchronize_manager_events();
        let snapshot = engine.manager_snapshot();
        assert!(!snapshot.admin_pin_configured);
        assert_eq!(snapshot.slots[0].slot, slot);
        assert_eq!(snapshot.slots[0].health, SlotHealth::NeedsAttention);
    }
}
