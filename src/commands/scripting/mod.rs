//! Redis-compatible Lua scripting: `EVAL` / `EVALSHA` / `SCRIPT`, plus the
//! sandboxed `redis`, `cjson`, `cmsgpack`, and `bit` libraries scripts expect.
//!
//! Scripts run on an embedded Lua 5.1 (Redis' scripting version). The keyspace
//! lock is already held by [`super::execute`] when we get here, so a whole
//! script executes atomically and each `redis.call` re-enters
//! [`super::execute_locked`] against that same locked keyspace — exactly how
//! Redis' single thread runs a script start to finish without interleaving.

mod cjson;
mod cmsgpack;
mod luabit;

use super::{parse_int, upper, wrong_args};
use crate::db::Db;
use crate::resp::{format_double, Frame};
use crate::server::{ConnState, Shared};
use crate::sha1::sha1_hex;
use bytes::Bytes;
use mlua::{Function, Lua, LuaOptions, StdLib, Table, Value, Variadic};
use std::cell::RefCell;

/// Where an `EVAL`-family command gets its script body.
pub(crate) enum Source {
    /// `EVAL`: the body is inline (arg 1) and is cached under its SHA.
    Body,
    /// `EVALSHA`: arg 1 is a SHA that must already be in the cache.
    Sha,
}

/// `redis.call` is a thin Lua wrapper over `redis.pcall` that turns an error
/// reply table into a raised Lua error, matching Redis' semantics (a raised
/// `{err=...}` is catchable by `pcall` as a table).
const REDIS_CALL_WRAPPER: &str = r#"
local pcall_fn = ...
return function(...)
  local reply = pcall_fn(...)
  if type(reply) == 'table' and reply.err ~= nil then
    return error(reply)
  end
  return reply
end
"#;

/// The `SCRIPT` command family: `LOAD`, `EXISTS`, `FLUSH`.
pub(crate) fn script(shared: &Shared, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("script");
    }
    match upper(&args[1]).as_str() {
        "LOAD" => {
            if args.len() != 3 {
                return unknown_subcommand("load");
            }
            let body = args[2].clone();
            if let Err(e) = compile_check(&body) {
                return Frame::err(format!("Error compiling script (new function): {e}"));
            }
            let sha = sha1_hex(&body);
            shared.scripts.lock().unwrap().insert(sha.clone(), body);
            Frame::Bulk(Bytes::from(sha))
        }
        "EXISTS" => {
            let cache = shared.scripts.lock().unwrap();
            Frame::Array(
                args[2..]
                    .iter()
                    .map(|a| {
                        let sha = String::from_utf8_lossy(a).to_ascii_lowercase();
                        Frame::Integer(cache.contains_key(&sha) as i64)
                    })
                    .collect(),
            )
        }
        "FLUSH" => {
            // An optional ASYNC | SYNC modifier is accepted and ignored.
            shared.scripts.lock().unwrap().clear();
            Frame::ok()
        }
        other => unknown_subcommand(&other.to_lowercase()),
    }
}

/// `EVAL` / `EVALSHA` (and their `_RO` aliases).
pub(crate) fn eval(
    db: &mut Db,
    shared: &Shared,
    conn: &mut ConnState,
    args: &[Bytes],
    source: Source,
) -> Frame {
    // <EVAL script | EVALSHA sha> numkeys [key ...] [arg ...]
    if args.len() < 3 {
        return wrong_args(match source {
            Source::Body => "eval",
            Source::Sha => "evalsha",
        });
    }
    let numkeys = match parse_int(&args[2]) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if numkeys < 0 {
        return Frame::err("Number of keys can't be negative");
    }
    let numkeys = numkeys as usize;
    let rest = &args[3..];
    if numkeys > rest.len() {
        return Frame::err("Number of keys can't be greater than number of args");
    }
    let keys = rest[..numkeys].to_vec();
    let argv = rest[numkeys..].to_vec();

    let body: Bytes = match source {
        Source::Sha => {
            let sha = String::from_utf8_lossy(&args[1]).to_ascii_lowercase();
            match shared.scripts.lock().unwrap().get(&sha) {
                Some(b) => b.clone(),
                None => {
                    return Frame::Error("NOSCRIPT No matching script. Please use EVAL.".into())
                }
            }
        }
        Source::Body => {
            let body = args[1].clone();
            let sha = sha1_hex(&body);
            shared.scripts.lock().unwrap().insert(sha, body.clone());
            body
        }
    };

    run_script(db, shared, conn, &body, keys, argv)
}

/// The mutable state a script's `redis.call`/`pcall` needs, shared through a
/// `RefCell` so a single scoped Lua closure can reach it across calls.
struct Ctx<'a> {
    db: &'a mut Db,
    conn: &'a mut ConnState,
}

fn run_script(
    db: &mut Db,
    shared: &Shared,
    conn: &mut ConnState,
    body: &[u8],
    keys: Vec<Bytes>,
    argv: Vec<Bytes>,
) -> Frame {
    let lua = match new_sandbox() {
        Ok(l) => l,
        Err(e) => return Frame::err(format!("Error creating Lua environment: {e}")),
    };
    if let Err(e) = set_keys_argv(&lua, &keys, &argv) {
        return Frame::err(format!("Error preparing script: {e}"));
    }

    let ctx = RefCell::new(Ctx { db, conn });

    let result: mlua::Result<Frame> = lua.scope(|scope| {
        let pcall = scope.create_function_mut(|lua, cmd: Variadic<Value>| {
            let mut guard = ctx.borrow_mut();
            let c: &mut Ctx = &mut guard;
            redis_pcall(lua, shared, &mut *c.db, &mut *c.conn, cmd)
        })?;

        let redis: Table = lua.globals().get("redis")?;
        let call: Function = lua
            .load(REDIS_CALL_WRAPPER)
            .set_name("@redis.call")
            .call(pcall.clone())?;
        redis.set("pcall", pcall)?;
        redis.set("call", call)?;

        let user_fn = lua.load(body).set_name("@user_script").into_function()?;
        // Run under pcall so we can recover the error value (table or string).
        let runner: Function = lua
            .load("local f = ...\nlocal ok, res = pcall(f)\nreturn ok, res")
            .set_name("@__run")
            .into_function()?;
        let (ok, value): (bool, Value) = runner.call(user_fn)?;
        Ok(if ok {
            lua_to_frame(value)
        } else {
            error_value_to_frame(value)
        })
    });

    match result {
        Ok(frame) => frame,
        // The user script is compiled before it runs, so a syntax error lands
        // here; runtime errors are caught by the pcall trampoline above.
        Err(e) => Frame::err(format!("Error compiling script (new function): {e}")),
    }
}

/// A fresh sandbox: base + table/string/math (Redis' subset — no `os`/`io`/
/// `debug`; on Lua 5.1 `coroutine` comes with the base library), with `redis`,
/// `cjson`, `cmsgpack`, and `bit` registered.
/// `redis.call`/`pcall` are added per-run inside the scope, since they borrow
/// the keyspace.
fn new_sandbox() -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH,
        LuaOptions::default(),
    )?;
    let redis = lua.create_table()?;
    register_redis_helpers(&lua, &redis)?;
    lua.globals().set("redis", redis)?;
    cjson::register(&lua)?;
    cmsgpack::register(&lua)?;
    luabit::register(&lua)?;
    Ok(lua)
}

fn compile_check(body: &[u8]) -> Result<(), String> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH,
        LuaOptions::default(),
    )
    .map_err(|e| e.to_string())?;
    lua.load(body)
        .set_name("@user_script")
        .into_function()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn register_redis_helpers(lua: &Lua, redis: &Table) -> mlua::Result<()> {
    redis.set(
        "error_reply",
        lua.create_function(|lua, msg: mlua::String| {
            let t = lua.create_table()?;
            t.set("err", msg)?;
            Ok(t)
        })?,
    )?;
    redis.set(
        "status_reply",
        lua.create_function(|lua, msg: mlua::String| {
            let t = lua.create_table()?;
            t.set("ok", msg)?;
            Ok(t)
        })?,
    )?;
    redis.set(
        "sha1hex",
        lua.create_function(|_, s: mlua::String| Ok(sha1_hex(&s.as_bytes())))?,
    )?;
    // Logging and replication controls are no-ops in a single-node dev server.
    redis.set("log", lua.create_function(|_, _: Variadic<Value>| Ok(()))?)?;
    redis.set("replicate_commands", lua.create_function(|_, ()| Ok(true))?)?;
    redis.set("set_repl", lua.create_function(|_, _: Value| Ok(()))?)?;
    redis.set("setresp", lua.create_function(|_, _: Value| Ok(()))?)?;
    redis.set(
        "breakpoint",
        lua.create_function(|_, _: Variadic<Value>| Ok(false))?,
    )?;
    redis.set(
        "debug",
        lua.create_function(|_, _: Variadic<Value>| Ok(()))?,
    )?;

    for (name, level) in [
        ("LOG_DEBUG", 0),
        ("LOG_VERBOSE", 1),
        ("LOG_NOTICE", 2),
        ("LOG_WARNING", 3),
    ] {
        redis.set(name, level)?;
    }
    for (name, val) in [
        ("REPL_NONE", 0),
        ("REPL_AOF", 1),
        ("REPL_SLAVE", 2),
        ("REPL_REPLICA", 2),
        ("REPL_ALL", 3),
    ] {
        redis.set(name, val)?;
    }
    Ok(())
}

fn set_keys_argv(lua: &Lua, keys: &[Bytes], argv: &[Bytes]) -> mlua::Result<()> {
    let kt = lua.create_table()?;
    for (i, k) in keys.iter().enumerate() {
        kt.set(i + 1, lua.create_string(&k[..])?)?;
    }
    let at = lua.create_table()?;
    for (i, a) in argv.iter().enumerate() {
        at.set(i + 1, lua.create_string(&a[..])?)?;
    }
    lua.globals().set("KEYS", kt)?;
    lua.globals().set("ARGV", at)?;
    Ok(())
}

/// Execute one `redis.call`/`pcall`. Command errors come back as a `{err=...}`
/// table (never a raised Lua error); `redis.call`'s Lua wrapper is what turns
/// that into a raise. Malformed calls (non-string args, blocked commands) do
/// raise, as Redis does.
fn redis_pcall(
    lua: &Lua,
    shared: &Shared,
    db: &mut Db,
    conn: &mut ConnState,
    cmd: Variadic<Value>,
) -> mlua::Result<Value> {
    if cmd.is_empty() {
        return Err(mlua::Error::RuntimeError(
            "Please specify at least one argument for this redis lib call".into(),
        ));
    }
    let mut argv: Vec<Bytes> = Vec::with_capacity(cmd.len());
    for v in cmd.iter() {
        match value_to_arg(v) {
            Some(b) => argv.push(b),
            None => {
                return Err(mlua::Error::RuntimeError(
                    "Lua redis lib command arguments must be strings or integers".into(),
                ))
            }
        }
    }
    let name = String::from_utf8_lossy(&argv[0]).to_ascii_uppercase();
    if is_blocked(&name) {
        return Err(mlua::Error::RuntimeError(
            "This Redis command is not allowed from script".into(),
        ));
    }
    let frame = super::execute_locked(db, shared, conn, &name, &argv);
    frame_to_lua(lua, frame)
}

/// Commands Redis refuses to run from inside a script.
fn is_blocked(name: &str) -> bool {
    matches!(
        name,
        "EVAL"
            | "EVAL_RO"
            | "EVALSHA"
            | "EVALSHA_RO"
            | "SCRIPT"
            | "FUNCTION"
            | "FCALL"
            | "FCALL_RO"
            | "MULTI"
            | "EXEC"
            | "DISCARD"
            | "WATCH"
            | "UNWATCH"
            | "SUBSCRIBE"
            | "UNSUBSCRIBE"
            | "PSUBSCRIBE"
            | "PUNSUBSCRIBE"
            | "SSUBSCRIBE"
            | "SUNSUBSCRIBE"
    )
}

fn value_to_arg(v: &Value) -> Option<Bytes> {
    match v {
        Value::String(s) => Some(Bytes::copy_from_slice(&s.as_bytes())),
        Value::Integer(i) => Some(Bytes::from(i.to_string())),
        Value::Number(n) => Some(Bytes::from(number_to_arg(*n))),
        _ => None,
    }
}

fn number_to_arg(n: f64) -> String {
    if n.is_finite() && n == n.trunc() && n.abs() < 1e15 {
        (n as i64).to_string()
    } else {
        format!("{n}")
    }
}

/// Convert a command reply to a Lua value, applying RESP2 rules (the mode
/// scripts see by default): status → `{ok=}`, error → `{err=}`, nil → false,
/// maps/pairs/sets flatten to arrays, doubles become strings.
fn frame_to_lua(lua: &Lua, frame: Frame) -> mlua::Result<Value> {
    Ok(match frame {
        Frame::Integer(n) => Value::Integer(n),
        Frame::Simple(s) => {
            let t = lua.create_table()?;
            t.set("ok", s)?;
            Value::Table(t)
        }
        Frame::Error(s) => {
            let t = lua.create_table()?;
            t.set("err", s)?;
            Value::Table(t)
        }
        Frame::Bulk(b) => Value::String(lua.create_string(&b[..])?),
        Frame::Null | Frame::NullArray => Value::Boolean(false),
        Frame::Double(d) => Value::String(lua.create_string(format_double(d))?),
        Frame::Array(items) | Frame::Set(items) | Frame::Push(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.into_iter().enumerate() {
                t.set(i + 1, frame_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
        Frame::Map(pairs) | Frame::Pairs(pairs) => {
            let t = lua.create_table()?;
            let mut idx = 1;
            for (k, v) in pairs {
                t.set(idx, frame_to_lua(lua, k)?)?;
                idx += 1;
                t.set(idx, frame_to_lua(lua, v)?)?;
                idx += 1;
            }
            Value::Table(t)
        }
        Frame::XReadReply(pairs) => {
            // Scripts see the RESP2 shape: `[[key, entries], ...]`.
            let t = lua.create_table()?;
            for (i, (k, v)) in pairs.into_iter().enumerate() {
                let inner = lua.create_table()?;
                inner.set(1, frame_to_lua(lua, k)?)?;
                inner.set(2, frame_to_lua(lua, v)?)?;
                t.set(i + 1, inner)?;
            }
            Value::Table(t)
        }
    })
}

/// Convert a script's return value to a reply, applying Redis' rules: numbers
/// truncate to integers, `false`/`nil` → nil, tables become arrays up to the
/// first nil, and `{ok=}`/`{err=}` become status/error replies.
fn lua_to_frame(value: Value) -> Frame {
    match value {
        Value::Nil => Frame::Null,
        Value::Boolean(true) => Frame::Integer(1),
        Value::Boolean(false) => Frame::Null,
        Value::Integer(i) => Frame::Integer(i),
        Value::Number(n) => Frame::Integer(n as i64),
        Value::String(s) => Frame::Bulk(Bytes::copy_from_slice(&s.as_bytes())),
        Value::Table(t) => table_to_frame(t),
        _ => Frame::Null,
    }
}

fn table_to_frame(t: Table) -> Frame {
    if let Ok(Value::String(e)) = t.get::<Value>("err") {
        return Frame::Error(bytes_to_string(&e));
    }
    if let Ok(Value::String(s)) = t.get::<Value>("ok") {
        return Frame::Simple(bytes_to_string(&s));
    }
    let mut items = Vec::new();
    let mut i = 1i64;
    loop {
        match t.get::<Value>(i) {
            Ok(Value::Nil) | Err(_) => break,
            Ok(v) => items.push(lua_to_frame(v)),
        }
        i += 1;
    }
    Frame::Array(items)
}

fn error_value_to_frame(value: Value) -> Frame {
    match value {
        Value::Table(t) => match t.get::<Value>("err") {
            Ok(Value::String(e)) => Frame::Error(bytes_to_string(&e)),
            _ => Frame::err("script raised a table without an 'err' field"),
        },
        Value::String(s) => {
            let msg = bytes_to_string(&s);
            // A raised code (e.g. "WRONGTYPE ...") is surfaced verbatim;
            // a bare message gets the generic ERR prefix, as Redis does.
            if has_error_code(&msg) {
                Frame::Error(msg)
            } else {
                Frame::err(msg)
            }
        }
        _ => Frame::err("script raised a non-string error"),
    }
}

fn has_error_code(msg: &str) -> bool {
    let first = msg.split(' ').next().unwrap_or("");
    !first.is_empty() && first.chars().all(|c| c.is_ascii_uppercase())
}

fn bytes_to_string(s: &mlua::String) -> String {
    String::from_utf8_lossy(&s.as_bytes()).into_owned()
}

fn unknown_subcommand(sub: &str) -> Frame {
    Frame::err(format!(
        "Unknown SCRIPT subcommand or wrong number of arguments for '{sub}'"
    ))
}
