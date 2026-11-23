use mac_action_agent::*;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

fn yes() -> Arc<Confirm> { Arc::new(|_: &RunRequest, _| true) }

#[test]
fn cidr_matching() {
    let (nets, bad) = parse_nets("10.168.0.0/24, 127.0.0.0/8 ::1 nope");
    assert_eq!(bad, vec!["nope"]);
    assert!(allowed(&nets, "10.168.0.55".parse().unwrap()));
    assert!(allowed(&nets, "127.0.0.1".parse().unwrap()));
    assert!(allowed(&nets, "::1".parse().unwrap()));
    assert!(allowed(&nets, "::ffff:10.168.0.55".parse().unwrap()));
    assert!(!allowed(&nets, "10.168.1.1".parse().unwrap()));
    assert!(!allowed(&nets, "8.8.8.8".parse().unwrap()));
}

/// Starts a one-shot agent on a random port, sends the request and the extra frames,
/// and returns every frame it streamed back.
fn session(req: serde_json::Value, frames: &[&str]) -> Vec<AgentFrame> {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || serve(l.accept().unwrap().0, yes()));
    let mut c = TcpStream::connect(addr).unwrap();
    let mut req = req;
    req["shell"] = "/bin/sh".into();
    writeln!(c, "{req}").unwrap();
    for f in frames {
        writeln!(c, "{f}").unwrap();
    }
    let mut out = Vec::new();
    for line in BufReader::new(c).lines() {
        let line = line.unwrap();
        out.push(serde_json::from_str(&line).unwrap());
    }
    out
}

fn text(frames: &[AgentFrame], err: bool) -> String {
    frames
        .iter()
        .filter_map(|f| match (f, err) {
            (AgentFrame::Out { d }, false) | (AgentFrame::Err { d }, true) => Some(d.as_str()),
            _ => None,
        })
        .collect()
}

fn exit(frames: &[AgentFrame]) -> &AgentFrame {
    frames.last().unwrap()
}

#[test]
fn streams_output_and_exit_code() {
    let f = session(serde_json::json!({"cmd": "echo hi; echo err >&2; exit 3"}), &[]);
    assert_eq!(f[0], AgentFrame::Pending);
    assert!(matches!(f[1], AgentFrame::Start { .. }));
    assert_eq!(text(&f, false), "hi\n");
    assert_eq!(text(&f, true), "err\n");
    assert!(matches!(exit(&f), AgentFrame::Exit { code: Some(3), timed_out: false, .. }));
}

#[test]
fn interactive_stdin() {
    let f = session(serde_json::json!({"cmd": "cat"}), &[r#"{"t":"in","d":"abc\n"}"#, r#"{"t":"eof"}"#]);
    assert_eq!(text(&f, false), "abc\n");
    assert!(matches!(exit(&f), AgentFrame::Exit { code: Some(0), .. }));
}

#[test]
fn kill_frame() {
    let f = session(serde_json::json!({"cmd": "sleep 30"}), &[r#"{"t":"kill"}"#]);
    assert!(matches!(exit(&f), AgentFrame::Exit { killed: true, signal: Some(9), .. }));
}

#[test]
fn timeout_kills_pipeline() {
    let f = session(serde_json::json!({"cmd": "sleep 30 | cat", "timeout": 1}), &[]);
    match exit(&f) {
        AgentFrame::Exit { timed_out, duration_ms, .. } => assert!(*timed_out && *duration_ms < 5000),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bad_request() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || serve(l.accept().unwrap().0, yes()));
    let mut c = TcpStream::connect(addr).unwrap();
    writeln!(c, "not json").unwrap();
    let line = BufReader::new(c).lines().next().unwrap().unwrap();
    assert!(matches!(serde_json::from_str(&line).unwrap(), AgentFrame::Error { .. }));
}

#[test]
fn denied_command_never_runs() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let marker = std::env::temp_dir().join(format!("maa-denied-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    std::thread::spawn(move || serve(l.accept().unwrap().0, Arc::new(|_: &RunRequest, _| false)));
    let mut c = TcpStream::connect(addr).unwrap();
    writeln!(c, "{}", serde_json::json!({"cmd": format!("touch {}", marker.display()), "shell": "/bin/sh"})).unwrap();
    let frames: Vec<AgentFrame> =
        BufReader::new(c).lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(frames[0], AgentFrame::Pending);
    assert!(matches!(&frames[1], AgentFrame::Error { message } if message.contains("denied")));
    assert_eq!(frames.len(), 2);
    assert!(!marker.exists());
}

#[test]
fn bind_address_from_ifconfig() {
    let text = "lo0: flags=8049<UP,LOOPBACK>\n\tinet 127.0.0.1 netmask 0xff000000\n\tinet6 ::1 prefixlen 128\n\
en0: flags=8863<UP>\n\tinet 192.168.1.20 netmask 0xffffff00 broadcast 192.168.1.255\n\
utun4: flags=8051<UP,POINTOPOINT>\n\tinet 10.8.0.4 --> 10.8.0.1 netmask 0xffffff00\n";
    let addrs = ifconfig_addrs(text);
    assert_eq!(addrs.len(), 3);
    let (vpn, _) = parse_nets("10.8.0.0/24");
    assert_eq!(pick_bind_addr(&addrs, &vpn), Some("10.8.0.4".parse().unwrap()));
    let (other, _) = parse_nets("10.9.0.0/24");
    assert_eq!(pick_bind_addr(&addrs, &other), None);
}
