// use ed25519_dalek::{Signature, Signer, VerifyingKey};

use std::fmt;

use super::*;

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub version: u16,
    pub from: PeerId,
    pub payload: Payload,
    // pub sig: bool,
}

pub type SerializedMessage = Message;

// #[derive(Debug, Serialize, Deserialize)]
// pub struct SerializedMessage {
//     pub version: u16,
//     pub from: PeerId,
//     // pub payload: Bytes,
//     pub payload: Payload,
//     pub sig: Option<Signature>,
// }

impl Message {
    pub fn new(from: PeerId, payload: Payload) -> Self {
        Self {
            from,
            version: VERSION,
            payload,
            // sig,
        }
    }
    pub fn error(from: PeerId, request_id: Option<u64>, error: RpcError) -> Self {
        Message::new(
            from,
            Payload::Error(ErrorMessage { request_id, error }),
        )
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
            _ => None
        }

    }
    pub fn into_response(self) -> Option<Response> {
        match self.payload {
            Payload::Response(response) => Some(response),
            _ => None
        }
    }

    // pub fn decode(bytes: &[u8]) -> Result<Message, RpcError> {
    //     let message: SerializedMessage = postcard::from_bytes(bytes)?;
    //     let sig = if let Some(sig) = &message.sig {
    //         let peer_key = VerifyingKey::from_bytes(&message.from)?;
    //         peer_key.verify_strict(&message.payload, sig)?;
    //         true
    //     } else {
    //         false
    //     };
    //     let payload: Payload = postcard::from_bytes(&message.payload)?;
    //     Ok(Message {
    //         sig,
    //         payload,
    //         from: message.from,
    //         version: message.version,
    //     })
    // }
    // pub fn from_serialized(message: SerializedMessage) -> Result<Message, Error> {
    //     let sig = if let Some(sig) = &message.sig {
    //         // let peer_key = VerifyingKey::from_bytes(&message.from)?;
    //         // peer_key.verify_strict(&message.payload, sig)?;
    //         true
    //     } else {
    //         false
    //     };
    //     // let payload: Payload = postcard::from_bytes(&message.payload)?;
    //     Ok(Message {
    //         sig,
    //         payload: message.payload,
    //         from: message.from,
    //         version: message.version,
    //     })
    // }
    //
    // pub fn into_signed(self, key: &SigningKey) -> Result<SerializedMessage, Error> {
    //     // let payload = postcard::to_stdvec(&self.payload)?;
    //     Ok(SerializedMessage {
    //         version: self.version,
    //         from: self.from,
    //         // sig: self.sig.then(|| key.sign(&payload)),
    //         sig: self.sig.then(|| Signature::from_bytes(&[0u8; 64])),
    //         // payload: payload.into(),
    //         payload: self.payload
    //     })
    // }
    //
    // pub fn into_unsigned(self) -> Result<SerializedMessage, Error> {
    //     if self.sig {
    //         return Err(Error::MissingSignature);
    //     }
    //     // let payload = postcard::to_stdvec(&self.payload)?;
    //     Ok(SerializedMessage {
    //         version: self.version,
    //         from: self.from,
    //         sig: None,
    //         // payload: payload.into(),
    //         payload: self.payload
    //     })
    // }

    // pub fn encode(self, key: &SigningKey) -> Result<Bytes, EncodingError> {
    //     // TODO: Remove allocations and encode into buffer
    //     let bytes = postcard::to_stdvec(&self.into_signed(key)?)?;
    //     Ok(bytes.into())
    // }
}
// impl From<Request> for Message {
//     fn from(request: Request) -> Self {
//         Message::new(Payload::Request(request))
//     }
// }
// impl From<Response> for Message {
//     fn from(response: Response) -> Self {
//         Message::new(Payload::Response(response))
//     }
// }

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
    pub trackers: Option<Vec<TrackerInfo>>,
    pub fin: bool
}
impl Response {
    // fn with_error(request_id: u64, error: RpcError) -> Self {
    //     Self {
    //         request_id,
    //         error: Some(error),
    //         peers: None,
    //         trackers: None,
    //         fin: true
    //     }
    // }
    pub fn with_peers(request_id: u64, peers: Vec<PeerInfo>, fin: bool) -> Self {
        Self {
            request_id,
            error: None,
            peers: Some(peers),
            trackers: None,
            fin
        }
    }
    pub fn empty(request_id: u64) -> Self {
        Self {
            request_id,
            error: None,
            peers: None,
            trackers: None,
            fin: true
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Hello(Hello),
    Ping,
    LookupTopic(LookupTopic),
    LookupKey(LookupKey),
    ProvideTopic(ProvideTopic),
    ProvideKey(ProvideKey),
    // ProvideValue(ProvideValue),
    // GetValue(GetValue),
    Goodbye(Goodbye),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    // TODO: Sign your PeerId for proof
    // sig: Signature
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LookupKey {
    pub key: Key,
    pub topic: Option<Topic>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LookupTopic {
    pub topic: Topic,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvideTopic {
    pub topic: Topic,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProvideKey {
    pub key: Key,
    pub topic: Option<Key>,
}

// #[derive(Debug, Serialize, Deserialize)]
// pub struct ProvideValue {
//     key: Key,
//     topic: Option<Topic>,
//     value: Option<Bytes>,
// }

#[derive(Debug, Serialize, Deserialize)]
pub struct Goodbye {}

