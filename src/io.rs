use std::net::SocketAddr;

use flume::{Receiver, Sender};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::tracker::{parse_and_validate_message, proto::Message, Error};

pub mod quic;

pub type ConnId = usize;

#[derive(Debug)]
pub struct PacketIn {
    pub conn_id: ConnId,
    pub message: Message,
}
impl PacketIn {
    pub fn new(from: ConnId, message: Message) -> Self {
        Self {
            conn_id: from,
            message,
        }
    }
}

pub type FromConn = Sender<ConnEvent>;

#[derive(Debug)]
pub enum ConnEvent {
    Open(ConnHandle),
    Packet(PacketIn),
    Close(ConnId, SocketAddr),
}

#[derive(Debug)]
pub struct ConnHandle {
    pub addr: SocketAddr,
    pub conn_id: ConnId,
    out_tx: Sender<Message>,
}

impl ConnHandle {
    pub fn new(conn_id: ConnId, addr: SocketAddr) -> (Self, Receiver<Message>) {
        let (out_tx, out_rx) = flume::bounded(16);
        let handle = Self {
            conn_id,
            addr,
            out_tx,
        };
        (handle, out_rx)
    }

    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        self.out_tx.send_async(message).await?;
        Ok(())
    }
}

pub async fn read_message<R: AsyncRead + Send + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<Message>, Error> {
    let len = reader.read_u32().await? as usize;
    if len == 0 {
        return Ok(None);
    }
    if len > buf.len() {
        buf.resize(len, 0u8);
    }
    reader.read_exact(&mut buf[..len]).await?;
    let message = parse_and_validate_message(&buf[..len])?;
    Ok(Some(message))
}

pub async fn write_message<W: AsyncWrite + Send + Unpin>(
    writer: &mut W,
    message: &Message,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let len = postcard::experimental::serialized_size(&message)?;
    if len > buf.len() {
        buf.resize(len, 0u8);
    }
    let bytes = postcard::to_slice(&message, buf)?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    Ok(())
}

