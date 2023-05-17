use serde::{Deserialize, Serialize};
use std::{io, net::SocketAddr, time::Duration};

use self::proto::{Message, Response};

pub mod client;
pub mod proto;
pub mod server;

pub const VERSION: u16 = 1;
pub const MAX_LEN: usize = 1200;
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

pub type Key = [u8; 32];
pub type Topic = [u8; 32];
pub type PeerId = [u8; 32];
pub type RequestId = u64;

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct PeerInfo {
    pub use_from_addr: bool,
    pub addrs: Vec<SocketAddr>,
    pub relay: Option<SocketAddr>,
}
impl Default for PeerInfo {
    fn default() -> Self {
        Self {
            use_from_addr: true,
            addrs: vec![],
            relay: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TrackerInfo {
    addr: SocketAddr,
    // addrs: Vec<SocketAddr>,
    peer_id: Option<PeerId>,
}
impl From<SocketAddr> for TrackerInfo {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            // addrs: vec![addr],
            peer_id: None,
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("PeerID does not match signing key")]
    WrongPeerId,
    #[error("Codec error: {0}")]
    Codec(#[from] postcard::Error),
    #[error("Message may not be sent unsigned")]
    MissingSignature,
    #[error("Unsupported operation")]
    UnsupportedOperation,
    #[error("Invalid message")]
    InvalidMessage,
    #[error("Invalid Signature")]
    InvalidSignature(#[from] ed25519_dalek::SignatureError),
    #[error("Request timed out")]
    Timeout,
    #[error("Message too long")]
    MessageTooLong,
    #[error("IO error: {0}")]
    IO(#[from] io::Error),
    #[error("Protocol error: {0}")]
    Proto(#[from] proto::RpcError),
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Unsupported command")]
    UnsupportedCommand,
    #[error("Bad encoding: {0}")]
    BadEncoding(#[from] postcard::Error),
}

// #[derive(Debug)]
// pub struct TrackerState {
//     info: TrackerInfo,
//     last_seen: Instant,
// }
// pub enum NodeStatus {
//     Good,
//     Questionable,
//     Bad,
// }
//
// impl From<TrackerInfo> for TrackerState {
//     fn from(info: TrackerInfo) -> Self {
//         Self {
//             info,
//             last_seen: Instant::now(),
//         }
//     }
// }

#[derive(Debug)]
pub enum Event {
    Packet(SocketAddr, Message),
    Response(Response),
}

pub fn parse_and_validate_message(bytes: &[u8]) -> Result<Message, Error> {
    let message: Message = postcard::from_bytes(&bytes[..])?;
    validate_message(&message)?;
    Ok(message)
}

pub fn validate_message(message: &Message) -> Result<(), proto::RpcError> {
    if message.version != VERSION {
        return Err(proto::RpcError::NeedsProtocolVersion(VERSION));
    }
    Ok(())
}
