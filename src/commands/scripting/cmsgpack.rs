//! A Rust reimplementation of the `cmsgpack` library Redis exposes to scripts.
//!
//! It implements standard MessagePack (the wire format BullMQ's `msgpackr`
//! emits with `useRecords: false`), operating on Lua values the same way
//! lua-cmsgpack does: `pack(...)` concatenates the encodings of each argument
//! and `unpack(s)` returns every value found in the buffer as multiple results.
//! Table encoding uses the array-vs-map rule (contiguous `1..n` integer keys →
//! array, otherwise map).

use mlua::{Lua, MultiValue, Result, Value, Variadic};

/// Register the global `cmsgpack` table.
pub(super) fn register(lua: &Lua) -> Result<()> {
    let cmsgpack = lua.create_table()?;
    cmsgpack.set(
        "pack",
        lua.create_function(|lua, args: Variadic<Value>| {
            let mut out = Vec::new();
            for v in args.iter() {
                encode_value(v, &mut out).map_err(mlua::Error::RuntimeError)?;
            }
            lua.create_string(&out)
        })?,
    )?;
    cmsgpack.set(
        "unpack",
        lua.create_function(|lua, s: mlua::String| {
            let bytes = s.as_bytes();
            let mut dec = Decoder {
                data: &bytes,
                pos: 0,
            };
            let mut values = Vec::new();
            while dec.pos < dec.data.len() {
                values.push(dec.decode(lua).map_err(mlua::Error::RuntimeError)?);
            }
            Ok(MultiValue::from_vec(values))
        })?,
    )?;
    lua.globals().set("cmsgpack", cmsgpack)?;
    Ok(())
}

// --- encoding ---

fn encode_value(v: &Value, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    match v {
        Value::Nil => out.push(0xc0),
        Value::Boolean(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Value::Integer(i) => encode_int(*i, out),
        Value::Number(n) => {
            // lua-cmsgpack packs integral numbers as integers, else as a double.
            if n.is_finite() && *n == n.trunc() && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                encode_int(*n as i64, out);
            } else {
                out.push(0xcb);
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
        Value::String(s) => encode_str(&s.as_bytes(), out),
        Value::Table(t) => encode_table(t, out)?,
        other => return Err(format!("cannot msgpack {}", other.type_name())),
    }
    Ok(())
}

fn encode_int(i: i64, out: &mut Vec<u8>) {
    if (0..=127).contains(&i) {
        out.push(i as u8); // positive fixint
    } else if (-32..0).contains(&i) {
        out.push((i as i8) as u8); // negative fixint
    } else if i >= i8::MIN as i64 && i <= i8::MAX as i64 {
        out.push(0xd0);
        out.push(i as i8 as u8);
    } else if i >= i16::MIN as i64 && i <= i16::MAX as i64 {
        out.push(0xd1);
        out.extend_from_slice(&(i as i16).to_be_bytes());
    } else if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
        out.push(0xd2);
        out.extend_from_slice(&(i as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

fn encode_str(s: &[u8], out: &mut Vec<u8>) {
    let len = s.len();
    if len < 32 {
        out.push(0xa0 | len as u8);
    } else if len < 256 {
        out.push(0xd9);
        out.push(len as u8);
    } else if len < 65536 {
        out.push(0xda);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(s);
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

    if is_array && count == border {
        encode_array_header(border, out);
        for i in 1..=border {
            let v: Value = t.raw_get(i).map_err(|e| e.to_string())?;
            encode_value(&v, out)?;
        }
    } else {
        encode_map_header(count, out);
        for pair in t.pairs::<Value, Value>() {
            let (k, v) = pair.map_err(|e| e.to_string())?;
            encode_value(&k, out)?;
            encode_value(&v, out)?;
        }
    }
    Ok(())
}

fn encode_array_header(n: usize, out: &mut Vec<u8>) {
    if n < 16 {
        out.push(0x90 | n as u8);
    } else if n < 65536 {
        out.push(0xdc);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0xdd);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
}

fn encode_map_header(n: usize, out: &mut Vec<u8>) {
    if n < 16 {
        out.push(0x80 | n as u8);
    } else if n < 65536 {
        out.push(0xde);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0xdf);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    }
}

// --- decoding ---

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn take(&mut self, n: usize) -> std::result::Result<&[u8], String> {
        let slice = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or("truncated msgpack input")?;
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> std::result::Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn be_u(&mut self, n: usize) -> std::result::Result<u64, String> {
        let mut v = 0u64;
        for &b in self.take(n)? {
            v = (v << 8) | b as u64;
        }
        Ok(v)
    }

    fn decode(&mut self, lua: &Lua) -> std::result::Result<Value, String> {
        let tag = self.u8()?;
        match tag {
            0x00..=0x7f => Ok(Value::Integer(tag as i64)), // positive fixint
            0xe0..=0xff => Ok(Value::Integer((tag as i8) as i64)), // negative fixint
            0x80..=0x8f => self.decode_map(lua, (tag & 0x0f) as usize),
            0x90..=0x9f => self.decode_array(lua, (tag & 0x0f) as usize),
            0xa0..=0xbf => self.decode_str(lua, (tag & 0x1f) as usize),
            0xc0 => Ok(Value::Nil),
            0xc2 => Ok(Value::Boolean(false)),
            0xc3 => Ok(Value::Boolean(true)),
            0xc4 => {
                let n = self.u8()? as usize;
                self.decode_str(lua, n)
            }
            0xc5 => {
                let n = self.be_u(2)? as usize;
                self.decode_str(lua, n)
            }
            0xc6 => {
                let n = self.be_u(4)? as usize;
                self.decode_str(lua, n)
            }
            0xca => {
                let bits = self.be_u(4)? as u32;
                Ok(Value::Number(f32::from_bits(bits) as f64))
            }
            0xcb => {
                let bits = self.be_u(8)?;
                Ok(Value::Number(f64::from_bits(bits)))
            }
            0xcc => Ok(Value::Integer(self.u8()? as i64)),
            0xcd => Ok(Value::Integer(self.be_u(2)? as i64)),
            0xce => Ok(Value::Integer(self.be_u(4)? as i64)),
            0xcf => {
                let v = self.be_u(8)?;
                Ok(int_or_number(v as i128))
            }
            0xd0 => Ok(Value::Integer((self.u8()? as i8) as i64)),
            0xd1 => Ok(Value::Integer((self.be_u(2)? as u16 as i16) as i64)),
            0xd2 => Ok(Value::Integer((self.be_u(4)? as u32 as i32) as i64)),
            0xd3 => Ok(Value::Integer(self.be_u(8)? as i64)),
            0xd9 => {
                let n = self.u8()? as usize;
                self.decode_str(lua, n)
            }
            0xda => {
                let n = self.be_u(2)? as usize;
                self.decode_str(lua, n)
            }
            0xdb => {
                let n = self.be_u(4)? as usize;
                self.decode_str(lua, n)
            }
            0xdc => {
                let n = self.be_u(2)? as usize;
                self.decode_array(lua, n)
            }
            0xdd => {
                let n = self.be_u(4)? as usize;
                self.decode_array(lua, n)
            }
            0xde => {
                let n = self.be_u(2)? as usize;
                self.decode_map(lua, n)
            }
            0xdf => {
                let n = self.be_u(4)? as usize;
                self.decode_map(lua, n)
            }
            other => Err(format!("unsupported msgpack tag {other:#x}")),
        }
    }

    fn decode_str(&mut self, lua: &Lua, n: usize) -> std::result::Result<Value, String> {
        let bytes = self.take(n)?;
        Ok(Value::String(
            lua.create_string(bytes).map_err(|e| e.to_string())?,
        ))
    }

    fn decode_array(&mut self, lua: &Lua, n: usize) -> std::result::Result<Value, String> {
        let t = lua.create_table().map_err(|e| e.to_string())?;
        for i in 1..=n {
            let v = self.decode(lua)?;
            t.raw_set(i, v).map_err(|e| e.to_string())?;
        }
        Ok(Value::Table(t))
    }

    fn decode_map(&mut self, lua: &Lua, n: usize) -> std::result::Result<Value, String> {
        let t = lua.create_table().map_err(|e| e.to_string())?;
        for _ in 0..n {
            let k = self.decode(lua)?;
            let v = self.decode(lua)?;
            if !matches!(v, Value::Nil) {
                t.raw_set(k, v).map_err(|e| e.to_string())?;
            }
        }
        Ok(Value::Table(t))
    }
}

/// A msgpack uint64 that overflows `i64` falls back to a float, matching how
/// lua-cmsgpack surfaces large unsigned values on a Lua 5.1 (double) runtime.
fn int_or_number(v: i128) -> Value {
    if v <= i64::MAX as i128 {
        Value::Integer(v as i64)
    } else {
        Value::Number(v as f64)
    }
}
