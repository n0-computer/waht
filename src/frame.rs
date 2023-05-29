use std::{
    collections::HashMap,
    future::poll_fn,
    pin::Pin,
    task::{self, Context, Poll},
};

use futures::{
    ready,
    stream::{self, BoxStream},
    StreamExt,
};
use quinn::{
    AcceptBi, AcceptUni, OpenBi, OpenUni, ReadExactError, RecvStream, SendStream, StreamId,
};
use std::future::Future;
use tokio::sync::mpsc::Sender;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::mpsc::Receiver,
};
use tokio_util::sync::ReusableBoxFuture;

use crate::tracker::proto2::frame::{Frame, FrameHeader};

#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub struct StreamAddr {
    conn_id: usize,
    stream_id: StreamId,
}
impl From<(StreamAddr, Frame)> for StreamFrame {
    fn from((stream, frame): (StreamAddr, Frame)) -> Self {
        Self {
            addr: stream,
            frame,
        }
    }
}

pub struct StreamFrame {
    addr: StreamAddr,
    frame: Frame,
}

pub struct ConnActor {
    conn: Connection,
    bistreams: HashMap<StreamAddr, FrameBiStream>,
    control_send: Option<FrameSendStream>,
    control_recv: Option<FrameRecvStream>,
    frame_out: Sender<StreamFrame>,
    frame_in: Receiver<StreamFrame>,
    parked_frame: Option<StreamFrame>,
}

impl ConnActor {
    fn stream_addr(&self, stream_id: StreamId) -> StreamAddr {
        StreamAddr {
            conn_id: self.conn.conn.stable_id(),
            stream_id,
        }
    }

    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> anyhow::Result<()> {
        while let Poll::Ready(r) = self.conn.poll_accept_bi(cx) {
            let r = r?;
            if let Some(r) = r {
                let id = self.stream_addr(r.0.id());
                let streams = (FrameSendStream::new(r.0), FrameRecvStream::new(r.1));
                self.bistreams.insert(id, streams);
            }
        }

        for (id, (send, recv)) in self.bistreams.iter_mut() {
            'recv: loop {
                let permit = self.frame_out.try_reserve();
                match permit {
                    Err(_) => break 'recv,
                    Ok(permit) => {
                        let frame = recv.poll_recv(cx)?;
                        match frame {
                            Poll::Ready(Some(frame)) => {
                                permit.send((*id, frame).into());
                            }
                            Poll::Ready(None) => todo!(),
                            Poll::Pending => break 'recv,
                        }
                    }
                }
            }

            loop {
                if let Poll::Ready(res) = send.poll_send(cx) {
                    res?;
                }
            }
        }

        while let Poll::Ready(frame) = self.frame_in.poll_recv(cx) {
            match frame {
                None => todo!(),
                Some(frame) => {
                    let addr = frame.addr;
                    let frame = frame.frame;
                }
            }
        }
        todo!()
    }

    fn try_send_frame(&mut self, frame: StreamFrame) -> Option<StreamFrame> {
        if let Some(stream) = self.bistreams.get_mut(&frame.addr) {
            match stream.0.start_send(&frame.frame) {
                Ok(_) => None,
                Err(_) => Some(frame),
            }
        } else {
            Some(frame)
        }
    }
}

/// A QUIC connection backed by Quinn
///
/// Implements a [`quic::Connection`] backed by a [`quinn::Connection`].
pub struct Connection {
    conn: quinn::Connection,
    incoming_bi: BoxStream<'static, <AcceptBi<'static> as Future>::Output>,
    opening_bi: Option<BoxStream<'static, <OpenBi<'static> as Future>::Output>>,
    incoming_uni: BoxStream<'static, <AcceptUni<'static> as Future>::Output>,
    opening_uni: Option<BoxStream<'static, <OpenUni<'static> as Future>::Output>>,
}

pub type BiStream = (SendStream, RecvStream);
pub type FrameBiStream = (FrameSendStream, FrameRecvStream);
pub type ConnError = anyhow::Error;

impl Connection {
    /// Create a [`Connection`] from a [`quinn::NewConnection`]
    pub fn new(conn: quinn::Connection) -> Self {
        Self {
            conn: conn.clone(),
            incoming_bi: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_bi().await, conn))
            })),
            opening_bi: None,
            incoming_uni: Box::pin(stream::unfold(conn, |conn| async {
                Some((conn.accept_uni().await, conn))
            })),
            opening_uni: None,
        }
    }

    fn poll_accept_bi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<BiStream>, ConnError>> {
        let (send, recv) = match ready!(self.incoming_bi.poll_next_unpin(cx)) {
            Some(x) => x?,
            None => return Poll::Ready(Ok(None)),
        };
        Poll::Ready(Ok(Some((send, recv))))
    }

    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<RecvStream>, ConnError>> {
        let recv = match ready!(self.incoming_uni.poll_next_unpin(cx)) {
            Some(x) => x?,
            None => return Poll::Ready(Ok(None)),
        };
        Poll::Ready(Ok(Some(recv)))
    }

    fn poll_open_bidi(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<BiStream, ConnError>> {
        if self.opening_bi.is_none() {
            self.opening_bi = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.clone().open_bi().await, conn))
            })));
        }

        let (send, recv) =
            ready!(self.opening_bi.as_mut().unwrap().poll_next_unpin(cx)).unwrap()?;
        Poll::Ready(Ok((send, recv)))
    }

    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<SendStream, ConnError>> {
        if self.opening_uni.is_none() {
            self.opening_uni = Some(Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_uni().await, conn))
            })));
        }

        let send = ready!(self.opening_uni.as_mut().unwrap().poll_next_unpin(cx)).unwrap()?;
        Poll::Ready(Ok(send))
    }
}

pub struct FrameRecvStream {
    stream: quinn::RecvStream,
    buf: Vec<u8>, // bufs: BufList<Bytes>, // bufs: VecDeque<Bytes>
    read_pos: usize,
    header: Option<FrameHeader>,
}

impl FrameRecvStream {
    pub fn new(stream: quinn::RecvStream) -> Self {
        Self {
            stream,
            buf: vec![0u8; FrameHeader::LEN],
            read_pos: 0,
            header: None,
        }
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<anyhow::Result<Option<Frame>>> {
        loop {
            let mut buf = ReadBuf::new(&mut self.buf[self.read_pos..]);
            ready!(Pin::new(&mut self.stream).poll_read(cx, &mut buf))?;
            let n = buf.filled().len();
            // EOF
            if n == 0 {
                return Poll::Ready(Ok(None));
            }
            self.read_pos += buf.filled().len();

            if self.header.is_none() && self.read_pos >= FrameHeader::LEN {
                let header = FrameHeader::decode(&self.buf[..FrameHeader::LEN])?;
                if self.buf.len() < header.total_len() {
                    self.buf.resize(header.total_len(), 0u8);
                }
                self.header = Some(header);
            }

            if let Some(header) = &self.header {
                if self.read_pos >= header.len() {
                    let frame = Frame::decode(&header, &self.buf[FrameHeader::LEN..self.read_pos])?;
                    self.read_pos = 0;
                    self.header = None;
                    return Poll::Ready(Ok(Some(frame)));
                }
            }
        }
    }

    pub async fn recv(&mut self) -> anyhow::Result<Option<Frame>> {
        futures::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
}

pub struct FrameSendStream {
    stream: quinn::SendStream,
    buf: Vec<u8>,
    state: SendState,
}

enum SendState {
    Idle,
    Sending { pos: usize, end: usize },
}

impl FrameSendStream {
    pub fn new(stream: quinn::SendStream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            state: SendState::Idle,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, SendState::Idle)
    }

    pub fn start_send(&mut self, frame: &Frame) -> anyhow::Result<()> {
        if !matches!(self.state, SendState::Idle) {
            return Err(anyhow::anyhow!("Cannot start to send: Send in progress"));
        }
        let len = frame.encoded_len()?;
        if len > self.buf.len() {
            self.buf.resize(len, 0u8);
        }
        let len = frame.encode(&mut self.buf)?;
        self.state = SendState::Sending { pos: 0, end: len };
        Ok(())
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        loop {
            match &mut self.state {
                SendState::Idle => return Poll::Ready(Ok(())),
                SendState::Sending { pos, end } => {
                    let n =
                        ready!(Pin::new(&mut self.stream).poll_write(cx, &self.buf[*pos..*end]))?;
                    *pos += n;
                    if pos == end {
                        self.state = SendState::Idle;
                    }
                }
            }
        }
    }

    pub async fn send(&mut self, frame: Frame) -> anyhow::Result<()> {
        self.start_send(&frame)?;
        poll_fn(|cx| self.poll_send(cx)).await
    }

    // pub async fn send(&mut self, frame: Frame) -> anyhow::Result<()> {
    //     let len = frame.encoded_len()?;
    //     if len > self.buf.len() {
    //         self.buf.resize(len, 0u8);
    //     }
    //     let len = frame.encode(&mut self.buf)?;
    //     self.stream.write_all(&self.buf[..len]).await?;
    //     Ok(())
    // }
}

pub mod conn {
    use anyhow::{anyhow, Context};
    use bytes::BufMut;
    use quinn::{Connecting, Connection, RecvStream, SendStream};
    use tokio::sync::mpsc::Sender;

    use crate::{
        frame::{FrameRecvStream, FrameSendStream},
        io::ConnEvent,
        tracker::proto2::frame::{AnnounceVerified, End, Frame, Query},
    };

    #[derive(Eq, PartialEq, Debug)]
    pub enum StreamType {
        Control,
        Announce,
        Query,
    }
    impl StreamType {
        pub fn from_u8(typ: u8) -> Result<Self, StreamError> {
            match typ {
                1 => Ok(Self::Control),
                2 => Ok(Self::Announce),
                3 => Ok(Self::Query),
                _ => Err(StreamError::TypeMismatch),
            }
        }
        pub fn to_u8(&self) -> u8 {
            match self {
                Self::Control => 1,
                Self::Announce => 2,
                Self::Query => 3,
            }
        }
        pub fn encode(&self, mut buf: &mut [u8]) {
            buf.put_u8(self.to_u8());
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub enum StreamError {
        #[error("Unexpected stream type")]
        TypeMismatch,
        #[error("Connection error: {0}")]
        ConnectionError(#[from] quinn::ConnectionError),
        #[error("Write error: {0}")]
        WriteError(#[from] quinn::WriteError),
        #[error("Read error: {0}")]
        ReadError(#[from] quinn::ReadExactError),
    }

    async fn send_typ(
        mut stream: SendStream,
        typ: StreamType,
    ) -> Result<FrameSendStream, StreamError> {
        stream.write_all(&[typ.to_u8()]).await?;
        let stream = FrameSendStream::new(stream);
        Ok(stream)
    }

    async fn recv_typ(
        mut stream: RecvStream,
    ) -> Result<(StreamType, FrameRecvStream), StreamError> {
        let mut buf = [0u8];
        stream.read_exact(&mut buf).await?;
        let typ = StreamType::from_u8(buf[0])?;
        let stream = FrameRecvStream::new(stream);
        Ok((typ, stream))
    }

    async fn recv_typ_exact(
        stream: RecvStream,
        expected_typ: StreamType,
    ) -> Result<FrameRecvStream, StreamError> {
        let (typ, stream) = recv_typ(stream).await?;
        if typ != expected_typ {
            return Err(StreamError::TypeMismatch);
        }
        Ok(stream)
    }

    pub struct QueryRecvStream(FrameRecvStream);
    impl QueryRecvStream {
        pub async fn recv(&mut self) -> anyhow::Result<Option<AnnounceVerified>> {
            let Some(frame) = self.0.recv().await? else {
                return Ok(None);
            };
            match frame {
                Frame::Announce(announce) => Ok(Some(announce.decode_and_verify()?)),
                Frame::End(_end) => Ok(None),
                _ => Err(anyhow!("Unexpected frame")),
            }
        }
    }

    pub struct QueryStream {
        send: FrameSendStream,
        recv: QueryRecvStream,
    }
    impl QueryStream {
        pub async fn new(conn: &Conn, query: Query) -> anyhow::Result<Self> {
            let (send, recv) = conn.open_query().await?;
            let mut this = Self {
                send,
                recv: QueryRecvStream(recv),
            };
            this.send.send(Frame::Query(query)).await?;
            Ok(this)
        }

        pub async fn cancel(mut self) -> anyhow::Result<()> {
            self.send.send(Frame::End(End {})).await?;
            Ok(())
        }

        pub async fn recv(&mut self) -> anyhow::Result<Option<AnnounceVerified>> {
            self.recv.recv().await
        }
    }

    #[derive(Clone)]
    pub struct ClientSession {
        conn: Conn,
    }
    impl ClientSession {
        pub async fn query(&self, query: Query) -> anyhow::Result<QueryStream> {
            QueryStream::new(&self.conn, query).await
        }
    }

    #[derive(Clone)]
    pub struct Conn {
        conn: Connection,
    }
    impl Conn {
        pub async fn open_control(&self) -> Result<FrameSendStream, StreamError> {
            let stream = self.conn.open_uni().await?;
            let stream = send_typ(stream, StreamType::Control).await?;
            Ok(stream)
        }

        pub async fn accept_control(&self) -> Result<FrameRecvStream, StreamError> {
            let stream = self.conn.accept_uni().await?;
            let stream = recv_typ_exact(stream, StreamType::Control).await?;
            Ok(stream)
        }

        pub async fn open_query(&self) -> Result<(FrameSendStream, FrameRecvStream), StreamError> {
            let (send, recv) = self.conn.open_bi().await?;
            let send = send_typ(send, StreamType::Query).await?;
            let recv = FrameRecvStream::new(recv);
            Ok((send, recv))
        }

        pub async fn open_announce(
            &self,
        ) -> Result<(FrameSendStream, FrameRecvStream), StreamError> {
            let (send, recv) = self.conn.open_bi().await?;
            let send = send_typ(send, StreamType::Announce).await?;
            let recv = FrameRecvStream::new(recv);
            Ok((send, recv))
        }

        pub async fn accept(
            &self,
        ) -> Result<(StreamType, FrameSendStream, FrameRecvStream), StreamError> {
            let (send, recv) = self.conn.accept_bi().await?;
            let (typ, recv) = recv_typ(recv).await?;
            let send = FrameSendStream::new(send);
            Ok((typ, send, recv))
        }
    }

    // pub async fn conn_actor(
    //     conn: Connecting,
    //     tx: Sender<ConnEvent>,
    //     is_initiator: bool,
    // ) -> anyhow::Result<()> {
    //     let conn = conn.await?;
    //     let conn_id = conn.stable_id();
    //
    //     let ctrl_send = conn.open_uni().await?;
    //     let ctrl_send = FrameSendStream::new(ctrl_send);
    //
    //     let ctrl_recv = conn.accept_uni().await?;
    //     let ctrl_recv = FrameRecvStream::new(ctrl_recv);
    //
    //     // let (incoming_tx, incoming_rx)
    //     tokio::spawn(async move {
    //         while let Ok((typ, send, recv)) = conn.accept().await {
    //             tokio::spawn(async move {
    //                 // tx.send(ConnEvent::Packet((), ()))
    //             })
    //         }
    //     })
    //
    //     todo!()
    // }
}
