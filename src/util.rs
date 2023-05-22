use std::{env, path::PathBuf};

use anyhow::anyhow;
use ed25519_dalek::SigningKey;

use crate::tracker::PeerId;

pub const WAHT_DIR: &str = "waht";

pub fn generate_peer_id() -> (PeerId, SigningKey) {
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let peer_id: PeerId = signing_key.verifying_key().to_bytes();
    (peer_id, signing_key)
}

/// Returns the path to the user's waht data directory.
///
/// If the `WAHT_DATA_DIR` environment variable is set it will be used unconditionally.
/// Otherwise the returned value depends on the operating system according to the following
/// table.
///
/// | Platform | Value                                         | Example                                  |
/// | -------- | --------------------------------------------- | ---------------------------------------- |
/// | Linux    | `$XDG_DATA_HOME`/waht or `$HOME`/.local/share/waht | /home/alice/.local/share/waht                 |
/// | macOS    | `$HOME`/Library/Application Support/waht      | /Users/Alice/Library/Application Support/waht |
/// | Windows  | `{FOLDERID_RoamingAppData}/waht`              | C:\Users\Alice\AppData\Roaming\waht           |
pub fn waht_data_root() -> anyhow::Result<PathBuf> {
    if let Some(val) = env::var_os("WAHT_DATA_DIR") {
        return Ok(PathBuf::from(val));
    }
    let path = dirs_next::data_dir().ok_or_else(|| {
        anyhow!("operating environment provides no directory for application data")
    })?;
    Ok(path.join(WAHT_DIR))
}

pub mod time {
    use std::{
        collections::{BTreeMap, HashMap, HashSet},
        fmt,
        hash::Hash,
        time::{Duration, Instant},
    };
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

        pub fn next_tick(&self) -> Instant {
            self.next
        }
    }

    #[derive(Debug)]
    pub struct ExpirationMap<K> {
        items: HashMap<K, Instant>,
        ttls: InstantMap<K>,
        default_ttl: Duration,
    }

    impl<K> ExpirationMap<K>
    where
        K: Hash + Eq + PartialEq + Copy + fmt::Debug,
    {
        pub fn new(ttl: Duration) -> Self {
            Self {
                items: HashMap::new(),
                ttls: Default::default(),
                default_ttl: ttl,
            }
        }

        pub fn insert(&mut self, id: K, ttl: Instant) {
            if let Some(old_ttl) = self.items.get(&id) {
                self.ttls.update(*old_ttl, ttl, id);
            } else {
                self.ttls.insert(ttl, id);
            }
            self.items.insert(id, ttl);
        }

        pub fn renew(&mut self, id: K, now: Instant) {
            self.insert(id, now + self.default_ttl)
        }

        pub fn remove(&mut self, id: &K) -> Option<Instant> {
            match self.items.remove(id) {
                None => None,
                Some(ttl) => {
                    self.ttls.remove(ttl, id);
                    Some(ttl)
                }
            }
        }

        pub fn drain_expired(&mut self, now: Instant) -> impl Iterator<Item = (K, Instant)> + '_ {
            self.ttls
                .drain_expired(now)
                .map(|id| self.items.remove(&id).map(|ttl| (id, ttl)))
                .flatten()
        }

        pub fn iter(&self) -> impl Iterator<Item = (&K, &Instant)> {
            self.items.iter()
        }
    }
    pub trait HasTtl<K: Hash + PartialEq> {
        fn id(&self) -> K;
        fn ttl(&self) -> Instant;
        fn set_ttl(&self, ttl: Instant) -> Instant;
    }

    pub struct InstantMap<T> {
        inner: BTreeMap<Instant, HashSet<T>>,
    }

    impl<T: fmt::Debug> fmt::Debug for InstantMap<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "InstantMap({:?})", self.inner)
        }
    }

    impl<T> Default for InstantMap<T> {
        fn default() -> Self {
            Self {
                inner: Default::default(),
            }
        }
    }

    impl<T: PartialEq + Eq + Hash> InstantMap<T> {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn insert(&mut self, instant: Instant, value: T) {
            if let Some(values) = self.inner.get_mut(&instant) {
                values.insert(value);
            } else {
                self.inner.insert(instant, HashSet::from([value]));
            }
        }

        pub fn update(&mut self, old_instant: Instant, new_instant: Instant, value: T) {
            self.remove(old_instant, &value);
            self.insert(new_instant, value);
        }

        pub fn remove(&mut self, instant: Instant, value: &T) {
            if let Some(values) = self.inner.get_mut(&instant) {
                values.remove(&value);
                if values.is_empty() {
                    self.inner.remove(&instant);
                }
            }
        }

        pub fn drain_expired(&mut self, now: Instant) -> impl Iterator<Item = T> {
            let mut not_expired = self.inner.split_off(&now);
            std::mem::swap(&mut not_expired, &mut self.inner);
            let expired = not_expired;
            expired.into_values().map(|v| v.into_iter()).flatten()
        }

        // pub fn expired(&self, now: Instant) -> impl Iterator<Item = &T> {
        //     self.inner
        //         .range(..now)
        //         .map(|(_instant, values)| values.iter())
        //         .flatten()
        // }
        //
        // pub fn expired_mut(&mut self, now: Instant) -> impl Iterator<Item = &mut T> {
        //     self.inner
        //         .range_mut(..now)
        //         .map(|(_instant, values)| values.iter_mut())
        //         .flatten()
        // }
    }
}
