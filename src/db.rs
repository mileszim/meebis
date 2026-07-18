//! The in-memory keyspace: values, expiry, and the sorted-set type.
//!
//! There is deliberately no persistence and no durability. A single [`Db`] is
//! shared behind a mutex; every command locks it mutably, which lets us purge
//! expired keys lazily on access.

use bytes::Bytes;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
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
        }
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

/// The keyspace. Redis' concept of numbered databases is collapsed to a single
/// map — `SELECT` is accepted but ignored.
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

    pub fn clear(&mut self) {
        self.data.clear();
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
