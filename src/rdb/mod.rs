//! RDB: reading and writing Redis' snapshot file format.
//!
//! meebis is still an in-memory server — nothing here makes it durable. What a
//! dump file buys is the ability to *hand state across*: seed a fresh instance
//! from a snapshot a real Redis wrote, or keep a worktree's keyspace across a
//! restart. The keyspace lives in RAM either way.
//!
//! The two directions are deliberately asymmetric:
//!
//! * **Reading** accepts everything — every encoding across RDB versions 1 to
//!   11, because Redis picks the encoding and meebis has to cope.
//! * **Writing** emits only the flat, oldest-spelling encodings, which current
//!   Redis still loads. Verified against Redis 7.2: a hand-built file using
//!   type 1 for a list and type 4 for a hash loads without complaint.
//!
//! Multiple databases are what made this worth doing. A single-database server
//! could not represent a dump that used `SELECT`, so any Redis snapshot with
//! keys outside db 0 was unrepresentable until [`Keyspace`] grew them.

mod crc64;
mod cursor;
mod lzf;
mod packed;
mod read;
mod stream;
mod write;

use crate::db::Keyspace;
use bytes::Bytes;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The file could not be read or written at all.
    Io(std::io::Error),
    /// The bytes are not a well-formed RDB — truncated, mis-encoded, or failing
    /// their own checksum.
    Corrupt(String),
    /// Well-formed, but containing something meebis has no way to represent.
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Corrupt(m) => write!(f, "corrupt dump: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported dump: {m}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

/// What a load actually did, for the boot log.
#[derive(Debug, Default)]
pub struct LoadStats {
    pub keys: usize,
    /// Keys whose TTL had already passed, dropped rather than materialized.
    pub expired: usize,
    /// Consumer groups discarded: meebis implements streams without them.
    pub dropped_groups: usize,
    /// `FUNCTION` libraries discarded: meebis has `EVAL` but no function store.
    pub dropped_functions: usize,
    /// Auxiliary header fields, chiefly `redis-ver`.
    pub aux: Vec<(Bytes, Bytes)>,
}

impl LoadStats {
    /// Who wrote the dump, phrased for the boot line.
    ///
    /// meebis stamps its own version alongside the `redis-ver` every RDB
    /// carries — the latter has to claim a Redis version for compatibility, so
    /// reporting it alone would credit Redis for meebis' own files.
    pub fn writer_version(&self) -> Option<String> {
        let aux = |name: &[u8]| {
            self.aux
                .iter()
                .find(|(k, _)| k.as_ref() == name)
                .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        };
        match aux(b"meebis-ver") {
            Some(v) => Some(format!("meebis {v}")),
            None => aux(b"redis-ver").map(|v| format!("Redis {v}")),
        }
    }

    /// Anything the load silently could not carry across, phrased for a warning.
    pub fn losses(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.dropped_groups > 0 {
            out.push(format!(
                "{} consumer group(s) dropped — meebis implements streams without them",
                self.dropped_groups
            ));
        }
        if self.dropped_functions > 0 {
            out.push(format!(
                "{} function library/libraries dropped — meebis has EVAL but not FUNCTION",
                self.dropped_functions
            ));
        }
        out
    }
}

/// Serialize the keyspace to an in-memory image.
pub fn to_bytes(ks: &mut Keyspace) -> Vec<u8> {
    write::to_bytes(ks)
}

/// Parse an in-memory image into `ks`.
pub fn from_bytes(buf: &[u8], ks: &mut Keyspace) -> Result<LoadStats> {
    read::from_bytes(buf, ks)
}

/// Load `path` into `ks`. A missing file is not an error — it is the normal
/// first boot — and reports zero keys loaded.
pub fn load(path: &Path, ks: &mut Keyspace) -> Result<Option<LoadStats>> {
    let buf = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    // An empty file is what a crashed writer leaves behind; treat it as absent
    // rather than as corruption, since there is nothing to lose either way.
    if buf.is_empty() {
        return Ok(None);
    }
    from_bytes(&buf, ks).map(Some)
}

/// Write `ks` to `path` via a temp file and a rename, so a reader never
/// observes a half-written dump and a failed write leaves the previous
/// snapshot intact.
pub fn save(path: &Path, ks: &mut Keyspace) -> Result<()> {
    let image = to_bytes(ks);
    let tmp = temp_path(path);

    // Scoped so the handle is closed before the rename.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&image)?;
        f.sync_all()?;
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Io(e));
    }
    Ok(())
}

/// Sibling temp path for the atomic write. Kept in the same directory so the
/// rename stays within one filesystem.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{now_ms, Stream, StreamId, Value, ZSet};
    use std::collections::{HashMap, HashSet, VecDeque};

    /// Round-trip a keyspace through the codec and hand back the reloaded one.
    fn round_trip(ks: &mut Keyspace) -> Keyspace {
        let image = to_bytes(ks);
        let mut out = Keyspace::new(16);
        from_bytes(&image, &mut out).expect("failed to reload our own dump");
        out
    }

    #[test]
    fn strings_and_expiries_survive() {
        let mut ks = Keyspace::new(16);
        let future = now_ms() + 600_000;
        ks.db(0)
            .put(Bytes::from("plain"), Value::String(Bytes::from("v")), None);
        ks.db(0).put(
            Bytes::from("volatile"),
            Value::String(Bytes::from("v")),
            Some(future),
        );
        // Binary-safe: a value with NULs and high bytes must not be mangled.
        ks.db(0).put(
            Bytes::from("binary"),
            Value::String(Bytes::from(vec![0u8, 0xff, b'a', 0, 0x80])),
            None,
        );

        let mut back = round_trip(&mut ks);
        let db = back.db(0);
        assert!(matches!(db.get(b"plain"), Some(Value::String(s)) if s == "v"));
        assert_eq!(db.expire_at(b"volatile"), Some(future));
        assert!(db.expire_at(b"plain").is_none());
        assert!(
            matches!(db.get(b"binary"), Some(Value::String(s)) if s.as_ref() == [0, 0xff, b'a', 0, 0x80])
        );
    }

    /// Already-expired keys are dropped on load rather than reintroduced for
    /// the sweeper to find, which is what Redis does loading as a master.
    ///
    /// meebis' own writer never emits one, so this hand-builds a file that
    /// has one — a real Redis dump easily does, since keys go on expiring
    /// while the file sits on disk.
    #[test]
    fn expired_keys_are_dropped_on_load() {
        let mut image = Vec::new();
        image.extend_from_slice(b"REDIS0011");
        image.push(0xfe); // SELECTDB
        write::put_length(&mut image, 0);

        image.push(0xfc); // EXPIRETIME_MS, long past
        image.extend_from_slice(&1u64.to_le_bytes());
        image.push(0); // STRING
        write::put_raw_string(&mut image, b"gone");
        write::put_raw_string(&mut image, b"v");

        image.push(0xfc); // EXPIRETIME_MS, comfortably ahead
        image.extend_from_slice(&(now_ms() + 600_000).to_le_bytes());
        image.push(0);
        write::put_raw_string(&mut image, b"stays");
        write::put_raw_string(&mut image, b"v");

        image.push(0xff);
        let checksum = crc64::checksum(&image);
        image.extend_from_slice(&checksum.to_le_bytes());

        let mut back = Keyspace::new(16);
        let stats = from_bytes(&image, &mut back).unwrap();
        assert_eq!(stats.keys, 1);
        assert_eq!(stats.expired, 1);
        assert!(back.db(0).get(b"gone").is_none());
        assert!(back.db(0).get(b"stays").is_some());
    }

    /// Redis emits a key's expiry *before* its LRU/LFU metadata, so the two
    /// opcodes are separated by a third. Any loader that treats "an opcode that
    /// is not an expiry" as the end of the expiry's scope drops the TTL — and
    /// only for dumps taken with an eviction policy set, which is exactly the
    /// kind of thing that never shows up in a round-trip test.
    #[test]
    fn an_expiry_survives_the_lru_and_lfu_opcodes_between_it_and_its_key() {
        let future = now_ms() + 600_000;
        let mut image = Vec::new();
        image.extend_from_slice(b"REDIS0011");
        image.push(0xfe);
        write::put_length(&mut image, 0);

        // EXPIRETIME_MS, then FREQ (LFU), then the key.
        image.push(0xfc);
        image.extend_from_slice(&future.to_le_bytes());
        image.push(0xf9);
        image.push(200);
        image.push(0);
        write::put_raw_string(&mut image, b"lfu");
        write::put_raw_string(&mut image, b"v");

        // EXPIRETIME_MS, then IDLE (LRU), then the key.
        image.push(0xfc);
        image.extend_from_slice(&future.to_le_bytes());
        image.push(0xf8);
        write::put_length(&mut image, 4242);
        image.push(0);
        write::put_raw_string(&mut image, b"lru");
        write::put_raw_string(&mut image, b"v");

        image.push(0xff);
        let checksum = crc64::checksum(&image);
        image.extend_from_slice(&checksum.to_le_bytes());

        let mut back = Keyspace::new(16);
        let stats = from_bytes(&image, &mut back).unwrap();
        assert_eq!(stats.keys, 2);
        assert_eq!(back.db(0).expire_at(b"lfu"), Some(future));
        assert_eq!(back.db(0).expire_at(b"lru"), Some(future));
    }

    /// The converse: an expiry consumed by one key must not leak onto the next.
    #[test]
    fn an_expiry_does_not_leak_to_the_following_key() {
        let future = now_ms() + 600_000;
        let mut image = Vec::new();
        image.extend_from_slice(b"REDIS0011");
        image.push(0xfe);
        write::put_length(&mut image, 0);

        image.push(0xfc);
        image.extend_from_slice(&future.to_le_bytes());
        image.push(0);
        write::put_raw_string(&mut image, b"volatile");
        write::put_raw_string(&mut image, b"v");

        image.push(0); // no expiry opcode this time
        write::put_raw_string(&mut image, b"permanent");
        write::put_raw_string(&mut image, b"v");

        image.push(0xff);
        let checksum = crc64::checksum(&image);
        image.extend_from_slice(&checksum.to_le_bytes());

        let mut back = Keyspace::new(16);
        from_bytes(&image, &mut back).unwrap();
        assert_eq!(back.db(0).expire_at(b"volatile"), Some(future));
        assert_eq!(back.db(0).expire_at(b"permanent"), None);
    }

    /// The other half: a key that has already expired is not written out at
    /// all, so a save/load cycle does not resurrect it.
    #[test]
    fn already_expired_keys_are_not_written() {
        let mut ks = Keyspace::new(16);
        ks.db(0).put(
            Bytes::from("gone"),
            Value::String(Bytes::from("v")),
            Some(1),
        );
        ks.db(0)
            .put(Bytes::from("stays"), Value::String(Bytes::from("v")), None);

        let mut back = round_trip(&mut ks);
        assert!(back.db(0).get(b"gone").is_none());
        assert!(back.db(0).get(b"stays").is_some());
    }

    #[test]
    fn every_collection_type_survives() {
        let mut ks = Keyspace::new(16);

        let list: VecDeque<Bytes> = [b"a".as_ref(), b"b", b"c"]
            .iter()
            .map(|s| Bytes::copy_from_slice(s))
            .collect();
        ks.db(0)
            .put(Bytes::from("list"), Value::List(list.clone()), None);

        let set: HashSet<Bytes> = ["x", "y", "z"].iter().map(|s| Bytes::from(*s)).collect();
        ks.db(0)
            .put(Bytes::from("set"), Value::Set(set.clone()), None);

        let mut hash = HashMap::new();
        hash.insert(Bytes::from("f1"), Bytes::from("v1"));
        hash.insert(Bytes::from("f2"), Bytes::from("v2"));
        ks.db(0)
            .put(Bytes::from("hash"), Value::Hash(hash.clone()), None);

        let mut zset = ZSet::new();
        zset.insert(Bytes::from("m1"), 1.5);
        zset.insert(Bytes::from("m2"), -0.25);
        zset.insert(Bytes::from("inf"), f64::INFINITY);
        zset.insert(Bytes::from("neginf"), f64::NEG_INFINITY);
        ks.db(0).put(Bytes::from("zset"), Value::ZSet(zset), None);

        let mut back = round_trip(&mut ks);
        let db = back.db(0);

        assert!(matches!(db.get(b"list"), Some(Value::List(l)) if *l == list));
        assert!(matches!(db.get(b"set"), Some(Value::Set(s)) if *s == set));
        assert!(matches!(db.get(b"hash"), Some(Value::Hash(h)) if *h == hash));
        match db.get(b"zset") {
            Some(Value::ZSet(z)) => {
                assert_eq!(z.score(b"m1"), Some(1.5));
                assert_eq!(z.score(b"m2"), Some(-0.25));
                assert_eq!(z.score(b"inf"), Some(f64::INFINITY));
                assert_eq!(z.score(b"neginf"), Some(f64::NEG_INFINITY));
            }
            _ => panic!("zset did not survive"),
        }
    }

    /// Lists keep their order and their duplicates — a set-like round trip
    /// would silently pass a weaker test.
    #[test]
    fn list_order_and_duplicates_survive() {
        let mut ks = Keyspace::new(16);
        let list: VecDeque<Bytes> = ["b", "a", "b", "c", "a"]
            .iter()
            .map(|s| Bytes::from(*s))
            .collect();
        ks.db(0)
            .put(Bytes::from("l"), Value::List(list.clone()), None);
        let mut back = round_trip(&mut ks);
        assert!(matches!(back.db(0).get(b"l"), Some(Value::List(l)) if *l == list));
    }

    #[test]
    fn streams_survive_including_deleted_tail_tracking() {
        let mut ks = Keyspace::new(16);
        let mut s = Stream::default();
        s.entries.insert(
            StreamId { ms: 100, seq: 0 },
            vec![(Bytes::from("f"), Bytes::from("v"))],
        );
        s.entries.insert(
            StreamId { ms: 100, seq: 1 },
            vec![
                (Bytes::from("a"), Bytes::from("1")),
                (Bytes::from("b"), Bytes::from("2")),
            ],
        );
        s.entries.insert(
            StreamId {
                ms: 999_999_999_999,
                seq: 7,
            },
            vec![(Bytes::from("late"), Bytes::from("yes"))],
        );
        s.last_id = StreamId {
            ms: 999_999_999_999,
            seq: 7,
        };
        s.max_deleted_id = StreamId { ms: 50, seq: 3 };

        ks.db(0)
            .put(Bytes::from("stream"), Value::Stream(s.clone()), None);

        let mut back = round_trip(&mut ks);
        match back.db(0).get(b"stream") {
            Some(Value::Stream(got)) => {
                assert_eq!(got.entries, s.entries);
                assert_eq!(got.last_id, s.last_id);
                assert_eq!(got.max_deleted_id, s.max_deleted_id);
            }
            _ => panic!("stream did not survive"),
        }
    }

    #[test]
    fn empty_stream_survives() {
        let mut ks = Keyspace::new(16);
        let s = Stream {
            last_id: StreamId { ms: 5, seq: 5 },
            ..Default::default()
        };
        ks.db(0).put(Bytes::from("s"), Value::Stream(s), None);

        let mut back = round_trip(&mut ks);
        match back.db(0).get(b"s") {
            Some(Value::Stream(got)) => {
                assert!(got.entries.is_empty());
                assert_eq!(got.last_id, StreamId { ms: 5, seq: 5 });
            }
            _ => panic!("empty stream did not survive"),
        }
    }

    /// Keys must land back in the database they came from — the whole reason
    /// this became implementable only after multiple databases existed.
    #[test]
    fn keys_stay_in_their_own_databases() {
        let mut ks = Keyspace::new(16);
        for i in [0usize, 1, 9, 15] {
            ks.db(i).put(
                Bytes::from("where"),
                Value::String(Bytes::from(i.to_string())),
                None,
            );
        }
        let mut back = round_trip(&mut ks);
        for i in [0usize, 1, 9, 15] {
            let expected = i.to_string();
            assert!(
                matches!(back.db(i).get(b"where"), Some(Value::String(s)) if s == &expected),
                "database {i} lost its key"
            );
        }
        // Databases that were never written stay empty rather than picking up
        // stragglers from a mis-tracked SELECT.
        assert_eq!(back.db(2).len(), 0);
    }

    /// A dump that reaches past this server's database count is a
    /// configuration mismatch, and the error should say how to fix it.
    #[test]
    fn selecting_a_database_we_do_not_have_is_a_clear_error() {
        let mut wide = Keyspace::new(16);
        wide.db(15)
            .put(Bytes::from("k"), Value::String(Bytes::from("v")), None);
        let image = to_bytes(&mut wide);

        let mut narrow = Keyspace::new(4);
        match from_bytes(&image, &mut narrow) {
            Err(Error::Unsupported(msg)) => {
                assert!(msg.contains("--databases 16"), "unhelpful message: {msg}");
            }
            other => panic!("expected an Unsupported error, got {other:?}"),
        }
    }

    #[test]
    fn a_flipped_byte_is_caught_by_the_checksum() {
        let mut ks = Keyspace::new(16);
        ks.db(0).put(
            Bytes::from("k"),
            Value::String(Bytes::from("some value here")),
            None,
        );
        let mut image = to_bytes(&mut ks);

        let victim = image.len() / 2;
        image[victim] ^= 0x01;

        let mut back = Keyspace::new(16);
        assert!(matches!(
            from_bytes(&image, &mut back),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut ks = Keyspace::new(16);
        for i in 0..20 {
            ks.db(0).put(
                Bytes::from(format!("key-{i}")),
                Value::String(Bytes::from(format!("value-{i}"))),
                None,
            );
        }
        let image = to_bytes(&mut ks);

        // Every prefix must fail cleanly rather than panicking.
        for cut in (9..image.len()).step_by(7) {
            let mut back = Keyspace::new(16);
            assert!(
                from_bytes(&image[..cut], &mut back).is_err(),
                "a {cut}-byte prefix was accepted as a whole file"
            );
        }
    }

    #[test]
    fn garbage_is_rejected() {
        let mut back = Keyspace::new(16);
        assert!(from_bytes(b"", &mut back).is_err());
        assert!(from_bytes(b"NOTANRDB0011", &mut back).is_err());
        assert!(from_bytes(b"REDIS9999\xff", &mut back).is_err());
    }

    #[test]
    fn empty_keyspace_round_trips() {
        let mut ks = Keyspace::new(16);
        let mut back = round_trip(&mut ks);
        assert_eq!(back.db(0).len(), 0);
    }

    #[test]
    fn save_and_load_through_a_file() {
        let dir = std::env::temp_dir().join(format!("meebis-rdb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.rdb");

        let mut ks = Keyspace::new(16);
        ks.db(3)
            .put(Bytes::from("k"), Value::String(Bytes::from("v")), None);
        save(&path, &mut ks).unwrap();

        let mut back = Keyspace::new(16);
        let stats = load(&path, &mut back).unwrap().expect("file should exist");
        assert_eq!(stats.keys, 1);
        assert!(matches!(back.db(3).get(b"k"), Some(Value::String(s)) if s == "v"));

        // No temp file is left behind by a successful save.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file was not cleaned up");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing file is the normal first boot, not a failure.
    #[test]
    fn loading_a_missing_file_is_not_an_error() {
        let mut ks = Keyspace::new(16);
        let path = std::env::temp_dir().join("meebis-definitely-not-here.rdb");
        assert!(load(&path, &mut ks).unwrap().is_none());
    }
}
