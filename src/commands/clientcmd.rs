//! Connection-level commands: PING, ECHO, HELLO, AUTH, SELECT, CLIENT.

use super::{parse_int, upper, wrong_args};
use crate::resp::Frame;
use crate::server::{ClientInfo, ConnState, Shared};
use bytes::Bytes;

pub fn ping(args: &[Bytes]) -> Frame {
    match args.len() {
        1 => Frame::Simple("PONG".into()),
        2 => Frame::Bulk(args[1].clone()),
        _ => wrong_args("ping"),
    }
}

pub fn echo(args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("echo");
    }
    Frame::Bulk(args[1].clone())
}

pub fn select(args: &[Bytes]) -> Frame {
    if args.len() != 2 {
        return wrong_args("select");
    }
    match parse_int(&args[1]) {
        Ok(n) if n >= 0 => Frame::ok(),
        Ok(_) => Frame::err("DB index is out of range"),
        Err(_) => Frame::err("value is not an integer or out of range"),
    }
}

pub fn auth(shared: &Shared, conn: &mut ConnState, args: &[Bytes]) -> Frame {
    if args.len() < 2 || args.len() > 3 {
        return wrong_args("auth");
    }
    // AUTH [username] password — the username is ignored (single default user).
    let password = args.last().unwrap();
    match &shared.requirepass {
        None => Frame::err(
            "Client sent AUTH, but no password is set. Did you mean AUTH <username> <password>?",
        ),
        Some(pass) => {
            if password.as_ref() == pass.as_bytes() {
                conn.authenticated = true;
                Frame::ok()
            } else {
                Frame::Error(
                    "WRONGPASS invalid username-password pair or user is disabled.".into(),
                )
            }
        }
    }
}

pub fn hello(shared: &Shared, conn: &mut ConnState, args: &[Bytes]) -> Frame {
    let mut i = 1;
    // Optional protocol version. We speak both RESP2 and RESP3.
    if i < args.len() {
        match parse_int(&args[i]) {
            Ok(2) => {
                conn.resp3 = false;
                i += 1;
            }
            Ok(3) => {
                conn.resp3 = true;
                i += 1;
            }
            _ => {
                return Frame::Error("NOPROTO unsupported protocol version".into());
            }
        }
    }

    // Optional AUTH / SETNAME options.
    let mut pending_name: Option<Bytes> = None;
    let mut authed_here = false;
    while i < args.len() {
        match upper(&args[i]).as_str() {
            "AUTH" if i + 2 < args.len() => {
                let password = &args[i + 2];
                match &shared.requirepass {
                    None => {
                        return Frame::err(
                            "Client sent AUTH, but no password is set. Did you mean AUTH <username> <password>?",
                        )
                    }
                    Some(pass) => {
                        if password.as_ref() == pass.as_bytes() {
                            conn.authenticated = true;
                            authed_here = true;
                        } else {
                            return Frame::Error(
                                "WRONGPASS invalid username-password pair or user is disabled."
                                    .into(),
                            );
                        }
                    }
                }
                i += 3;
            }
            "SETNAME" if i + 1 < args.len() => {
                pending_name = Some(args[i + 1].clone());
                i += 2;
            }
            _ => return Frame::err("syntax error in HELLO"),
        }
    }

    // If a password is required and the client has not authenticated (here or
    // earlier), HELLO is rejected.
    if shared.requirepass.is_some() && !conn.authenticated && !authed_here {
        return Frame::Error(
            "NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time".into(),
        );
    }

    if let Some(name) = pending_name {
        conn.name = name;
        update_registry_name(shared, conn);
    }
    // Reflect the negotiated protocol in the client registry (for CLIENT LIST).
    if let Some(info) = shared.clients.lock().unwrap().get_mut(&conn.id) {
        info.resp3 = conn.resp3;
    }

    Frame::Map(vec![
        (Frame::bulk("server"), Frame::bulk("redis")),
        (Frame::bulk("version"), Frame::bulk("7.4.0")),
        (Frame::bulk("proto"), Frame::Integer(if conn.resp3 { 3 } else { 2 })),
        (Frame::bulk("id"), Frame::Integer(conn.id as i64)),
        (Frame::bulk("mode"), Frame::bulk("standalone")),
        (Frame::bulk("role"), Frame::bulk("master")),
        (Frame::bulk("modules"), Frame::Array(vec![])),
    ])
}

pub fn client(shared: &Shared, conn: &mut ConnState, args: &[Bytes]) -> Frame {
    if args.len() < 2 {
        return wrong_args("client");
    }
    match upper(&args[1]).as_str() {
        "ID" => Frame::Integer(conn.id as i64),
        "GETNAME" => Frame::Bulk(conn.name.clone()),
        "SETNAME" => {
            if args.len() != 3 {
                return wrong_args("client|setname");
            }
            if args[2].iter().any(|&b| b == b' ' || b == b'\n') {
                return Frame::err("Client names cannot contain spaces, newlines or special characters.");
            }
            conn.name = args[2].clone();
            update_registry_name(shared, conn);
            Frame::ok()
        }
        "SETINFO" => Frame::ok(),
        "LIST" => {
            let clients = shared.clients.lock().unwrap();
            let mut out = String::new();
            let mut sorted: Vec<&ClientInfo> = clients.values().collect();
            sorted.sort_by_key(|c| c.id);
            for c in sorted {
                out.push_str(&format_client_line(c));
                out.push('\n');
            }
            Frame::Bulk(Bytes::from(out))
        }
        "INFO" => {
            let me = ClientInfo {
                id: conn.id,
                addr: conn.addr.to_string(),
                name: String::from_utf8_lossy(&conn.name).into_owned(),
                resp3: conn.resp3,
            };
            Frame::Bulk(Bytes::from(format_client_line(&me)))
        }
        "NO-EVICT" | "NO-TOUCH" | "UNPAUSE" | "PAUSE" | "REPLY" => Frame::ok(),
        other => Frame::err(format!(
            "Unknown CLIENT subcommand or wrong number of arguments for '{}'",
            other.to_lowercase()
        )),
    }
}

fn update_registry_name(shared: &Shared, conn: &ConnState) {
    if let Some(info) = shared.clients.lock().unwrap().get_mut(&conn.id) {
        info.name = String::from_utf8_lossy(&conn.name).into_owned();
    }
}

fn format_client_line(c: &ClientInfo) -> String {
    format!(
        "id={} addr={} laddr=127.0.0.1:0 fd=8 name={} age=0 idle=0 flags=N db=0 sub=0 psub=0 ssub=0 multi=-1 watch=0 qbuf=0 qbuf-free=0 argv-mem=0 multi-mem=0 tot-net-in=0 tot-net-out=0 rbs=1024 rbp=0 obl=0 oll=0 omem=0 tot-mem=0 events=r cmd=client|info user=default redir=-1 resp={} lib-name= lib-ver=",
        c.id,
        c.addr,
        c.name,
        if c.resp3 { 3 } else { 2 }
    )
}
