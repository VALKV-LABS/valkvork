//! Service discovery on the node: DNS for `<service>.<domain>`.
//!
//! The controller sends the agent every service's reachable node addresses
//! (`Endpoints`); [`Endpoints`] keeps the last set, in memory and in
//! `<data_dir>/endpoints.json`, so it answers through an agent restart while
//! the controller is unreachable. [`DnsServer`] answers A and AAAA queries for
//! names in the domain from it and forwards every other query, unchanged, to
//! the node's own resolvers.
//!
//! It listens on one address, by default the Docker bridge's gateway, which
//! only this node and its containers can reach (FR-ARC-01: no listener on the
//! network). Containers get that address as their only nameserver: with a
//! second one, musl (Alpine) would take whichever answers first, including an
//! upstream "no such name" for a service.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use valkvork_proto::v1 as pb;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_ANY: u16 = 255;
const RCODE_FORMERR: u16 = 1;
const RCODE_SERVFAIL: u16 = 2;
const RCODE_NXDOMAIN: u16 = 3;
const UDP_LIMIT: usize = 512;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
const TCP_IDLE: Duration = Duration::from_secs(10);

/// The last set from the controller: service id → addresses.
pub struct Endpoints {
    map: RwLock<BTreeMap<String, Vec<IpAddr>>>,
    path: PathBuf,
    rotate: AtomicUsize,
}

impl Endpoints {
    /// Load the set saved by a previous run, if any.
    pub fn load(data_dir: &Path) -> Arc<Self> {
        let path = data_dir.join("endpoints.json");
        let map = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<BTreeMap<String, Vec<IpAddr>>>(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            map: RwLock::new(map),
            path,
            rotate: AtomicUsize::new(0),
        })
    }

    /// Replace the set with the controller's, and save it.
    pub fn update(&self, e: pb::Endpoints) {
        let map: BTreeMap<String, Vec<IpAddr>> = e
            .services
            .into_iter()
            .map(|s| {
                let addrs = s.addresses.iter().filter_map(|a| a.parse().ok()).collect();
                (s.service_id.to_ascii_lowercase(), addrs)
            })
            .collect();
        if *self.map.read().unwrap() == map {
            return;
        }
        let services = map.len();
        let saved = serde_json::to_vec(&map).ok().and_then(|json| {
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, json)
                .and_then(|()| std::fs::rename(&tmp, &self.path))
                .ok()
        });
        if saved.is_none() {
            tracing::warn!(path = %self.path.display(), "could not save the service endpoints");
        }
        *self.map.write().unwrap() = map;
        tracing::info!(version = e.version, services, "service endpoints updated");
    }

    /// A service's addresses, rotated on each call to spread clients over
    /// them. None: no such service.
    pub fn lookup(&self, service: &str) -> Option<Vec<IpAddr>> {
        let mut addrs = self.map.read().unwrap().get(service)?.clone();
        if !addrs.is_empty() {
            let n = self.rotate.fetch_add(1, Ordering::Relaxed) % addrs.len();
            addrs.rotate_left(n);
        }
        Some(addrs)
    }
}

/// Nameservers in a resolv.conf, at port 53, except `exclude` (our own
/// address, to avoid a loop).
pub fn resolv_conf_nameservers(text: &str, exclude: IpAddr) -> Vec<SocketAddr> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            (it.next()? == "nameserver").then_some(())?;
            it.next()?.parse::<IpAddr>().ok()
        })
        .filter(|ip| *ip != exclude)
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

pub struct DnsServer {
    endpoints: Arc<Endpoints>,
    /// Lowercase, without a trailing dot.
    domain: String,
    upstream: Vec<SocketAddr>,
    ttl: u32,
}

/// A parsed query: what the answer needs.
struct Query {
    id: u16,
    flags: u16,
    /// Lowercase, without a trailing dot.
    name: String,
    qtype: u16,
    /// End of the question section.
    question_end: usize,
}

fn parse_query(pkt: &[u8]) -> Result<Query, ()> {
    if pkt.len() < 12 {
        return Err(());
    }
    let be = |i: usize| u16::from_be_bytes([pkt[i], pkt[i + 1]]);
    let (id, flags, qdcount) = (be(0), be(2), be(4));
    if flags & 0x8000 != 0 || qdcount != 1 {
        return Err(()); // a response, or not one question
    }
    let mut labels: Vec<String> = Vec::new();
    let mut i = 12;
    loop {
        let len = *pkt.get(i).ok_or(())? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return Err(()); // compression pointers are not used in questions
        }
        let label = pkt.get(i..i + len).ok_or(())?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        i += len;
    }
    if i + 4 > pkt.len() {
        return Err(());
    }
    Ok(Query {
        id,
        flags,
        name: labels.join("."),
        qtype: be(i),
        question_end: i + 4,
    })
}

/// Header and question of a response to `pkt`, with `rcode` and `answers`
/// records to follow.
fn response_head(pkt: &[u8], q: &Query, rcode: u16, answers: u16, authoritative: bool) -> Vec<u8> {
    // QR, the query's opcode and RD, RA, and AA for our own names.
    let mut flags = 0x8000 | (q.flags & 0x7900) | 0x0080 | rcode;
    if authoritative {
        flags |= 0x0400;
    }
    let mut out = Vec::with_capacity(UDP_LIMIT);
    out.extend_from_slice(&q.id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&answers.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&pkt[12..q.question_end]);
    out
}

/// An error response with just the header, for a query we cannot parse.
fn header_error(pkt: &[u8], rcode: u16) -> Option<Vec<u8>> {
    if pkt.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if flags & 0x8000 != 0 {
        return None; // never answer a response
    }
    let mut out = pkt[..12].to_vec();
    let flags = 0x8000 | (flags & 0x7900) | 0x0080 | rcode;
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[4..12].fill(0);
    Some(out)
}

impl DnsServer {
    pub fn new(
        endpoints: Arc<Endpoints>,
        domain: &str,
        upstream: Vec<SocketAddr>,
        ttl: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            endpoints,
            domain: domain.trim_end_matches('.').to_ascii_lowercase(),
            upstream,
            ttl,
        })
    }

    /// Bind UDP and TCP port 53 on `ip`, then serve in the background.
    /// Binding is done here, so the caller knows whether to point containers
    /// at it.
    pub async fn bind_and_serve(self: &Arc<Self>, ip: IpAddr) -> std::io::Result<()> {
        self.bind_and_serve_on(SocketAddr::new(ip, 53))
            .await
            .map(|_| ())
    }

    /// As `bind_and_serve`, on any address (tests use port 0). Returns the
    /// UDP and TCP addresses.
    pub async fn bind_and_serve_on(
        self: &Arc<Self>,
        addr: SocketAddr,
    ) -> std::io::Result<(SocketAddr, SocketAddr)> {
        let udp = Arc::new(UdpSocket::bind(addr).await?);
        let tcp = TcpListener::bind(SocketAddr::new(addr.ip(), udp.local_addr()?.port())).await?;
        let addrs = (udp.local_addr()?, tcp.local_addr()?);
        let me = self.clone();
        tokio::spawn(async move { me.serve_udp(udp).await });
        let me = self.clone();
        tokio::spawn(async move { me.serve_tcp(tcp).await });
        tracing::info!(listen = %addrs.0, domain = %self.domain, upstream = ?self.upstream, "service discovery DNS listening");
        Ok(addrs)
    }

    async fn serve_udp(self: Arc<Self>, sock: Arc<UdpSocket>) {
        let mut buf = vec![0u8; 4096];
        loop {
            let (n, from) = match sock.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "DNS receive failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let pkt = buf[..n].to_vec();
            let (me, sock) = (self.clone(), sock.clone());
            tokio::spawn(async move {
                if let Some(resp) = me.handle(&pkt, false).await {
                    let _ = sock.send_to(&resp, from).await;
                }
            });
        }
    }

    async fn serve_tcp(self: Arc<Self>, listener: TcpListener) {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let me = self.clone();
            tokio::spawn(async move { me.tcp_conn(conn).await });
        }
    }

    /// Length-prefixed queries on one connection, until it closes or idles.
    async fn tcp_conn(&self, mut conn: TcpStream) {
        loop {
            let mut len = [0u8; 2];
            match tokio::time::timeout(TCP_IDLE, conn.read_exact(&mut len)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
            let mut pkt = vec![0u8; u16::from_be_bytes(len) as usize];
            match tokio::time::timeout(TCP_IDLE, conn.read_exact(&mut pkt)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
            let Some(resp) = self.handle(&pkt, true).await else {
                return;
            };
            let mut out = (resp.len() as u16).to_be_bytes().to_vec();
            out.extend_from_slice(&resp);
            if conn.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    /// The response to one query: from the endpoints for our domain,
    /// otherwise from upstream.
    pub async fn handle(&self, pkt: &[u8], tcp: bool) -> Option<Vec<u8>> {
        let Ok(q) = parse_query(pkt) else {
            return header_error(pkt, RCODE_FORMERR);
        };
        if q.name == self.domain || q.name.ends_with(&format!(".{}", self.domain)) {
            return Some(self.answer(pkt, &q, tcp));
        }
        match self.forward(pkt, tcp).await {
            Some(resp) => Some(resp),
            None => Some(response_head(pkt, &q, RCODE_SERVFAIL, 0, false)),
        }
    }

    fn answer(&self, pkt: &[u8], q: &Query, tcp: bool) -> Vec<u8> {
        if q.name == self.domain {
            return response_head(pkt, q, 0, 0, true); // the zone exists
        }
        let service = &q.name[..q.name.len() - self.domain.len() - 1];
        let addrs = match (service.contains('.'), self.endpoints.lookup(service)) {
            (false, Some(addrs)) => addrs,
            _ => return response_head(pkt, q, RCODE_NXDOMAIN, 0, true),
        };
        let records: Vec<(u16, Vec<u8>)> = addrs
            .iter()
            .filter_map(|a| match (a, q.qtype) {
                (IpAddr::V4(v4), TYPE_A | TYPE_ANY) => Some((TYPE_A, v4.octets().to_vec())),
                (IpAddr::V6(v6), TYPE_AAAA | TYPE_ANY) => Some((TYPE_AAAA, v6.octets().to_vec())),
                _ => None,
            })
            .collect();
        // Over UDP, as many as fit in 512 bytes: clients use one anyway, and
        // the rotation spreads them.
        let limit = if tcp { u16::MAX as usize } else { UDP_LIMIT };
        let head = response_head(pkt, q, 0, 0, true);
        let mut body = Vec::new();
        let mut count: u16 = 0;
        for (rtype, rdata) in records {
            let mut rr = vec![0xC0, 0x0C]; // the question's name
            rr.extend_from_slice(&rtype.to_be_bytes());
            rr.extend_from_slice(&1u16.to_be_bytes()); // IN
            rr.extend_from_slice(&self.ttl.to_be_bytes());
            rr.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            rr.extend_from_slice(&rdata);
            if head.len() + body.len() + rr.len() > limit {
                break;
            }
            body.extend_from_slice(&rr);
            count += 1;
        }
        let mut out = head;
        out[6..8].copy_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Relay the query to each upstream in turn until one answers.
    async fn forward(&self, pkt: &[u8], tcp: bool) -> Option<Vec<u8>> {
        for up in &self.upstream {
            let result = if tcp {
                tokio::time::timeout(UPSTREAM_TIMEOUT, forward_tcp(*up, pkt)).await
            } else {
                tokio::time::timeout(UPSTREAM_TIMEOUT, forward_udp(*up, pkt)).await
            };
            match result {
                Ok(Ok(resp)) if resp.len() >= 2 && resp[..2] == pkt[..2] => return Some(resp),
                Ok(Ok(_)) => tracing::debug!(upstream = %up, "upstream answered another id"),
                Ok(Err(e)) => tracing::debug!(upstream = %up, error = %e, "upstream failed"),
                Err(_) => tracing::debug!(upstream = %up, "upstream timed out"),
            }
        }
        None
    }
}

async fn forward_udp(up: SocketAddr, pkt: &[u8]) -> std::io::Result<Vec<u8>> {
    let local: SocketAddr = if up.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let sock = UdpSocket::bind(local).await?;
    sock.connect(up).await?;
    sock.send(pkt).await?;
    let mut buf = vec![0u8; 4096];
    let n = sock.recv(&mut buf).await?;
    buf.truncate(n);
    Ok(buf)
}

async fn forward_tcp(up: SocketAddr, pkt: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut conn = TcpStream::connect(up).await?;
    let mut out = (pkt.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(pkt);
    conn.write_all(&out).await?;
    let mut len = [0u8; 2];
    conn.read_exact(&mut len).await?;
    let mut resp = vec![0u8; u16::from_be_bytes(len) as usize];
    conn.read_exact(&mut resp).await?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut p = id.to_be_bytes().to_vec();
        p.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, one question
        for label in name.split('.') {
            p.push(label.len() as u8);
            p.extend_from_slice(label.as_bytes());
        }
        p.push(0);
        p.extend_from_slice(&qtype.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p
    }

    /// (rcode, authoritative, addresses in the answer section)
    fn decode(resp: &[u8], qlen: usize) -> (u16, bool, Vec<IpAddr>) {
        let flags = u16::from_be_bytes([resp[2], resp[3]]);
        let an = u16::from_be_bytes([resp[6], resp[7]]);
        let mut i = qlen;
        let mut out = Vec::new();
        for _ in 0..an {
            assert_eq!(&resp[i..i + 2], &[0xC0, 0x0C]);
            let rtype = u16::from_be_bytes([resp[i + 2], resp[i + 3]]);
            let rdlen = u16::from_be_bytes([resp[i + 10], resp[i + 11]]) as usize;
            let rdata = &resp[i + 12..i + 12 + rdlen];
            out.push(match rtype {
                TYPE_A => IpAddr::from(<[u8; 4]>::try_from(rdata).unwrap()),
                TYPE_AAAA => IpAddr::from(<[u8; 16]>::try_from(rdata).unwrap()),
                t => panic!("type {t}"),
            });
            i += 12 + rdlen;
        }
        assert_eq!(i, resp.len());
        (flags & 0x000F, flags & 0x0400 != 0, out)
    }

    fn set(services: &[(&str, &[&str])]) -> pb::Endpoints {
        pb::Endpoints {
            version: 1,
            services: services
                .iter()
                .map(|(id, a)| pb::ServiceEndpoints {
                    service_id: id.to_string(),
                    addresses: a.iter().map(|s| s.to_string()).collect(),
                })
                .collect(),
        }
    }

    fn server(dir: &Path, upstream: Vec<SocketAddr>) -> Arc<DnsServer> {
        let e = Endpoints::load(dir);
        e.update(set(&[
            ("global-cp", &["10.0.0.1", "10.0.0.2", "fd00::1"]),
            ("idle", &[]),
        ]));
        DnsServer::new(e, "svc.valkvork.internal.", upstream, 5)
    }

    #[tokio::test]
    async fn answers_services_from_the_endpoints() {
        let d = tempfile::tempdir().unwrap();
        let s = server(d.path(), vec![]);
        let q = query(7, "Global-CP.svc.valkvork.internal", TYPE_A);
        let r = s.handle(&q, false).await.unwrap();
        assert_eq!(&r[..2], &7u16.to_be_bytes());
        let (rcode, aa, mut addrs) = decode(&r, q.len());
        addrs.sort();
        assert_eq!((rcode, aa), (0, true));
        assert_eq!(
            addrs,
            [
                "10.0.0.1".parse::<IpAddr>().unwrap(),
                "10.0.0.2".parse().unwrap()
            ]
        );
        // TTL
        assert_eq!(&r[q.len() + 6..q.len() + 10], &5u32.to_be_bytes());

        let q = query(8, "global-cp.svc.valkvork.internal", TYPE_AAAA);
        let (_, _, addrs) = decode(&s.handle(&q, false).await.unwrap(), q.len());
        assert_eq!(addrs, ["fd00::1".parse::<IpAddr>().unwrap()]);

        // Rotation: the first address changes between queries.
        let q = query(9, "global-cp.svc.valkvork.internal", TYPE_A);
        let firsts: std::collections::HashSet<IpAddr> =
            futures::future::join_all((0..4).map(|_| s.handle(&q, false)))
                .await
                .into_iter()
                .map(|r| decode(&r.unwrap(), q.len()).2[0])
                .collect();
        assert_eq!(firsts.len(), 2);

        // A service with no addresses exists but has no records; an unknown
        // one, or a deeper name, does not exist.
        for (name, want) in [
            ("idle.svc.valkvork.internal", 0),
            ("nope.svc.valkvork.internal", RCODE_NXDOMAIN),
            ("a.global-cp.svc.valkvork.internal", RCODE_NXDOMAIN),
            ("svc.valkvork.internal", 0),
        ] {
            let q = query(1, name, TYPE_A);
            let (rcode, aa, addrs) = decode(&s.handle(&q, false).await.unwrap(), q.len());
            assert_eq!((rcode, aa, addrs.len()), (want, true, 0), "{name}");
        }
    }

    #[tokio::test]
    async fn udp_answers_fit_in_512_bytes() {
        let d = tempfile::tempdir().unwrap();
        let e = Endpoints::load(d.path());
        let many: Vec<String> = (0..100).map(|i| format!("10.0.1.{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        e.update(set(&[("big", &refs)]));
        let s = DnsServer::new(e, "svc.valkvork.internal", vec![], 5);
        let q = query(1, "big.svc.valkvork.internal", TYPE_A);
        let udp = s.handle(&q, false).await.unwrap();
        assert!(udp.len() <= 512, "{}", udp.len());
        assert!(decode(&udp, q.len()).2.len() >= 20);
        let tcp = s.handle(&q, true).await.unwrap();
        assert_eq!(decode(&tcp, q.len()).2.len(), 100);
    }

    #[tokio::test]
    async fn the_set_survives_a_restart() {
        let d = tempfile::tempdir().unwrap();
        Endpoints::load(d.path()).update(set(&[("pg", &["10.0.0.9"])]));
        let again = Endpoints::load(d.path());
        assert_eq!(again.lookup("pg"), Some(vec!["10.0.0.9".parse().unwrap()]));
        assert_eq!(again.lookup("other"), None);
    }

    #[tokio::test]
    async fn other_names_are_forwarded_over_udp_and_tcp() {
        // A fake upstream that answers every query with one fixed address.
        let up_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = up_udp.local_addr().unwrap().port();
        let up_tcp = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let fake = |q: &[u8]| {
            let mut r = q.to_vec();
            r[2] |= 0x80;
            r[7] = 1;
            r.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216, 34]);
            r
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = up_udp.recv_from(&mut buf).await {
                let _ = up_udp.send_to(&fake(&buf[..n]), from).await;
            }
        });
        tokio::spawn(async move {
            while let Ok((mut c, _)) = up_tcp.accept().await {
                let mut len = [0u8; 2];
                c.read_exact(&mut len).await.unwrap();
                let mut q = vec![0u8; u16::from_be_bytes(len) as usize];
                c.read_exact(&mut q).await.unwrap();
                let r = fake(&q);
                let mut out = (r.len() as u16).to_be_bytes().to_vec();
                out.extend_from_slice(&r);
                c.write_all(&out).await.unwrap();
            }
        });

        let d = tempfile::tempdir().unwrap();
        // A dead upstream first: the next one is tried.
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let s = server(
            d.path(),
            vec![dead, SocketAddr::from(([127, 0, 0, 1], port))],
        );
        let (udp_addr, tcp_addr) = s
            .bind_and_serve_on("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        let q = query(42, "example.com", TYPE_A);
        let want: IpAddr = "93.184.216.34".parse().unwrap();
        // Through the listening UDP socket.
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(&q, udp_addr).await.unwrap();
        let mut buf = [0u8; 512];
        let n = tokio::time::timeout(Duration::from_secs(5), client.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decode(&buf[..n], q.len()).2, [want]);

        // Through TCP: one of ours, then a forwarded one, on one connection.
        let mut c = TcpStream::connect(tcp_addr).await.unwrap();
        for (q, want) in [
            (query(1, "global-cp.svc.valkvork.internal", TYPE_A), 2),
            (query(2, "example.com", TYPE_A), 1),
        ] {
            let mut out = (q.len() as u16).to_be_bytes().to_vec();
            out.extend_from_slice(&q);
            c.write_all(&out).await.unwrap();
            let mut len = [0u8; 2];
            c.read_exact(&mut len).await.unwrap();
            let mut r = vec![0u8; u16::from_be_bytes(len) as usize];
            c.read_exact(&mut r).await.unwrap();
            assert_eq!(&r[..2], &q[..2]);
            assert_eq!(decode(&r, q.len()).2.len(), want);
        }
    }

    #[tokio::test]
    async fn no_upstream_is_servfail_and_garbage_is_formerr() {
        let d = tempfile::tempdir().unwrap();
        let s = server(d.path(), vec![]);
        let q = query(3, "example.com", TYPE_A);
        let (rcode, aa, _) = decode(&s.handle(&q, false).await.unwrap(), q.len());
        assert_eq!((rcode, aa), (RCODE_SERVFAIL, false));

        let mut bad = query(4, "x", TYPE_A);
        bad.truncate(14);
        let r = s.handle(&bad, false).await.unwrap();
        assert_eq!(r.len(), 12);
        assert_eq!(u16::from_be_bytes([r[2], r[3]]) & 0xF, RCODE_FORMERR);
        assert!(s.handle(&[1, 2, 3], false).await.is_none());
        // A response is never answered.
        let mut resp = query(5, "x", TYPE_A);
        resp[2] |= 0x80;
        assert!(s.handle(&resp, false).await.is_none());
    }

    #[test]
    fn nameservers_from_resolv_conf() {
        let text = "# generated\nnameserver 127.0.0.53\nnameserver 172.17.0.1\noptions edns0\nsearch ec2.internal\nnameserver fd00::2\n";
        assert_eq!(
            resolv_conf_nameservers(text, "172.17.0.1".parse().unwrap()),
            [
                "127.0.0.53:53".parse::<SocketAddr>().unwrap(),
                "[fd00::2]:53".parse().unwrap()
            ]
        );
    }
}
