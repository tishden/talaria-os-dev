//! TCP access to the control plane: `talaria serve [addr]` starts a line-based
//! TCP server serving telemetry and accepting commands over the network; the client
//! `talaria remote <addr> <command...>` sends one command and prints the answer.
//!
//! The threat model of the design document: the control plane sits behind an external
//! perimeter (a trusted management network), hence no authentication — like IPMI/BMC
//! in a protected segment. On an untrusted network put a TLS tunnel in front of it.
//!
//! Protocol (text, line by line; one connection — many commands, keep-alive):
//!   status            — telemetry of all cells
//!   list              — names of the cells that are up
//!   down <name>       — graceful stop of a cell
//!   ping              — "pong" (liveness probe)
//!   help              — help
//!   quit              — close the connection

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

const DEFAULT_ADDR: &str = "0.0.0.0:7373";

/// `talaria serve [addr]` — a blocking TCP server (a thread per connection).
pub fn serve(addr: &str) -> i32 {
    let addr = if addr.is_empty() { DEFAULT_ADDR } else { addr };
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => { eprintln!("serve: bind {addr}: {e}"); return 1; }
    };
    println!("talaria control-plane TCP on {addr} (status/list/down/ping; no authentication — trusted network only)");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => { std::thread::spawn(move || handle(s)); }
            Err(e) => eprintln!("serve: accept: {e}"),
        }
    }
    0
}

fn handle(stream: TcpStream) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
    let mut w = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let mut r = BufReader::new(stream);
    let _ = writeln!(w, "talaria control-plane; commands: status list down <name> ping help quit");
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => break,          // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let mut it = line.trim().split_whitespace();
        let cmd = it.next().unwrap_or("");
        let arg = it.next().unwrap_or("");
        let resp = dispatch(cmd, arg, &peer);
        match resp {
            Some(text) => { if w.write_all(text.as_bytes()).is_err() { break; } }
            None => break, // quit
        }
    }
}

/// Execute one command. None -> close the connection (quit).
fn dispatch(cmd: &str, arg: &str, peer: &str) -> Option<String> {
    match cmd {
        "" => Some(String::new()),
        "status" => Some(crate::status_text(false)),
        "list" => {
            let names = crate::cell_names();
            Some(if names.is_empty() { "no cells are up\n".into() } else { format!("{}\n", names.join("\n")) })
        }
        "down" => {
            if arg.is_empty() { return Some("usage: down <name>\n".into()); }
            eprintln!("serve: down '{arg}' from {peer}");
            Some(crate::down_result(arg))
        }
        "ping" => Some("pong\n".into()),
        "help" => Some("status | list | down <name> | ping | help | quit\n".into()),
        "quit" | "exit" => None,
        other => Some(format!("unknown command '{other}' (help for the list)\n")),
    }
}

/// `talaria remote <addr> <command...>` — the client: one command, print the answer.
pub fn remote(addr: &str, parts: &[String]) -> i32 {
    if addr.is_empty() || parts.is_empty() {
        eprintln!("usage: talaria remote <host:port> <command...>");
        return 2;
    }
    let addr = with_default_port(addr);
    let mut stream = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => { eprintln!("remote: connect {addr}: {e}"); return 1; }
    };
    let cmd = parts.join(" ");
    if writeln!(stream, "{cmd}").is_err() { eprintln!("remote: write"); return 1; }
    if writeln!(stream, "quit").is_err() { return 1; }
    let _ = stream.flush();
    let r = BufReader::new(stream);
    // the first line is the server banner; print everything after it
    let mut first = true;
    for line in r.lines() {
        match line {
            Ok(l) => { if first { first = false; continue; } println!("{l}"); }
            Err(_) => break,
        }
    }
    0
}

fn with_default_port(addr: &str) -> String {
    if addr.contains(':') { addr.to_string() } else { format!("{addr}:7373") }
}
