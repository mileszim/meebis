//! A Rust reimplementation of the `bit` library (Lua BitOp) Redis exposes to
//! scripts. All operations work on 32-bit values and return signed 32-bit
//! results, matching Lua BitOp's semantics on a Lua 5.1 (double) runtime.

use mlua::{Lua, Result, Variadic};

/// Register the global `bit` table.
pub(super) fn register(lua: &Lua) -> Result<()> {
    let bit = lua.create_table()?;

    bit.set("tobit", lua.create_function(|_, x: f64| Ok(tobit(x)))?)?;
    bit.set("bnot", lua.create_function(|_, x: f64| Ok(!tobit(x)))?)?;

    bit.set(
        "band",
        lua.create_function(|_, xs: Variadic<f64>| Ok(fold(xs, -1, |a, b| a & b)))?,
    )?;
    bit.set(
        "bor",
        lua.create_function(|_, xs: Variadic<f64>| Ok(fold(xs, 0, |a, b| a | b)))?,
    )?;
    bit.set(
        "bxor",
        lua.create_function(|_, xs: Variadic<f64>| Ok(fold(xs, 0, |a, b| a ^ b)))?,
    )?;

    bit.set(
        "lshift",
        lua.create_function(|_, (x, n): (f64, i64)| Ok(((tobit(x) as u32) << (n & 31)) as i32))?,
    )?;
    bit.set(
        "rshift",
        lua.create_function(|_, (x, n): (f64, i64)| Ok(((tobit(x) as u32) >> (n & 31)) as i32))?,
    )?;
    bit.set(
        "arshift",
        lua.create_function(|_, (x, n): (f64, i64)| Ok(tobit(x) >> (n & 31)))?,
    )?;
    bit.set(
        "rol",
        lua.create_function(|_, (x, n): (f64, i64)| Ok(tobit(x).rotate_left((n & 31) as u32)))?,
    )?;
    bit.set(
        "ror",
        lua.create_function(|_, (x, n): (f64, i64)| Ok(tobit(x).rotate_right((n & 31) as u32)))?,
    )?;
    bit.set(
        "bswap",
        lua.create_function(|_, x: f64| Ok(tobit(x).swap_bytes()))?,
    )?;

    bit.set(
        "tohex",
        lua.create_function(|_, (x, n): (f64, Option<i64>)| Ok(tohex(x, n)))?,
    )?;

    lua.globals().set("bit", bit)?;
    Ok(())
}

/// Normalise a Lua number to a signed 32-bit integer, as `bit.tobit` does.
fn tobit(x: f64) -> i32 {
    x as i64 as u32 as i32
}

fn fold(xs: Variadic<f64>, init: i32, op: fn(i32, i32) -> i32) -> i32 {
    xs.iter().fold(init, |acc, &x| op(acc, tobit(x)))
}

fn tohex(x: f64, n: Option<i64>) -> String {
    let digits = n.unwrap_or(8);
    let width = (digits.unsigned_abs() as usize).min(8);
    let value = (tobit(x) as u32) as u64;
    let hex = if digits < 0 {
        format!("{value:08X}")
    } else {
        format!("{value:08x}")
    };
    // Keep the low `width` nibbles (BitOp shows the least-significant digits).
    hex[hex.len() - width..].to_string()
}
