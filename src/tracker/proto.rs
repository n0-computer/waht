// use ed25519_dalek::{Signature, Signer, VerifyingKey};

use std::fmt;

use super::*;

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub version: u16,
    pub from: PeerId,
    pub payload: Payload,
}

impl Message {
    pub fn new(from: PeerId, payload: Payload) -> Self {
        Self {
            version: VERSION,
            from,
            payload,
        }
    }
    pub fn error(from: PeerId, request_id: Option<u64>, error: RpcError) -> Self {
        Message::new(from, Payload::Error(ErrorMessage { request_id, error }))
    }
    pub fn request(from: PeerId, request: Request) -> Self {
        Message::new(from, Payload::Request(request))
    }
    pub fn response(from: PeerId, response: Response) -> Self {
        Message::new(from, Payload::Response(response))
    }

    pub fn as_response(&self) -> Option<&Response> {
        match &self.payload {
            Payload::Response(response) => Some(response),
            _ => None,
        }
    }
    pub fn into_response(self) -> Option<Response> {
        match self.payload {
            Payload::Response(response) => Some(response),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Payload {
    Request(Request),
    Response(Response),
    Error(ErrorMessage),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorMessage {
    request_id: Option<u64>,
    error: RpcError,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcError {
    NeedsProtocolVersion(u16),
    InvalidSignature,
    UnsupportedOperation,
}
impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for RpcError {}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub request_id: u64,
    pub command: Command,
    pub peer_info: Option<PeerInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub request_id: u64,
    pub error: Option<RpcError>,
    pub peers: Option<Vec<PeerInfo>>,
    // pub trackers: Option<Vec<TrackerInfo>>,
    pub fin: bool,
}
impl Response {
    pub fn with_peers(request_id: u64, peers: Vec<PeerInfo>, fin: bool) -> Self {
        Self {
            request_id,
            error: None,
            peers: Some(peers),
            // trackers: None,
            fin,
        }
    }
    pub fn empty(request_id: u64) -> Self {
        Self {
            request_id,
            error: None,
            peers: None,
            // trackers: None,
            fin: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Hello(Hello),
    Ping,
    LookupPeer(LookupPeer),
    LookupKey(LookupKey),
    ProvideKey(ProvideKey),
    // LookupTopic(LookupTopic),
    // ProvideTopic(ProvideTopic),
    // ProvideValue(ProvideValue),
    // GetValue(GetValue),
    // Goodbye(Goodbye),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    // TODO: Sign your PeerId for proof
    // sig: Signature
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LookupPeer {
    pub peer_id: PeerId,
    pub topic: Option<Topic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LookupKey {
    pub key: Key,
    pub topic: Option<Topic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvideKey {
    pub key: Key,
    pub topic: Option<Key>,
}

// #[derive(Clone, Debug, Serialize, Deserialize)]
// pub struct LookupTopic {
//     pub topic: Topic,
// }
// #[derive(Clone, Debug, Serialize, Deserialize)]
// pub struct ProvideTopic {
//     pub topic: Topic,
// }
// #[derive(Debug, Serialize, Deserialize)]
// pub struct ProvideValue {
//     key: Key,
//     topic: Option<Topic>,
//     value: Option<Bytes>,
// }
// #[derive(Clone, Debug, Serialize, Deserialize)]
// pub struct Goodbye {}
