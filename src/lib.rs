//! Core of mac-action-agent: the peer allowlist and the per-connection command session.
//! Unix-only; tests run on Linux and macOS.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// An IPv4/IPv6 network in CIDR notation.
#[derive(Debug, Clone, PartialEq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    pub fn parse(s: &str) -> Option<Net> {
        let s = s.trim();
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = a.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse().ok().filter(|&p| p <= max)?,
            None => max,
        };
        Some(Net { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 peer may arrive as an IPv4-mapped IPv6 address on a dual-stack socket.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    }
}

/// Parses a comma/space separated CIDR list, skipping (and reporting) bad entries.
pub fn parse_nets(s: &str) -> (Vec<Net>, Vec<String>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for part in s.split([',', ' ', '\n']).filter(|p| !p.trim().is_empty()) {
        match Net::parse(part) {
            Some(n) => ok.push(n),
            None => bad.push(part.trim().to_string()),
        }
    }
    (ok, bad)
}

pub fn allowed(nets: &[Net], ip: IpAddr) -> bool {
    nets.iter().any(|n| n.contains(ip))
}

#[derive(Debug, Deserialize)]
pub struct RunRequest {
    /// Shell command line, run with `<shell> -lc`.
    pub cmd: String,
    /// Seconds before the command is killed. Default 600; 0 = no limit (interactive use).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Working directory; defaults to the user's home.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Shell, default $MAA_SHELL or /bin/zsh.
    #[serde(default)]
    pub shell: Option<String>,
}

/// Frames the client sends after the request line.
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum ClientFrame {
    In { d: String },
    Eof,
    Kill,
}

/// Frames the agent sends back.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum AgentFrame {
    /// Waiting for the user to allow the command on the Mac.
    Pending,
    Start { pid: u32 },
    Out { d: String },
    Err { d: String },
    Exit { code: Option<i32>, signal: Option<i32>, timed_out: bool, killed: bool, duration_ms: u64 },
    Error { message: String },
}

pub const DEFAULT_TIMEOUT: u64 = 600;

type Sink = Arc<Mutex<TcpStream>>;

fn send(sink: &Sink, f: &AgentFrame) -> bool {
    let mut line = serde_json::to_string(f).unwrap();
    line.push('\n');
    let mut s = sink.lock().unwrap();
    s.write_all(line.as_bytes()).and_then(|_| s.flush()).is_ok()
}

/// Streams a pipe as frames, never splitting a UTF-8 sequence across two frames.
fn pump(mut r: impl Read, sink: Sink, err: bool) {
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        let cut = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(), // incomplete tail: keep it
            Err(_) => pending.len(),                              // invalid bytes: send lossy
        };
        if cut == 0 {
            continue;
        }
        let d = String::from_utf8_lossy(&pending[..cut]).into_owned();
        pending.drain(..cut);
        let f = if err { AgentFrame::Err { d } } else { AgentFrame::Out { d } };
        if !send(&sink, &f) {
            break;
        }
    }
    if !pending.is_empty() {
        let d = String::from_utf8_lossy(&pending).into_owned();
        send(&sink, &if err { AgentFrame::Err { d } } else { AgentFrame::Out { d } });
    }
}

/// Asks whether a request may run; true = allowed. Called once per command.
pub type Confirm = dyn Fn(&RunRequest, Option<IpAddr>) -> bool + Send + Sync;

/// Serves one connection: request line in, confirmation, frames out until the command exits.
pub fn serve(stream: TcpStream, confirm: Arc<Confirm>) {
    let peer = stream.peer_addr().ok().map(|a| a.ip());
    let sink: Sink = Arc::new(Mutex::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    }));
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let req: RunRequest = match serde_json::from_str(first.trim()) {
        Ok(r) => r,
        Err(e) => {
            send(&sink, &AgentFrame::Error { message: format!("bad request: {e}") });
            return;
        }
    };
    send(&sink, &AgentFrame::Pending);
    if !confirm(&req, peer) {
        log::info!("denied: {}", req.cmd);
        send(&sink, &AgentFrame::Error { message: "denied by user".into() });
        return;
    }
    log::info!("run: {}", req.cmd);

    let shell = req.shell.clone().or_else(|| std::env::var("MAA_SHELL").ok()).unwrap_or_else(|| "/bin/zsh".into());
    let mut cmd = Command::new(shell);
    cmd.arg("-lc").arg(&req.cmd);
    if let Some(dir) = req.cwd.clone().or_else(|| std::env::var("HOME").ok()) {
        cmd.current_dir(dir);
    }
    // Own process group, so kill/timeout takes the whole pipeline down, not just the shell.
    cmd.process_group(0);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send(&sink, &AgentFrame::Error { message: format!("spawn failed: {e}") });
            return;
        }
    };
    let pid = child.id();
    send(&sink, &AgentFrame::Start { pid });

    let killed = Arc::new(AtomicBool::new(false));
    let out_t = { let s = sink.clone(); let o = child.stdout.take().unwrap(); thread::spawn(move || pump(o, s, false)) };
    let err_t = { let s = sink.clone(); let e = child.stderr.take().unwrap(); thread::spawn(move || pump(e, s, true)) };

    // Client frames: stdin data, EOF, kill. A dropped connection kills the command.
    {
        let mut stdin = child.stdin.take();
        let killed = killed.clone();
        thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        killed.store(true, Ordering::SeqCst);
                        kill_group(pid);
                        return;
                    }
                    Ok(_) => {}
                }
                match serde_json::from_str::<ClientFrame>(line.trim()) {
                    Ok(ClientFrame::In { d }) => {
                        if let Some(s) = stdin.as_mut() {
                            if s.write_all(d.as_bytes()).and_then(|_| s.flush()).is_err() {
                                stdin = None;
                            }
                        }
                    }
                    Ok(ClientFrame::Eof) => stdin = None,
                    Ok(ClientFrame::Kill) => {
                        killed.store(true, Ordering::SeqCst);
                        kill_group(pid);
                    }
                    Err(e) => log::warn!("bad client frame {:?}: {e}", line.trim()),
                }
            }
        });
    }

    let limit = match req.timeout.unwrap_or(DEFAULT_TIMEOUT) {
        0 => None,
        s => Some(Duration::from_secs(s)),
    };
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => break None,
        }
        if !timed_out && limit.is_some_and(|l| start.elapsed() >= l) {
            timed_out = true;
            kill_group(pid);
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Kill leftovers (backgrounded children holding the pipes) so the pumps can finish.
    kill_group(pid);
    let _ = out_t.join();
    let _ = err_t.join();
    let exit = AgentFrame::Exit {
        code: status.and_then(|s| s.code()),
        signal: status.and_then(|s| s.signal()),
        timed_out,
        killed: killed.load(Ordering::SeqCst),
        duration_ms: start.elapsed().as_millis() as u64,
    };
    log::info!("  -> {exit:?}");
    send(&sink, &exit);
    let _ = sink.lock().unwrap().shutdown(std::net::Shutdown::Both);
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn kill_group(pgid: u32) {
    unsafe {
        kill(-(pgid as i32), 9);
    }
}

/// IPv4 addresses from `ifconfig` output (`inet 10.8.0.4 --> 10.8.0.1 netmask …`).
pub fn ifconfig_addrs(text: &str) -> Vec<IpAddr> {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("inet "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|a| a.parse().ok())
        .collect()
}

/// The first local address that falls inside one of `nets`.
pub fn pick_bind_addr(addrs: &[IpAddr], nets: &[Net]) -> Option<IpAddr> {
    addrs.iter().copied().find(|a| allowed(nets, *a))
}
