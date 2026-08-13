//! Verbose logging: a running trace of what each client sends and gets back.
//!
//! Off by default. Turned on at boot with `--verbose` (or `--loglevel
//! verbose|debug`) and at runtime with `CONFIG SET loglevel verbose`. Every
//! entry point checks the flag first — a single relaxed atomic load — so a
//! quiet server pays essentially nothing per command.
//!
//! Lines go to stdout, next to the boot banner, and look like:
//!
//! ```text
//! 2026-08-13T18:04:21.512Z #3 * connected from 127.0.0.1:52814
//! 2026-08-13T18:04:21.513Z #3 > SET greeting "hello world" EX 30
//! 2026-08-13T18:04:21.513Z #3 < OK (21µs)
//! ```
//!
//! `>` is a command in, `<` a reply out, `*` a connection event.

use crate::resp::Frame;
use crate::server::{ConnState, Shared};
use bytes::Bytes;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Longest value rendered before it is elided.
const MAX_VALUE: usize = 96;
/// Most arguments rendered from one command.
const MAX_ARGS: usize = 24;
/// Most elements rendered from one aggregate reply.
const MAX_ELEMS: usize = 12;
/// Hard cap on a rendered line, applied after everything else.
const MAX_LINE: usize = 400;

/// Whether a Redis `loglevel` value means "log every command". Redis' levels,
/// quietest first: nothing, warning, notice, verbose, debug. Returns `None` for
/// a level we don't recognize.
pub fn level_is_verbose(level: &str) -> Option<bool> {
    match level.to_ascii_lowercase().as_str() {
        "debug" | "verbose" => Some(true),
        "notice" | "warning" | "nothing" => Some(false),
        _ => None,
    }
}

/// Log an inbound command. Returns the instant it started (when logging is on)
/// so [`reply`] can report how long the command took.
pub fn cmd(shared: &Shared, conn: &ConnState, args: &[Bytes]) -> Option<Instant> {
    cmd_tagged(shared, conn, args, "")
}

/// Log one outbound frame. `started` adds the elapsed time; pass `None` for
/// frames that aren't a direct answer to a command (pub/sub pushes).
pub fn reply(shared: &Shared, conn: &ConnState, frame: &Frame, started: Option<Instant>) {
    reply_tagged(shared, conn, frame, started, "")
}

/// Log a `redis.call` made from inside a Lua script, tagged `(lua)`, so an
/// `EVAL` shows the commands it ran and not just its final reply.
pub fn script_cmd(shared: &Shared, conn: &ConnState, args: &[Bytes]) -> Option<Instant> {
    cmd_tagged(shared, conn, args, "(lua) ")
}

/// The reply to a script's `redis.call`.
pub fn script_reply(shared: &Shared, conn: &ConnState, frame: &Frame, started: Option<Instant>) {
    reply_tagged(shared, conn, frame, started, "(lua) ")
}

fn cmd_tagged(shared: &Shared, conn: &ConnState, args: &[Bytes], tag: &str) -> Option<Instant> {
    if !shared.verbose() || args.is_empty() {
        return None;
    }
    line(conn.id, '>', &format!("{tag}{}", fmt_cmd(args)));
    Some(Instant::now())
}

fn reply_tagged(
    shared: &Shared,
    conn: &ConnState,
    frame: &Frame,
    started: Option<Instant>,
    tag: &str,
) {
    if !shared.verbose() {
        return;
    }
    let mut body = format!("{tag}{}", fmt_frame(frame));
    if let Some(t) = started {
        body.push_str(&format!(" ({})", fmt_dur(t.elapsed())));
    }
    line(conn.id, '<', &body);
}

/// Log a batch of frames produced by one command (subscribe acknowledgements).
/// Only the first carries the timing, since they were all produced together.
pub fn replies(shared: &Shared, conn: &ConnState, frames: &[Frame], started: Option<Instant>) {
    if !shared.verbose() {
        return;
    }
    for (i, f) in frames.iter().enumerate() {
        reply(shared, conn, f, if i == 0 { started } else { None });
    }
}

/// Log a connection-level event: connected, disconnected, parked on a
/// blocking command.
pub fn event(shared: &Shared, id: u64, msg: &str) {
    if !shared.verbose() {
        return;
    }
    line(id, '*', msg);
}

/// Log a server-wide note, not tied to any client. Unconditional — callers use
/// it to report that logging itself just turned on or off.
pub fn note(msg: &str) {
    println!("{} * {}", now(), msg);
}

fn line(id: u64, dir: char, body: &str) {
    println!("{} #{} {} {}", now(), id, dir, clamp(body, MAX_LINE));
}

// --- rendering ---

/// Render a command as a single line, redacting credentials.
fn fmt_cmd(args: &[Bytes]) -> String {
    let secrets = secret_args(args);
    let mut out = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
    for (i, arg) in args.iter().enumerate().skip(1).take(MAX_ARGS) {
        out.push(' ');
        if secrets.contains(&i) {
            out.push_str("<redacted>");
        } else {
            out.push_str(&quote(arg));
        }
    }
    if args.len() > MAX_ARGS + 1 {
        out.push_str(&format!(" ... (+{} args)", args.len() - MAX_ARGS - 1));
    }
    out
}

/// Argument positions holding a credential, which are logged as `<redacted>`
/// rather than in the clear.
fn secret_args(args: &[Bytes]) -> Vec<usize> {
    match String::from_utf8_lossy(&args[0])
        .to_ascii_uppercase()
        .as_str()
    {
        // AUTH <password> | AUTH <username> <password>
        "AUTH" if args.len() >= 2 => vec![args.len() - 1],
        // HELLO <ver> [AUTH <username> <password>] [SETNAME <name>]
        "HELLO" => match args.iter().position(|a| a.eq_ignore_ascii_case(b"AUTH")) {
            Some(i) if i + 2 < args.len() => vec![i + 2],
            _ => Vec::new(),
        },
        // CONFIG SET <param> <value> [<param> <value> ...]
        "CONFIG" if args.len() >= 4 && args[1].eq_ignore_ascii_case(b"SET") => {
            let mut out = Vec::new();
            let mut i = 2;
            while i + 1 < args.len() {
                if args[i].eq_ignore_ascii_case(b"requirepass")
                    || args[i].eq_ignore_ascii_case(b"masterauth")
                {
                    out.push(i + 1);
                }
                i += 2;
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Render a byte string the way `redis-cli` does: bare when it is plain
/// printable ASCII, otherwise double-quoted with escapes. Long values are
/// truncated with a note of how much was dropped.
fn quote(b: &[u8]) -> String {
    let bare = !b.is_empty()
        && b.iter()
            .all(|c| c.is_ascii_graphic() && *c != b'"' && *c != b'\\');
    let shown = &b[..b.len().min(MAX_VALUE)];
    let mut s = String::new();
    if !bare {
        s.push('"');
    }
    for &c in shown {
        match c {
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            b'\t' => s.push_str("\\t"),
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    if shown.len() < b.len() {
        s.push_str("...");
    }
    if !bare {
        s.push('"');
    }
    if shown.len() < b.len() {
        s.push_str(&format!(" (+{} bytes)", b.len() - shown.len()));
    }
    s
}

/// Render a reply frame compactly, in the spirit of `redis-cli` output but on
/// one line.
fn fmt_frame(frame: &Frame) -> String {
    match frame {
        Frame::Simple(s) => s.clone(),
        Frame::Error(e) => format!("(error) {e}"),
        Frame::Integer(i) => format!("(integer) {i}"),
        Frame::Double(d) => format!("(double) {d}"),
        Frame::Bulk(b) => quote(b),
        Frame::Null | Frame::NullArray => "(nil)".to_string(),
        Frame::Array(v) | Frame::Set(v) => fmt_list(v),
        Frame::Push(v) => format!("(push) {}", fmt_list(v)),
        Frame::Map(p) | Frame::Pairs(p) | Frame::XReadReply(p) => fmt_pairs(p),
    }
}

fn fmt_list(items: &[Frame]) -> String {
    if items.is_empty() {
        return "(empty)".to_string();
    }
    let mut s = "[".to_string();
    for (i, f) in items.iter().take(MAX_ELEMS).enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&fmt_frame(f));
    }
    if items.len() > MAX_ELEMS {
        s.push_str(&format!(", ... (+{} items)", items.len() - MAX_ELEMS));
    }
    s.push(']');
    s
}

fn fmt_pairs(pairs: &[(Frame, Frame)]) -> String {
    if pairs.is_empty() {
        return "(empty)".to_string();
    }
    let mut s = "{".to_string();
    for (i, (k, v)) in pairs.iter().take(MAX_ELEMS).enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("{}: {}", fmt_frame(k), fmt_frame(v)));
    }
    if pairs.len() > MAX_ELEMS {
        s.push_str(&format!(", ... (+{} items)", pairs.len() - MAX_ELEMS));
    }
    s.push('}');
    s
}

fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{us}µs")
    } else if us < 1_000_000 {
        format!("{:.2}ms", us as f64 / 1000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

// --- timestamps ---

fn now() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format_epoch_ms(ms)
}

/// Format unix milliseconds as `2026-08-13T18:04:21.512Z`. UTC, so logs from
/// several instances (and from other services) line up without guessing at a
/// timezone.
fn format_epoch_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let (y, mo, d) = civil_from_days((secs / 86400) as i64);
    let sod = secs % 86400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        mo,
        d,
        sod / 3600,
        (sod / 60) % 60,
        sod % 60,
        ms % 1000
    )
}

/// Days since the unix epoch to `(year, month, day)` — Howard Hinnant's
/// `civil_from_days`, which avoids pulling in a date crate for one log prefix.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month shifted so March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<Bytes> {
        parts.iter().map(|p| Bytes::from(p.to_string())).collect()
    }

    #[test]
    fn timestamps_are_iso_utc() {
        assert_eq!(format_epoch_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_epoch_ms(1_755_109_461_512),
            "2025-08-13T18:24:21.512Z"
        );
        // A leap day, and the last millisecond of a year.
        assert_eq!(
            format_epoch_ms(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
        assert_eq!(
            format_epoch_ms(1_735_689_599_999),
            "2024-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn plain_args_are_bare_and_odd_ones_quoted() {
        assert_eq!(quote(b"foo"), "foo");
        assert_eq!(quote(b""), "\"\"");
        assert_eq!(quote(b"hello world"), "\"hello world\"");
        assert_eq!(quote(b"line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(quote(&[0x00, 0xff]), "\"\\x00\\xff\"");
        assert_eq!(quote(b"say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn long_values_are_truncated_with_a_byte_count() {
        let rendered = quote(&[b'a'; MAX_VALUE + 10]);
        assert!(rendered.starts_with("aaa"), "{rendered}");
        assert!(rendered.ends_with("... (+10 bytes)"), "{rendered}");
    }

    #[test]
    fn commands_render_with_uppercase_name() {
        assert_eq!(
            fmt_cmd(&args(&["set", "greeting", "hello world", "EX", "30"])),
            "SET greeting \"hello world\" EX 30"
        );
    }

    #[test]
    fn long_commands_note_the_dropped_args() {
        let mut parts = vec!["rpush".to_string(), "k".to_string()];
        parts.extend((0..MAX_ARGS).map(|i| i.to_string()));
        let owned: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
        let rendered = fmt_cmd(&args(&owned));
        assert!(rendered.ends_with("... (+1 args)"), "{rendered}");
    }

    #[test]
    fn credentials_are_redacted() {
        assert_eq!(fmt_cmd(&args(&["AUTH", "hunter2"])), "AUTH <redacted>");
        assert_eq!(
            fmt_cmd(&args(&["auth", "alice", "hunter2"])),
            "AUTH alice <redacted>"
        );
        assert_eq!(
            fmt_cmd(&args(&[
                "HELLO", "3", "AUTH", "default", "hunter2", "SETNAME", "app"
            ])),
            "HELLO 3 AUTH default <redacted> SETNAME app"
        );
        assert_eq!(
            fmt_cmd(&args(&["config", "set", "requirepass", "hunter2"])),
            "CONFIG set requirepass <redacted>"
        );
        // Non-secret CONFIG SET values are still shown.
        assert_eq!(
            fmt_cmd(&args(&["config", "set", "maxmemory", "0"])),
            "CONFIG set maxmemory 0"
        );
    }

    #[test]
    fn frames_render_one_line() {
        assert_eq!(fmt_frame(&Frame::ok()), "OK");
        assert_eq!(fmt_frame(&Frame::Integer(-3)), "(integer) -3");
        assert_eq!(fmt_frame(&Frame::Null), "(nil)");
        assert_eq!(fmt_frame(&Frame::err("nope")), "(error) ERR nope");
        assert_eq!(fmt_frame(&Frame::Array(vec![])), "(empty)");
        assert_eq!(
            fmt_frame(&Frame::Array(vec![Frame::bulk("a"), Frame::Integer(1)])),
            "[a, (integer) 1]"
        );
        assert_eq!(
            fmt_frame(&Frame::Map(vec![(Frame::bulk("k"), Frame::bulk("v"))])),
            "{k: v}"
        );
        assert_eq!(
            fmt_frame(&Frame::Push(vec![
                Frame::bulk("message"),
                Frame::bulk("news"),
                Frame::bulk("hi"),
            ])),
            "(push) [message, news, hi]"
        );
    }

    #[test]
    fn big_aggregates_note_the_dropped_items() {
        let items: Vec<Frame> = (0..MAX_ELEMS + 3)
            .map(|i| Frame::Integer(i as i64))
            .collect();
        let rendered = fmt_frame(&Frame::Array(items));
        assert!(rendered.ends_with(", ... (+3 items)]"), "{rendered}");
    }

    #[test]
    fn lines_are_clamped() {
        let long = "x".repeat(MAX_LINE * 2);
        let clamped = clamp(&long, MAX_LINE);
        assert_eq!(clamped.len(), MAX_LINE + 3);
        assert!(clamped.ends_with("..."));
        assert_eq!(clamp("short", MAX_LINE), "short");
    }

    #[test]
    fn durations_pick_a_readable_unit() {
        assert_eq!(fmt_dur(Duration::from_micros(21)), "21µs");
        assert_eq!(fmt_dur(Duration::from_micros(4210)), "4.21ms");
        assert_eq!(fmt_dur(Duration::from_millis(1500)), "1.50s");
    }
}
