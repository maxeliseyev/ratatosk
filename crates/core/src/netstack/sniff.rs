//! TLS ClientHello sniffing for flows that arrive at a bare address.
//!
//! Domain-based routing needs a name. [`FakeDns`](super::fakedns) supplies one
//! for every name the client asked the tunnel's resolver, but a client with a
//! resolver of its own (DoH, a hardcoded address in an app) connects to real
//! addresses, and then a `.ru` host would leave through the exit because there
//! is nothing left to match a domain rule against.
//!
//! The ClientHello carries the name in the clear (except with ECH), so the
//! netstack peeks at the first record before dialing. The peeked bytes are
//! returned to the caller, which replays them to the target: sniffing must
//! never cost a byte.

use crate::protocol::fingerprint::ParsedHello;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// TLS record: content type (22 = handshake), version (2), length (2).
const RECORD_HEADER: usize = 5;
/// Content type 22: handshake.
const RECORD_HANDSHAKE: u8 = 0x16;
/// Handshake type 1: ClientHello.
const HANDSHAKE_CLIENT_HELLO: u8 = 1;
/// `server_name` extension (RFC 6066).
const EXT_SERVER_NAME: u16 = 0;
/// A ClientHello record is a few hundred bytes; anything bigger is not one we
/// want to sit on.
const MAX_RECORD: usize = 8 * 1024;

/// Peek the first TLS record of `reader` looking for a server name.
///
/// Returns the name (if the record is a ClientHello that carries one) together
/// with **every byte read**, so the caller can hand them on. A timeout or a
/// non-TLS first record yields `None` for the name and still returns the bytes.
pub async fn client_hello_sni<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: Duration,
) -> (Option<String>, Vec<u8>) {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(done) = parse_record(&buf) {
            return (done, buf);
        }
        if buf.len() >= MAX_RECORD {
            return (None, buf);
        }
        let mut chunk = [0u8; 1024];
        match tokio::time::timeout_at(deadline, reader.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return (None, buf),
            Ok(Ok(n)) => {
                let room = MAX_RECORD - buf.len();
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
            Ok(Err(_)) => return (None, buf),
        }
    }
}

/// `Some(Some(name))` once `buf` holds a complete first record, `Some(None)`
/// when that record is not a ClientHello we can use, `None` while incomplete.
fn parse_record(buf: &[u8]) -> Option<Option<String>> {
    let header = buf.get(..RECORD_HEADER)?;
    if header[0] != RECORD_HANDSHAKE {
        return Some(None);
    }
    let len = u16::from_be_bytes([header[3], header[4]]) as usize;
    if len == 0 || len + RECORD_HEADER > MAX_RECORD {
        return Some(None);
    }
    let body = buf.get(RECORD_HEADER..RECORD_HEADER + len)?;
    Some(parse_hello(body))
}

fn parse_hello(handshake: &[u8]) -> Option<String> {
    if handshake.first() != Some(&HANDSHAKE_CLIENT_HELLO) {
        return None;
    }
    let hello = ParsedHello::parse(handshake)?;
    server_name(hello.extension(EXT_SERVER_NAME)?)
}

/// `server_name` extension: list length, then (type 0, length, name).
fn server_name(ext: &[u8]) -> Option<String> {
    let list_len = u16::from_be_bytes(ext.get(..2)?.try_into().ok()?) as usize;
    let list = ext.get(2..2 + list_len)?;
    if *list.first()? != 0 {
        return None; // not a host_name entry
    }
    let len = u16::from_be_bytes(list.get(1..3)?.try_into().ok()?) as usize;
    let name = std::str::from_utf8(list.get(3..3 + len)?).ok()?;
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// A stream that replays bytes already read from it, then reads `inner`.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Some(rest) = this.prefix.get(this.pos..).filter(|r| !r.is_empty()) {
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            this.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Prefixed<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A ClientHello with `sni`, wrapped in a TLS record as a client sends it.
    fn hello_record(sni: Option<&str>) -> Vec<u8> {
        let exts = match sni {
            Some(name) => {
                let mut list = vec![0u8]; // host_name
                list.extend_from_slice(&(name.len() as u16).to_be_bytes());
                list.extend_from_slice(name.as_bytes());
                let mut data = (list.len() as u16).to_be_bytes().to_vec();
                data.extend_from_slice(&list);
                vec![(EXT_SERVER_NAME, data)]
            }
            None => Vec::new(),
        };
        let hello = ParsedHello {
            version: 0x0303,
            random: [7u8; 32],
            session_id: vec![1, 2, 3],
            cipher_suites: vec![0x1301, 0x1302],
            compression: vec![0],
            extensions: exts,
        }
        .encode();
        let mut record = vec![RECORD_HANDSHAKE, 0x03, 0x01];
        record.extend_from_slice(&(hello.len() as u16).to_be_bytes());
        record.extend_from_slice(&hello);
        record
    }

    async fn sniff(bytes: &[u8]) -> (Option<String>, Vec<u8>) {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let data = bytes.to_vec();
        tokio::spawn(async move {
            let _ = b.write_all(&data).await;
            // Keep the write side open: the sniffer must stop on its own.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        client_hello_sni(&mut a, Duration::from_millis(200)).await
    }

    #[tokio::test]
    async fn a_client_hello_yields_its_server_name() {
        let record = hello_record(Some("www.ozon.ru"));
        let (sni, seen) = sniff(&record).await;
        assert_eq!(sni.as_deref(), Some("www.ozon.ru"));
        assert_eq!(seen, record, "every peeked byte is handed back");
    }

    #[tokio::test]
    async fn a_hello_without_sni_yields_nothing() {
        let record = hello_record(None);
        let (sni, seen) = sniff(&record).await;
        assert_eq!(sni, None);
        assert_eq!(seen, record);
    }

    #[tokio::test]
    async fn plain_http_is_passed_through_unread() {
        let request = b"GET / HTTP/1.1\r\nHost: ozon.ru\r\n\r\n";
        let (sni, seen) = sniff(request).await;
        assert_eq!(sni, None);
        assert_eq!(seen, request);
    }

    #[tokio::test]
    async fn a_truncated_hello_keeps_what_it_read() {
        let record = hello_record(Some("ozon.ru"));
        let (sni, seen) = sniff(&record[..20]).await;
        assert_eq!(sni, None);
        assert_eq!(seen, &record[..20]);
    }

    #[tokio::test]
    async fn a_record_split_across_writes_is_still_parsed() {
        let record = hello_record(Some("ozon.ru"));
        let (mut a, mut b) = tokio::io::duplex(4096);
        let (head, tail) = record.split_at(9);
        let (h, t) = (head.to_vec(), tail.to_vec());
        tokio::spawn(async move {
            b.write_all(&h).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            b.write_all(&t).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let (sni, seen) = client_hello_sni(&mut a, Duration::from_millis(500)).await;
        assert_eq!(sni.as_deref(), Some("ozon.ru"));
        assert_eq!(seen, record);
    }

    #[tokio::test]
    async fn a_prefixed_stream_replays_then_reads() {
        let (a, mut b) = tokio::io::duplex(64);
        b.write_all(b"world").await.unwrap();
        let mut stream = Prefixed::new(b"hello,".to_vec(), a);
        let mut out = [0u8; 11];
        stream.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"hello,world");
    }
}
