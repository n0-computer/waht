use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;

use crate::tracker::PeerId;

pub fn generate_peer_id() -> (PeerId, SigningKey) {
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let peer_id: PeerId = signing_key.verifying_key().to_bytes();
    (peer_id, signing_key)
}

#[derive(Debug)]
pub struct Interval {
    interval: Duration,
    next: Instant,
}

impl Interval {
    pub fn from_now(interval: Duration) -> Self {
        Self::new(Instant::now(), interval)
    }

    pub fn new(now: Instant, interval: Duration) -> Self {
        Self {
            interval,
            next: now + interval,
        }
    }

    pub fn has_elapsed(&self, now: Instant) -> bool {
        now > self.next
    }

    pub fn reset(&mut self, now: Instant) {
        self.next = now + self.interval
    }

    pub fn next(&self) -> Instant {
        self.next
    }
}
