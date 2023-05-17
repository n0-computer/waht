pub enum Timer {
    RemovePeer(SocketAddr),
}

pub struct Timers {
    timers: BTreeMap<Instant, Timer>,
}
impl Timers {
    pub fn add(&mut self, when: Instant) {}
}

