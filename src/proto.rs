use self::crypto::VerificationError;

pub type Timestamp = u64;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("unknown frame type {0}")]
    UnknownFrame(u16),
    #[error("unexpected frame type for stream")]
    InvalidFrameType,
    #[error("frame incomplete, missing {0} bytes")]
    Incomplete(usize),
    #[error("frame malformed: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("failed to verify: {0}")]
    Verification(#[from] VerificationError),
}

pub mod traits {
    use super::FrameError;
    use serde::{de::DeserializeOwned, Serialize};

    pub trait Encode: Serialize {
        fn encode(&self, buf: &mut [u8]) -> Result<usize, FrameError> {
            Ok(postcard::to_slice(&self, buf)?.len())
        }
        fn encoded_len(&self) -> Result<usize, FrameError> {
            Ok(postcard::experimental::serialized_size(&self)?)
        }
    }

    pub trait Decode<T: DeserializeOwned = Self> {
        fn decode(buf: &mut [u8]) -> Result<T, FrameError> {
            Ok(postcard::from_bytes(&buf)?)
        }
    }

    impl<T> Encode for T where T: Serialize {}
    impl<T> Decode for T where T: DeserializeOwned {}
}

mod util {
    use bytes::{Bytes, BytesMut};
    use serde::{de::DeserializeOwned, Deserialize, Serialize};
    use std::marker::PhantomData;

    use super::{traits::Encode, FrameError};

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(from = "Bytes", into = "Bytes", bound(serialize = "T: Clone"))]
    pub struct TypedBytes<T> {
        bytes: Bytes,
        typ: PhantomData<T>,
    }
    impl<T: Clone> Clone for TypedBytes<T> {
        fn clone(&self) -> Self {
            Self {
                bytes: self.bytes.clone(),
                typ: PhantomData,
            }
        }
    }
    impl<T> TypedBytes<T> {
        pub fn new<U: Into<Bytes>>(bytes: U) -> Self {
            Into::<Bytes>::into(bytes).into()
        }
        pub fn as_bytes(&self) -> &[u8] {
            &self.bytes
        }
        pub fn into_bytes(self) -> Bytes {
            self.bytes
        }
    }

    impl<T> From<Bytes> for TypedBytes<T> {
        fn from(bytes: Bytes) -> Self {
            Self { bytes, typ: PhantomData }
        }
    }

    impl<T> From<TypedBytes<T>> for Bytes {
        fn from(value: TypedBytes<T>) -> Self {
            value.bytes
        }
    }
    impl<T: Serialize> TypedBytes<T> {
        pub fn encode(value: &T) -> Result<Self, FrameError> {
            Self::encode_into(BytesMut::new(), value)
        }
        pub fn encode_into(mut bytes: BytesMut, value: &T) -> Result<Self, FrameError> {
            bytes.resize(value.encoded_len()?, 0u8);
            value.encode(&mut bytes)?;
            Ok(Self {
                bytes: bytes.freeze(),
                typ: PhantomData,
            })
        }
    }
    impl<T: DeserializeOwned> TypedBytes<T> {
        pub fn decode(&self) -> Result<T, FrameError> {
            Ok(postcard::from_bytes(&self.as_bytes())?)
        }
    }
}

pub mod crypto {
    use bytes::Bytes;
    use ed25519_dalek::{Signature, SignatureError, Signer, SigningKey};
    use serde::{de::DeserializeOwned, Deserialize, Serialize};

    use super::{util::TypedBytes, FrameError};

    pub type PublicKey = [u8; 32];

    #[derive(Debug, thiserror::Error)]
    pub enum VerificationError {
        #[error("Signature error: {0}")]
        BadEdSignature(#[from] SignatureError),
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum Proof {
        EdSignature(Signature),
    }

    impl Proof {
        pub fn verify(&self, issuer: &[u8; 32], payload: &[u8]) -> Result<bool, VerificationError> {
            match self {
                Self::EdSignature(signature) => {
                    let key = ed25519_dalek::VerifyingKey::from_bytes(&issuer)?;
                    key.verify_strict(&payload, &signature)?;
                    Ok(true)
                }
            }
        }
        pub fn sign(issuer: &SigningKey, payload: &[u8]) -> (PublicKey, Self) {
            let signature = issuer.sign(&payload);
            (issuer.verifying_key().to_bytes(), Self::EdSignature(signature))
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(bound(serialize = "T: Clone"))]
    pub struct SealedEnvelope<T> {
        issuer: PublicKey,
        proof: Proof,
        payload: TypedBytes<T>,
    }

    impl<T: Serialize + DeserializeOwned> SealedEnvelope<T> {
        pub fn sign_and_encode(issuer: &SigningKey, payload: &T) -> Result<Self, FrameError> {
            let payload = TypedBytes::encode(payload)?;
            let (principal, proof) = Proof::sign(issuer, payload.as_bytes());
            Ok(Self {
                issuer: principal,
                payload,
                proof,
            })
        }
        pub fn verify_and_decode(self) -> Result<VerifiedEnvelope<T>, FrameError> {
            self.proof.verify(&self.issuer, &self.payload.as_bytes())?;
            let payload = self.payload.decode()?;
            Ok(VerifiedEnvelope {
                issuer: self.issuer,
                payload,
                proof: self.proof,
            })
        }
    }

    #[derive(Debug, Clone)]
    pub struct VerifiedEnvelope<T = Bytes> {
        pub issuer: PublicKey,
        pub payload: T,
        pub proof: Proof,
    }

    impl<T> VerifiedEnvelope<T> {
        pub fn new(payload: T, issuer: PublicKey, proof: Proof) -> Self {
            Self {
                issuer,
                payload,
                proof,
            }
        }
        pub fn split(self) -> (T, PublicKey, Proof) {
            (self.payload, self.issuer, self.proof)
        }

        pub fn map<U, F: FnOnce(T) -> U>(self, map: F) -> VerifiedEnvelope<U> {
            VerifiedEnvelope {
                issuer: self.issuer,
                proof: self.proof,
                payload: map(self.payload),
            }
        }
    }

    impl<T: Serialize> VerifiedEnvelope<T> {
        pub fn into_sealed(self) -> anyhow::Result<SealedEnvelope<T>> {
            let payload = TypedBytes::encode(&self.payload)?;
            Ok(SealedEnvelope {
                proof: self.proof,
                issuer: self.issuer,
                payload,
            })
        }
    }
}

pub mod data {
    use super::{
        crypto::{Proof, PublicKey},
        wire::{self, Rid, SignedFrame, UnsignedFrame},
        FrameError,
    };
    use derive_more::From;

    pub trait FrameType: Sized + Unpin + Send + 'static {
        fn from_payload(frame: DataFramePayload) -> Result<Self, FrameError>;
        fn from_frame(frame: DataFrame) -> Result<(Self, FrameMeta), FrameError> {
            let payload = Self::from_payload(frame.payload)?;
            Ok((payload, frame.meta))
        }
        fn into_payload(self) -> DataFramePayload;
    }

    #[derive(Debug)]
    pub struct ConnId(pub u64);

    #[derive(Debug)]
    pub struct Authorization {
        pub issuer: PublicKey,
        pub proof: Proof,
    }
    impl From<(PublicKey, Proof)> for Authorization {
        fn from((issuer, proof): (PublicKey, Proof)) -> Self {
            Self { issuer, proof }
        }
    }

    #[derive(Debug)]
    pub struct FrameMeta {
        pub rid: Option<Rid>,
        pub authorization: Option<Authorization>,
    }

    #[derive(Debug)]
    pub struct DataFrame {
        pub meta: FrameMeta,
        pub payload: DataFramePayload,
    }

    #[derive(Debug, From)]
    pub enum DataFramePayload {
        Hello(wire::Hello),
        Announce(wire::Announcement),
        // Unannounce(wire::Unannouncement),
        Query(wire::Query),
        Ack(wire::Ack),
        End(wire::End),
        Error(wire::Error),
    }

    #[derive(Debug, From)]
    pub enum Request {
        Control(ControlMessage),
        Announce(AnnounceRequest),
        Query(QueryRequest)
    }

    #[derive(Debug, From)]
    pub enum Response {
        Control(ControlMessage),
        Announce(AnnounceResponse),
        Query(QueryResponse)
    }

    #[derive(Debug, From)]
    pub enum AnnounceRequest {
        Announce(wire::Announcement),
        // Unannounce(wire::Unannouncement),
        End(wire::End),
    }

    #[derive(Debug, From)]
    pub enum AnnounceResponse {
        Ack(wire::Ack),
        End(wire::End),
    }

    #[derive(Debug, From)]
    pub enum QueryRequest {
        Query(wire::Query),
        End(wire::End),
    }

    #[derive(Debug, From)]
    pub enum QueryResponse {
        Announce(wire::Announcement),
        // Unannounce(wire::Unannouncement),
        End(wire::End),
    }

    impl wire::Frame {
        pub fn verify_if_signed(self) -> Result<DataFrame, FrameError> {
            let (payload, authorization) = match self.payload {
                wire::FramePayload::Unsigned(frame) => {
                    let payload = match frame {
                        UnsignedFrame::Hello(payload) => payload.into(),
                        UnsignedFrame::Query(payload) => payload.into(),
                        UnsignedFrame::End(payload) => payload.into(),
                        UnsignedFrame::Error(payload) => payload.into(),
                        UnsignedFrame::Ack(payload) => payload.into(),
                    };
                    (payload, None)
                }
                wire::FramePayload::Signed(frame) => {
                    let (payload, issuer, proof) = frame.verify_and_decode()?.split();
                    let payload = match payload {
                        SignedFrame::Announce(payload) => payload.into(),
                        // SignedFrame::Unannounce(payload) => payload.into(),
                    };
                    let authorization = Authorization::from((issuer, proof));
                    (payload, Some(authorization))
                }
            };
            let meta = FrameMeta {
                rid: self.rid,
                authorization,
            };
            Ok(DataFrame { payload, meta })
        }

        pub fn verify_into<T: FrameType>(self) -> Result<(T, FrameMeta), FrameError>
        where
            T: FrameType,
        {
            let frame = self.verify_if_signed()?;
            Ok(T::from_frame(frame)?)
        }
    }

    #[derive(Debug, From)]
    pub enum ControlMessage {
        Hello(wire::Hello),
    }

    impl FrameType for ControlMessage {
        fn from_payload(frame: DataFramePayload) -> Result<Self, FrameError> {
            match frame {
                DataFramePayload::Hello(data) => Ok(data.into()),
                _ => Err(FrameError::InvalidFrameType),
            }
        }
        fn into_payload(self) -> DataFramePayload {
            match self {
                Self::Hello(payload) => DataFramePayload::Hello(payload),
            }
        }
    }

    impl FrameType for AnnounceRequest {
        fn from_payload(frame: DataFramePayload) -> Result<Self, FrameError> {
            match frame {
                DataFramePayload::Announce(data) => Ok(data.into()),
                // DataFramePayload::Unannounce(data) => Ok(data.into()),
                DataFramePayload::End(data) => Ok(data.into()),
                _ => Err(FrameError::InvalidFrameType),
            }
        }
        fn into_payload(self) -> DataFramePayload {
            match self {
                Self::Announce(payload) => payload.into(),
                Self::End(payload) => payload.into(),
            }
        }
    }

    impl FrameType for AnnounceResponse {
        fn from_payload(frame: DataFramePayload) -> Result<Self, FrameError> {
            match frame {
                DataFramePayload::Ack(data) => Ok(data.into()),
                DataFramePayload::End(data) => Ok(data.into()),
                _ => Err(FrameError::InvalidFrameType),
            }
        }
        fn into_payload(self) -> DataFramePayload {
            match self {
                Self::Ack(payload) => payload.into(),
                Self::End(payload) => payload.into(),
            }
        }
    }

    impl FrameType for QueryRequest {
        fn from_payload(frame: DataFramePayload) -> Result<Self, FrameError> {
            match frame {
                DataFramePayload::Query(data) => Ok(data.into()),
                DataFramePayload::End(data) => Ok(data.into()),
                _ => Err(FrameError::InvalidFrameType),
            }
        }
        fn into_payload(self) -> DataFramePayload {
            match self {
                Self::Query(payload) => payload.into(),
                Self::End(payload) => payload.into(),
            }
        }
    }

    impl FrameType for QueryResponse {
        fn from_payload(frame: DataFramePayload) -> Result<Self, FrameError> {
            match frame {
                DataFramePayload::Announce(data) => Ok(data.into()),
                // DataFramePayload::Unannounce(data) => Ok(data.into()),
                DataFramePayload::End(data) => Ok(data.into()),
                _ => Err(FrameError::InvalidFrameType),
            }
        }
        fn into_payload(self) -> DataFramePayload {
            match self {
                Self::Announce(payload) => payload.into(),
                Self::End(payload) => payload.into(),
            }
        }
    }
}

pub mod wire {
    //! waht wire protocol
    //!
    //! all structs in here are part of the wire protocol, therefore any change
    //! in struct fields are wire-protocol breaking changes!

    use std::net::SocketAddr;

    use derive_more::From;
    use serde::{Deserialize, Serialize};

    use crate::tracker::{Key, PeerId, Topic};

    use super::crypto::SealedEnvelope;
    use super::Timestamp;

    pub type Rid = u64;

    pub const MAX_FRAME_LEN: usize = 1024 * 4;

    #[derive(Debug, From, Serialize, Deserialize)]
    pub struct Frame {
        pub rid: Option<Rid>,
        pub payload: FramePayload,
    }

    #[derive(Debug, From, Serialize, Deserialize)]
    pub enum FramePayload {
        Unsigned(UnsignedFrame),
        Signed(SealedEnvelope<SignedFrame>),
    }

    #[derive(Debug, Clone, From, Serialize, Deserialize)]
    pub enum SignedFrame {
        Announce(Announcement),
        // Unannounce(Unannouncement),
    }

    #[derive(Debug, From, Serialize, Deserialize)]
    pub enum UnsignedFrame {
        Hello(Hello),
        Query(Query),
        Ack(Ack),
        End(End),
        Error(Error),
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Hello {
        pub version: u16,
        pub peer_id: PeerId,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Announcement {
        pub timestamp: Timestamp,
        pub payload: AnnouncePayload,
    }

    // #[derive(Debug, Clone, Serialize, Deserialize)]
    // pub struct Unannouncement {
    //     pub timestamp: Timestamp,
    //     pub payload: AnnouncePayload,
    // }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Ack {
        pub rid: Rid,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum AnnouncePayload {
        PeerInfo(PeerInfo),
        ProvideHash(ProvideHash),
        UpdateTopic(SealedEnvelope<UpdateTopic>),
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PeerInfo {
        pub addr: SocketAddr,
        pub relay: Option<SocketAddr>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ProvideHash {
        pub target: Key,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct UpdateTopic {
        pub value: Key,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Query {
        // pub rid: Rid,
        pub settings: QuerySettings,
        pub query: QueryPayload,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct QuerySettings {
        pub stop_after: u32,
        pub keepalive: bool,
        pub federate: bool,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub enum QueryPayload {
        Peers(QueryPeers),
        Keys(QueryKeys),
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct QueryPeers {
        pub peer_ids: Vec<PeerId>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct QueryKeys {
        pub keys: Vec<Key>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Redirect {
        pub server: String,
        pub topics: Vec<Topic>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct End {}

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Error {
        pub code: u32,
        pub message: Option<String>,
    }
}
