//! The in-memory keyspace: values, expiry, and the sorted-set type.
//!
//! There is deliberately no persistence and no durability. A single
//! [`Keyspace`] — Redis' numbered databases — is shared behind one mutex; every
//! command locks it mutably, which lets us purge expired keys lazily on access.

use bytes::Bytes;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

/// A value stored at a key. The variant determines which commands are legal
/// against the key (mismatches yield `WRONGTYPE`).
#[derive(Debug, Clone)]
pub enum Value {
    String(Bytes),
    List(VecDeque<Bytes>),
    Set(HashSet<Bytes>),
    Hash(HashMap<Bytes, Bytes>),
    ZSet(ZSet),
    Stream(Stream),
}

impl Value {
    /// The name reported by the `TYPE` command.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_) => "string",
            Value::List(_) => "list",
            Value::Set(_) => "set",
            Value::Hash(_) => "hash",
            Value::ZSet(_) => "zset",
            Value::Stream(_) => "stream",
        }
    }
}

/// A Redis Stream: an ordered, append-mostly log of entries, each a list of
/// field/value pairs identified by a 128-bit `<ms>-<seq>` id. Backed by a
/// `BTreeMap` for `O(log n)` ordered iteration by id (what `XRANGE`/`XREAD`
/// need).
#[derive(Debug, Clone, Default)]
pub struct Stream {
    pub entries: BTreeMap<StreamId, Vec<(Bytes, Bytes)>>,
    /// The largest id ever assigned in this stream (whether present or not).
    /// New auto-generated ids must be strictly greater than this.
    pub last_id: StreamId,
    /// Tracked so that after deleting the tail we still reject smaller ids.
    pub max_deleted_id: StreamId,
}

/// A stream entry id, ordered lexicographically by `(ms, seq)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId {
    pub ms: u64,
    pub seq: u64,
}

impl StreamId {
    pub const MIN: StreamId = StreamId { ms: 0, seq: 0 };
    pub const MAX: StreamId = StreamId {
        ms: u64::MAX,
        seq: u64::MAX,
    };

    pub fn next(self) -> Option<StreamId> {
        if self.seq == u64::MAX {
            if self.ms == u64::MAX {
                None
            } else {
                Some(StreamId {
                    ms: self.ms + 1,
                    seq: 0,
                })
            }
        } else {
            Some(StreamId {
                ms: self.ms,
                seq: self.seq + 1,
            })
        }
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.ms, self.seq)
    }
}

struct Entry {
    value: Value,
    /// Absolute expiry in unix milliseconds, if the key is volatile.
    expire_at: Option<u64>,
}

/// Current unix time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Redis' numbered databases: `N` independent [`Db`]s, selected per-connection
/// with `SELECT` and reported by `CONFIG GET databases`.
///
/// All of them live behind the single keyspace mutex rather than one lock each.
/// That keeps the guarantee the rest of the server is built on — a command, or
/// a whole `EVAL` script, sees one consistent view — and lets the cross-database
/// commands (`SWAPDB`, `MOVE`, `COPY ... DB`) touch two databases at once
/// without a second lock to order against the first.
pub struct Keyspace {
    dbs: Vec<Db>,
}

impl Keyspace {
    /// Build `count` empty databases. Empty ones cost a `HashMap` header and no
    /// allocation, so the default 16 is effectively free.
    pub fn new(count: usize) -> Keyspace {
        Keyspace {
            dbs: (0..count.max(1)).map(|_| Db::new()).collect(),
        }
    }

    /// The database at `index`, which callers must have already validated
    /// (connections can only reach one through `SELECT`, which range-checks).
    pub fn db(&mut self, index: usize) -> &mut Db {
        &mut self.dbs[index]
    }

    /// Two *distinct* databases at once, for `MOVE` and `COPY ... DB`.
    ///
    /// Panics if `a == b`; both callers reject that case earlier with Redis'
    /// "source and destination objects are the same" error.
    pub fn pair(&mut self, a: usize, b: usize) -> (&mut Db, &mut Db) {
        assert_ne!(a, b, "Keyspace::pair requires distinct databases");
        if a < b {
            let (left, right) = self.dbs.split_at_mut(b);
            (&mut left[a], &mut right[0])
        } else {
            let (left, right) = self.dbs.split_at_mut(a);
            (&mut right[0], &mut left[b])
        }
    }

    /// `SWAPDB`: exchange the contents of two databases. Swapping the `Db`s
    /// themselves is O(1) and, like Redis, leaves clients pointed at the same
    /// *index* — so they see the other database's data without reconnecting.
    pub fn swap(&mut self, a: usize, b: usize) {
        self.dbs.swap(a, b);
    }

    /// `FLUSHALL`: empty every database.
    pub fn clear_all(&mut self) {
        for db in &mut self.dbs {
            db.clear();
        }
    }

    /// Every database in index order, for the periodic expiry sweep and
    /// `INFO keyspace`.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Db> {
        self.dbs.iter_mut()
    }

    /// Whether `index` names a database that exists. Takes an `i64` because
    /// every caller has just parsed one off the wire and must reject negatives.
    pub fn is_valid(&self, index: i64) -> bool {
        index >= 0 && (index as u64) < self.dbs.len() as u64
    }
}

impl Default for Keyspace {
    fn default() -> Keyspace {
        Keyspace::new(DEFAULT_DATABASES)
    }
}

/// Redis' default `databases` setting.
pub const DEFAULT_DATABASES: usize = 16;

/// One numbered database: a flat map of keys to values plus their expiries.
#[derive(Default)]
pub struct Db {
    data: HashMap<Bytes, Entry>,
}

impl Db {
    pub fn new() -> Db {
        Db::default()
    }

    /// Remove the key if it exists and has passed its expiry. Returns true if
    /// a key was expired away by this call.
    fn purge_if_expired(&mut self, key: &[u8]) -> bool {
        let expired = match self.data.get(key) {
            Some(e) => matches!(e.expire_at, Some(at) if at <= now_ms()),
            None => false,
        };
        if expired {
            self.data.remove(key);
        }
        expired
    }

    pub fn get(&mut self, key: &[u8]) -> Option<&Value> {
        self.purge_if_expired(key);
        self.data.get(key).map(|e| &e.value)
    }

    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut Value> {
        self.purge_if_expired(key);
        self.data.get_mut(key).map(|e| &mut e.value)
    }

    pub fn contains(&mut self, key: &[u8]) -> bool {
        self.purge_if_expired(key);
        self.data.contains_key(key)
    }

    /// Insert or replace a value, clearing any existing TTL.
    pub fn set(&mut self, key: Bytes, value: Value) {
        self.data.insert(
            key,
            Entry {
                value,
                expire_at: None,
            },
        );
    }

    /// Insert or replace a value, preserving an existing TTL if present.
    pub fn set_keep_ttl(&mut self, key: Bytes, value: Value) {
        let expire_at = self.data.get(&key).and_then(|e| e.expire_at);
        self.data.insert(key, Entry { value, expire_at });
    }

    pub fn remove(&mut self, key: &[u8]) -> bool {
        self.purge_if_expired(key);
        self.data.remove(key).is_some()
    }

    /// Remove a key whose container value has become empty. Redis deletes the
    /// key entirely when the last element of a list/set/hash/zset is removed.
    pub fn remove_if_empty(&mut self, key: &[u8]) {
        let empty = match self.data.get(key).map(|e| &e.value) {
            Some(Value::List(l)) => l.is_empty(),
            Some(Value::Set(s)) => s.is_empty(),
            Some(Value::Hash(h)) => h.is_empty(),
            Some(Value::ZSet(z)) => z.is_empty(),
            _ => false,
        };
        if empty {
            self.data.remove(key);
        }
    }

    /// Absolute expiry (unix ms) for a key, if any.
    pub fn expire_at(&mut self, key: &[u8]) -> Option<u64> {
        self.purge_if_expired(key);
        self.data.get(key).and_then(|e| e.expire_at)
    }

    /// Set an absolute expiry. No-op (returns false) if the key is missing.
    pub fn set_expire(&mut self, key: &[u8], at_ms: u64) -> bool {
        self.purge_if_expired(key);
        match self.data.get_mut(key) {
            Some(e) => {
                e.expire_at = Some(at_ms);
                true
            }
            None => false,
        }
    }

    /// Clear any expiry (make the key persistent). Returns true if a TTL was
    /// actually removed.
    pub fn persist(&mut self, key: &[u8]) -> bool {
        self.purge_if_expired(key);
        match self.data.get_mut(key) {
            Some(e) if e.expire_at.is_some() => {
                e.expire_at = None;
                true
            }
            _ => false,
        }
    }

    pub fn rename(&mut self, src: &[u8], dst: Bytes) -> bool {
        self.purge_if_expired(src);
        match self.data.remove(src) {
            Some(entry) => {
                self.data.insert(dst, entry);
                true
            }
            None => false,
        }
    }

    /// Number of live keys. Expired-but-not-yet-purged keys are not counted.
    pub fn len(&self) -> usize {
        let now = now_ms();
        self.data
            .values()
            .filter(|e| !matches!(e.expire_at, Some(at) if at <= now))
            .count()
    }

    /// Number of live keys carrying a TTL — the `expires=` field of
    /// `INFO keyspace`.
    pub fn expires_count(&self) -> usize {
        let now = now_ms();
        self.data
            .values()
            .filter(|e| matches!(e.expire_at, Some(at) if at > now))
            .count()
    }

    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Detach a key along with its expiry, for handing to another database.
    /// Paired with [`Db::put`] to implement `MOVE` and `COPY ... DB`, both of
    /// which carry the TTL across.
    pub fn take(&mut self, key: &[u8]) -> Option<(Value, Option<u64>)> {
        self.purge_if_expired(key);
        self.data.remove(key).map(|e| (e.value, e.expire_at))
    }

    /// Insert a value with an explicit absolute expiry — the counterpart of
    /// [`Db::take`], and unlike [`Db::set`] it does not clear the TTL.
    pub fn put(&mut self, key: Bytes, value: Value, expire_at: Option<u64>) {
        self.data.insert(key, Entry { value, expire_at });
    }

    /// Iterate live keys matching an optional glob pattern.
    pub fn keys_matching(&self, pattern: Option<&[u8]>) -> Vec<Bytes> {
        let now = now_ms();
        self.data
            .iter()
            .filter(|(_, e)| !matches!(e.expire_at, Some(at) if at <= now))
            .filter(|(k, _)| pattern.map_or(true, |p| glob_match(p, k)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// All live keys (used by SCAN, which we implement as a full snapshot).
    pub fn all_keys(&self) -> Vec<Bytes> {
        self.keys_matching(None)
    }

    /// A content fingerprint for a key, used by `WATCH` to detect changes.
    /// Returns `None` if the key is absent. Order-insensitive for sets and
    /// hashes; order-sensitive for lists and sorted sets.
    pub fn fingerprint(&mut self, key: &[u8]) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        fn hash_one(bytes: &[u8]) -> u64 {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        }
        let value = self.get(key)?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match value {
            Value::String(s) => {
                0u8.hash(&mut h);
                s.hash(&mut h);
            }
            Value::List(l) => {
                1u8.hash(&mut h);
                l.len().hash(&mut h);
                for item in l {
                    item.hash(&mut h);
                }
            }
            Value::Set(s) => {
                2u8.hash(&mut h);
                s.len().hash(&mut h);
                // XOR-fold so member order does not affect the result.
                let fold = s.iter().fold(0u64, |acc, m| acc ^ hash_one(m));
                fold.hash(&mut h);
            }
            Value::Hash(map) => {
                3u8.hash(&mut h);
                map.len().hash(&mut h);
                let fold = map.iter().fold(0u64, |acc, (k, v)| {
                    acc ^ hash_one(k).wrapping_mul(31).wrapping_add(hash_one(v))
                });
                fold.hash(&mut h);
            }
            Value::ZSet(z) => {
                4u8.hash(&mut h);
                z.len().hash(&mut h);
                let fold = z.scores.iter().fold(0u64, |acc, (m, score)| {
                    acc ^ hash_one(m).wrapping_mul(31).wrapping_add(score.to_bits())
                });
                fold.hash(&mut h);
            }
            Value::Stream(s) => {
                5u8.hash(&mut h);
                s.entries.len().hash(&mut h);
                s.last_id.hash(&mut h);
            }
        }
        Some(h.finish())
    }

    /// Actively drop every key whose TTL has passed. Called periodically so
    /// that abandoned volatile keys do not accumulate.
    pub fn sweep_expired(&mut self) {
        let now = now_ms();
        self.data
            .retain(|_, e| !matches!(e.expire_at, Some(at) if at <= now));
    }
}

/// Marker returned when a command is used against a key of the wrong type.
pub struct WrongType;

/// Typed accessors. Read variants return `Ok(None)` when the key is absent;
/// `get_or_create_*` variants materialize an empty container on demand. All of
/// them return `Err(WrongType)` when the existing value is a different type.
impl Db {
    pub fn get_str(&mut self, key: &[u8]) -> Result<Option<&Bytes>, WrongType> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_list(&mut self, key: &[u8]) -> Result<Option<&VecDeque<Bytes>>, WrongType> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::List(l)) => Ok(Some(l)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_list_mut(&mut self, key: &[u8]) -> Result<Option<&mut VecDeque<Bytes>>, WrongType> {
        match self.get_mut(key) {
            None => Ok(None),
            Some(Value::List(l)) => Ok(Some(l)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_or_create_list(&mut self, key: Bytes) -> Result<&mut VecDeque<Bytes>, WrongType> {
        self.purge_if_expired(&key);
        match self
            .data
            .entry(key)
            .or_insert_with(|| Entry {
                value: Value::List(VecDeque::new()),
                expire_at: None,
            })
            .value
        {
            Value::List(ref mut l) => Ok(l),
            _ => Err(WrongType),
        }
    }

    pub fn get_set(&mut self, key: &[u8]) -> Result<Option<&HashSet<Bytes>>, WrongType> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Set(s)) => Ok(Some(s)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_or_create_set(&mut self, key: Bytes) -> Result<&mut HashSet<Bytes>, WrongType> {
        self.purge_if_expired(&key);
        match self
            .data
            .entry(key)
            .or_insert_with(|| Entry {
                value: Value::Set(HashSet::new()),
                expire_at: None,
            })
            .value
        {
            Value::Set(ref mut s) => Ok(s),
            _ => Err(WrongType),
        }
    }

    pub fn get_hash(&mut self, key: &[u8]) -> Result<Option<&HashMap<Bytes, Bytes>>, WrongType> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Hash(h)) => Ok(Some(h)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_or_create_hash(
        &mut self,
        key: Bytes,
    ) -> Result<&mut HashMap<Bytes, Bytes>, WrongType> {
        self.purge_if_expired(&key);
        match self
            .data
            .entry(key)
            .or_insert_with(|| Entry {
                value: Value::Hash(HashMap::new()),
                expire_at: None,
            })
            .value
        {
            Value::Hash(ref mut h) => Ok(h),
            _ => Err(WrongType),
        }
    }

    pub fn get_zset(&mut self, key: &[u8]) -> Result<Option<&ZSet>, WrongType> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::ZSet(z)) => Ok(Some(z)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_or_create_zset(&mut self, key: Bytes) -> Result<&mut ZSet, WrongType> {
        self.purge_if_expired(&key);
        match self
            .data
            .entry(key)
            .or_insert_with(|| Entry {
                value: Value::ZSet(ZSet::new()),
                expire_at: None,
            })
            .value
        {
            Value::ZSet(ref mut z) => Ok(z),
            _ => Err(WrongType),
        }
    }

    pub fn get_stream(&mut self, key: &[u8]) -> Result<Option<&Stream>, WrongType> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Stream(s)) => Ok(Some(s)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_stream_mut(&mut self, key: &[u8]) -> Result<Option<&mut Stream>, WrongType> {
        match self.get_mut(key) {
            None => Ok(None),
            Some(Value::Stream(s)) => Ok(Some(s)),
            Some(_) => Err(WrongType),
        }
    }

    pub fn get_or_create_stream(&mut self, key: Bytes) -> Result<&mut Stream, WrongType> {
        self.purge_if_expired(&key);
        match self
            .data
            .entry(key)
            .or_insert_with(|| Entry {
                value: Value::Stream(Stream::default()),
                expire_at: None,
            })
            .value
        {
            Value::Stream(ref mut s) => Ok(s),
            _ => Err(WrongType),
        }
    }
}

/// Total-order wrapper over `f64` for use in a `BTreeSet` sorted-set index.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF64(f64);
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// A sorted set: a member→score map plus a `(score, member)` ordered index.
/// Rank/range operations scan the index, which is fine at dev-tool scale.
#[derive(Debug, Default, Clone)]
pub struct ZSet {
    scores: HashMap<Bytes, f64>,
    sorted: BTreeSet<(OrdF64, Bytes)>,
}

impl ZSet {
    pub fn new() -> ZSet {
        ZSet::default()
    }

    pub fn len(&self) -> usize {
        self.scores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scores.is_empty()
    }

    pub fn score(&self, member: &[u8]) -> Option<f64> {
        self.scores.get(member).copied()
    }

    /// Insert or update a member's score. Returns true if the member is new.
    pub fn insert(&mut self, member: Bytes, score: f64) -> bool {
        if let Some(&old) = self.scores.get(&member) {
            self.sorted.remove(&(OrdF64(old), member.clone()));
            self.sorted.insert((OrdF64(score), member.clone()));
            self.scores.insert(member, score);
            false
        } else {
            self.sorted.insert((OrdF64(score), member.clone()));
            self.scores.insert(member, score);
            true
        }
    }

    pub fn remove(&mut self, member: &[u8]) -> bool {
        if let Some(score) = self.scores.remove(member) {
            self.sorted
                .remove(&(OrdF64(score), Bytes::copy_from_slice(member)));
            true
        } else {
            false
        }
    }

    /// Members in ascending (score, member) order.
    pub fn iter_asc(&self) -> impl Iterator<Item = (&Bytes, f64)> {
        self.sorted.iter().map(|(s, m)| (m, s.0))
    }

    /// 0-based rank of a member in ascending order.
    pub fn rank(&self, member: &[u8]) -> Option<usize> {
        let score = self.scores.get(member)?;
        let target = (OrdF64(*score), Bytes::copy_from_slice(member));
        Some(self.sorted.iter().take_while(|e| **e != target).count())
    }
}

/// Redis-style glob match: `*`, `?`, `[...]` classes (with ranges and `^`
/// negation), and `\` escaping. Operates on raw bytes.
pub fn glob_match(pattern: &[u8], s: &[u8]) -> bool {
    glob_inner(pattern, s)
}

fn glob_inner(mut p: &[u8], mut s: &[u8]) -> bool {
    // Iterative matcher with backtracking on `*`.
    let (mut star_p, mut star_s): (Option<&[u8]>, &[u8]) = (None, &[]);
    loop {
        if let Some((&pc, prest)) = p.split_first() {
            match pc {
                b'*' => {
                    // Collapse consecutive stars and record a backtrack point.
                    star_p = Some(prest);
                    star_s = s;
                    p = prest;
                    continue;
                }
                b'?' => {
                    if !s.is_empty() {
                        s = &s[1..];
                        p = prest;
                        continue;
                    }
                }
                b'[' => {
                    if let Some((matched, prest2)) = match_class(prest, s.first().copied()) {
                        if matched {
                            s = &s[1..];
                            p = prest2;
                            continue;
                        }
                    } else {
                        // Malformed class: treat '[' literally.
                        if s.first() == Some(&b'[') {
                            s = &s[1..];
                            p = prest;
                            continue;
                        }
                    }
                }
                b'\\' if !prest.is_empty() => {
                    if s.first() == Some(&prest[0]) {
                        s = &s[1..];
                        p = &prest[1..];
                        continue;
                    }
                }
                c => {
                    if s.first() == Some(&c) {
                        s = &s[1..];
                        p = prest;
                        continue;
                    }
                }
            }
        } else if s.is_empty() {
            return true;
        }
        // Mismatch: backtrack to the last `*` if we have one.
        if let Some(sp) = star_p {
            if star_s.is_empty() {
                return false;
            }
            star_s = &star_s[1..];
            s = star_s;
            p = sp;
        } else {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pair` splits the backing Vec differently depending on index order, so
    /// exercise both directions and confirm the halves are not transposed.
    #[test]
    fn keyspace_pair_returns_the_right_databases_either_way() {
        let mut ks = Keyspace::new(16);
        for (i, db) in ks.iter_mut().enumerate() {
            db.set(
                Bytes::from("who"),
                Value::String(Bytes::from(i.to_string())),
            );
        }
        let read = |db: &mut Db| match db.get(b"who") {
            Some(Value::String(s)) => String::from_utf8_lossy(s).into_owned(),
            _ => unreachable!(),
        };

        let (low, high) = ks.pair(2, 11);
        assert_eq!((read(low), read(high)), ("2".into(), "11".into()));

        // Descending: the same two databases, in the order asked for.
        let (high, low) = ks.pair(11, 2);
        assert_eq!((read(high), read(low)), ("11".into(), "2".into()));

        // Adjacent indexes are the boundary case for the split point.
        let (a, b) = ks.pair(6, 7);
        assert_eq!((read(a), read(b)), ("6".into(), "7".into()));
    }

    #[test]
    fn expires_count_tracks_only_live_volatile_keys() {
        let mut db = Db::new();
        db.set(Bytes::from("plain"), Value::String(Bytes::from("v")));
        db.set(Bytes::from("future"), Value::String(Bytes::from("v")));
        db.set(Bytes::from("past"), Value::String(Bytes::from("v")));
        db.set_expire(b"future", now_ms() + 60_000);
        db.set_expire(b"past", now_ms() - 1);

        // The expired key counts as neither live nor expiring.
        assert_eq!(db.expires_count(), 1);
        assert_eq!(db.len(), 2);
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"h?llo", b"hello"));
        assert!(glob_match(b"h?llo", b"hallo"));
        assert!(!glob_match(b"h?llo", b"heello"));
        assert!(glob_match(b"h*o", b"ho"));
        assert!(glob_match(b"h*o", b"hbthbtho"));
        assert!(!glob_match(b"h*o", b"hbthbthx"));
        assert!(glob_match(b"", b""));
        assert!(!glob_match(b"", b"x"));
    }

    #[test]
    fn glob_char_classes() {
        assert!(glob_match(b"h[ae]llo", b"hello"));
        assert!(glob_match(b"h[ae]llo", b"hallo"));
        assert!(!glob_match(b"h[ae]llo", b"hillo"));
        assert!(glob_match(b"h[a-c]llo", b"hbllo"));
        assert!(!glob_match(b"h[a-c]llo", b"hdllo"));
        assert!(glob_match(b"h[^x]llo", b"hello"));
        assert!(!glob_match(b"h[^e]llo", b"hello"));
    }

    #[test]
    fn glob_escapes() {
        assert!(glob_match(b"h\\*o", b"h*o"));
        assert!(!glob_match(b"h\\*o", b"hxo"));
    }

    #[test]
    fn lazy_expiry_hides_key() {
        let mut db = Db::new();
        db.set(Bytes::from("k"), Value::String(Bytes::from("v")));
        db.set_expire(b"k", now_ms().saturating_sub(1));
        assert!(db.get(b"k").is_none());
        assert!(!db.contains(b"k"));
    }

    #[test]
    fn zset_orders_by_score_then_member() {
        let mut z = ZSet::new();
        z.insert(Bytes::from("b"), 2.0);
        z.insert(Bytes::from("a"), 1.0);
        z.insert(Bytes::from("c"), 2.0);
        let order: Vec<_> = z.iter_asc().map(|(m, _)| m.clone()).collect();
        assert_eq!(
            order,
            vec![Bytes::from("a"), Bytes::from("b"), Bytes::from("c")]
        );
        assert_eq!(z.rank(b"c"), Some(2));
        z.insert(Bytes::from("a"), 5.0); // move a to the end
        let order: Vec<_> = z.iter_asc().map(|(m, _)| m.clone()).collect();
        assert_eq!(
            order,
            vec![Bytes::from("b"), Bytes::from("c"), Bytes::from("a")]
        );
    }

    #[test]
    fn fingerprint_is_order_insensitive_for_sets() {
        let mut a = Db::new();
        let mut set_a = HashSet::new();
        set_a.insert(Bytes::from("x"));
        set_a.insert(Bytes::from("y"));
        a.set(Bytes::from("k"), Value::Set(set_a));

        let mut b = Db::new();
        let mut set_b = HashSet::new();
        set_b.insert(Bytes::from("y"));
        set_b.insert(Bytes::from("x"));
        b.set(Bytes::from("k"), Value::Set(set_b));

        assert_eq!(a.fingerprint(b"k"), b.fingerprint(b"k"));
    }
}

/// Match a `[...]` class against `ch`. `class` begins just after `[`.
/// Returns `(matched, rest_after_class)`, or `None` if the class is unterminated.
fn match_class(class: &[u8], ch: Option<u8>) -> Option<(bool, &[u8])> {
    let mut i = 0;
    let negate = class.first() == Some(&b'^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < class.len() {
        match class[i] {
            b']' if !first => {
                let result = matched ^ negate;
                return Some((ch.is_some() && result, &class[i + 1..]));
            }
            b'\\' if i + 1 < class.len() => {
                if ch == Some(class[i + 1]) {
                    matched = true;
                }
                i += 2;
            }
            // Range like a-z.
            c if i + 2 < class.len() && class[i + 1] == b'-' && class[i + 2] != b']' => {
                let lo = c;
                let hi = class[i + 2];
                if let Some(ch) = ch {
                    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                    if ch >= lo && ch <= hi {
                        matched = true;
                    }
                }
                i += 3;
            }
            c => {
                if ch == Some(c) {
                    matched = true;
                }
                i += 1;
            }
        }
        first = false;
    }
    // Unterminated class.
    None
}
