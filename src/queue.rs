use std::{
    collections::{HashMap, VecDeque},
    future::poll_fn,
    task::{Context, Poll},
};

use crate::{
    proto::wire::Frame,
    stream::{FrameSendStream, StreamAddr, StreamError},
};
pub struct SendQueue {
    queue: Outqueues<Frame>,
    streams: HashMap<StreamAddr, FrameSendStream>,
    sent: usize,
}

impl Default for SendQueue {
    fn default() -> Self {
        Self {
            queue: Outqueues::new(),
            streams: HashMap::new(),
            sent: 0,
        }
    }
}

impl SendQueue {
    pub fn len(&self) -> usize {
        self.queue.total_len()
    }

    pub fn push_stream(&mut self, stream: FrameSendStream) {
        self.streams.insert(stream.addr(), stream);
    }

    pub fn push_frame(&mut self, addr: StreamAddr, frame: Frame) {
        self.queue.push_back(addr, frame);
    }

    pub fn remove(&mut self, addr: &StreamAddr) {
        self.streams.remove(addr);
        self.queue.remove(addr);
    }

    pub async fn send(&mut self) -> Result<(), StreamError> {
        poll_fn(|cx| self.poll_send(cx)).await
    }

    pub fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        let mut iter = self.streams.iter_mut();
        let err = 'outer: loop {
            let Some((addr, stream)) = iter.next() else {
                break 'outer None;
            };
            'inner: while let Poll::Ready(res) = stream.poll_send(cx) {
                self.sent += 1;
                match res {
                    Ok(()) => {
                        if let Some(frame) = self.queue.pop_front(&addr) {
                            stream.start_send(&frame)?;
                        } else {
                            break 'inner;
                        }
                    }
                    Err(err) => {
                        break 'outer Some((*addr, err));
                    }
                }
            }
        };
        if let Some((addr, err)) = err {
            self.remove(&addr);
            Poll::Ready(Err(err))
        } else {
            Poll::Pending
        }
    }
}

pub struct Outqueues<T> {
    queues: HashMap<StreamAddr, VecDeque<T>>,
    len: usize,
}

impl<T> Default for Outqueues<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Outqueues<T> {
    pub fn new() -> Self {
        Self {
            queues: HashMap::new(),
            len: 0,
        }
    }
    pub fn pop_front(&mut self, addr: &StreamAddr) -> Option<T> {
        let frame = self
            .queues
            .get_mut(addr)
            .map(|queue| queue.pop_front())
            .flatten();
        if let Some(_) = &frame {
            self.len = self.len.saturating_sub(1);
        }
        frame
    }

    pub fn push_back(&mut self, addr: StreamAddr, frame: T) {
        self.len += 1;
        match self.queues.get_mut(&addr) {
            Some(queue) => {
                queue.push_back(frame);
            }
            None => {
                self.queues.insert(addr, [frame].into());
            }
        }
    }

    pub fn is_empty(&self, addr: &StreamAddr) -> bool {
        self.queues
            .get(addr)
            .map(|queue| queue.is_empty())
            .unwrap_or(true)
    }

    pub fn remove(&mut self, addr: &StreamAddr) -> usize {
        let len = self
            .queues
            .remove(addr)
            .iter()
            .map(|q| q.iter())
            .flatten()
            .count();
        self.len = self.len.saturating_sub(len);
        len
    }

    pub fn total_len(&self) -> usize {
        self.len
    }
}

// pub struct MappedSendQueue<T> {
//     queue: Outqueues<T>,
//     streams: HashMap<StreamAddr, MappedSendStream<T>>,
//     sent: usize,
// }
//
// impl<T> Default for MappedSendQueue<T> {
//     fn default() -> Self {
//         Self {
//             queue: Outqueues::new(),
//             streams: HashMap::new(),
//             sent: 0,
//         }
//     }
// }
//
// impl<T: FrameType> MappedSendQueue<T> {
//     pub fn push_stream(&mut self, addr: StreamAddr, stream: MappedSendStream<T>) {
//         self.streams.insert(addr, stream);
//     }
//
//     pub fn push_frame(&mut self, addr: StreamAddr, frame: T) {
//         self.queue.push_back(addr, frame);
//     }
//
//     pub fn remove(&mut self, addr: &StreamAddr) {
//         self.streams.remove(addr);
//         self.queue.remove(addr);
//     }
//
//     async fn send(&mut self) -> Result<(), StreamError> {
//         poll_fn(|cx| self.poll_send(cx)).await
//     }
//
//     pub fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
//         for (addr, stream) in self.streams.iter_mut() {
//             while let Poll::Ready(res) = stream.poll_send(cx) {
//                 self.sent += 1;
//                 // TODO: Handle error
//                 match res {
//                     Ok(()) => {
//                         if let Some(frame) = self.queue.pop_front(&addr) {
//                             stream.start_send(&frame)?;
//                         } else {
//                             break;
//                         }
//                     }
//                     Err(err) => {
//                         self.remove(addr);
//                         return Poll::Ready(Err(err));
//                     }
//                 }
//             }
//         }
//         Poll::Pending
//     }
// }
//
