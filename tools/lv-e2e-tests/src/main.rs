use anyhow::{ensure, Context};
use bitcoin::{hashes::Hash, Network};
use fedimint_core::{invite_code::InviteCode, Amount};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_remote_client::FinalRemoteReceiveOperationState;
use lv_core::{
    Catalog, CommandEnvelope, CommandId, CommandResult, KioskEngine, ManagerCommand, ManagerEvent,
    ManagerRequest, ManagerResponse, PersistentState, SelectionOutcome, SlotId, StateRevision,
    TransactionStatus,
};
use lv_vendimint::{request, Machine, MachineState, Manager, ManagerProtocolHandler, MintVersion};
use std::{
    num::NonZeroUsize,
    path::Path,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, timeout};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const PAYMENT_TIMEOUT: Duration = Duration::from_secs(90);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TEST_PAYMENT: Amount = Amount::from_sats(100);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Box::pin(
        devimint::run_devfed_test().call(|dev_fed, _process_manager| async move {
            let federation = dev_fed.fed().await?;
            let invite_code: InviteCode = federation.invite_code()?.parse()?;
            federation
                .pegin_gateways(
                    1_000_000,
                    vec![dev_fed.gw_lnd().await?, dev_fed.gw_ldk().await?],
                )
                .await?;

            let machine_storage = tempfile::tempdir()?;
            let (handler, mut manager_requests) =
                ManagerProtocolHandler::channel(NonZeroUsize::new(8).unwrap());
            let builder =
                handler.register(Machine::builder(machine_storage.path(), Network::Regtest))?;
            let mut machine = builder.build().await?;

            let manager_storage = tempfile::tempdir()?;
            let mut manager = Manager::new(manager_storage.path(), Network::Regtest).await?;

            let scenario = async {
                pair(&machine, &manager).await?;
                manager.update_federation(invite_code.clone()).await?;
                ensure!(
                    manager.get_mint_version(invite_code.federation_id()).await
                        == Some(MintVersion::V2),
                    "a fresh dual-module wallet did not select mint v2"
                );
                wait_until_configured(&machine, &invite_code).await?;

                let machine_ids = wait_for_claimed_machine(&manager).await?;
                ensure!(
                    machine_ids.len() == 1,
                    "manager did not retain exactly one machine"
                );
                let mut kiosk =
                    exercise_manager_service(&manager, &machine_ids[0], &mut manager_requests)
                        .await?;
                pay_vend_and_sweep(
                    &dev_fed,
                    &machine,
                    &manager,
                    &invite_code,
                    &machine_ids[0],
                    &mut manager_requests,
                    &mut kiosk,
                )
                .await
            }
            .await;

            let machine_shutdown = machine.shutdown().await;
            let manager_shutdown = manager.shutdown().await;
            scenario?;
            machine_shutdown.context("Vendimint machine did not shut down cleanly")?;
            manager_shutdown.context("Vendimint manager did not shut down cleanly")?;
            Ok(())
        }),
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn pay_vend_and_sweep(
    dev_fed: &devimint::devfed::DevJitFed,
    machine: &Machine,
    manager: &Manager,
    invite_code: &InviteCode,
    machine_id: &iroh::EndpointId,
    manager_requests: &mut lv_vendimint::ManagerRequestReceiver,
    kiosk: &mut KioskEngine,
) -> anyhow::Result<()> {
    let slot = SlotId::from_str("B1")?;
    let before_payment = kiosk.manager_snapshot().through_sequence;
    let SelectionOutcome::LightningPaymentRequired {
        transaction_id,
        price,
        ..
    } = kiosk.machine_selected(&slot)?
    else {
        anyhow::bail!("kiosk did not reserve inventory for the Lightning selection");
    };
    ensure!(price.as_u64() == TEST_PAYMENT.msats);
    kiosk.begin_lightning_invoice(
        transaction_id,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .saturating_add(PAYMENT_TIMEOUT.as_secs()),
    )?;

    let gateway = dev_fed.gw_ldk().await?;
    let gateway_address: fedimint_core::util::SafeUrl = gateway.addr.parse()?;
    let (invoice, operation_id) = timeout(PAYMENT_TIMEOUT, async {
        loop {
            match machine
                .receive_payment(
                    TEST_PAYMENT,
                    u32::try_from(PAYMENT_TIMEOUT.as_secs()).unwrap(),
                    Bolt11InvoiceDescription::Direct("LightningVEND E2E purchase".to_owned()),
                    Some(gateway_address.clone()),
                )
                .await
            {
                Ok(invoice) => return Ok::<_, anyhow::Error>(invoice),
                Err(error) if error.to_string().starts_with("Client for federation ") => {
                    sleep(STATE_POLL_INTERVAL).await;
                }
                Err(error) => return Err(error).context("machine could not create an invoice"),
            }
        }
    })
    .await
    .context("timed out waiting for the machine wallet to join the federation")??;

    let expires_at = invoice
        .expires_at()
        .context("Vendimint returned an invoice without an expiration")?;
    kiosk.lightning_invoice_created(
        transaction_id,
        lv_core::LightningInvoice {
            bolt11: invoice.to_string(),
            operation_id: operation_id.0,
            payment_hash: invoice.payment_hash().to_byte_array(),
            expires_at_unix_seconds: expires_at.as_secs(),
        },
    )?;
    ensure!(kiosk.state().reserved_inventory(&slot) == 1);

    timeout(
        PAYMENT_TIMEOUT,
        dev_fed.lnd().await?.pay_bolt11_invoice(invoice.to_string()),
    )
    .await
    .context("timed out paying the Vendimint invoice")??;

    let payment_state = timeout(
        PAYMENT_TIMEOUT,
        machine.await_receive_payment_final_state(operation_id),
    )
    .await
    .context("timed out waiting for the machine to observe payment")??;
    ensure!(
        payment_state == FinalRemoteReceiveOperationState::Funded,
        "machine reported an unexpected final payment state: {payment_state:?}"
    );

    let approved = kiosk.lightning_payment_accepted(transaction_id)?;
    ensure!(matches!(approved, SelectionOutcome::VendApproved { .. }));
    kiosk.vend_succeeded(transaction_id)?;
    kiosk.synchronize_manager_events();
    ensure!(kiosk.state().inventory(&slot) == 0);
    ensure!(
        kiosk
            .state()
            .transactions()
            .iter()
            .find(|transaction| transaction.id() == transaction_id)
            .is_some_and(|transaction| transaction.status() == TransactionStatus::Succeeded),
        "successful simulated MDB result did not complete the durable transaction"
    );

    let payment_events = call_manager_service(
        manager,
        machine_id,
        manager_requests,
        kiosk,
        ManagerRequest::SubscribeEvents {
            after: before_payment,
        },
    )
    .await?;
    let ManagerResponse::Events(payment_events) = payment_events else {
        anyhow::bail!("manager service did not return post-payment events");
    };
    ensure!(payment_events.iter().any(|event| {
        matches!(
            &event.event,
            ManagerEvent::PurchaseChanged(purchase)
                if purchase.id == transaction_id
                    && purchase.state == lv_core::PurchaseSummaryState::Dispensed
        )
    }));
    ensure!(payment_events.iter().any(|event| {
        matches!(
            &event.event,
            ManagerEvent::InventorySet { slot, quantity }
                if slot.as_str() == "B1" && *quantity == 0
        )
    }));

    let export = timeout(PAYMENT_TIMEOUT, async {
        loop {
            if let Some(notes) = manager
                .sweep_all_ecash_notes(
                    invite_code.federation_id(),
                    Duration::from_secs(30),
                    false,
                    None::<()>,
                )
                .await
                .context("manager could not sweep the funded invoice")?
            {
                return Ok::<_, anyhow::Error>(notes);
            }
            sleep(STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .context("timed out waiting for the manager to sweep the payment")??;
    ensure!(
        export.mint_version() == MintVersion::V2,
        "manager exported the payment with an unexpected mint generation"
    );
    ensure!(
        !export.reclaims_automatically(),
        "mint-v2 export unexpectedly advertised automatic reclaim"
    );
    let swept = export.total_amount();
    ensure!(swept > Amount::ZERO, "manager swept an empty payment");
    ensure!(
        swept <= TEST_PAYMENT,
        "manager swept more than the invoice amount"
    );
    Ok(())
}

async fn pair(machine: &Machine, manager: &Manager) -> anyhow::Result<()> {
    let MachineState::Unclaimed(machine_address) = machine.get_machine_state().await? else {
        anyhow::bail!("new Vendimint machine was already claimed");
    };
    let (manager_pin, manager_response) = manager.claim_machine(machine_address).await?;
    let (machine_pin, machine_response) =
        timeout(EVENT_TIMEOUT, machine.await_next_incoming_claim_request())
            .await
            .context("timed out waiting for the machine claim request")?
            .context("machine stopped before receiving the claim request")?;
    ensure!(
        manager_pin == machine_pin,
        "Vendimint claim PINs did not match"
    );
    machine_response
        .send(true)
        .map_err(|_| anyhow::anyhow!("machine claim response receiver was dropped"))?;
    manager_response
        .send(true)
        .map_err(|_| anyhow::anyhow!("manager claim response receiver was dropped"))?;
    Ok(())
}

async fn wait_until_configured(
    machine: &Machine,
    expected_invite: &InviteCode,
) -> anyhow::Result<()> {
    timeout(EVENT_TIMEOUT, async {
        loop {
            if let MachineState::Claimed(Some(config)) = machine.get_machine_state().await? {
                ensure!(
                    config.federation_invite_code == *expected_invite,
                    "machine received an unexpected federation configuration"
                );
                return Ok::<(), anyhow::Error>(());
            }
            sleep(STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .context("timed out waiting for the manager to configure the machine")?
}

async fn wait_for_claimed_machine(manager: &Manager) -> anyhow::Result<Vec<iroh::EndpointId>> {
    timeout(EVENT_TIMEOUT, async {
        loop {
            let machine_ids = manager.list_machine_ids().await?;
            if !machine_ids.is_empty() {
                return Ok::<_, anyhow::Error>(machine_ids);
            }
            sleep(STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .context("timed out waiting for the claimed machine to sync to the manager")?
}

#[allow(clippy::too_many_lines)]
async fn exercise_manager_service(
    manager: &Manager,
    machine_id: &iroh::EndpointId,
    requests: &mut lv_vendimint::ManagerRequestReceiver,
) -> anyhow::Result<KioskEngine> {
    let catalog = Catalog::parse(
        r#"
            [[products]]
            id = "water"
            name = "Sparkling Water"

            [[slots]]
            id = "A1"
            product = "water"
            payment = "promo"

            [[products]]
            id = "snack"
            name = "Test Snack"

            [[slots]]
            id = "B1"
            product = "snack"
            payment = "lightning"
            price_msats = 100000
        "#,
        Path::new("."),
    )?;
    let mut kiosk = KioskEngine::new(catalog, PersistentState::default());
    kiosk.synchronize_manager_events();

    let initial = call_manager_service(
        manager,
        machine_id,
        requests,
        &mut kiosk,
        ManagerRequest::GetSnapshot,
    )
    .await?;
    let ManagerResponse::Snapshot(initial) = initial else {
        anyhow::bail!("manager service did not return its initial snapshot");
    };
    ensure!(initial.revision == StateRevision(0));

    let command_id = CommandId::new();
    let command = ManagerRequest::Command(CommandEnvelope {
        id: command_id,
        command: ManagerCommand::SetInventory {
            slot: SlotId::from_str("A1")?,
            quantity: 7,
            expected_revision: initial.revision,
        },
    });
    let applied =
        call_manager_service(manager, machine_id, requests, &mut kiosk, command.clone()).await?;
    ensure!(matches!(
        applied,
        ManagerResponse::CommandResult {
            command_id: returned_id,
            result: CommandResult::Applied {
                revision: StateRevision(1)
            },
        } if returned_id == command_id
    ));
    let repeated = call_manager_service(manager, machine_id, requests, &mut kiosk, command).await?;
    ensure!(repeated == applied, "command retry was not idempotent");

    let stock_lightning = ManagerRequest::Command(CommandEnvelope {
        id: CommandId::new(),
        command: ManagerCommand::SetInventory {
            slot: SlotId::from_str("B1")?,
            quantity: 1,
            expected_revision: StateRevision(1),
        },
    });
    let stocked =
        call_manager_service(manager, machine_id, requests, &mut kiosk, stock_lightning).await?;
    ensure!(matches!(
        stocked,
        ManagerResponse::CommandResult {
            result: CommandResult::Applied {
                revision: StateRevision(2)
            },
            ..
        }
    ));

    let events = call_manager_service(
        manager,
        machine_id,
        requests,
        &mut kiosk,
        ManagerRequest::SubscribeEvents {
            after: initial.through_sequence,
        },
    )
    .await?;
    let ManagerResponse::Events(events) = events else {
        anyhow::bail!("manager service did not return its event stream");
    };
    ensure!(events.iter().any(|event| matches!(
        &event.event,
        ManagerEvent::InventorySet { slot, quantity }
            if slot.as_str() == "A1" && *quantity == 7
    )));
    ensure!(events.iter().any(|event| matches!(
        &event.event,
        ManagerEvent::InventorySet { slot, quantity }
            if slot.as_str() == "B1" && *quantity == 1
    )));
    Ok(kiosk)
}

async fn call_manager_service(
    manager: &Manager,
    machine_id: &iroh::EndpointId,
    requests: &mut lv_vendimint::ManagerRequestReceiver,
    kiosk: &mut KioskEngine,
    manager_request: ManagerRequest,
) -> anyhow::Result<ManagerResponse> {
    let client = request(manager, machine_id, manager_request.clone());
    let server = async {
        let incoming = timeout(EVENT_TIMEOUT, requests.recv())
            .await
            .context("timed out waiting for the authenticated manager request")?
            .context("manager request channel closed unexpectedly")?;
        ensure!(
            incoming.request() == &manager_request,
            "manager request changed during transport"
        );
        let response = kiosk.handle_manager_request(incoming.request().clone());
        incoming
            .respond(response)
            .map_err(|_| anyhow::anyhow!("manager response receiver was dropped"))?;
        Ok::<(), anyhow::Error>(())
    };
    let (response, ()) = tokio::try_join!(client, server)?;
    Ok(response)
}
