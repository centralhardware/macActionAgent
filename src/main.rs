//! mac-action-agent: a TCP socket on the Mac that runs shell commands for callers on the
//! private network and streams their output back. There is no token: the only gate is
//! the peer address, which must fall inside `AllowedNetworks`.
//!
//! Settings (`defaults write me.centralhardware.mac-action-agent <Key> …`, or env MAA_<KEY>):
//!   Port             listen port, default 8765
//!   AllowedNetworks  comma-separated CIDRs, default "10.168.0.0/24,127.0.0.0/8,::1"
//!
//! Protocol: newline-delimited JSON, one command per connection.
//!   client -> {"cmd":"…","timeout":600,"cwd":null,"shell":null}   (timeout 0 = none)
//!   agent  <- {"t":"start","pid":123}
//!   agent  <- {"t":"out","d":"…"} | {"t":"err","d":"…"}            (streamed)
//!   client -> {"t":"in","d":"…"} | {"t":"eof"} | {"t":"kill"}      (any time)
//!   agent  <- {"t":"exit","code":0,"signal":null,"timed_out":false,"killed":false,"duration_ms":12}
//!   agent  <- {"t":"error","message":"…"}                          (bad request / spawn failure)
//! Closing the connection early kills the command's whole process group.

use mac_action_agent::{allowed, parse_nets, serve};
use std::io::Write;
use std::net::TcpListener;
use std::process::Command;

const DOMAIN: &str = "me.centralhardware.mac-action-agent";
const DEFAULT_NETS: &str = "10.168.0.0/24,127.0.0.0/8,::1";

fn pref(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(format!("MAA_{}", key.to_uppercase())) {
        return Some(v);
    }
    let out = Command::new("/usr/bin/defaults").args(["read", DOMAIN, key]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if std::env::args().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let port: u16 = pref("Port").and_then(|p| p.parse().ok()).unwrap_or(8765);
    let (nets, bad) = parse_nets(&pref("AllowedNetworks").unwrap_or_else(|| DEFAULT_NETS.into()));
    for b in &bad {
        log::warn!("ignoring bad AllowedNetworks entry {b:?}");
    }
    if nets.is_empty() {
        log::error!("AllowedNetworks is empty; refusing to start");
        std::process::exit(1);
    }
    // Dual-stack where possible; IPv4 peers then show up as ::ffff:a.b.c.d, which Net handles.
    let listener = match TcpListener::bind(("::", port)).or_else(|_| TcpListener::bind(("0.0.0.0", port))) {
        Ok(l) => l,
        Err(e) => {
            log::error!("cannot listen on port {port}: {e}");
            std::process::exit(1);
        }
    };
    if std::env::args().any(|a| a == "--check") {
        println!("ok: would listen on :{port}, allowed {nets:?}");
        return;
    }
    log::info!("mac-action-agent {} listening on :{port}, allowed {nets:?}", env!("CARGO_PKG_VERSION"));
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let peer = stream.peer_addr().ok();
        if !peer.is_some_and(|p| allowed(&nets, p.ip())) {
            log::warn!("rejected {peer:?}");
            let _ = stream.write_all(b"{\"t\":\"error\",\"message\":\"forbidden\"}\n");
            let _ = stream.shutdown(std::net::Shutdown::Write);
            std::thread::sleep(std::time::Duration::from_millis(100)); // let the client read it before RST
            continue;
        }
        log::info!("connection from {peer:?}");
        let _ = stream.set_nodelay(true);
        std::thread::spawn(move || serve(stream));
    }
}
