//! Streams — the one type with no plain RDB encoding.
//!
//! Every other value meebis holds can be written in a flat, obvious form that
//! Redis still loads. A stream cannot: `RDB_TYPE_STREAM_LISTPACKS*` is defined
//! in terms of listpacks, so this module is the only place meebis *builds* one.
//!
//! Inside a node's listpack, entries are delta-encoded against a "master entry"
//! at the head: the master carries the field names once, and each entry stores
//! its id as a difference from the node's id plus (usually) just its values.
//! meebis writes the simplest legal shape — one node per entry, each its own
//! master — which trades file size for a writer with no packing decisions in it.
//!
//! ## Consumer groups
//!
//! meebis does not implement them (see [`crate::commands::stream`]). Reading
//! parses the group, PEL, and consumer records exactly — skipping them by size
//! is not possible, the records are variable-length — and then discards them,
//! counting the loss so the caller can warn. Writing always emits zero groups.

use super::cursor::Reader;
use super::packed::{self, Element, ListpackWriter};
use super::{LoadStats, Result};
use crate::db::{Stream, StreamId, Value};
use bytes::Bytes;

const STREAM_LISTPACKS_2: u8 = 19;
const STREAM_LISTPACKS_3: u8 = 21;

/// Entry flags stored as the first element of each listpack entry.
const FLAG_DELETED: i64 = 1;
const FLAG_SAMEFIELDS: i64 = 2;

/// The encoding meebis writes. `_2` is the oldest generation that carries
/// `max_deleted_entry_id`, which meebis tracks and would otherwise lose across
/// a save/load cycle — it is what keeps `XADD` rejecting ids below a deleted
/// tail. `_3` adds only a per-consumer timestamp, and we have no consumers.
pub const WRITE_TYPE: u8 = STREAM_LISTPACKS_2;

// ------------------------------------------------------------------ reading

/// Read a stream value. `type_code` selects which trailing fields are present.
pub fn read(r: &mut Reader, type_code: u8, stats: &mut LoadStats) -> Result<Value> {
    let mut stream = Stream::default();

    let nodes = r.count()?;
    for _ in 0..nodes {
        let key = r.string()?;
        let master_id = stream_id_from_key(&key)
            .ok_or_else(|| r.corrupt("stream node key is not a 16-byte id"))?;
        let blob = r.string()?;
        let elements =
            packed::listpack(&blob).ok_or_else(|| r.corrupt("malformed stream listpack"))?;
        read_node(r, &elements, master_id, &mut stream)?;
    }

    // Bookkeeping that follows the nodes. The stored length is Redis' own
    // cached count; the entries we just decoded are the authority here.
    let _length = r.count_u64()?;
    stream.last_id = StreamId {
        ms: r.count_u64()?,
        seq: r.count_u64()?,
    };

    if type_code >= STREAM_LISTPACKS_2 {
        // first_id is recoverable from the entries themselves, and
        // entries_added is a counter meebis does not model.
        let _first_ms = r.count_u64()?;
        let _first_seq = r.count_u64()?;
        stream.max_deleted_id = StreamId {
            ms: r.count_u64()?,
            seq: r.count_u64()?,
        };
        let _entries_added = r.count_u64()?;
    }

    let groups = r.count()?;
    for _ in 0..groups {
        skip_consumer_group(r, type_code)?;
    }
    stats.dropped_groups += groups;

    Ok(Value::Stream(stream))
}

/// Decode one node's listpack into `stream`.
fn read_node(
    r: &Reader,
    elements: &[Element],
    master_id: StreamId,
    stream: &mut Stream,
) -> Result<()> {
    let int_at = |i: usize| -> Result<i64> {
        elements
            .get(i)
            .and_then(Element::as_int)
            .ok_or_else(|| r.corrupt("stream listpack: expected an integer"))
    };

    // Master entry: count, deleted, num-fields, fields..., 0
    let count = int_at(0)?;
    let deleted = int_at(1)?;
    let num_master_fields = int_at(2)?;
    if count < 0 || deleted < 0 || num_master_fields < 0 {
        return Err(r.corrupt("stream listpack: negative master entry counter"));
    }
    let num_master_fields = num_master_fields as usize;

    let master_fields: Vec<Bytes> = elements
        .get(3..3 + num_master_fields)
        .ok_or_else(|| r.corrupt("stream listpack: truncated master fields"))?
        .iter()
        .cloned()
        .map(Element::into_bytes)
        .collect();

    // The master entry closes with a zero in the lp-count slot.
    if int_at(3 + num_master_fields)? != 0 {
        return Err(r.corrupt("stream listpack: master entry is not zero-terminated"));
    }
    let mut i = 4 + num_master_fields;

    // Both live and tombstoned entries are present and must be walked. The two
    // counters come straight from the file, so saturate rather than overflow —
    // a bogus pair fails on the first out-of-range element read below.
    for _ in 0..count.saturating_add(deleted) {
        let flags = int_at(i)?;
        let ms_diff = int_at(i + 1)?;
        let seq_diff = int_at(i + 2)?;
        i += 3;

        let id = StreamId {
            ms: master_id.ms.wrapping_add(ms_diff as u64),
            seq: master_id.seq.wrapping_add(seq_diff as u64),
        };

        let fields: Vec<(Bytes, Bytes)> = if flags & FLAG_SAMEFIELDS != 0 {
            // Values only; names are inherited from the master entry.
            let values = elements
                .get(i..i + num_master_fields)
                .ok_or_else(|| r.corrupt("stream listpack: truncated entry values"))?;
            i += num_master_fields;
            master_fields
                .iter()
                .cloned()
                .zip(values.iter().cloned().map(Element::into_bytes))
                .collect()
        } else {
            let n = int_at(i)?;
            if n < 0 {
                return Err(r.corrupt("stream listpack: negative field count"));
            }
            let n = n as usize;
            i += 1;
            let flat = elements
                .get(i..i + n * 2)
                .ok_or_else(|| r.corrupt("stream listpack: truncated entry fields"))?;
            i += n * 2;
            flat.chunks(2)
                .map(|pair| (pair[0].clone().into_bytes(), pair[1].clone().into_bytes()))
                .collect()
        };

        i += 1; // the trailing lp-count, only needed for backward traversal

        if flags & FLAG_DELETED == 0 {
            stream.entries.insert(id, fields);
        }
    }

    Ok(())
}

/// Parse and discard one consumer group. The records are variable-length, so
/// this has to decode them properly rather than skipping a byte count.
fn skip_consumer_group(r: &mut Reader, type_code: u8) -> Result<()> {
    r.string()?; // group name
    r.count_u64()?; // last delivered id: ms
    r.count_u64()?; // last delivered id: seq
    if type_code >= STREAM_LISTPACKS_2 {
        r.count_u64()?; // entries_read
    }

    // Global pending-entries list.
    let pending = r.count()?;
    for _ in 0..pending {
        r.take(16)?; // entry id, written raw rather than length-prefixed
        r.u64le()?; // delivery time
        r.count_u64()?; // delivery count
    }

    let consumers = r.count()?;
    for _ in 0..consumers {
        r.string()?; // consumer name
        r.u64le()?; // seen time
        if type_code >= STREAM_LISTPACKS_3 {
            r.u64le()?; // active time
        }
        let owned = r.count()?;
        for _ in 0..owned {
            r.take(16)?; // ids only; the details live in the global PEL
        }
    }
    Ok(())
}

/// A stream id is keyed in the rax by its 16-byte big-endian form.
fn stream_id_from_key(raw: &[u8]) -> Option<StreamId> {
    if raw.len() != 16 {
        return None;
    }
    Some(StreamId {
        ms: u64::from_be_bytes(raw[..8].try_into().ok()?),
        seq: u64::from_be_bytes(raw[8..].try_into().ok()?),
    })
}

// ------------------------------------------------------------------ writing

/// Append a stream body (everything after the key) in [`WRITE_TYPE`] form.
pub fn write(out: &mut Vec<u8>, s: &Stream) {
    use super::write::{put_length, put_raw_string};

    // One node per entry: each is its own master, so every entry can use the
    // SAMEFIELDS shorthand and no packing decisions are needed.
    put_length(out, s.entries.len() as u64);
    for (id, fields) in &s.entries {
        let mut key = Vec::with_capacity(16);
        key.extend_from_slice(&id.ms.to_be_bytes());
        key.extend_from_slice(&id.seq.to_be_bytes());
        put_raw_string(out, &key);

        let mut lp = ListpackWriter::new();
        // Master entry: one live item, none deleted, then the field names.
        lp.int(1);
        lp.int(0);
        lp.int(fields.len() as i64);
        for (field, _) in fields {
            lp.str(field);
        }
        lp.int(0); // master entry terminator

        // The single entry, expressed as a zero delta from its own master.
        lp.int(FLAG_SAMEFIELDS);
        lp.int(0); // ms diff
        lp.int(0); // seq diff
        for (_, value) in fields {
            lp.str(value);
        }
        // lp-count counts this entry's elements excluding itself: the three
        // fixed fields plus one value per field.
        lp.int(fields.len() as i64 + 3);

        put_raw_string(out, &lp.finish());
    }

    put_length(out, s.entries.len() as u64); // length
    put_length(out, s.last_id.ms);
    put_length(out, s.last_id.seq);

    // WRITE_TYPE is LISTPACKS_2, so the extended id block is required.
    let first = s.entries.keys().next().copied().unwrap_or(StreamId::MIN);
    put_length(out, first.ms);
    put_length(out, first.seq);
    put_length(out, s.max_deleted_id.ms);
    put_length(out, s.max_deleted_id.seq);
    // meebis does not count lifetime insertions; the stream's current length is
    // the same default Redis itself substitutes when loading an older dump.
    put_length(out, s.entries.len() as u64);

    put_length(out, 0); // no consumer groups
}
