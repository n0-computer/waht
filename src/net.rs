pub mod conn {
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        future::{poll_fn, Future},
        hash::Hash,
        pin::Pin,
        task::{Context, Poll},
    };

    use futures::stream::{self, BoxStream, FuturesUnordered};
    use futures::{ready, StreamExt};
    use quinn::{AcceptBi, AcceptUni, OpenBi, OpenUni, RecvStream, StreamId};
    use tokio::sync::mpsc::Receiver;
    use tracing::warn;

    use crate::{
        proto::{
            crypto::{PublicKey, SealedEnvelope, VerifiedEnvelope},
            data::{AnnounceRequest, ControlMessage, DataFrame, QueryRequest},
            traits::{Decode, Encode},
            wire::{AnnouncePayload, Frame, PeerInfo, QueryPayload, SignedFrame},
        },
        stream::{
            next_frame,
            server::{AnnounceBiStream, AnnounceRecvStream, QueryBiStream},
            ConnId, ControlBiStream, ControlRecvStream, FrameBiStream, FrameRecvStream,
            FrameSendStream, MappedRecvStream, NextFrame, StreamAddr, StreamType,
        },
        tracker::PeerId,
    };

    // pub type FutureOutputStream<T> = BoxStream<'static, <T<'static> as Future>::Output>;
    pub struct Connection {
        conn: quinn::Connection,
        incoming_bi: BoxStream<'static, anyhow::Result<(StreamType, FrameBiStream)>>,
        opening_bi: Option<BoxStream<'static, <OpenBi<'static> as Future>::Output>>,
        incoming_uni: BoxStream<'static, anyhow::Result<(StreamType, FrameRecvStream)>>,
        opening_uni: Option<BoxStream<'static, <OpenUni<'static> as Future>::Output>>,
    }

    impl Connection {
        /// Create a [`Connection`] from a [`quinn::NewConnection`]
        pub fn new(conn: quinn::Connection) -> Self {
            Self {
                conn: conn.clone(),
                incoming_bi: Box::pin(stream::unfold(conn.clone(), |conn| async {
                    Some((accept_bi_framed(&conn).await, conn))
                })),
                opening_bi: None,
                incoming_uni: Box::pin(stream::unfold(conn, |conn| async {
                    Some((accept_uni_framed(&conn).await, conn))
                })),
                opening_uni: None,
            }
        }

        pub fn poll(&mut self, cx: &mut Context<'_>, events: &mut VecDeque<ServerConnEvent>) {}
    }

    pub enum ServerConnEvent {
        ControlStream(StreamAddr, ControlBiStream),
        AnnounceStream(StreamAddr, AnnounceBiStream),
        QueryStream(StreamAddr, QueryBiStream),
    }
    struct ServerActor {
        conns: HashMap<ConnId, Connection>,
        streams_by_conn: HashMap<ConnId, HashMap<StreamId, StreamType>>,
        announce_in: FuturesUnordered<NextFrame<AnnounceRequest>>,
        query_in: FuturesUnordered<NextFrame<QueryRequest>>,
        control_in: FuturesUnordered<NextFrame<ControlMessage>>,
        control_out: HashMap<ConnId, FrameSendStream>,
        senders: HashMap<StreamAddr, FrameSendStream>,
        // outqueue: VecDeque<(StreamAddr, Frame)>,
        outqueues: Outqueues,
        peer_announcements: HashMap<PublicKey, SealedEnvelope<SignedFrame>>,
        peer_queries: HashMapSet<PeerId, StreamAddr>,
    }

    struct Outqueues {
        queues: HashMap<StreamAddr, VecDeque<Frame>>,
    }
    impl Outqueues {
        pub fn pop_front(&mut self, addr: &StreamAddr) -> Option<Frame> {
            self.queues
                .get_mut(addr)
                .map(|queue| queue.pop_front())
                .flatten()
        }

        pub fn is_empty(&self, addr: &StreamAddr) -> bool {
            self.queues
                .get(addr)
                .map(|queue| queue.is_empty())
                .unwrap_or(true)
        }

        pub fn push_back(&mut self, addr: StreamAddr, frame: Frame) {
            match self.queues.get_mut(&addr) {
                Some(queue) => {
                    queue.push_back(frame);
                }
                None => {
                    self.queues.insert(addr, [frame].into());
                }
            }
        }

        pub fn total_len(&self) -> usize {
            self.queues.values().map(|q| q.len()).sum()
        }
    }

    struct HashMapSet<K, V>(pub(crate) HashMap<K, HashSet<V>>);
    impl<K, V> HashMapSet<K, V> {
        pub fn new() -> Self {
            Self(Default::default())
        }
    }
    impl<K: Eq + Hash, V: Eq + Hash> HashMapSet<K, V> {
        pub fn insert(&mut self, key: K, value: V) {
            match self.0.get_mut(&key) {
                Some(set) => {
                    set.insert(value);
                }
                None => {
                    self.0.insert(key, [value].into());
                }
            };
        }
        pub fn iter(&self, key: &K) -> impl Iterator<Item = &V> {
            self.0.get(key).map(HashSet::iter).into_iter().flatten()
        }
    }

    pub struct FrameOut {
        addr: StreamAddr,
        frame: Frame,
    }

    impl ServerActor {
        fn insert_stream_addr(&mut self, addr: StreamAddr, typ: StreamType) {
            self.streams_by_conn
                .entry(addr.conn_id)
                .and_modify(|streams| {
                    streams.insert(addr.stream_id, typ);
                })
                .or_insert_with(|| HashMap::from_iter([(addr.stream_id, typ)]));
        }

        fn remove_conn(&mut self, conn_id: &ConnId) {
            if let Some(streams) = self.streams_by_conn.remove(conn_id) {
                for (stream_id, typ) in streams.into_iter() {
                    match typ {
                        StreamType::Control => {
                            self.control_out.remove(conn_id);
                        }
                        StreamType::Announce | StreamType::Query => {
                            self.senders.remove(&(*conn_id, stream_id).into());
                        }
                    }
                }
            }
        }

        fn has_conn(&self, conn_id: &ConnId) -> bool {
            self.control_out.contains_key(conn_id)
        }

        pub fn flush(&mut self) {}

        pub async fn run(
            mut self,
            endpoint_events: &mut Receiver<ServerConnEvent>,
        ) -> anyhow::Result<()> {
            loop {
                let res = poll_fn(|cx| self.poll_run(cx, endpoint_events)).await?;
                if res.is_none() {
                    return Ok(());
                }
            }
        }

        pub fn poll_run(
            &mut self,
            cx: &mut Context<'_>,
            endpoint_events: &mut Receiver<ServerConnEvent>,
        ) -> Poll<anyhow::Result<Option<()>>> {
            // process endpoint events
            while let Poll::Ready(event) = endpoint_events.poll_recv(cx) {
                let Some(event) = event else {
                        // endpoint sender was dropped, abort.
                        return Poll::Ready(Ok(None));
                    };
                match event {
                    ServerConnEvent::AnnounceStream(addr, (send, recv)) => {
                        self.announce_in.push(next_frame(addr, recv));
                        self.senders.insert(addr, send);
                        self.insert_stream_addr(addr, StreamType::Announce);
                    }
                    ServerConnEvent::QueryStream(addr, (send, recv)) => {
                        self.query_in.push(next_frame(addr, recv));
                        self.senders.insert(addr, send);
                        self.insert_stream_addr(addr, StreamType::Query);
                    }
                    ServerConnEvent::ControlStream(addr, (send, recv)) => {
                        // TODO: Check if control_out already has this conn, and if so, error.
                        self.control_in.push(next_frame(addr, recv));
                        self.control_out.insert(addr.conn_id, send);
                        self.insert_stream_addr(addr, StreamType::Control);
                    }
                    _ => todo!(),
                }
            }

            // process incoming annunce packets
            while let Poll::Ready(Some((addr, stream, frame))) =
                self.announce_in.poll_next_unpin(cx)
            {
                match frame {
                    Ok(Some(frame)) => {
                        // handle announce
                        // queue next announce
                        self.handle_announce(addr, frame);
                        self.announce_in.push(next_frame(addr, stream));
                    }
                    Ok(None) => {
                        // stream was closed gracefully
                    }
                    Err(err) => {
                        // stream produced error
                        if self.has_conn(&addr.conn_id) {
                            warn!("error on announce stream {err:?}");
                            self.remove_conn(&addr.conn_id)
                        }
                    }
                }
            }
            while let Poll::Ready(Some((addr, stream, frame))) = self.query_in.poll_next_unpin(cx) {
                match frame {
                    Ok(Some(frame)) => {
                        // handle query
                        // queue next query
                        self.handle_query(addr, frame);
                        self.query_in.push(next_frame(addr, stream));
                    }
                    Ok(None) => {
                        // stream was closed gracefully
                    }
                    Err(err) => {
                        // stream produced error
                        if self.has_conn(&addr.conn_id) {
                            warn!("error on query stream {err:?}");
                            self.remove_conn(&addr.conn_id)
                        }
                    }
                }
            }

            for (addr, sender) in self.senders.iter_mut() {
                while let Poll::Ready(res) = sender.poll_send(cx) {
                    // TODO: Handle error
                    match res {
                        Ok(()) => {
                            if let Some(frame) = self.outqueues.pop_front(&addr) {
                                sender.start_send(&frame).unwrap()
                            } else {
                                break;
                            }
                        }
                        Err(_err) => {
                            break;
                        }
                    }
                }
            }

            // handle outqueue
            // let len = self.outqueue.len();
            // for _i in 0..len {
            //     let (addr, frame) = self.outqueue.pop_front().unwrap();
            //     if let Some(stream) = self.senders.get_mut(&addr) {
            //         if stream.is_idle() {
            //             stream.start_send(&frame)?;
            //         } else {
            //             self.outqueue.push_back((addr, frame));
            //         }
            //     }
            // }

            // self.flush()

            // while let Some(frame) = self.

            // Pin::new(conn_events).poll_recv(cx);

            Poll::Pending
        }

        fn handle_query(&mut self, addr: StreamAddr, req: QueryRequest) {
            match req {
                QueryRequest::Query(req) => match req.query {
                    QueryPayload::Peers(peers) => {
                        for id in peers.peer_ids {
                            if let Some(frame) = self.peer_announcements.get(&id) {
                                let frame: Frame = frame.clone().into();
                                self.outqueues.push_back(addr, frame);
                            }
                            self.peer_queries.insert(id, addr);
                        }
                    }
                    QueryPayload::Keys(_) => todo!(),
                },
                QueryRequest::End(_) => {
                    // TODO: Remove from self.peer_queries
                }
            }
            // self.outqueue.push((stream, Frame::))
        }
        fn handle_announce(&mut self, addr: StreamAddr, req: AnnounceRequest) {
            match req {
                AnnounceRequest::Announce(announce) => {
                    match &announce.payload.payload {
                        AnnouncePayload::PeerInfo(_info) => {
                            let issuer = announce.issuer;
                            let frame = announce.map(SignedFrame::Announce);
                            // TODO: Remove unwrap?
                            let frame = frame.into_sealed().unwrap();
                            self.peer_announcements.insert(issuer, frame.clone());
                            // Push to running queries
                            for addr in self.peer_queries.iter(&issuer) {
                                self.outqueues.push_back(*addr, frame.clone().into());
                            }
                        }
                        AnnouncePayload::ProvideHash(_) => todo!(),
                        AnnouncePayload::UpdateTopic(_) => todo!(),
                    }
                }
                AnnounceRequest::Unannounce(_) => todo!(),
                AnnounceRequest::End(_) => todo!(),
            }
        }
    }
}
