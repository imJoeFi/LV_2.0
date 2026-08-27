//! Vendimint-backed identity, pairing, and authenticated manager RPC transport.

mod kiosk;

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
    EndpointId,
};
use lv_core::{
    ManagerRequest, ManagerResponse, WireRequest, WireResponse, MANAGER_ALPN,
    MANAGER_PROTOCOL_VERSION,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{fmt, num::NonZeroUsize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

pub use kiosk::{
    ClaimRequest, PaymentController, PaymentControllerConfig, PaymentControllerEvent,
    PaymentControllerStopped, PaymentMachineState,
};
pub use vendimint::{Machine, MachineBuilder, MachineState, Manager};

const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// The receiving side of the kiosk's authenticated manager-RPC channel.
///
/// Only the claimed Vendimint manager can cause a request to appear here. The
/// kiosk should drain this receiver from the same actor which owns its durable
/// state, apply the request, persist it, and then respond.
pub struct ManagerRequestReceiver {
    receiver: mpsc::Receiver<IncomingManagerRequest>,
}

impl ManagerRequestReceiver {
    pub async fn recv(&mut self) -> Option<IncomingManagerRequest> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Result<IncomingManagerRequest, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

/// A manager request authenticated by Vendimint's claim relationship.
pub struct IncomingManagerRequest {
    remote_id: EndpointId,
    request: ManagerRequest,
    response: oneshot::Sender<ManagerResponse>,
}

impl IncomingManagerRequest {
    pub const fn remote_id(&self) -> EndpointId {
        self.remote_id
    }

    pub const fn request(&self) -> &ManagerRequest {
        &self.request
    }

    pub fn respond(self, response: ManagerResponse) -> Result<(), Box<ManagerResponse>> {
        self.response.send(response).map_err(Box::new)
    }
}

impl fmt::Debug for IncomingManagerRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IncomingManagerRequest")
            .field("remote_id", &self.remote_id)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

/// Iroh protocol handler installed on a Vendimint [`MachineBuilder`].
#[derive(Debug, Clone)]
pub struct ManagerProtocolHandler {
    requests: mpsc::Sender<IncomingManagerRequest>,
}

impl ManagerProtocolHandler {
    /// Creates a handler and the actor-side request receiver.
    pub fn channel(capacity: NonZeroUsize) -> (Self, ManagerRequestReceiver) {
        let (requests, receiver) = mpsc::channel(capacity.get());
        (Self { requests }, ManagerRequestReceiver { receiver })
    }

    /// Registers this handler under `LightningVEND`'s stable manager ALPN.
    pub fn register(self, builder: MachineBuilder) -> anyhow::Result<MachineBuilder> {
        builder.accept_manager_protocol(MANAGER_ALPN, self)
    }
}

impl ProtocolHandler for ManagerProtocolHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.handle_connection(connection)
            .await
            .map_err(AcceptError::from_err)
    }
}

impl ManagerProtocolHandler {
    async fn handle_connection(&self, connection: Connection) -> Result<(), TransportError> {
        let remote_id = connection.remote_id();
        let (mut send, mut receive) = connection.accept_bi().await?;
        let wire: WireRequest = read_frame(&mut receive).await?;

        let response = if wire.version == MANAGER_PROTOCOL_VERSION {
            let (response, response_receiver) = oneshot::channel();
            self.requests
                .send(IncomingManagerRequest {
                    remote_id,
                    request: wire.request,
                    response,
                })
                .await
                .map_err(|_| TransportError::KioskUnavailable)?;
            response_receiver
                .await
                .map_err(|_| TransportError::KioskUnavailable)?
        } else {
            ManagerResponse::ProtocolError {
                message: format!(
                    "unsupported manager protocol version {}; expected {}",
                    wire.version, MANAGER_PROTOCOL_VERSION
                ),
            }
        };

        write_frame(&mut send, &WireResponse::new(response)).await?;
        send.finish()?;
        send.stopped().await?;
        Ok(())
    }
}

/// Sends one command/query over the claimed manager's Vendimint identity.
pub async fn request(
    manager: &Manager,
    machine_id: &EndpointId,
    request: ManagerRequest,
) -> anyhow::Result<ManagerResponse> {
    let connection = manager.connect_machine(machine_id, MANAGER_ALPN).await?;
    let (mut send, mut receive) = connection.open_bi().await?;
    write_frame(&mut send, &WireRequest::new(request)).await?;
    send.finish()?;
    let response: WireResponse = read_frame(&mut receive).await?;
    let mut trailing = [0_u8; 1];
    if AsyncReadExt::read(&mut receive, &mut trailing).await? != 0 {
        return Err(TransportError::TrailingData.into());
    }
    if response.version != MANAGER_PROTOCOL_VERSION {
        anyhow::bail!(
            "kiosk returned manager protocol version {}; expected {}",
            response.version,
            MANAGER_PROTOCOL_VERSION
        );
    }
    connection.close(0_u32.into(), b"request complete");
    Ok(response.response)
}

async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin + Send,
    T: Serialize + Sync,
{
    let payload = serde_json::to_vec(value)?;
    let length = u32::try_from(payload.len()).map_err(|_| TransportError::FrameTooLarge {
        actual: payload.len(),
        maximum: MAX_FRAME_BYTES,
    })?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge {
            actual: payload.len(),
            maximum: MAX_FRAME_BYTES,
        });
    }
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R, T>(reader: &mut R) -> Result<T, TransportError>
where
    R: AsyncRead + Unpin + Send,
    T: DeserializeOwned + Send,
{
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge {
            actual: length,
            maximum: MAX_FRAME_BYTES,
        });
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

#[derive(Debug, Error)]
enum TransportError {
    #[error("manager RPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("manager RPC connection failed: {0}")]
    Connection(#[from] iroh::endpoint::ConnectionError),
    #[error("manager RPC stream closed before completion: {0}")]
    ClosedStream(#[from] iroh::endpoint::ClosedStream),
    #[error("manager RPC response was not acknowledged: {0}")]
    Stopped(#[from] iroh::endpoint::StoppedError),
    #[error("manager RPC frame could not be encoded or decoded: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manager RPC frame is {actual} bytes; maximum is {maximum}")]
    FrameTooLarge { actual: usize, maximum: usize },
    #[error("manager RPC response contained data after its frame")]
    TrailingData,
    #[error("the kiosk state actor is unavailable")]
    KioskUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_preserve_protocol_version() {
        let request = WireRequest::new(ManagerRequest::GetSnapshot);
        let (mut client, mut server) = tokio::io::duplex(1024);
        let writer = tokio::spawn(async move { write_frame(&mut client, &request).await });
        let decoded: WireRequest = read_frame(&mut server).await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(decoded, WireRequest::new(ManagerRequest::GetSnapshot));
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocation() {
        let (mut client, mut server) = tokio::io::duplex(16);
        client
            .write_all(&u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_be_bytes())
            .await
            .unwrap();
        let error = read_frame::<_, WireRequest>(&mut server).await.unwrap_err();
        assert!(matches!(error, TransportError::FrameTooLarge { .. }));
    }
}
