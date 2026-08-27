use anyhow::{ensure, Context};
use bitcoin::Network;
use fedimint_core::invite_code::InviteCode;
use lv_core::{ManagerRequest, ManagerResponse};
use lv_vendimint::{request, Machine, MachineState, Manager, ManagerProtocolHandler};
use std::{num::NonZeroUsize, time::Duration};
use tokio::time::{sleep, timeout};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Box::pin(
        devimint::run_devfed_test().call(|dev_fed, _process_manager| async move {
            let federation = dev_fed.fed().await?;
            let invite_code: InviteCode = federation.invite_code()?.parse()?;

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
                round_trip_manager_request(&manager, &machine_ids[0], &mut manager_requests).await
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
