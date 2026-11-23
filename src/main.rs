//! mac-action-agent: a TCP socket on the Mac that runs shell commands for callers on the
//! private network and streams their output back.
//!
//! Three gates, no token:
//!   1. it binds only to the Mac's own address inside `BindNetwork` (the VPN that links it
//!      to the dedicated server), never to all interfaces; while the VPN is down it waits,
//!      and if that address disappears it exits so launchd restarts it and it rebinds;
//!   2. the peer must fall inside `AllowedNetworks` (the dedicated server's subnet);
//!   3. every command is shown in a dialog on the Mac and runs only after "Allow"
//!      (no answer within `ConfirmTimeout` seconds = denied).
//!
//! Settings (`defaults write me.centralhardware.mac-action-agent <Key> …`, or env MAA_<KEY>):
//!   Port             default 8765
//!   BindNetwork      default "10.8.0.0/24"
//!   AllowedNetworks  default "10.168.0.0/24"
//!   ConfirmTimeout   default 60
//!
//! Protocol: newline-delimited JSON, one command per connection.
//!   client -> {"cmd":"…","timeout":600,"cwd":null,"shell":null}   (timeout 0 = none)
//!   agent  <- {"t":"pending"}                                       (dialog shown on the Mac)
//!   agent  <- {"t":"start","pid":123}
//!   agent  <- {"t":"out","d":"…"} | {"t":"err","d":"…"}            (streamed)
//!   client -> {"t":"in","d":"…"} | {"t":"eof"} | {"t":"kill"}      (any time)
//!   agent  <- {"t":"exit","code":0,"signal":null,"timed_out":false,"killed":false,"duration_ms":12}
//!   agent  <- {"t":"error","message":"…"}       (forbidden, denied by user, bad request, spawn failure)
//! Closing the connection early kills the command's whole process group.

use mac_action_agent::{allowed, ifconfig_addrs, parse_nets, pick_bind_addr, serve, Confirm, Net, RunRequest};
use std::io::Write;
use std::net::{IpAddr, TcpListener};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DOMAIN: &str = "me.centralhardware.mac-action-agent";

fn pref(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(format!("MAA_{}", key.to_uppercase())) {
        return Some(v);
    }
    let out = Command::new("/usr/bin/defaults").args(["read", DOMAIN, key]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn nets_pref(key: &str, default: &str) -> Vec<Net> {
    let (nets, bad) = parse_nets(&pref(key).unwrap_or_else(|| default.into()));
    for b in &bad {
        log::warn!("ignoring bad {key} entry {b:?}");
    }
    if nets.is_empty() {
        log::error!("{key} is empty; refusing to start");
        std::process::exit(1);
    }
    nets
}

fn local_addrs() -> Vec<IpAddr> {
    Command::new("/sbin/ifconfig")
        .output()
        .map(|o| ifconfig_addrs(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Shows the command in a dialog; true only on an explicit "Allow".
fn dialog(req: &RunRequest, peer: Option<IpAddr>, timeout: u64) -> bool {
    let peer = peer.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    let mut text = format!("{peer} wants to run:\n\n{}", req.cmd);
    if let Some(cwd) = &req.cwd {
        text.push_str(&format!("\n\nin {cwd}"));
    }
    // The text goes in as argv, so nothing in the command needs AppleScript escaping.
    let out = Command::new("/usr/bin/osascript")
        .args([
            "-e", "on run argv",
            "-e", &format!(
                "display dialog (item 1 of argv) with title \"mac-action-agent\" buttons {{\"Deny\", \"Allow\"}} \
                 default button \"Deny\" cancel button \"Deny\" with icon caution giving up after {timeout}"
            ),
            "-e", "end run",
            &text,
        ])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            o.status.success() && s.contains("button returned:Allow") && !s.contains("gave up:true")
        }
        Err(e) => {
            log::error!("cannot show confirmation dialog: {e}");
            false
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if std::env::args().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let port: u16 = pref("Port").and_then(|p| p.parse().ok()).unwrap_or(8765);
    let bind_nets = nets_pref("BindNetwork", "10.8.0.0/24");
    let nets = nets_pref("AllowedNetworks", "10.168.0.0/24");
    let confirm_timeout: u64 = pref("ConfirmTimeout").and_then(|p| p.parse().ok()).unwrap_or(60);

    // Wait for the VPN address to show up, then bind to it alone.
    let bind_ip = loop {
        if let Some(ip) = pick_bind_addr(&local_addrs(), &bind_nets) {
            break ip;
        }
        log::warn!("no local address in {bind_nets:?} yet (VPN down?); retrying in 10 s");
        std::thread::sleep(Duration::from_secs(10));
    };
    let listener = match TcpListener::bind((bind_ip, port)) {
        Ok(l) => l,
        Err(e) => {
            log::error!("cannot listen on {bind_ip}:{port}: {e}");
            std::process::exit(1);
        }
    };
    // If the VPN address goes away (or changes), exit so launchd restarts us and we rebind.
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        if !local_addrs().contains(&bind_ip) {
            log::warn!("{bind_ip} is gone; exiting to rebind");
            std::process::exit(2);
        }
    });

    // One dialog at a time, so concurrent requests can't stack prompts on top of each other.
    let gate = Mutex::new(());
    let confirm: Arc<Confirm> = Arc::new(move |req: &RunRequest, peer: Option<IpAddr>| {
        let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
        dialog(req, peer, confirm_timeout)
    });

    log::info!(
        "mac-action-agent {} listening on {bind_ip}:{port}, allowed {nets:?}, confirm timeout {confirm_timeout}s",
        env!("CARGO_PKG_VERSION")
    );
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let peer = stream.peer_addr().ok();
        if !peer.is_some_and(|p| allowed(&nets, p.ip())) {
            log::warn!("rejected {peer:?}");
            std::thread::spawn(move || {
                let _ = stream.write_all(b"{\"t\":\"error\",\"message\":\"forbidden\"}\n");
                let _ = stream.shutdown(std::net::Shutdown::Write);
                std::thread::sleep(Duration::from_millis(100)); // let the client read it before RST
            });
            continue;
        }
        log::info!("connection from {peer:?}");
        let _ = stream.set_nodelay(true);
        let confirm = confirm.clone();
        std::thread::spawn(move || serve(stream, confirm));
    }
}
