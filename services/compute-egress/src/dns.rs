//! The resolver: answers `A` for the allow-listed names with the advertised address, an empty
//! NOERROR for any other type or class of a listed name, and NXDOMAIN for every other name. It
//! never forwards or recurses, so it can't carry anything to an upstream: nothing here sends a
//! query, and `hickory-proto` is built without its networking or resolver code.
//!
//! Messages are parsed and built with `hickory-proto`. A message that isn't a single
//! well-formed query is dropped (shorter than a header, a response, oversized) or answered
//! with FORMERR or NOTIMP, never a panic.
//!
//! The work is split so DNSSEC can be added without a rewrite: [`respond`] handles the wire
//! (limits, header checks, decoding, encoding), [`answer`] turns one decoded query into a
//! response [`Message`], and [`edns`] decides the OPT record (where the DO bit is handled).
//! DNSSEC support comes from `hickory-proto`'s `dnssec-aws-lc-rs` feature, never
//! `dnssec-ring`.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::Arc;

use std::time::Duration;

use hickory_proto::op::{Edns, Header, Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{DNSClass, Name, RData, Record, RecordType};
use hickory_proto::serialize::binary::{BinDecodable, BinDecoder, BinEncodable};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;

use crate::limits::Limits;

/// The longest message read, over UDP or TCP; a longer one is dropped.
pub const MAX_MESSAGE: usize = 4096;
/// Seconds an answer may be cached.
pub const TTL: u32 = 30;
/// The UDP payload size an EDNS answer advertises (the DNS flag day 2020 value).
pub const EDNS_PAYLOAD: u16 = 1232;

#[cfg(test)]
const RCODE_FORMERR: u8 = 1;
#[cfg(test)]
const RCODE_NXDOMAIN: u8 = 3;
#[cfg(test)]
const RCODE_NOTIMP: u8 = 4;
#[cfg(test)]
const TYPE_A: u16 = 1;

/// The listed names (lower case, no trailing dot) and the address they resolve to.
#[derive(Debug, Clone)]
pub struct Names {
    names: HashSet<String>,
    answer: Ipv4Addr,
}

impl Names {
    pub fn new(names: impl IntoIterator<Item = String>, answer: Ipv4Addr) -> Self {
        Names {
            names: names.into_iter().collect(),
            answer,
        }
    }

    /// Whether `name` is listed, compared label by label without regard to letter case. A
    /// label holding any byte but a letter, digit or hyphen (a `.` included) is in no listed
    /// name, so the name is not listed: labels are never joined into a string that could
    /// collide with one.
    fn lists(&self, name: &Name) -> bool {
        let mut joined = String::with_capacity(255);
        for label in name.iter() {
            if !label
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            {
                return false;
            }
            if !joined.is_empty() {
                joined.push('.');
            }
            joined.extend(label.iter().map(|b| b.to_ascii_lowercase() as char));
        }
        self.names.contains(&joined)
    }
}

/// Seconds a DNS-over-TCP connection may wait for its next message or a write.
pub const TCP_IDLE: Duration = Duration::from_secs(10);

/// The answer to `query`, or `None` to drop it.
pub fn respond(names: &Names, query: &[u8]) -> Option<Vec<u8>> {
    if query.len() > MAX_MESSAGE {
        return None;
    }
    // Shorter than a header: nothing to answer to.
    let header = Header::read(&mut BinDecoder::new(query)).ok()?;
    if header.message_type == MessageType::Response {
        return None;
    }
    if header.op_code != OpCode::Query {
        return encode(&error(&header, ResponseCode::NotImp));
    }
    if header.counts.queries != 1 {
        return encode(&error(&header, ResponseCode::FormErr));
    }
    // Every section must decode, and two OPT records are an error too (RFC 6891 6.1.1).
    let Ok(request) = Message::from_vec(query) else {
        return encode(&error(&header, ResponseCode::FormErr));
    };
    let question = request.queries.first()?;
    // The question goes back exactly as it came, letter case included. One that doesn't
    // encode back to its own bytes used name compression, which a question never needs.
    let echoed = question.to_bytes().ok()?;
    if query.get(12..12 + echoed.len()) != Some(echoed.as_slice()) {
        return encode(&error(&header, ResponseCode::FormErr));
    }
    encode(&answer(names, &request, question))
}

/// The response to `request`, whose one question is `question`: authoritative, RD echoed, RA
/// clear, the question echoed, at most one `A` answer, and an OPT record when the request had
/// one.
fn answer(names: &Names, request: &Message, question: &Query) -> Message {
    let mut resp = Message::response(request.id, OpCode::Query);
    resp.metadata.authoritative = true;
    resp.metadata.recursion_desired = request.recursion_desired;
    resp.add_query(question.clone());
    let edns = request.edns.as_ref().map(edns);
    if request.edns.as_ref().is_some_and(|e| e.version() != 0) {
        // RFC 6891 6.1.3: only version 0 is answered.
        resp.metadata.response_code = ResponseCode::BADVERS;
    } else if !names.lists(question.name()) {
        resp.metadata.response_code = ResponseCode::NXDomain;
    } else if question.query_type() == RecordType::A && question.query_class() == DNSClass::IN {
        resp.add_answer(Record::from_rdata(
            question.name().clone(),
            TTL,
            RData::A(A(names.answer)),
        ));
    }
    if let Some(edns) = edns {
        resp.set_edns(edns);
    }
    resp
}

/// The OPT record answering the request's: version 0, [`EDNS_PAYLOAD`], no options, and DO
/// clear (there is no DNSSEC yet).
fn edns(_request: &Edns) -> Edns {
    let mut e = Edns::new();
    e.set_max_payload(EDNS_PAYLOAD);
    e.set_version(0);
    e
}

/// A header-only error answer to `header`: its id and opcode, RD echoed, RA clear.
fn error(header: &Header, rcode: ResponseCode) -> Message {
    let mut resp = Message::error_msg(header.id, header.op_code, rcode);
    resp.metadata.authoritative = true;
    resp.metadata.recursion_desired = header.recursion_desired;
    resp
}

fn encode(m: &Message) -> Option<Vec<u8>> {
    m.to_vec().ok()
}

/// The shape of [`respond`]; the serve loops take it so a test can inject a panicking one.
pub type Responder = fn(&Names, &[u8]) -> Option<Vec<u8>>;

/// `responder` on one message; a panic inside it (a decoder bug) drops that message and is
/// logged, so one bad datagram never ends the loop that serves every other client.
fn guarded(responder: Responder, names: &Names, msg: &[u8]) -> Option<Vec<u8>> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| responder(names, msg))).unwrap_or_else(
        |_| {
            tracing::error!("dns: a message made the decoder panic; dropped");
            None
        },
    )
}

/// Answers UDP queries until the task is stopped. A datagram over [`MAX_MESSAGE`] is dropped.
pub async fn serve_udp(sock: UdpSocket, names: Arc<Names>) {
    serve_udp_with(sock, names, respond).await;
}

/// [`serve_udp`] with the responder given.
pub async fn serve_udp_with(sock: UdpSocket, names: Arc<Names>, responder: Responder) {
    let mut buf = vec![0u8; MAX_MESSAGE + 1];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "dns: receive failed");
                continue;
            }
        };
        if n > MAX_MESSAGE {
            continue;
        }
        if let Some(resp) = guarded(responder, &names, &buf[..n]) {
            let _ = sock.send_to(&resp, peer).await;
        }
    }
}

/// Answers TCP queries (length-prefixed), one task per connection within `limits`. A
/// connection ends on an oversized or unanswerable message, or after [`TCP_IDLE`] without one.
pub async fn serve_tcp(listener: TcpListener, names: Arc<Names>, limits: Arc<Limits>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "dns: accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(permit) = limits.try_acquire(peer.ip()) else {
            continue;
        };
        let names = names.clone();
        tokio::spawn(async move {
            let _permit = permit;
            serve_tcp_connection(stream, &names).await;
        });
    }
}

async fn serve_tcp_connection(mut s: TcpStream, names: &Names) {
    loop {
        let mut len = [0u8; 2];
        if !matches!(timeout(TCP_IDLE, s.read_exact(&mut len)).await, Ok(Ok(_))) {
            return;
        }
        let len = u16::from_be_bytes(len) as usize;
        if len > MAX_MESSAGE {
            return;
        }
        let mut msg = vec![0u8; len];
        if !matches!(timeout(TCP_IDLE, s.read_exact(&mut msg)).await, Ok(Ok(_))) {
            return;
        }
        let Some(resp) = guarded(respond, names, &msg) else {
            return;
        };
        let mut out = (resp.len() as u16).to_be_bytes().to_vec();
        out.extend_from_slice(&resp);
        if !matches!(timeout(TCP_IDLE, s.write_all(&out)).await, Ok(Ok(()))) {
            return;
        }
    }
}

/// Query building and answer reading, for tests. Written by hand from RFC 1035 and RFC 6891,
/// independent of the parser under test.
#[cfg(test)]
pub(crate) mod wire {
    use super::*;

    const TYPE_OPT: u16 = 41;

    /// A standard query (recursion desired) for `name` of type `qtype`, class IN. Each label is
    /// written as given, letter case included.
    pub fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut q = id.to_be_bytes().to_vec();
        q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.trim_end_matches('.').split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    /// [`query`] with an OPT record: `payload`, EDNS `version`, and the DO bit when `dnssec_ok`.
    pub fn query_edns(id: u16, name: &str, qtype: u16, version: u8, dnssec_ok: bool) -> Vec<u8> {
        let mut q = query(id, name, qtype);
        q[11] = 1;
        q.push(0); // the root name
        q.extend_from_slice(&TYPE_OPT.to_be_bytes());
        q.extend_from_slice(&1232u16.to_be_bytes());
        q.extend_from_slice(&[0, version, if dnssec_ok { 0x80 } else { 0 }, 0]);
        q.extend_from_slice(&[0, 0]);
        q
    }

    /// The OPT record of a response.
    #[derive(Debug, PartialEq, Eq)]
    pub struct Opt {
        pub payload: u16,
        pub rcode_high: u8,
        pub version: u8,
        pub flags: u16,
        pub rdata: Vec<u8>,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct Answer {
        pub id: u16,
        pub rcode: u8,
        pub authoritative: bool,
        pub recursion_available: bool,
        pub qdcount: u16,
        pub addrs: Vec<Ipv4Addr>,
        pub opt: Option<Opt>,
    }

    /// The length of the question at the start of `b` (labels, then type and class).
    fn question_len(b: &[u8]) -> usize {
        let mut at = 0;
        while b[at] != 0 {
            at += 1 + b[at] as usize;
        }
        at + 1 + 4
    }

    /// Reads one of this resolver's responses to `query`: the question echoed byte for byte,
    /// `A` answers by a pointer to it with the TTL, no authority records, and at most an OPT
    /// record in the additional section, only when the query had one.
    pub fn read(query: &[u8], resp: &[u8]) -> Answer {
        assert!(resp.len() >= 12, "short response {resp:?}");
        assert!(resp[2] & 0x80 != 0, "not a response");
        assert_eq!(resp[3] & 0x70, 0, "Z, AD and CD clear");
        let count = |b: &[u8], i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        let (qdcount, ancount, nscount, arcount) = (
            count(resp, 4),
            count(resp, 6),
            count(resp, 8),
            count(resp, 10),
        );
        assert_eq!(nscount, 0, "authority records");
        let query_has_opt = count(query, 10) == 1;
        assert!(
            arcount == 0 || (arcount == 1 && query_has_opt),
            "additional records: {arcount}"
        );
        let mut at = 12;
        if qdcount == 1 {
            let qlen = question_len(&query[12..]);
            assert_eq!(
                &resp[12..12 + qlen],
                &query[12..12 + qlen],
                "the question is echoed"
            );
            at += qlen;
        }
        let mut addrs = Vec::new();
        for _ in 0..ancount {
            let rr = &resp[at..at + 16];
            assert_eq!(&rr[..10], &[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, TTL as u8]);
            assert_eq!(&rr[10..12], &[0, 4]);
            addrs.push(Ipv4Addr::new(rr[12], rr[13], rr[14], rr[15]));
            at += 16;
        }
        let mut opt = None;
        if arcount == 1 {
            let rr = &resp[at..];
            assert_eq!(&rr[..3], &[0, 0, TYPE_OPT as u8], "an OPT record");
            let rdlen = count(rr, 9) as usize;
            opt = Some(Opt {
                payload: count(rr, 3),
                rcode_high: rr[5],
                version: rr[6],
                flags: count(rr, 7),
                rdata: rr[11..11 + rdlen].to_vec(),
            });
            at += 11 + rdlen;
        }
        assert_eq!(at, resp.len(), "trailing bytes");
        Answer {
            id: u16::from_be_bytes([resp[0], resp[1]]),
            rcode: resp[3] & 0x0f,
            authoritative: resp[2] & 0x04 != 0,
            recursion_available: resp[3] & 0x80 != 0,
            qdcount,
            addrs,
            opt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wire::{self, query, read};
    use super::*;
    use proptest::prelude::*;

    const AAAA: u16 = 28;

    fn names() -> Names {
        Names::new(
            ["db.example.com".to_string(), "warehouse.ext".to_string()],
            Ipv4Addr::new(10, 9, 8, 7),
        )
    }

    fn ask(name: &str, qtype: u16) -> super::wire::Answer {
        let q = query(0x1234, name, qtype);
        let resp = respond(&names(), &q).unwrap_or_else(|| panic!("no answer for {name}"));
        read(&q, &resp)
    }

    #[test]
    fn answers_a_for_a_listed_name() {
        let a = ask("db.example.com", TYPE_A);
        assert_eq!(a.id, 0x1234);
        assert_eq!((a.rcode, a.qdcount), (0, 1));
        assert!(a.authoritative && !a.recursion_available);
        assert_eq!(a.addrs, [Ipv4Addr::new(10, 9, 8, 7)]);
        assert_eq!(
            ask("warehouse.ext", TYPE_A).addrs,
            [Ipv4Addr::new(10, 9, 8, 7)]
        );
    }

    #[test]
    fn answers_empty_for_other_types_of_a_listed_name() {
        for qtype in [AAAA, 15, 16, 255, 65] {
            let a = ask("db.example.com", qtype);
            assert_eq!((a.rcode, a.addrs.len()), (0, 0), "type {qtype}");
        }
    }

    #[test]
    fn answers_nxdomain_for_every_other_name() {
        for name in [
            "example.com",
            "x.db.example.com",
            "db.example.co",
            "db.example.com.evil",
            "gateway",
            "localhost",
        ] {
            for qtype in [TYPE_A, AAAA] {
                let a = ask(name, qtype);
                assert_eq!(
                    (a.rcode, a.addrs.len()),
                    (RCODE_NXDOMAIN, 0),
                    "{name} {qtype}"
                );
            }
        }
        let q = query(1, "db.example.com", TYPE_A);
        let empty = Names::new([], Ipv4Addr::new(10, 9, 8, 7));
        assert_eq!(
            read(&q, &respond(&empty, &q).unwrap()).rcode,
            RCODE_NXDOMAIN
        );
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(
            ask("DB.Example.COM", TYPE_A).addrs,
            [Ipv4Addr::new(10, 9, 8, 7)]
        );
    }

    #[test]
    fn a_label_holding_a_dot_is_not_a_listed_name() {
        // One label "db.example" then "com": not the three labels of the listed name.
        let mut q = vec![0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 10];
        q.extend_from_slice(b"db.example");
        q.extend_from_slice(&[3, b'c', b'o', b'm', 0, 0, 1, 0, 1]);
        assert_eq!(
            read(&q, &respond(&names(), &q).unwrap()).rcode,
            RCODE_NXDOMAIN
        );
    }

    #[test]
    fn another_class_gets_no_address() {
        let mut q = query(7, "db.example.com", TYPE_A);
        let n = q.len();
        q[n - 1] = 3; // CH
        let a = read(&q, &respond(&names(), &q).unwrap());
        assert_eq!((a.rcode, a.addrs.len()), (0, 0));
    }

    #[test]
    fn answers_an_edns_query_with_a_minimal_opt() {
        for (qtype, addrs) in [(TYPE_A, vec![Ipv4Addr::new(10, 9, 8, 7)]), (AAAA, vec![])] {
            // DO set: there is no DNSSEC, so the answer clears it.
            let q = wire::query_edns(9, "Db.Example.com", qtype, 0, true);
            let a = read(&q, &respond(&names(), &q).expect("an answer"));
            assert_eq!((a.rcode, a.addrs), (0, addrs), "type {qtype}");
            assert_eq!(
                a.opt,
                Some(wire::Opt {
                    payload: EDNS_PAYLOAD,
                    rcode_high: 0,
                    version: 0,
                    flags: 0,
                    rdata: vec![],
                })
            );
        }
        let q = wire::query_edns(9, "other.example.com", TYPE_A, 0, false);
        let a = read(&q, &respond(&names(), &q).unwrap());
        assert_eq!(a.rcode, RCODE_NXDOMAIN);
        assert!(a.opt.is_some());
        // Without an OPT in the query, none in the answer.
        assert_eq!(ask("db.example.com", TYPE_A).opt, None);
    }

    #[test]
    fn answers_badvers_to_another_edns_version() {
        let q = wire::query_edns(9, "db.example.com", TYPE_A, 1, false);
        let a = read(&q, &respond(&names(), &q).unwrap());
        // BADVERS is 16: 1 in the OPT's high bits, 0 in the header.
        assert_eq!(a.rcode, 0);
        assert!(a.addrs.is_empty());
        let opt = a.opt.expect("an OPT record");
        assert_eq!((opt.rcode_high, opt.version), (1, 0));
    }

    #[test]
    fn answers_formerr_to_two_opt_records() {
        let mut q = wire::query_edns(9, "db.example.com", TYPE_A, 0, false);
        let opt = q[q.len() - 11..].to_vec();
        q.extend_from_slice(&opt);
        q[11] = 2;
        assert_eq!(rcode(&q), (RCODE_FORMERR, 0));
    }

    #[test]
    fn drops_what_is_not_a_query() {
        let names = names();
        assert_eq!(respond(&names, &[]), None);
        assert_eq!(respond(&names, &[0; 11]), None);
        let mut resp = query(1, "db.example.com", TYPE_A);
        resp[2] |= 0x80; // a response: never answered
        assert_eq!(respond(&names, &resp), None);
        assert_eq!(respond(&names, &vec![0; MAX_MESSAGE + 1]), None);
    }

    #[track_caller]
    fn rcode(q: &[u8]) -> (u8, u16) {
        let resp = respond(&names(), q).expect("an error answer");
        assert_eq!(&resp[..2], &q[..2]);
        assert_eq!(resp.len(), 12, "a header-only error answer");
        (resp[3] & 0x0f, u16::from_be_bytes([resp[4], resp[5]]))
    }

    #[test]
    fn answers_formerr_to_a_malformed_question() {
        let good = query(5, "db.example.com", TYPE_A);
        // No question, two questions.
        for qd in [0u8, 2] {
            let mut q = good.clone();
            q[5] = qd;
            assert_eq!(rcode(&q), (RCODE_FORMERR, 0), "qdcount {qd}");
        }
        // Truncated anywhere inside the question.
        for cut in 13..good.len() {
            assert_eq!(rcode(&good[..cut]).0, RCODE_FORMERR, "cut at {cut}");
        }
        // A compression pointer, a reserved label type, a label over 63 bytes.
        for first in [0xc0u8, 0x40, 64] {
            let mut q = good[..12].to_vec();
            q.push(first);
            q.extend_from_slice(&[0; 70]);
            assert_eq!(rcode(&q).0, RCODE_FORMERR, "label byte {first:#x}");
        }
        // A name over 255 bytes.
        let mut q = good[..12].to_vec();
        for _ in 0..5 {
            q.push(63);
            q.extend_from_slice(&[b'a'; 63]);
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        assert_eq!(rcode(&q).0, RCODE_FORMERR);
    }

    #[test]
    fn answers_notimp_to_another_opcode() {
        let mut q = query(5, "db.example.com", TYPE_A);
        q[2] = 0x10 | 0x01; // opcode 2 (status), RD
        let resp = respond(&names(), &q).unwrap();
        assert_eq!(resp[3] & 0x0f, RCODE_NOTIMP);
        assert_eq!((resp[2] >> 3) & 0x0f, 2, "the opcode is echoed");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn survives_malformed_queries(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
            if let Some(resp) = respond(&names(), &bytes) {
                prop_assert!(resp.len() >= 12);
                prop_assert_eq!(&resp[..2], &bytes[..2]);
                prop_assert!(resp[2] & 0x80 != 0);
            }
        }

        #[test]
        fn answers_case_randomised_queries(
            listed in prop::sample::select(vec!["db.example.com", "warehouse.ext"]),
            flips in proptest::collection::vec(any::<bool>(), 16),
            dot in any::<bool>(),
            edns in any::<bool>(),
        ) {
            let mut name: String = listed
                .chars()
                .zip(flips.iter().cycle())
                .map(|(c, &up)| if up { c.to_ascii_uppercase() } else { c })
                .collect();
            if dot {
                name.push('.');
            }
            let q = if edns {
                wire::query_edns(11, &name, TYPE_A, 0, false)
            } else {
                query(11, &name, TYPE_A)
            };
            // `read` checks the question comes back byte for byte, letter case included.
            let a = read(&q, &respond(&names(), &q).unwrap());
            prop_assert_eq!((a.rcode, a.addrs), (0, vec![Ipv4Addr::new(10, 9, 8, 7)]));
        }

        #[test]
        fn survives_mutated_queries(
            name in "[a-z]{1,10}(\\.[a-z]{1,10}){0,3}",
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 0..6),
            cut in any::<usize>(),
        ) {
            let mut q = query(3, &name, TYPE_A);
            for (at, byte) in flips {
                let i = at % q.len();
                q[i] = byte;
            }
            let len = q.len();
            q.truncate(cut % (len + 1));
            if let Some(resp) = respond(&names(), &q) {
                prop_assert!(resp.len() >= 12);
                prop_assert_eq!(&resp[..2], &q[..2]);
            }
        }

        #[test]
        fn survives_hostile_records_after_the_question(
            name in "[a-zA-Z]{1,10}(\\.[a-zA-Z]{1,10}){0,3}",
            counts in proptest::array::uniform3(0u16..4),
            records in proptest::collection::vec(
                (
                    prop::sample::select(vec![
                        vec![0u8],
                        vec![0xc0, 0x0c],
                        vec![0xc0, 0x00],
                        vec![0xc0, 0xff],
                        vec![3, b'a', b'b', b'c', 0],
                        vec![0x40],
                    ]),
                    prop_oneof![
                        Just(41u16), Just(1), Just(2), Just(5), Just(6), Just(16), Just(28),
                        Just(33), Just(43), Just(46), Just(47), Just(48), Just(50), Just(250),
                        Just(255), any::<u16>(),
                    ],
                    any::<u16>(),
                    any::<u32>(),
                    proptest::collection::vec(any::<u8>(), 0..64),
                    any::<bool>(),
                ),
                0..4,
            ),
            tail in proptest::collection::vec(any::<u8>(), 0..32),
        ) {
            let mut q = query(5, &name, TYPE_A);
            q[6..8].copy_from_slice(&counts[0].to_be_bytes());
            q[8..10].copy_from_slice(&counts[1].to_be_bytes());
            q[10..12].copy_from_slice(&counts[2].to_be_bytes());
            for (owner, rtype, class, ttl, rdata, honest_len) in records {
                q.extend_from_slice(&owner);
                q.extend_from_slice(&rtype.to_be_bytes());
                q.extend_from_slice(&class.to_be_bytes());
                q.extend_from_slice(&ttl.to_be_bytes());
                let len = if honest_len { rdata.len() as u16 } else { rdata.len() as u16 ^ 0x15 };
                q.extend_from_slice(&len.to_be_bytes());
                q.extend_from_slice(&rdata);
            }
            q.extend_from_slice(&tail);
            if let Some(resp) = respond(&names(), &q) {
                prop_assert!(resp.len() >= 12);
                prop_assert_eq!(&resp[..2], &q[..2]);
                prop_assert!(resp[2] & 0x80 != 0);
                prop_assert!(resp.len() <= q.len() + 16, "{} > {} + 16", resp.len(), q.len());
            }
        }

        #[test]
        fn answers_an_address_only_for_a_listed_name(
            name in prop::sample::select(vec![
                "db.example.com", "DB.example.COM", "warehouse.ext", "db.example.co", "x.ext",
            ]),
            edns in any::<bool>(),
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 0..4),
        ) {
            let mut q = if edns {
                wire::query_edns(3, name, TYPE_A, 0, false)
            } else {
                query(3, name, TYPE_A)
            };
            for (at, byte) in flips {
                let i = at % q.len();
                q[i] = byte;
            }
            let Some(resp) = respond(&names(), &q) else {
                return Ok(());
            };
            // No amplification: at most the query plus one `A` record.
            prop_assert!(resp.len() <= q.len() + 16, "{} > {} + 16", resp.len(), q.len());
            if u16::from_be_bytes([resp[6], resp[7]]) == 0 {
                return Ok(());
            }
            // An answer: the question, read here label by label, is a listed name, of type A
            // and class IN.
            let mut at = 12;
            let mut labels = Vec::new();
            while resp[at] != 0 {
                let len = resp[at] as usize;
                prop_assert!(len <= 63);
                labels.push(String::from_utf8(resp[at + 1..at + 1 + len].to_ascii_lowercase()).unwrap());
                at += 1 + len;
            }
            prop_assert!(["db.example.com", "warehouse.ext"].contains(&labels.join(".").as_str()));
            prop_assert!(labels.iter().all(|l| !l.contains('.')));
            prop_assert_eq!(&resp[at + 1..at + 5], &[0, 1, 0, 1]);
        }
    }
}
