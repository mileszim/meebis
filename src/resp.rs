//! RESP (REdis Serialization Protocol) parsing and encoding.
//!
//! Requests are parsed as either a RESP array of bulk strings (what real
//! clients send) or a whitespace-separated inline command (handy for `nc` /
//! `telnet` debugging). Replies are built as [`Frame`]s and encoded as RESP2
//! or RESP3 depending on what the connection negotiated via `HELLO`.

use bytes::{Buf, Bytes, BytesMut};

/// A RESP value used to build replies. Encoding differs between RESP2 and
/// RESP3 for several variants (`Null`, `Map`, `Set`, `Double`, ...); the
/// [`Frame::encode`] method picks the right wire form.
#[derive(Debug, Clone)]
pub enum Frame {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Bytes),
    /// Null bulk string (`$-1` in RESP2, `_` in RESP3).
    Null,
    /// Null array (`*-1` in RESP2, `_` in RESP3).
    NullArray,
    Array(Vec<Frame>),
    /// Key/value pairs. RESP2: flat array of 2n items. RESP3: `%` map.
    Map(Vec<(Frame, Frame)>),
    /// A set. RESP2: array. RESP3: `~` set.
    Set(Vec<Frame>),
    /// A floating point number. RESP2: bulk string. RESP3: `,` double.
    Double(f64),
    /// Two-column pairs (`ZRANGE ... WITHSCORES`, `ZPOPMIN count`,
    /// `HRANDFIELD ... WITHVALUES`, ...). RESP2: a flat array of `2n` items.
    /// RESP3: an array of `n` two-element arrays.
    Pairs(Vec<(Frame, Frame)>),
    /// Out-of-band push (pub/sub). RESP2: array. RESP3: `>` push.
    Push(Vec<Frame>),
}

impl Frame {
    pub fn ok() -> Frame {
        Frame::Simple("OK".into())
    }

    /// Build a bulk string from anything byte-like.
    pub fn bulk(data: impl Into<Bytes>) -> Frame {
        Frame::Bulk(data.into())
    }

    /// A generic `-ERR <msg>` error.
    pub fn err(msg: impl Into<String>) -> Frame {
        Frame::Error(format!("ERR {}", msg.into()))
    }

    /// The standard wrong-type error.
    pub fn wrongtype() -> Frame {
        Frame::Error("WRONGTYPE Operation against a key holding the wrong kind of value".into())
    }

    /// Encode this frame onto `out`, using RESP3 forms when `resp3` is true.
    pub fn encode(&self, resp3: bool, out: &mut BytesMut) {
        match self {
            Frame::Simple(s) => {
                out.extend_from_slice(b"+");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Frame::Error(s) => {
                out.extend_from_slice(b"-");
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Frame::Integer(n) => {
                out.extend_from_slice(b":");
                out.extend_from_slice(n.to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Frame::Bulk(data) => encode_bulk(data, out),
            Frame::Null => {
                if resp3 {
                    out.extend_from_slice(b"_\r\n");
                } else {
                    out.extend_from_slice(b"$-1\r\n");
                }
            }
            Frame::NullArray => {
                if resp3 {
                    out.extend_from_slice(b"_\r\n");
                } else {
                    out.extend_from_slice(b"*-1\r\n");
                }
            }
            Frame::Array(items) => {
                encode_header(b'*', items.len(), out);
                for item in items {
                    item.encode(resp3, out);
                }
            }
            Frame::Set(items) => {
                let marker = if resp3 { b'~' } else { b'*' };
                encode_header(marker, items.len(), out);
                for item in items {
                    item.encode(resp3, out);
                }
            }
            Frame::Push(items) => {
                let marker = if resp3 { b'>' } else { b'*' };
                encode_header(marker, items.len(), out);
                for item in items {
                    item.encode(resp3, out);
                }
            }
            Frame::Map(pairs) => {
                if resp3 {
                    encode_header(b'%', pairs.len(), out);
                } else {
                    encode_header(b'*', pairs.len() * 2, out);
                }
                for (k, v) in pairs {
                    k.encode(resp3, out);
                    v.encode(resp3, out);
                }
            }
            Frame::Double(d) => encode_double(*d, resp3, out),
            Frame::Pairs(pairs) => {
                if resp3 {
                    encode_header(b'*', pairs.len(), out);
                    for (a, b) in pairs {
                        out.extend_from_slice(b"*2\r\n");
                        a.encode(true, out);
                        b.encode(true, out);
                    }
                } else {
                    encode_header(b'*', pairs.len() * 2, out);
                    for (a, b) in pairs {
                        a.encode(false, out);
                        b.encode(false, out);
                    }
                }
            }
        }
    }
}

fn encode_double(d: f64, resp3: bool, out: &mut BytesMut) {
    if resp3 {
        out.extend_from_slice(b",");
        out.extend_from_slice(format_double(d).as_bytes());
        out.extend_from_slice(b"\r\n");
    } else {
        encode_bulk(format_double(d).as_bytes(), out);
    }
}

fn encode_header(marker: u8, len: usize, out: &mut BytesMut) {
    out.extend_from_slice(&[marker]);
    out.extend_from_slice(len.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn encode_bulk(data: &[u8], out: &mut BytesMut) {
    out.extend_from_slice(b"$");
    out.extend_from_slice(data.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
}

/// Format a float the way Redis reports scores: `inf`/`-inf` for infinities,
/// no decimal point for integral values, otherwise a shortest round-trip form.
pub fn format_double(d: f64) -> String {
    if d.is_nan() {
        "nan".into()
    } else if d.is_infinite() {
        if d > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        }
    } else if d == d.trunc() && d.abs() < 1e17 {
        format!("{}", d as i64)
    } else {
        format!("{}", d)
    }
}

/// Errors from the incremental command parser.
#[derive(Debug)]
pub enum ParseError {
    /// The buffer does not yet hold a complete request; wait for more bytes.
    Incomplete,
    /// The bytes are not valid RESP; the connection should be closed.
    Protocol(String),
}

/// Try to parse a single command (a vector of argument byte-strings) from the
/// front of `buf`. On success the consumed bytes are removed from `buf`.
/// Returns `Ok(None)` when more data is needed.
pub fn parse_command(buf: &mut BytesMut) -> Result<Option<Vec<Bytes>>, ParseError> {
    if buf.is_empty() {
        return Ok(None);
    }
    let result = if buf[0] == b'*' {
        parse_array(&buf[..])
    } else {
        parse_inline(&buf[..])
    };
    match result {
        Ok((args, consumed)) => {
            buf.advance(consumed);
            Ok(Some(args))
        }
        Err(ParseError::Incomplete) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Find the index of the next `\n` at or after `start`.
fn find_newline(buf: &[u8], start: usize) -> Option<usize> {
    buf[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| start + p)
}

/// Strip a single trailing `\r` from a line slice.
fn trim_cr(line: &[u8]) -> &[u8] {
    if let Some((&b'\r', rest)) = line.split_last() {
        rest
    } else {
        line
    }
}

fn parse_int(bytes: &[u8]) -> Result<i64, ParseError> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .ok_or_else(|| ParseError::Protocol("invalid multibulk length".into()))
}

/// Parse a RESP array of bulk strings. Returns the args and bytes consumed.
fn parse_array(buf: &[u8]) -> Result<(Vec<Bytes>, usize), ParseError> {
    let nl = find_newline(buf, 0).ok_or(ParseError::Incomplete)?;
    let count = parse_int(trim_cr(&buf[1..nl]))?;
    let mut pos = nl + 1;
    if count <= 0 {
        return Ok((Vec::new(), pos));
    }
    let mut args = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if pos >= buf.len() {
            return Err(ParseError::Incomplete);
        }
        if buf[pos] != b'$' {
            return Err(ParseError::Protocol(format!(
                "expected '$', got '{}'",
                buf[pos] as char
            )));
        }
        let nl = find_newline(buf, pos).ok_or(ParseError::Incomplete)?;
        let len = parse_int(trim_cr(&buf[pos + 1..nl]))?;
        pos = nl + 1;
        if len < 0 {
            args.push(Bytes::new());
            continue;
        }
        let len = len as usize;
        if buf.len() < pos + len + 2 {
            return Err(ParseError::Incomplete);
        }
        let data = Bytes::copy_from_slice(&buf[pos..pos + len]);
        pos += len;
        if &buf[pos..pos + 2] != b"\r\n" {
            return Err(ParseError::Protocol(
                "expected CRLF after bulk string".into(),
            ));
        }
        pos += 2;
        args.push(data);
    }
    Ok((args, pos))
}

/// Parse an inline command: a single line split on whitespace, honoring
/// simple double/single quoting like `redis-cli` does.
fn parse_inline(buf: &[u8]) -> Result<(Vec<Bytes>, usize), ParseError> {
    let nl = find_newline(buf, 0).ok_or(ParseError::Incomplete)?;
    let line = trim_cr(&buf[0..nl]);
    let args = split_inline(line)?;
    Ok((args, nl + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(frame: Frame, resp3: bool) -> Vec<u8> {
        let mut out = BytesMut::new();
        frame.encode(resp3, &mut out);
        out.to_vec()
    }

    #[test]
    fn parses_resp_array() {
        let mut buf = BytesMut::from(&b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n"[..]);
        let cmd = parse_command(&mut buf).unwrap().unwrap();
        assert_eq!(cmd, vec![Bytes::from("GET"), Bytes::from("foo")]);
        assert!(buf.is_empty());
    }

    #[test]
    fn waits_for_incomplete_frames() {
        let mut buf = BytesMut::from(&b"*2\r\n$3\r\nGET\r\n$3\r\nfo"[..]);
        assert!(parse_command(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"o\r\n");
        let cmd = parse_command(&mut buf).unwrap().unwrap();
        assert_eq!(cmd, vec![Bytes::from("GET"), Bytes::from("foo")]);
    }

    #[test]
    fn parses_two_pipelined_commands() {
        let mut buf = BytesMut::from(&b"*1\r\n$4\r\nPING\r\n*1\r\n$4\r\nPING\r\n"[..]);
        assert_eq!(
            parse_command(&mut buf).unwrap().unwrap(),
            vec![Bytes::from("PING")]
        );
        assert_eq!(
            parse_command(&mut buf).unwrap().unwrap(),
            vec![Bytes::from("PING")]
        );
        assert!(parse_command(&mut buf).unwrap().is_none());
    }

    #[test]
    fn parses_inline_with_quotes() {
        let mut buf = BytesMut::from(&b"SET x \"a b\"\r\n"[..]);
        let cmd = parse_command(&mut buf).unwrap().unwrap();
        assert_eq!(
            cmd,
            vec![Bytes::from("SET"), Bytes::from("x"), Bytes::from("a b")]
        );
    }

    #[test]
    fn parses_bare_inline_and_lone_newline() {
        let mut buf = BytesMut::from(&b"PING\n"[..]);
        assert_eq!(
            parse_command(&mut buf).unwrap().unwrap(),
            vec![Bytes::from("PING")]
        );
    }

    #[test]
    fn encodes_resp2_types() {
        assert_eq!(enc(Frame::Simple("OK".into()), false), b"+OK\r\n");
        assert_eq!(enc(Frame::Integer(42), false), b":42\r\n");
        assert_eq!(enc(Frame::bulk("hi"), false), b"$2\r\nhi\r\n");
        assert_eq!(enc(Frame::Null, false), b"$-1\r\n");
        assert_eq!(enc(Frame::NullArray, false), b"*-1\r\n");
    }

    #[test]
    fn null_and_map_differ_by_protocol() {
        assert_eq!(enc(Frame::Null, true), b"_\r\n");
        let map = Frame::Map(vec![(Frame::bulk("a"), Frame::Integer(1))]);
        assert_eq!(enc(map.clone(), false), b"*2\r\n$1\r\na\r\n:1\r\n");
        assert_eq!(enc(map, true), b"%1\r\n$1\r\na\r\n:1\r\n");
    }

    #[test]
    fn double_is_bulk_in_resp2() {
        assert_eq!(enc(Frame::Double(1.5), false), b"$3\r\n1.5\r\n");
        assert_eq!(enc(Frame::Double(1.5), true), b",1.5\r\n");
    }

    #[test]
    fn format_double_matches_redis_style() {
        assert_eq!(format_double(3.0), "3");
        assert_eq!(format_double(3.5), "3.5");
        assert_eq!(format_double(f64::INFINITY), "inf");
        assert_eq!(format_double(f64::NEG_INFINITY), "-inf");
        assert_eq!(format_double(3000.0), "3000");
    }
}

fn split_inline(line: &[u8]) -> Result<Vec<Bytes>, ParseError> {
    let mut args = Vec::new();
    let mut i = 0;
    let n = line.len();
    while i < n {
        // Skip leading whitespace.
        while i < n && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let mut cur = Vec::new();
        match line[i] {
            b'"' => {
                i += 1;
                loop {
                    if i >= n {
                        return Err(ParseError::Protocol("unbalanced quotes".into()));
                    }
                    match line[i] {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' if i + 1 < n => {
                            i += 1;
                            cur.push(match line[i] {
                                b'n' => b'\n',
                                b'r' => b'\r',
                                b't' => b'\t',
                                b'b' => 0x08,
                                b'a' => 0x07,
                                other => other,
                            });
                            i += 1;
                        }
                        c => {
                            cur.push(c);
                            i += 1;
                        }
                    }
                }
            }
            b'\'' => {
                i += 1;
                loop {
                    if i >= n {
                        return Err(ParseError::Protocol("unbalanced quotes".into()));
                    }
                    match line[i] {
                        b'\'' => {
                            i += 1;
                            break;
                        }
                        b'\\' if i + 1 < n && line[i + 1] == b'\'' => {
                            cur.push(b'\'');
                            i += 2;
                        }
                        c => {
                            cur.push(c);
                            i += 1;
                        }
                    }
                }
            }
            _ => {
                while i < n && !line[i].is_ascii_whitespace() {
                    cur.push(line[i]);
                    i += 1;
                }
            }
        }
        args.push(Bytes::from(cur));
    }
    Ok(args)
}
