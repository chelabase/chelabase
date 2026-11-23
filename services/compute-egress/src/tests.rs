//! The service end to end on loopback: listeners, resolver, policy, forwarding, limits and
//! logs. Destinations stand on other loopback addresses (127.0.0.2, …) and are resolved by an
//! injected lookup; the policy counts loopback as public through the test-only
//! [`Policy::with_public_check`], except where a test checks the production policy itself.
//! The listen address (127.0.0.1) is one of the service's own addresses, so it stays refused.
//!
//! Run under nextest (one process per test): the socket checks read this process's sockets.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chelabase_http::outbound::{HostLookup, Net};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};

use crate::allow::{ENV_DNS_PORT, ENV_HEALTH_PORT, Settings, parse_allow};
use crate::dns::wire;
use crate::policy::Policy;
use crate::{Running, serve_until, start};

const LISTEN: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const D2: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
const D3: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 3);
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const WAIT: Duration = Duration::from_secs(3);

/// A port free for TCP and UDP on `ip`, outside the ephemeral range (so no outgoing
/// connection of a test running in parallel takes it before the service binds it), chosen at
/// random so parallel tests rarely pick the same one.
fn free_port(ip: Ipv4Addr) -> u16 {
    use std::hash::{BuildHasher, RandomState};
    let range =
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap_or_default();
    let mut bounds = range
        .split_whitespace()
        .filter_map(|p| p.parse::<u32>().ok());
    let (lo, hi) = (
        bounds.next().unwrap_or(32768),
        bounds.next().unwrap_or(60999),
    );
    let state = RandomState::new();
    for n in 0u32.. {
        let port = 10_000 + (state.hash_one(n) % 50_000) as u32;
        if (lo..=hi).contains(&port) {
            continue;
        }
        let port = port as u16;
        if std::net::TcpListener::bind((ip, port)).is_ok()
            && std::net::UdpSocket::bind((ip, port)).is_ok()
        {
            return port;
        }
    }
    unreachable!()
}

type Lookup<'a> = Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// A scripted resolver that records every call. Scripted names are absolute: `a.example.com`
/// in a script answers a lookup of `a.example.com.` only, so a relative lookup (one the system
/// resolver could try under a search domain) finds nothing. `hang.example.com.` never answers;
/// a name with several scripted answers gets the next one on each call.
#[derive(Default)]
struct Fake {
    script: HashMap<String, Vec<Vec<IpAddr>>>,
    calls: Mutex<Vec<String>>,
}

impl Fake {
    fn new(script: &[(&str, &[&[IpAddr]])]) -> Arc<Self> {
        Arc::new(Fake {
            script: script
                .iter()
                .map(|(n, answers)| {
                    (
                        format!("{n}."),
                        answers.iter().map(|a| a.to_vec()).collect(),
                    )
                })
                .collect(),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl HostLookup for Fake {
    fn lookup<'a>(&'a self, host: &'a str) -> Lookup<'a> {
        Box::pin(async move {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(host.to_string());
                calls.iter().filter(|c| *c == host).count() - 1
            };
            if host == "hang.example.com." {
                std::future::pending::<()>().await;
            }
            match self.script.get(host) {
                Some(answers) => Ok(answers[n % answers.len()].clone()),
                None => Err(std::io::Error::other("no such name")),
            }
        })
    }
}

fn v4(a: Ipv4Addr) -> IpAddr {
    IpAddr::V4(a)
}

fn settings(allow: &str) -> Settings {
    settings_on(LISTEN, allow)
}

fn settings_on(listen: Ipv4Addr, allow: &str) -> Settings {
    let dns_port = free_port(listen);
    let health_port = free_port(Ipv4Addr::LOCALHOST);
    let entries = parse_allow(
        allow,
        &[(dns_port, ENV_DNS_PORT), (health_port, ENV_HEALTH_PORT)],
        &[],
    )
    .unwrap();
    Settings {
        entries,
        listen: v4(listen),
        advertise: listen,
        dns_port,
        health_port,
        internal: Vec::new(),
        deny: Vec::new(),
        max_connections: 64,
        max_connections_per_client: 64,
        dial_timeout: Duration::from_secs(2),
    }
}

fn loopback_is_public(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v) if v.is_loopback())
}

fn only_d3_is_public(ip: IpAddr) -> bool {
    ip == v4(D3)
}

async fn run(s: &Settings, lookup: &Arc<Fake>) -> Running {
    run_with(
        s,
        lookup,
        Policy::for_settings(s).with_public_check(loopback_is_public),
    )
    .await
}

async fn run_with(s: &Settings, lookup: &Arc<Fake>, policy: Policy) -> Running {
    let lookup: Arc<dyn HostLookup> = lookup.clone();
    start(s, policy, lookup).await.unwrap()
}

/// A destination: echoes, after sending `greeting` (if any) on accept.
struct Dest {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

/// `port` 0 picks one that is also free on the listen address.
async fn dest(ip: Ipv4Addr, port: u16, greeting: &'static [u8]) -> Dest {
    let listener = if port != 0 {
        TcpListener::bind((ip, port)).await.unwrap()
    } else {
        loop {
            if let Ok(l) = TcpListener::bind((ip, free_port(LISTEN))).await {
                break l;
            }
        }
    };
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if s.write_all(greeting).await.is_err() {
                    return;
                }
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    Dest { port, accepted }
}

async fn connect(port: u16) -> TcpStream {
    TcpStream::connect((LISTEN, port)).await.unwrap()
}

async fn connect_from(src: Ipv4Addr, port: u16) -> TcpStream {
    let sock = TcpSocket::new_v4().unwrap();
    sock.bind(SocketAddr::new(v4(src), 0)).unwrap();
    sock.connect(SocketAddr::new(v4(LISTEN), port))
        .await
        .unwrap()
}

async fn read_exact(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0; n];
    tokio::time::timeout(WAIT, s.read_exact(&mut buf))
        .await
        .expect("timed out reading")
        .expect("read");
    buf
}

async fn echo(s: &mut TcpStream, msg: &[u8]) {
    s.write_all(msg).await.unwrap();
    assert_eq!(read_exact(s, msg.len()).await, msg);
}

/// The service closed `s` without sending a byte.
async fn assert_closed(mut s: TcpStream) {
    let _ = s.write_all(b"hello").await;
    let mut buf = [0; 64];
    match tokio::time::timeout(WAIT, s.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("{n} bytes arrived on a refused connection"),
        Err(_) => panic!("the connection is still open"),
    }
}

async fn udp_ask(dns: SocketAddr, q: &[u8]) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    sock.send_to(q, dns).await.unwrap();
    let mut buf = vec![0; 4096];
    let (n, _) = tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf))
        .await
        .ok()?
        .unwrap();
    buf.truncate(n);
    Some(buf)
}

async fn tcp_ask(dns: SocketAddr, q: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(dns).await.unwrap();
    s.write_all(&(q.len() as u16).to_be_bytes()).await.unwrap();
    s.write_all(q).await.unwrap();
    let len = read_exact(&mut s, 2).await;
    read_exact(&mut s, u16::from_be_bytes([len[0], len[1]]) as usize).await
}

/// Asks over UDP and TCP; both answers must agree.
async fn ask(dns: SocketAddr, name: &str, qtype: u16) -> wire::Answer {
    let q = wire::query(0x4242, name, qtype);
    let udp = wire::read(&q, &udp_ask(dns, &q).await.expect("a UDP answer"));
    let tcp = wire::read(&q, &tcp_ask(dns, &q).await);
    assert_eq!(udp, tcp, "{name}");
    udp
}

/// This process's listening TCP sockets and bound UDP sockets (`/proc/self/fd`,
/// `/proc/self/net/*`).
fn own_sockets() -> HashSet<(&'static str, SocketAddr)> {
    let inodes: HashSet<u64> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter_map(|l| {
            l.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect();
    let mut found = HashSet::new();
    for (proto, file) in [
        ("tcp", "tcp"),
        ("tcp", "tcp6"),
        ("udp", "udp"),
        ("udp", "udp6"),
    ] {
        let table = std::fs::read_to_string(format!("/proc/self/net/{file}")).unwrap_or_default();
        for line in table.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if !f[9].parse().is_ok_and(|i: u64| inodes.contains(&i))
                || (proto == "tcp" && f[3] != "0A")
            {
                continue;
            }
            let (addr, port) = f[1].split_once(':').unwrap();
            let port = u16::from_str_radix(port, 16).unwrap();
            let ip = if addr.len() == 8 {
                v4(Ipv4Addr::from(
                    u32::from_str_radix(addr, 16).unwrap().swap_bytes(),
                ))
            } else {
                let mut octets = [0u8; 16];
                for (i, word) in (0..4).map(|i| &addr[i * 8..i * 8 + 8]).enumerate() {
                    let w = u32::from_str_radix(word, 16).unwrap().swap_bytes();
                    octets[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
                }
                IpAddr::V6(Ipv6Addr::from(octets))
            };
            found.insert((proto, SocketAddr::new(ip, port)));
        }
    }
    found
}

// Listeners.

#[tokio::test]
async fn an_empty_list_listens_on_nothing_else() {
    let listen = Ipv4Addr::new(127, 0, 0, 5);
    let s = settings_on(listen, "");
    let fake = Fake::new(&[]);
    let running = run(&s, &fake).await;
    assert!(running.listeners.is_empty());
    let dns = SocketAddr::new(v4(listen), s.dns_port);
    let health = SocketAddr::new(v4(Ipv4Addr::LOCALHOST), s.health_port);
    assert_eq!((running.dns, running.health), (dns, health));
    assert_eq!(
        own_sockets(),
        HashSet::from([("tcp", dns), ("udp", dns), ("tcp", health)])
    );
    for name in ["db.example.com", "example.com", "localhost"] {
        assert_eq!(ask(dns, name, TYPE_A).await.rcode, 3, "{name}");
    }
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn no_listener_binds_the_wildcard() {
    let listen = Ipv4Addr::new(127, 0, 0, 5);
    let (pa, pb) = (free_port(listen), free_port(listen));
    let s = settings_on(listen, &format!("a.example.com:{pa},b.example.com:{pb}"));
    let running = run(&s, &Fake::new(&[])).await;
    let sockets = own_sockets();
    assert_eq!(sockets.len(), 5, "{sockets:?}");
    for (_, addr) in &sockets {
        assert!(
            addr.ip() == v4(listen) || addr.ip() == v4(Ipv4Addr::LOCALHOST),
            "{addr}"
        );
        assert!(!addr.ip().is_unspecified(), "{addr}");
    }
    let ports: Vec<_> = running.listeners.iter().map(|(_, a)| *a).collect();
    assert_eq!(
        ports,
        [
            SocketAddr::new(v4(listen), pa),
            SocketAddr::new(v4(listen), pb)
        ]
    );
}

#[tokio::test]
async fn an_unlisted_port_has_no_listener() {
    let d = dest(D2, 0, b"").await;
    let s = settings(&format!("a.example.com:{}", d.port));
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let other = free_port(LISTEN);
    let err = TcpStream::connect((LISTEN, other)).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

#[tokio::test]
async fn an_assigned_port_reaches_its_own_destination() {
    let a = dest(D2, 0, b"A").await;
    let b = dest(D3, a.port, b"B").await;
    let assigned = free_port(LISTEN);
    let s = settings(&format!(
        "a.example.com:{p},b.example.com:{p}@{assigned}",
        p = a.port
    ));
    let fake = Fake::new(&[
        ("a.example.com", &[&[v4(D2)]]),
        ("b.example.com", &[&[v4(D3)]]),
    ]);
    let _running = run(&s, &fake).await;
    assert_eq!(read_exact(&mut connect(a.port).await, 1).await, b"A");
    assert_eq!(read_exact(&mut connect(assigned).await, 1).await, b"B");
    assert_eq!(
        (
            a.accepted.load(Ordering::SeqCst),
            b.accepted.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn a_clash_never_starts() {
    let s = Settings::from_lookup(|k| match k {
        "CHELABASE_COMPUTE_EGRESS_LISTEN" => Some("127.0.0.1".into()),
        "CHELABASE_COMPUTE_EGRESS_ALLOW" => Some("a.example.com:5432,b.example.com:5432".into()),
        _ => None,
    });
    let err = s.unwrap_err();
    assert!(
        err.contains("a.example.com:5432")
            && err.contains("b.example.com:5432")
            && err.contains("@<listen-port>")
    );
}

#[tokio::test]
async fn a_port_in_use_stops_the_start() {
    let taken = std::net::TcpListener::bind((LISTEN, 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let s = settings(&format!("a.example.com:{port}"));
    let lookup: Arc<dyn HostLookup> = Fake::new(&[]);
    let err = start(&s, Policy::for_settings(&s), lookup)
        .await
        .err()
        .expect("refused");
    assert!(err.contains("a.example.com"), "{err}");
}

#[tokio::test]
async fn health_answers_ok() {
    let s = settings("");
    let running = run(&s, &Fake::new(&[])).await;
    let mut h = TcpStream::connect(running.health).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(WAIT, h.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf, b"ok\n");
}

// Resolver.

#[tokio::test]
async fn answers_the_listen_address_for_a_listed_name() {
    let s = settings(&format!("db.example.com:{}", free_port(LISTEN)));
    let running = run(&s, &Fake::new(&[])).await;
    let a = ask(running.dns, "db.example.com", TYPE_A).await;
    assert_eq!((a.rcode, a.addrs), (0, vec![LISTEN]));
}

#[tokio::test]
async fn answers_the_advertised_address_when_set() {
    let mut s = settings(&format!(
        "db.example.com:{},pg.ext=1.1.1.1:{}",
        free_port(LISTEN),
        free_port(LISTEN)
    ));
    s.advertise = Ipv4Addr::new(10, 9, 8, 7);
    let running = run(&s, &Fake::new(&[])).await;
    for name in ["db.example.com", "pg.ext"] {
        assert_eq!(
            ask(running.dns, name, TYPE_A).await.addrs,
            [Ipv4Addr::new(10, 9, 8, 7)]
        );
    }
    // The alias is the name; an IP-literal destination has no name to answer.
    assert_eq!(ask(running.dns, "1.1.1.1", TYPE_A).await.rcode, 3);
}

#[tokio::test]
async fn answers_empty_for_aaaa() {
    let s = settings(&format!("db.example.com:{}", free_port(LISTEN)));
    let running = run(&s, &Fake::new(&[])).await;
    for qtype in [TYPE_AAAA, 255, 15] {
        let a = ask(running.dns, "db.example.com", qtype).await;
        assert_eq!((a.rcode, a.addrs.len()), (0, 0), "type {qtype}");
    }
}

#[tokio::test]
async fn is_case_and_dot_insensitive() {
    let s = settings(&format!("Db.Example.com.:{}", free_port(LISTEN)));
    let running = run(&s, &Fake::new(&[])).await;
    for name in ["db.example.com", "DB.EXAMPLE.COM.", "dB.eXample.Com"] {
        assert_eq!(
            ask(running.dns, name, TYPE_A).await.addrs,
            [LISTEN],
            "{name}"
        );
    }
}

#[tokio::test]
async fn answers_an_edns_query_on_the_wire() {
    let s = settings(&format!("db.example.com:{}", free_port(LISTEN)));
    let running = run(&s, &Fake::new(&[])).await;
    let q = wire::query_edns(0x4343, "dB.EXAMPLE.com", TYPE_A, 0, true);
    let udp = wire::read(&q, &udp_ask(running.dns, &q).await.expect("a UDP answer"));
    let tcp = wire::read(&q, &tcp_ask(running.dns, &q).await);
    assert_eq!(udp, tcp);
    assert_eq!((udp.rcode, &udp.addrs), (0, &vec![LISTEN]));
    let opt = udp.opt.expect("an OPT record");
    assert_eq!((opt.version, opt.flags), (0, 0), "version 0, DO clear");
}

#[tokio::test]
async fn refuses_an_unlisted_name_without_asking_upstream() {
    let s = settings(&format!("db.example.com:{}", free_port(LISTEN)));
    // The forwarder's resolver, recording: the DNS path must never reach it.
    let fake = Fake::new(&[
        ("db.example.com", &[&[v4(D2)]]),
        ("other.example.com", &[&[v4(D2)]]),
    ]);
    let running = run(&s, &fake).await;
    for name in [
        "other.example.com",
        "example.com",
        "x.db.example.com",
        "gateway",
    ] {
        for qtype in [TYPE_A, TYPE_AAAA, 255] {
            let a = ask(running.dns, name, qtype).await;
            assert_eq!((a.rcode, a.addrs.len()), (3, 0), "{name} {qtype}");
        }
    }
    assert_eq!(
        ask(running.dns, "db.example.com", TYPE_A).await.addrs,
        [LISTEN]
    );
    assert_eq!(fake.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn survives_malformed_queries_on_the_wire() {
    let s = settings(&format!("db.example.com:{}", free_port(LISTEN)));
    let running = run(&s, &Fake::new(&[])).await;
    let good = wire::query(7, "db.example.com", TYPE_A);
    // UDP: nothing, garbage, a truncated question, an oversized datagram.
    assert_eq!(udp_ask(running.dns, &[]).await, None);
    assert_eq!(udp_ask(running.dns, &[0xff; 5]).await, None);
    let truncated = udp_ask(running.dns, &good[..20]).await.expect("FORMERR");
    assert_eq!(truncated[3] & 0x0f, 1);
    let mut big = good.clone();
    big.resize(8000, 0);
    assert_eq!(udp_ask(running.dns, &big).await, None);
    // TCP: an oversized length and a short message each end their connection.
    for (len, body) in [(0xffffu16, vec![0u8; 16]), (5, vec![1u8; 5])] {
        let mut c = TcpStream::connect(running.dns).await.unwrap();
        c.write_all(&len.to_be_bytes()).await.unwrap();
        let _ = c.write_all(&body).await;
        let mut buf = Vec::new();
        let r = tokio::time::timeout(WAIT, c.read_to_end(&mut buf))
            .await
            .expect("closed");
        assert!(r.is_err() || buf.is_empty(), "{buf:?}");
    }
    // Still serving.
    assert_eq!(
        ask(running.dns, "db.example.com", TYPE_A).await.addrs,
        [LISTEN]
    );
}

// Forwarder.

#[tokio::test]
async fn forwards_both_ways() {
    let d = dest(D2, 0, b"").await;
    let s = settings(&format!("a.example.com:{}", d.port));
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let mut c = connect(d.port).await;
    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i * 7 % 251) as u8).collect();
    let (mut r, mut w) = c.split();
    let send = async {
        w.write_all(&payload).await.unwrap();
    };
    let recv = async {
        let mut got = vec![0; payload.len()];
        r.read_exact(&mut got).await.unwrap();
        got
    };
    let ((), got) = tokio::time::timeout(WAIT, async { tokio::join!(send, recv) })
        .await
        .unwrap();
    assert!(got == payload, "the bytes changed on the way");
}

#[tokio::test]
async fn propagates_half_close() {
    // The destination reads to the end, then answers with the count and closes.
    let listener = TcpListener::bind((D2, free_port(LISTEN))).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).await.unwrap();
        s.write_all(format!("got {}", all.len()).as_bytes())
            .await
            .unwrap();
    });
    let s = settings(&format!("a.example.com:{port}"));
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let mut c = connect(port).await;
    c.write_all(&[9; 1000]).await.unwrap();
    c.shutdown().await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(WAIT, c.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, b"got 1000");
}

#[tokio::test]
async fn an_idle_connection_stays_open() {
    let d = dest(D2, 0, b"").await;
    let mut s = settings(&format!("a.example.com:{}", d.port));
    // Longer idle than every timer the service has.
    s.dial_timeout = Duration::from_millis(100);
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let mut c = connect(d.port).await;
    echo(&mut c, b"before").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    echo(&mut c, b"after").await;
}

// Policy, per connection.

async fn closes_without_forwarding(
    resolved: IpAddr,
    dest_ip: Ipv4Addr,
    internal: &[&str],
    injected: bool,
) {
    let d = dest(dest_ip, 0, b"").await;
    let listen_port = free_port(LISTEN);
    let mut s = settings(&format!("a.example.com:{}@{listen_port}", d.port));
    s.internal = internal.iter().map(|n| n.parse::<Net>().unwrap()).collect();
    let fake = Fake::new(&[("a.example.com", &[&[resolved]])]);
    let policy = Policy::for_settings(&s);
    let policy = if injected {
        policy.with_public_check(loopback_is_public)
    } else {
        policy
    };
    let _running = run_with(&s, &fake, policy).await;
    assert_closed(connect(listen_port).await).await;
    assert_eq!(
        d.accepted.load(Ordering::SeqCst),
        0,
        "{resolved} was dialled"
    );
    assert_eq!(fake.calls(), ["a.example.com."]);
}

#[tokio::test]
async fn closes_without_forwarding_loopback() {
    closes_without_forwarding(v4(D2), D2, &[], false).await;
}

#[tokio::test]
async fn closes_without_forwarding_rfc1918() {
    for a in ["10.0.0.1", "172.16.0.1", "192.168.1.1"] {
        closes_without_forwarding(a.parse().unwrap(), D2, &[], false).await;
    }
}

#[tokio::test]
async fn closes_without_forwarding_cgnat() {
    closes_without_forwarding("100.64.0.1".parse().unwrap(), D2, &[], false).await;
}

#[tokio::test]
async fn closes_without_forwarding_link_local() {
    closes_without_forwarding("169.254.169.254".parse().unwrap(), D2, &[], false).await;
}

#[tokio::test]
async fn closes_without_forwarding_ula() {
    closes_without_forwarding("fd00::1".parse().unwrap(), D2, &[], false).await;
}

#[tokio::test]
async fn closes_without_forwarding_an_ipv4_mapped_private_address() {
    closes_without_forwarding("::ffff:10.0.0.1".parse().unwrap(), D2, &[], false).await;
    closes_without_forwarding("::ffff:127.0.0.2".parse().unwrap(), D2, &[], false).await;
}

#[tokio::test]
async fn closes_without_forwarding_an_internal_subnet() {
    // Counted public by the injected check, refused because it is internal.
    closes_without_forwarding(v4(D2), D2, &["127.0.0.0/30"], true).await;
}

#[tokio::test]
async fn closes_without_forwarding_its_own_address() {
    // The listen address: counted public by the injected check, refused as the service's own.
    closes_without_forwarding(v4(LISTEN), LISTEN, &[], true).await;
}

#[tokio::test]
async fn mixed_answers_dial_only_public_addresses() {
    let refused = dest(D2, 0, b"2").await;
    let allowed = dest(D3, refused.port, b"3").await;
    let s = settings(&format!("a.example.com:{}", refused.port));
    let fake = Fake::new(&[("a.example.com", &[&[v4(D2), v4(D3), v4(D2)]])]);
    let policy = Policy::for_settings(&s).with_public_check(only_d3_is_public);
    let _running = run_with(&s, &fake, policy).await;
    for _ in 0..3 {
        assert_eq!(read_exact(&mut connect(refused.port).await, 1).await, b"3");
    }
    assert_eq!(
        (
            refused.accepted.load(Ordering::SeqCst),
            allowed.accepted.load(Ordering::SeqCst)
        ),
        (0, 3)
    );
}

#[tokio::test]
async fn all_refused_closes_the_connection() {
    let d = dest(D2, 0, b"2").await;
    let s = settings(&format!("a.example.com:{}", d.port));
    let fake = Fake::new(&[("a.example.com", &[&[v4(D2), "10.0.0.1".parse().unwrap()]])]);
    let policy = Policy::for_settings(&s).with_public_check(only_d3_is_public);
    let _running = run_with(&s, &fake, policy).await;
    assert_closed(connect(d.port).await).await;
    assert_eq!(d.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_dialled_address_is_a_checked_one() {
    // Each lookup answers differently: a checked address first, then a refused one. One
    // lookup per connection, and the forwarder dials what it checked.
    let refused = dest(D2, 0, b"2").await;
    let allowed = dest(D3, refused.port, b"3").await;
    let s = settings(&format!("a.example.com:{}", refused.port));
    let fake = Fake::new(&[("a.example.com", &[&[v4(D3)], &[v4(D2)]])]);
    let policy = Policy::for_settings(&s).with_public_check(only_d3_is_public);
    let _running = run_with(&s, &fake, policy).await;
    assert_eq!(read_exact(&mut connect(refused.port).await, 1).await, b"3");
    assert_closed(connect(refused.port).await).await;
    assert_eq!(fake.calls(), ["a.example.com.", "a.example.com."]);
    assert_eq!(
        (
            refused.accepted.load(Ordering::SeqCst),
            allowed.accepted.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
}

#[tokio::test]
async fn the_lookup_gets_the_absolute_name() {
    // With a relative name the system resolver may try the search domains first (or after
    // the name fails) and dial what a name under them returns.
    let d = dest(D2, 0, b"2").await;
    let s = settings(&format!("a.example.com:{}", d.port));
    let fake = Fake::new(&[("a.example.com", &[&[v4(D2)]])]);
    let _running = run(&s, &fake).await;
    assert_eq!(read_exact(&mut connect(d.port).await, 1).await, b"2");
    assert_eq!(fake.calls(), ["a.example.com."]);
}

#[tokio::test]
async fn an_unresolvable_name_closes_the_connection() {
    let d = dest(D2, 0, b"").await;
    let s = settings(&format!("gone.example.com:{}", d.port));
    let _running = run(&s, &Fake::new(&[])).await;
    assert_closed(connect(d.port).await).await;
}

// Limits.

#[tokio::test]
async fn the_connection_limit_closes_new_connections() {
    let d = dest(D2, 0, b"").await;
    let mut s = settings(&format!("a.example.com:{}", d.port));
    s.max_connections = 2;
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let mut first = connect_from(Ipv4Addr::new(127, 0, 0, 11), d.port).await;
    let mut second = connect_from(Ipv4Addr::new(127, 0, 0, 12), d.port).await;
    echo(&mut first, b"one").await;
    echo(&mut second, b"two").await;
    assert_closed(connect_from(Ipv4Addr::new(127, 0, 0, 13), d.port).await).await;
    echo(&mut first, b"still one").await;
    echo(&mut second, b"still two").await;
    assert_eq!(d.accepted.load(Ordering::SeqCst), 2);
    // A closed connection frees its slot.
    drop(first);
    let mut third = None;
    for _ in 0..50 {
        let mut c = connect_from(Ipv4Addr::new(127, 0, 0, 13), d.port).await;
        c.write_all(b"x").await.unwrap();
        let mut b = [0; 1];
        if matches!(
            tokio::time::timeout(Duration::from_millis(200), c.read(&mut b)).await,
            Ok(Ok(1))
        ) {
            third = Some(c);
            break;
        }
    }
    assert!(third.is_some(), "the slot was never freed");
}

#[tokio::test]
async fn one_client_cannot_take_every_slot() {
    let d = dest(D2, 0, b"").await;
    let mut s = settings(&format!("a.example.com:{}", d.port));
    (s.max_connections, s.max_connections_per_client) = (4, 1);
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let mut mine = connect_from(Ipv4Addr::new(127, 0, 0, 11), d.port).await;
    echo(&mut mine, b"mine").await;
    assert_closed(connect_from(Ipv4Addr::new(127, 0, 0, 11), d.port).await).await;
    let mut other = connect_from(Ipv4Addr::new(127, 0, 0, 12), d.port).await;
    echo(&mut other, b"other").await;
    echo(&mut mine, b"mine again").await;
}

#[tokio::test]
async fn a_dial_timeout_frees_the_slot() {
    let d = dest(D2, 0, b"").await;
    let hang = free_port(LISTEN);
    let mut s = settings(&format!("hang.example.com:{hang},a.example.com:{}", d.port));
    s.max_connections = 1;
    s.dial_timeout = Duration::from_millis(300);
    let _running = run(&s, &Fake::new(&[("a.example.com", &[&[v4(D2)]])])).await;
    let started = Instant::now();
    let c = connect(hang).await;
    assert_closed(c).await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_secs(2),
        "{took:?}"
    );
    let mut c = connect(d.port).await;
    echo(&mut c, b"after the timeout").await;
}

// Logs.

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

impl LogBuf {
    fn lines_with(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(needle))
            .map(str::to_string)
            .collect()
    }

    async fn wait_for(&self, needle: &str) -> Vec<String> {
        self.wait_for_count(needle, 1).await
    }

    async fn wait_for_count(&self, needle: &str, count: usize) -> Vec<String> {
        for _ in 0..100 {
            let lines = self.lines_with(needle);
            if lines.len() >= count {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fewer than {count} log lines with {needle}");
    }
}

/// The operator deny list (`CHELABASE_COMPUTE_EGRESS_DENY`, read by `Settings::from_lookup` as
/// the binary reads it) refuses a listed name that resolves into it, on every connection, with
/// a `refused` line naming the entry, the address and the reason. The injected check counts the
/// address as public, so only the deny list stands between the name and the dial; an address
/// outside the list is still dialled.
#[tokio::test]
async fn a_listed_name_resolving_into_the_deny_list_is_refused_per_connection() {
    let logs = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let denied = dest(D2, 0, b"2").await;
    let allowed = dest(D3, denied.port, b"3").await;
    let (dns_port, health_port) = (free_port(LISTEN), free_port(Ipv4Addr::LOCALHOST));
    let listen_port = free_port(LISTEN);
    let s = Settings::from_lookup(|k| {
        Some(match k {
            "CHELABASE_COMPUTE_EGRESS_LISTEN" => LISTEN.to_string(),
            "CHELABASE_COMPUTE_EGRESS_DNS_PORT" => dns_port.to_string(),
            "CHELABASE_COMPUTE_EGRESS_HEALTH_PORT" => health_port.to_string(),
            "CHELABASE_COMPUTE_EGRESS_ALLOW" => format!(
                "a.example.com:{p}@{listen_port},b.example.com:{p}",
                p = denied.port
            ),
            // A network and a plain address; 127.0.0.2/32 is D2.
            "CHELABASE_COMPUTE_EGRESS_DENY" => "192.0.2.0/24, 127.0.0.2".to_string(),
            _ => return None,
        })
    })
    .unwrap();
    let fake = Fake::new(&[
        ("a.example.com", &[&[v4(D2)]]),
        ("b.example.com", &[&[v4(D3)]]),
    ]);
    let _running = run(&s, &fake).await;

    for _ in 0..2 {
        let c = connect(listen_port).await;
        let client = c.local_addr().unwrap().to_string();
        assert_closed(c).await;
        let lines = logs.wait_for(&client).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(
            line.contains("refused") && line.contains("a.example.com"),
            "{line}"
        );
        assert!(
            line.contains("127.0.0.2") && line.contains("denied"),
            "the reason: {line}"
        );
    }
    // Outside the list: dialled as before.
    assert_eq!(read_exact(&mut connect(denied.port).await, 1).await, b"3");
    assert_eq!(
        (
            denied.accepted.load(Ordering::SeqCst),
            allowed.accepted.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
    assert_eq!(
        fake.calls(),
        ["a.example.com.", "a.example.com.", "b.example.com."]
    );
}

#[tokio::test]
async fn logs_a_line_when_a_connection_opens_and_one_when_it_closes_without_payload() {
    let logs = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let d = dest(D2, 0, b"").await;
    let refused_port = free_port(LISTEN);
    let s = settings(&format!(
        "a.example.com:{p},b.example.com:{p}@{refused_port}",
        p = d.port
    ));
    let fake = Fake::new(&[
        ("a.example.com", &[&[v4(D2)]]),
        ("b.example.com", &[&[v4(D3), "10.0.0.1".parse().unwrap()]]),
    ]);
    let policy = Policy::for_settings(&s).with_public_check(only_d2_is_public);
    let _running = run_with(&s, &fake, policy).await;

    let mut c = connect(d.port).await;
    let client = c.local_addr().unwrap().to_string();
    echo(&mut c, b"SECRET-PAYLOAD-0123").await;
    // The connection is open: one line says so, with the entry and the dialled address.
    let lines = logs.wait_for(&client).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    let opened = &lines[0];
    assert!(
        opened.contains(&format!("a.example.com:{}", d.port)) && opened.contains("opened"),
        "{opened}"
    );
    assert!(
        opened.contains(&format!("127.0.0.2:{}", d.port)),
        "the dialled address: {opened}"
    );
    assert!(!opened.contains("sent="), "no byte counts yet: {opened}");
    drop(c);
    let lines = logs.wait_for_count(&client, 2).await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    let line = &lines[1];
    assert!(
        line.contains(&format!("a.example.com:{}", d.port)) && line.contains("forwarded"),
        "{line}"
    );
    assert!(
        line.contains(&format!("127.0.0.2:{}", d.port)),
        "the dialled address: {line}"
    );

    let r = connect(refused_port).await;
    let client = r.local_addr().unwrap().to_string();
    assert_closed(r).await;
    let lines = logs.wait_for(&client).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(
        line.contains("b.example.com") && line.contains("refused"),
        "{line}"
    );
    assert!(
        line.contains("10.0.0.1") && line.contains("not public"),
        "the reason: {line}"
    );

    let all = String::from_utf8_lossy(&logs.0.lock().unwrap()).to_string();
    assert!(
        !all.contains("SECRET-PAYLOAD") && !all.contains("hello"),
        "forwarded bytes in the log"
    );
}

fn only_d2_is_public(ip: IpAddr) -> bool {
    ip == v4(D2)
}

// Source.

/// Ordinary TCP and UDP sockets only, and no writes outside stdout and stderr: none of these
/// appear in the crate's code (comments aside).
#[test]
fn opens_only_ordinary_sockets() {
    let sources = [
        ("lib.rs", include_str!("lib.rs")),
        ("main.rs", include_str!("main.rs")),
        ("allow.rs", include_str!("allow.rs")),
        ("dns.rs", include_str!("dns.rs")),
        ("forward.rs", include_str!("forward.rs")),
        ("limits.rs", include_str!("limits.rs")),
        ("policy.rs", include_str!("policy.rs")),
    ];
    let forbidden = [
        concat!("SOCK_", "RAW"),
        concat!("Type::", "RAW"),
        concat!("Type::", "PACKET"),
        concat!("net", "link"),
        concat!("bind_", "device"),
        concat!("IP_", "TRANSPARENT"),
        concat!("set_", "mark"),
        concat!("set_", "freebind"),
        concat!("set_", "ip_transparent"),
        concat!("Socket::", "new"),
        concat!("libc", "::"),
        concat!("fs::", "write"),
        concat!("File::", "create"),
        concat!("Open", "Options"),
        concat!("std::", "process::Command"),
        concat!("unsafe", " "),
    ];
    for (file, text) in sources {
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for f in forbidden {
                assert!(!line.contains(f), "{file}:{}: {f}", n + 1);
            }
        }
    }
}

// Serve tasks.

/// A responder that panics on a query whose first name label is `boom`.
fn panics_on_boom(names: &crate::dns::Names, q: &[u8]) -> Option<Vec<u8>> {
    if q.get(13..17) == Some(b"boom") {
        panic!("injected decoder panic");
    }
    crate::dns::respond(names, q)
}

#[tokio::test]
async fn a_panicking_message_is_dropped_and_the_next_query_is_answered() {
    let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let dns = sock.local_addr().unwrap();
    let names = Arc::new(crate::dns::Names::new(
        ["a.example.com".to_string(), "boom.example.com".to_string()],
        LISTEN,
    ));
    let task = tokio::spawn(crate::dns::serve_udp_with(sock, names, panics_on_boom));

    let bad = wire::query(1, "boom.example.com", TYPE_A);
    assert_eq!(
        udp_ask(dns, &bad).await,
        None,
        "the panicking message is dropped"
    );
    let q = wire::query(2, "a.example.com", TYPE_A);
    let a = wire::read(&q, &udp_ask(dns, &q).await.expect("answered after a panic"));
    assert_eq!((a.rcode, &a.addrs), (0, &vec![LISTEN]));
    // Again, to show it is not a one-off.
    assert_eq!(udp_ask(dns, &bad).await, None);
    assert!(udp_ask(dns, &q).await.is_some());
    assert!(!task.is_finished(), "the UDP resolver is still serving");
}

#[tokio::test]
async fn a_serve_task_that_ends_ends_the_service() {
    let s = settings(&format!("a.example.com:{}", free_port(LISTEN)));
    let fake = Fake::new(&[]);
    let mut running = run(&s, &fake).await;
    running.tasks.spawn(async { "test task" });
    let result = tokio::time::timeout(WAIT, serve_until(&mut running, std::future::pending()))
        .await
        .expect("the service keeps running after a serve task ended");
    let why = result.expect_err("a serve task ended, so the service must stop");
    assert!(why.contains("test task"), "{why}");
}

#[tokio::test]
async fn a_stop_signal_ends_the_service_cleanly() {
    let s = settings(&format!("a.example.com:{}", free_port(LISTEN)));
    let fake = Fake::new(&[]);
    let mut running = run(&s, &fake).await;
    assert_eq!(serve_until(&mut running, async {}).await, Ok(()));
}

#[tokio::test]
async fn a_serve_task_that_panics_ends_the_service() {
    let s = settings(&format!("a.example.com:{}", free_port(LISTEN)));
    let fake = Fake::new(&[]);
    let mut running = run(&s, &fake).await;
    running.tasks.spawn(async { panic!("injected") });
    let why = tokio::time::timeout(WAIT, serve_until(&mut running, std::future::pending()))
        .await
        .expect("the service keeps running after a serve task panicked")
        .expect_err("a serve task panicked");
    assert!(why.contains("panicked"), "{why}");
}
