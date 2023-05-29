use std::{
    future::poll_fn,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, BufMut, BytesMut};
use derive_more::From;
use ed25519_dalek::SigningKey;
use futures::{ready, stream::FuturesUnordered, FutureExt, Stream};
use quinn::{RecvStream, SendStream, StreamId};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;
use tokio_util::{
    io::{poll_read_buf, poll_write_buf},
    sync::{CancellationToken, WaitForCancellationFutureOwned},
};

use crate::{
    io::quic::FrameIncoming,
    proto::{
        crypto::SealedEnvelope,
        data::{ControlMessage, DataFramePayload, FrameMeta, FrameType, Request, Response},
        traits::{Decode, Encode},
        wire::{Frame, FramePayload, Rid, SignedFrame},
        FrameError,
    },
    util::transpose,
};

pub type ConnId = usize;

#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub struct StreamAddr {
    pub conn_id: ConnId,
    pub stream_id: StreamId,
}
impl From<(ConnId, StreamId)> for StreamAddr {
    fn from((conn_id, stream_id): (ConnId, StreamId)) -> Self {
        Self { conn_id, stream_id }
    }
}
impl StreamAddr {
    fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    fn conn_id(&self) -> ConnId {
        self.conn_id
    }
}

pub type BiStream = (SendStream, RecvStream);
pub type FrameBiStream = (FrameSendStream, FrameRecvStream);
pub type ControlRecvStream = MappedRecvStream<ControlMessage>;
pub type ControlBiStream = (FrameSendStream, MappedRecvStream<ControlMessage>);

pub mod accept {
    use super::{FrameSendStream, MappedRecvStream, MappedSendStream};
    use crate::proto::data::{AnnounceRequest, AnnounceResponse, QueryRequest, QueryResponse};

    pub type AnnounceRecvStream = MappedRecvStream<AnnounceRequest>;
    // pub type AnnounceSendStream = MappedSendStream<AnnounceResponse>;
    pub type AnnounceSendStream = FrameSendStream;

    pub type QueryRecvStream = MappedRecvStream<QueryRequest>;
    // pub type QuerySendStream = MappedSendStream<QueryResponse>;
    pub type QuerySendStream = FrameSendStream;

    pub type AnnounceBiStream = (AnnounceSendStream, AnnounceRecvStream);
    pub type QueryBiStream = (QuerySendStream, QueryRecvStream);
}
pub mod open {
    use super::{FrameSendStream, MappedRecvStream, MappedSendStream};
    use crate::proto::data::{AnnounceRequest, AnnounceResponse, QueryRequest, QueryResponse};

    pub type AnnounceRecvStream = MappedRecvStream<AnnounceResponse>;
    pub type AnnounceSendStream = MappedSendStream<AnnounceRequest>;
    pub type QueryRecvStream = MappedRecvStream<QueryResponse>;
    pub type QuerySendStream = MappedSendStream<QueryRequest>;
    // pub type AnnounceBiStream = (AnnounceSendStream, AnnounceRecvStream);
    // pub type QueryBiStream = (QuerySendStream, QueryRecvStream);
    pub type AnnounceBiStream = (FrameSendStream, AnnounceRecvStream);
    pub type QueryBiStream = (FrameSendStream, QueryRecvStream);
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("Unexpected stream type")]
    InvalidStreamType,
    #[error("Connection error: {0}")]
    ConnectionError(#[from] quinn::ConnectionError),
    #[error("Write error: {0}")]
    WriteError(#[from] quinn::WriteError),
    #[error("Read error: {0}")]
    ReadError(#[from] quinn::ReadExactError),
    #[error("Bad frame: {0}")]
    BadFrame(#[from] FrameError),
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Failed to write header")]
    FailedToWriteHeader,
    #[error("Stream is not ready for sending")]
    NotReadyForSending,
    #[error("The stream was closed")]
    StreamClosed,
    #[error("Invalid connection")]
    InvalidConnection,
}

#[derive(Eq, PartialEq, Debug, Serialize, Deserialize, Copy, Clone)]
pub enum StreamType {
    Control,
    Announce,
    Query,
}

#[derive(Debug, From)]
pub enum AcceptStream {
    Control(ControlRecvStream),
    Announce(accept::AnnounceBiStream),
    Query(accept::QueryBiStream),
}

impl AcceptStream {
    pub fn split(self) -> (Option<AcceptStreamSend>, AcceptStreamRecv) {
        match self {
            AcceptStream::Control(recv) => (None, AcceptStreamRecv::Control(recv)),
            AcceptStream::Announce((send, recv)) => (
                Some(AcceptStreamSend::Announce(send)),
                AcceptStreamRecv::Announce(recv),
            ),
            AcceptStream::Query((send, recv)) => (
                Some(AcceptStreamSend::Query(send)),
                AcceptStreamRecv::Query(recv),
            ),
        }
    }
}

#[derive(Debug, From)]
pub enum AcceptStreamRecv {
    Control(ControlRecvStream),
    Announce(accept::AnnounceRecvStream),
    Query(accept::QueryRecvStream),
}

#[derive(Debug)]
pub enum AcceptStreamSend {
    Announce(accept::AnnounceSendStream),
    Query(accept::QuerySendStream),
}

impl AcceptStream {
    pub fn addr(&self) -> StreamAddr {
        match self {
            Self::Control(recv) => recv.addr(),
            Self::Announce((_send, recv)) => recv.addr(),
            Self::Query((_send, recv)) => recv.addr(),
        }
    }
}

#[derive(Debug, From)]
pub enum OpenStream {
    Control(FrameSendStream),
    Announce(open::AnnounceBiStream),
    Query(open::QueryBiStream),
}

pub async fn accept_uni(
    conn: &quinn::Connection,
    cancel: Option<CancellationToken>,
) -> Result<AcceptStream, StreamError> {
    let mut recv = conn.accept_uni().await?;
    let typ = read_stream_type(&mut recv).await?;
    let mut recv = FrameRecvStream::new(recv, conn.stable_id());
    recv.set_cancellation_token(cancel);
    match typ {
        StreamType::Control => Ok(AcceptStream::Control(recv.map())),
        StreamType::Announce | StreamType::Query => Err(StreamError::InvalidStreamType),
    }
}

pub async fn accept_bi(
    conn: &quinn::Connection,
    cancel: Option<CancellationToken>,
) -> Result<AcceptStream, StreamError> {
    let (send, mut recv) = conn.accept_bi().await?;
    let typ = read_stream_type(&mut recv).await?;
    let mut send = FrameSendStream::new(send, typ, conn.stable_id(), false);
    send.set_cancellation_token(cancel.clone());
    let mut recv = FrameRecvStream::new(recv, conn.stable_id());
    recv.set_cancellation_token(cancel);
    match typ {
        StreamType::Control => Err(StreamError::InvalidStreamType),
        StreamType::Announce => Ok(AcceptStream::Announce((send, recv.map()))),
        StreamType::Query => Ok(AcceptStream::Query((send, recv.map()))),
    }
}

pub async fn open(conn: quinn::Connection, typ: StreamType) -> Result<OpenStream, StreamError> {
    Ok(match typ {
        StreamType::Control => OpenStream::Control(open_control(conn).await?),
        StreamType::Announce => OpenStream::Announce(open_announce(conn).await?),
        StreamType::Query => OpenStream::Query(open_query(conn).await?),
    })
}

pub async fn open_control(conn: quinn::Connection) -> Result<FrameSendStream, StreamError> {
    let send = conn.open_uni().await?;
    let send = FrameSendStream::new(send, StreamType::Control, conn.stable_id(), true);
    Ok(send)
}

pub async fn open_announce(conn: quinn::Connection) -> Result<open::AnnounceBiStream, StreamError> {
    let (send, recv) = conn.open_bi().await?;
    let send = FrameSendStream::new(send, StreamType::Announce, conn.stable_id(), true);
    let recv = FrameRecvStream::new(recv, conn.stable_id());
    Ok((send, recv.map()))
}

pub async fn open_query(conn: quinn::Connection) -> Result<open::QueryBiStream, StreamError> {
    let (send, recv) = conn.open_bi().await?;
    let send = FrameSendStream::new(send, StreamType::Query, conn.stable_id(), true);
    let recv = FrameRecvStream::new(recv, conn.stable_id());
    Ok((send, recv.map()))
}

async fn read_stream_type(stream: &mut RecvStream) -> Result<StreamType, StreamError> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?;
    Ok(StreamType::decode(&mut buf)?)
}

pub struct RecvStreams<T> {
    futs: FuturesUnordered<NextFrame<T>>,
}

impl<T: FrameType> Default for RecvStreams<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: FrameType> RecvStreams<T> {
    pub fn new() -> Self {
        Self {
            futs: Default::default(),
        }
    }

    pub fn push_stream(&mut self, recv: MappedRecvStream<T>) {
        self.futs.push(next_frame(recv.addr(), recv));
    }

    pub fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<(T, FrameMeta, StreamAddr), StreamError>>> {
        loop {
            let res = ready!(Pin::new(&mut self.futs).poll_next(cx));
            return match res {
                // The FuturesUnordered is empty
                None => Poll::Ready(None),
                Some((addr, stream, frame)) => match frame {
                    // We got a frame from the stream. Return the frame and enqueue the future for
                    // the next frame.
                    Ok(Some((payload, meta))) => {
                        self.futs.push(next_frame(addr, stream));
                        Poll::Ready(Some(Ok((payload, meta, addr))))
                    }
                    // The recv stream ended or was cancelled. We continue without re-inserting the stream,
                    // effectively dropping the RecvStream.
                    Ok(None) => continue,
                    // On error the stream is removed as well, but the error is returned.
                    Err(err) => Poll::Ready(Some(Err(err.into()))),
                },
            };
        }
    }
}
impl<T: FrameType + Into<Request>> RecvStreams<T> {
    pub fn poll_next_as_request(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<FrameIncoming<Request>, StreamError>>> {
        match ready!(self.poll_next(cx)) {
            None => Poll::Ready(None),
            Some(res) => {
                let res = res?;
                let (frame, meta, addr) = res;
                Poll::Ready(Some(Ok(FrameIncoming::new(frame.into(), meta, addr))))
            }
        }
    }
}

impl<T: FrameType + Into<Response>> RecvStreams<T> {
    pub fn poll_next_as_response(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<FrameIncoming<Response>, StreamError>>> {
        match ready!(self.poll_next(cx)) {
            None => Poll::Ready(None),
            Some(res) => {
                let res = res?;
                let (frame, meta, addr) = res;
                Poll::Ready(Some(Ok(FrameIncoming::new(frame.into(), meta, addr))))
            }
        }
    }
}
impl<T: FrameType> Stream for RecvStreams<T> {
    type Item = Result<(T, FrameMeta, StreamAddr), StreamError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        RecvStreams::poll_next(self.get_mut(), cx)
    }
}

pub struct NextFrame<T> {
    addr: StreamAddr,
    stream: Option<MappedRecvStream<T>>,
}

pub fn next_frame<T: FrameType>(addr: StreamAddr, stream: MappedRecvStream<T>) -> NextFrame<T> {
    NextFrame {
        addr,
        stream: Some(stream),
    }
}

impl<T: FrameType> Future for NextFrame<T> {
    type Output = (
        StreamAddr,
        MappedRecvStream<T>,
        Result<Option<(T, FrameMeta)>, StreamError>,
    );
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut stream = self
            .stream
            .take()
            .expect("May not poll this future more than once");
        match Pin::new(&mut stream).poll_recv(cx) {
            Poll::Pending => {
                self.stream = Some(stream);
                Poll::Pending
            }
            Poll::Ready(res) => Poll::Ready((self.addr, stream, res)),
        }
    }
}

#[derive(Debug)]
pub struct MappedRecvStream<T> {
    stream: FrameRecvStream,
    typ: PhantomData<T>,
}

impl<T: FrameType> MappedRecvStream<T> {
    pub fn new(stream: FrameRecvStream, cancel: Option<CancellationToken>) -> Self {
        Self {
            stream,
            typ: PhantomData,
        }
    }

    pub fn set_cancellation_token(&mut self, cancel: Option<CancellationToken>) {
        self.stream.set_cancellation_token(cancel)
    }

    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    pub fn addr(&self) -> StreamAddr {
        self.stream.addr()
    }

    pub fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<(T, FrameMeta)>, StreamError>> {
        let frame = ready!(Pin::new(&mut self.stream).poll_recv(cx))?;
        Poll::Ready(Ok(match frame {
            None => None,
            Some(frame) => {
                let (payload, meta) = frame.verify_into()?;
                Some((payload, meta))
            }
        }))
    }

    pub async fn recv(&mut self) -> Result<Option<(T, FrameMeta)>, StreamError> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }
}

impl<T: FrameType> Stream for MappedRecvStream<T> {
    type Item = Result<(T, FrameMeta), StreamError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_recv(cx).map(transpose)
    }
}

#[derive(Debug)]
pub struct FrameRecvStream {
    conn_id: ConnId,
    stream: quinn::RecvStream,
    buf: BytesMut, // bufs: BufList<Bytes>, // bufs: VecDeque<Bytes>
    len: Option<usize>,
    cancel: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
}

impl FrameRecvStream {
    pub fn new(stream: quinn::RecvStream, conn_id: ConnId) -> Self {
        Self {
            conn_id,
            stream,
            buf: BytesMut::with_capacity(4),
            len: None,
            cancel: None,
        }
    }

    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    pub fn addr(&self) -> StreamAddr {
        (self.conn_id, self.stream.id()).into()
    }

    pub fn map<T: FrameType>(self) -> MappedRecvStream<T> {
        MappedRecvStream::new(self, None)
    }

    pub fn set_cancellation_token(&mut self, cancel: Option<CancellationToken>) {
        self.cancel = cancel.map(|cancel| Box::pin(cancel.cancelled_owned()));
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Frame>, StreamError>> {
        loop {
            if let Some(cancel) = self.cancel.as_mut() {
                if let Poll::Ready(()) = cancel.poll_unpin(cx) {
                    // TODO: Support passing an error code throught the cancellation?
                    let _ = self.stream.stop(0u32.into());
                    return Poll::Ready(Ok(None));
                }
            }
            let n = ready!(poll_read_buf(Pin::new(&mut self.stream), cx, &mut self.buf))?;
            if n == 0 {
                return Poll::Ready(Ok(None));
            }
            match self.len {
                None if self.buf.len() >= 4 => {
                    let len = self.buf.get_u32() as usize;
                    self.len = Some(len);
                    self.buf.clear();
                    self.buf.reserve(len);
                }
                Some(len) if self.buf.len() >= len => {
                    let frame = Frame::decode(&mut self.buf)?;
                    self.buf.clear();
                    self.len = None;
                    return Poll::Ready(Ok(Some(frame)));
                }
                _ => {}
            }
        }
    }

    pub async fn recv(&mut self) -> Result<Option<Frame>, StreamError> {
        futures::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
}

impl Stream for FrameRecvStream {
    type Item = Result<Frame, StreamError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_recv(cx).map(transpose)
    }
}

#[derive(Debug)]
pub struct FrameSendStream {
    conn_id: ConnId,
    stream: quinn::SendStream,
    buf: BytesMut,
    state: SendState,
    cancel: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
    cancelled: bool,
    typ: StreamType,
}

#[derive(Debug)]
enum SendState {
    Idle,
    Header([u8; 4]),
    Sending,
}

impl FrameSendStream {
    pub fn new(
        stream: quinn::SendStream,
        typ: StreamType,
        conn_id: ConnId,
        send_header: bool,
    ) -> Self {
        let state = if send_header {
            let mut header = [0u8; 4];
            typ.encode(&mut header).unwrap();
            SendState::Header(header)
        } else {
            SendState::Idle
        };
        Self {
            conn_id,
            stream,
            buf: BytesMut::new(),
            state,
            cancel: None,
            cancelled: false,
            typ,
        }
    }

    pub fn addr(&self) -> StreamAddr {
        (self.conn_id, self.stream.id()).into()
    }

    pub fn set_cancellation_token(&mut self, cancel: Option<CancellationToken>) {
        self.cancel = cancel.map(|cancel| Box::pin(cancel.cancelled_owned()));
    }

    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, SendState::Idle)
    }

    pub fn map<T: FrameType>(self) -> MappedSendStream<T> {
        MappedSendStream::new(self, None)
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<(), StreamError> {
        poll_fn(|cx| self.poll_send(cx)).await?;
        self.start_send(frame)?;
        poll_fn(|cx| self.poll_send(cx)).await?;
        Ok(())
    }

    pub fn start_send(&mut self, frame: &Frame) -> Result<(), StreamError> {
        // TODO: Can we enforce this at compile time?
        if !matches!(self.state, SendState::Idle) {
            panic!("FrameSendStream::start_send must only be called if ready to send");
            // return Err(anyhow::anyhow!("Cannot start to send: Send in progress"));
        }
        if self.cancelled {
            return Err(StreamError::StreamClosed);
        }
        self.buf.clear();
        let len = frame.encoded_len()?;
        self.buf.resize(len + 4, 0u8);
        (&mut self.buf[0..4]).put_u32(len as u32);
        frame.encode(&mut self.buf[4..])?;
        self.state = SendState::Sending;

        Ok(())
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        loop {
            if let Some(cancel) = self.cancel.as_mut() {
                if let Poll::Ready(()) = cancel.poll_unpin(cx) {
                    self.cancelled = true;
                    return Poll::Ready(Ok(()));
                }
            }
            match &mut self.state {
                SendState::Idle => return Poll::Ready(Ok(())),
                SendState::Header(header) => {
                    let n = ready!(Pin::new(&mut self.stream).poll_write(cx, header))?;
                    if n != header.len() {
                        return Poll::Ready(Err(StreamError::FailedToWriteHeader));
                    }
                    self.state = SendState::Idle;
                }
                SendState::Sending => {
                    while self.buf.has_remaining() {
                        ready!(poll_write_buf(
                            Pin::new(&mut self.stream),
                            cx,
                            &mut self.buf
                        ))?;
                    }
                    self.state = SendState::Idle;
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct MappedSendStream<T> {
    stream: FrameSendStream,
    phantom: PhantomData<T>,
}

impl<T: FrameType> MappedSendStream<T> {
    pub fn new(stream: FrameSendStream, cancel: Option<CancellationToken>) -> Self {
        Self {
            stream,
            phantom: PhantomData,
        }
    }
    pub fn id(&self) -> StreamId {
        self.stream.id()
    }

    pub fn addr(&self) -> StreamAddr {
        self.stream.addr()
    }

    pub fn set_cancellation_token(&mut self, cancel: Option<CancellationToken>) {
        self.stream.set_cancellation_token(cancel)
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        self.stream.poll_send(cx)
    }

    pub fn is_ready(&self) -> bool {
        self.stream.is_ready()
    }

    pub fn start_send(
        &mut self,
        data: T,
        rid: Option<Rid>,
        signing_key: &SigningKey,
    ) -> Result<(), StreamError> {
        let payload = data.into_payload();
        let payload: FramePayload = match payload {
            DataFramePayload::Hello(payload) => FramePayload::Unsigned(payload.into()),
            DataFramePayload::Announce(payload) => {
                let payload = SignedFrame::Announce(payload);
                let envelope = SealedEnvelope::sign_and_encode(signing_key, &payload)?;
                FramePayload::Signed(envelope)
            }
            DataFramePayload::Query(payload) => FramePayload::Unsigned(payload.into()),
            DataFramePayload::Ack(payload) => FramePayload::Unsigned(payload.into()),
            DataFramePayload::End(payload) => FramePayload::Unsigned(payload.into()),
            DataFramePayload::Error(payload) => FramePayload::Unsigned(payload.into()),
        };
        let frame = Frame { rid, payload };
        self.stream.start_send(&frame)
    }
}
