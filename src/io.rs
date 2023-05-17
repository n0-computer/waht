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

// pub fn encode_message(message: Message, buf: &mut [u8]) -> Result<&mut [u8], postcard::Error> {
//     postcard::to_slice(&message, &mut buf[..])
// }
// pub fn parse_hello(bytes: &[u8]) -> Result<Message, Error> {
//     let message = parse_and_validate_message(&bytes)?;
//     match &message.payload {
//         Payload::Request(Request {
//             command: Command::Hello(_hello),
//             ..
//         }) => {
//             // TODO: Validate a signature?
//             return Ok(message);
//         }
//         _ => {}
//     }
//     Err(Error::InvalidMessage)
// }
//
// pub async fn read_hello<R: AsyncRead + Send + Unpin>(
//     reader: &mut R,
//     buf: &mut Vec<u8>,
//     // buf: &mut [u8],
// ) -> Result<Message, Error> {
//     let message = read_message(reader, buf).await?;
//     if let Some(message) = message {
//         match &message.payload {
//             Payload::Request(Request {
//                 command: Command::Hello(_hello),
//                 ..
//             }) => {
//                 // TODO: Validate a signature?
//                 return Ok(message);
//             }
//             _ => {}
//         }
//     }
//     Err(Error::InvalidMessage)
// }
//
// pub async fn write_loop<W: AsyncWrite + Send + Unpin>(
//     out_rx: Receiver<Message>,
//     send_stream: &mut W,
//     buf: &mut Vec<u8>,
// ) -> anyhow::Result<()> {
//     loop {
//         let message = out_rx.recv_async().await?;
//         write_message(send_stream, &message, buf).await?;
//     }
// }
//
// pub async fn read_loop<R: AsyncRead + Send + Unpin>(
//     peer_id: PeerId,
//     tx: &Sender<ConnEvent>,
//     recv_stream: &mut R,
//     buf: &mut Vec<u8>,
// ) -> anyhow::Result<()> {
//     unimplemented!()
//     // while let Some(message) = read_message(recv_stream, buf).await? {
//     //     let packet = PacketIn::new(peer_id, message);
//     //     tx.send_async(ConnEvent::Packet(packet)).await?;
//     // }
//     // Ok(())
// }
