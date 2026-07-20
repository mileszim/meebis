//! A Rust reimplementation of the `cjson` library Redis exposes to scripts,
//! working directly on Lua values.
//!
//! Encoding follows lua-cjson's rules: a table whose keys are exactly the
//! contiguous integers `1..n` encodes as a JSON array, anything else as an
//! object, and the empty table as `{}`. Decoding turns objects into
//! string-keyed tables, arrays into 1-indexed tables, and JSON `null` into
//! `nil` (lua-cjson uses a sentinel; we take the simpler route, which is
//! sufficient for the JSON these scripts actually round-trip).

use mlua::{Lua, Result, Value};

/// Register the global `cjson` table.
pub(super) fn register(lua: &Lua) -> Result<()> {
    let cjson = lua.create_table()?;
    cjson.set(
        "encode",
        lua.create_function(|lua, v: Value| {
            let mut out = Vec::new();
            encode_value(&v, &mut out).map_err(mlua::Error::RuntimeError)?;
            lua.create_string(&out)
        })?,
    )?;
    cjson.set(
        "decode",
        lua.create_function(|lua, s: mlua::String| {
            let bytes = s.as_bytes();
            let mut p = Parser {
                data: &bytes,
                pos: 0,
            };
            p.skip_ws();
            let v = p.parse_value(lua).map_err(mlua::Error::RuntimeError)?;
            p.skip_ws();
            if p.pos != p.data.len() {
                return Err(mlua::Error::RuntimeError(
                    "Expected the end but found trailing garbage".into(),
                ));
            }
            Ok(v)
        })?,
    )?;
    lua.globals().set("cjson", cjson)?;
    Ok(())
}

// --- encoding ---

fn encode_value(v: &Value, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    match v {
        Value::Nil => out.extend_from_slice(b"null"),
        Value::Boolean(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Number(n) => {
            if !n.is_finite() {
                return Err("Cannot serialise number: must not be NaN or Infinity".into());
            }
            out.extend_from_slice(format_number(*n).as_bytes());
        }
        Value::String(s) => encode_string(&s.as_bytes(), out),
        Value::Table(t) => encode_table(t, out)?,
        other => {
            return Err(format!(
                "Cannot serialise {}: type not supported",
                other.type_name()
            ))
        }
    }
    Ok(())
}

fn encode_table(t: &mlua::Table, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    let border = t.raw_len();
    let mut count = 0usize;
    let mut is_array = true;
    for pair in t.pairs::<Value, Value>() {
        let (k, _) = pair.map_err(|e| e.to_string())?;
        count += 1;
        match k {
            Value::Integer(i) if i >= 1 && (i as usize) <= border => {}
            _ => is_array = false,
        }
    }

    if count == 0 {
        out.extend_from_slice(b"{}");
        return Ok(());
    }

    if is_array && count == border {
        out.push(b'[');
        for i in 1..=border {
            if i > 1 {
                out.push(b',');
            }
            let v: Value = t.raw_get(i).map_err(|e| e.to_string())?;
            encode_value(&v, out)?;
        }
        out.push(b']');
    } else {
        out.push(b'{');
        let mut first = true;
        for pair in t.pairs::<Value, Value>() {
            let (k, v) = pair.map_err(|e| e.to_string())?;
            if !first {
                out.push(b',');
            }
            first = false;
            match &k {
                Value::String(s) => encode_string(&s.as_bytes(), out),
                Value::Integer(i) => encode_string(i.to_string().as_bytes(), out),
                Value::Number(n) => encode_string(format_number(*n).as_bytes(), out),
                _ => return Err("Cannot serialise table: key must be a string or number".into()),
            }
            out.push(b':');
            encode_value(&v, out)?;
        }
        out.push(b'}');
    }
    Ok(())
}

fn encode_string(s: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

fn format_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        (n as i64).to_string()
    } else {
        format!("{n}")
    }
}

// --- decoding ---

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.data.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn parse_value(&mut self, lua: &Lua) -> std::result::Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(lua),
            Some(b'[') => self.parse_array(lua),
            Some(b'"') => {
                let bytes = self.parse_string()?;
                Ok(Value::String(
                    lua.create_string(&bytes).map_err(|e| e.to_string())?,
                ))
            }
            Some(b't') => {
                self.expect(b"true")?;
                Ok(Value::Boolean(true))
            }
            Some(b'f') => {
                self.expect(b"false")?;
                Ok(Value::Boolean(false))
            }
            Some(b'n') => {
                self.expect(b"null")?;
                Ok(Value::Nil)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err("unexpected character in JSON".into()),
        }
    }

    fn expect(&mut self, lit: &[u8]) -> std::result::Result<(), String> {
        if self.data[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err("invalid JSON token".into())
        }
    }

    fn parse_number(&mut self) -> std::result::Result<Value, String> {
        let start = self.pos;
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' | b'-' | b'+' => self.pos += 1,
                b'.' | b'e' | b'E' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.data[start..self.pos]).map_err(|_| "bad number")?;
        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Integer(i));
            }
        }
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| "invalid number".into())
    }

    fn parse_string(&mut self) -> std::result::Result<Vec<u8>, String> {
        self.pos += 1; // opening quote
        let mut out = Vec::new();
        loop {
            let c = *self.data.get(self.pos).ok_or("unterminated string")?;
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.data.get(self.pos).ok_or("unterminated escape")?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => self.parse_unicode_escape(&mut out)?,
                        _ => return Err("invalid escape".into()),
                    }
                }
                c => out.push(c),
            }
        }
        Ok(out)
    }

    fn read_hex4(&mut self) -> std::result::Result<u32, String> {
        let slice = self
            .data
            .get(self.pos..self.pos + 4)
            .ok_or("truncated \\u escape")?;
        let s = std::str::from_utf8(slice).map_err(|_| "bad \\u escape")?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "bad \\u escape")?;
        self.pos += 4;
        Ok(v)
    }

    fn parse_unicode_escape(&mut self, out: &mut Vec<u8>) -> std::result::Result<(), String> {
        let hi = self.read_hex4()?;
        let cp = if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate; expect a following low surrogate.
            if self.data.get(self.pos) == Some(&b'\\') && self.data.get(self.pos + 1) == Some(&b'u')
            {
                self.pos += 2;
                let lo = self.read_hex4()?;
                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
            } else {
                hi
            }
        } else {
            hi
        };
        match char::from_u32(cp) {
            Some(ch) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            None => out.extend_from_slice("\u{fffd}".as_bytes()),
        }
        Ok(())
    }

    fn parse_array(&mut self, lua: &Lua) -> std::result::Result<Value, String> {
        self.pos += 1; // '['
        let t = lua.create_table().map_err(|e| e.to_string())?;
        self.skip_ws();
        let mut idx = 1i64;
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Table(t));
        }
        loop {
            let v = self.parse_value(lua)?;
            t.raw_set(idx, v).map_err(|e| e.to_string())?;
            idx += 1;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or ']' in array".into()),
            }
        }
        Ok(Value::Table(t))
    }

    fn parse_object(&mut self, lua: &Lua) -> std::result::Result<Value, String> {
        self.pos += 1; // '{'
        let t = lua.create_table().map_err(|e| e.to_string())?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Table(t));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected string key in object".into());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("expected ':' in object".into());
            }
            self.pos += 1;
            let v = self.parse_value(lua)?;
            // Skip null-valued keys: Lua tables cannot store nil values.
            if !matches!(v, Value::Nil) {
                let k = lua.create_string(&key).map_err(|e| e.to_string())?;
                t.raw_set(k, v).map_err(|e| e.to_string())?;
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or '}' in object".into()),
            }
        }
        Ok(Value::Table(t))
    }
}
