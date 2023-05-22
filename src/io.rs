use std::net::SocketAddr;

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc::{self, Receiver, Sender},
};
use tokio_util::sync::CancellationToken;

use crate::tracker::{parse_and_validate_message, proto::Message, Error};

pub mod quic;

pub const TO_CONN_CHANNEL_CAP: usize = 8;
pub const FROM_CONN_CHANNEL_CAP: usize = 8;
pub const MAX_MESSAGE_LEN: usize = 1024 * 64;

pub type ConnId = usize;

#[derive(Debug)]
pub enum ConnEvent {
    Open(ConnId, ConnHandle),
    Packet(ConnId, Message),
    Close(ConnId),
}

#[derive(Debug)]
pub struct ConnHandle {
    conn_id: ConnId,
    remote_address: SocketAddr,
    out_tx: Sender<Message>,
    cancel: CancellationToken,
}

impl ConnHandle {
    pub fn new(
        conn_id: ConnId,
        remote_address: SocketAddr,
    ) -> (Self, Receiver<Message>, CancellationToken) {
        let (out_tx, out_rx) = mpsc::channel(TO_CONN_CHANNEL_CAP);
        let cancel = CancellationToken::new();
        let handle = Self {
            conn_id,
            remote_address,
            out_tx,
            cancel: cancel.clone(),
        };
        (handle, out_rx, cancel)
    }

    pub fn conn_id(&self) -> usize {
        self.conn_id
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.remote_address
    }

    pub fn cancel(self) {
        self.cancel.cancel()
    }

    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        self.out_tx.send(message).await?;
        Ok(())
    }
}

pub async fn read_message<R: AsyncRead + Send + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Message, Error> {
    let len = reader.read_u32().await? as usize;
    if len == 0 {
        return Err(Error::InvalidMessage);
    }
    if len > MAX_MESSAGE_LEN {
        return Err(Error::IncomingMessageTooLong);
    }
    if len > buf.len() {
        buf.resize(len, 0u8);
    }
    reader.read_exact(&mut buf[..len]).await?;
    let message = parse_and_validate_message(&buf[..len])?;
    Ok(message)
}

pub async fn write_message<W: AsyncWrite + Send + Unpin>(
    writer: &mut W,
    message: &Message,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let len = postcard::experimental::serialized_size(&message)?;
    if len > MAX_MESSAGE_LEN {
        return Err(Error::OutgoingMessageTooLong);
    }
    if len > buf.len() {
        buf.resize(len, 0u8);
    }
    let bytes = postcard::to_slice(&message, buf)?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    Ok(())
}
