use anyhow::{ensure, Context};
use bitcoin::Network;
use fedimint_core::{invite_code::InviteCode, Amount};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_remote_client::FinalRemoteReceiveOperationState;
use lv_core::{ManagerRequest, ManagerResponse};
use lv_vendimint::{request, Machine, MachineState, Manager, ManagerProtocolHandler};
use std::{num::NonZeroUsize, time::Duration};
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
                wait_until_configured(&machine, &invite_code).await?;

                let machine_ids = wait_for_claimed_machine(&manager).await?;
                ensure!(
                    machine_ids.len() == 1,
                    "manager did not retain exactly one machine"
                );
                round_trip_manager_request(&manager, &machine_ids[0], &mut manager_requests)
                    .await?;
                pay_and_sweep(&dev_fed, &machine, &manager, &invite_code).await
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

async fn pay_and_sweep(
    dev_fed: &devimint::devfed::DevJitFed,
    machine: &Machine,
    manager: &Manager,
    invite_code: &InviteCode,
) -> anyhow::Result<()> {
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

    let swept = timeout(PAYMENT_TIMEOUT, async {
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
                return Ok::<Amount, anyhow::Error>(notes.total_amount());
            }
            sleep(STATE_POLL_INTERVAL).await;
        }
    })
    .await
    .context("timed out waiting for the manager to sweep the payment")??;
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

async fn round_trip_manager_request(
    manager: &Manager,
    machine_id: &iroh::EndpointId,
    requests: &mut lv_vendimint::ManagerRequestReceiver,
) -> anyhow::Result<()> {
    let expected = ManagerResponse::ProtocolError {
        message: "E2E transport smoke response".to_owned(),
    };
    let client = request(manager, machine_id, ManagerRequest::GetSnapshot);
    let server = async {
        let incoming = timeout(EVENT_TIMEOUT, requests.recv())
            .await
            .context("timed out waiting for the authenticated manager request")?
            .context("manager request channel closed unexpectedly")?;
        ensure!(
            incoming.request() == &ManagerRequest::GetSnapshot,
            "manager request changed during transport"
        );
        incoming
            .respond(expected.clone())
            .map_err(|_| anyhow::anyhow!("manager response receiver was dropped"))?;
        Ok::<(), anyhow::Error>(())
    };
    let (response, ()) = tokio::try_join!(client, server)?;
    ensure!(
        response == expected,
        "manager response changed during transport"
    );
    Ok(())
}
