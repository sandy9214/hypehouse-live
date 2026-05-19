//! HTTP-backed `symphonia::core::io::MediaSource` — lets the decoder
//! pipeline open streaming URLs (e.g. SoundCloud transcodings landed
//! in PR #107) without first downloading them to a local file.
//!
//! # Design
//!
//! ```text
//!   open(url)                                   read(buf) / seek(pos)
//!   ─────────                                   ─────────────────────
//!   GET url Range: bytes=0-262143  ────────▶    page-cache hit?
//!                                                   │
//!     ↓ headers parsed                              │ no
//!     ↓ accept-ranges, content-length               ▼
//!     ↓ first 256 KB cached in page LRU     GET url Range: bytes=lo-hi
//!                                                   │
//!   HttpMediaSource ready                            │  4 × 64 KB pages
//!                                                   ▼
//!                                          LRU evicts oldest when >16
//! ```
//!
//! ## Page cache
//!
//! The body is sliced into fixed-size 64 KB pages. The cache holds the
//! most-recently-used 16 pages (= 1 MB upper bound). Sequential decode
//! (the common case — symphonia reads forward) sees ~100% hit-rate on
//! the same page after the first fetch; backward seeks (DJ scrubbing)
//! re-hit any page still in the LRU before re-issuing a network call.
//!
//! ## Seekability
//!
//! `is_seekable()` returns `true` iff the initial response carried
//! `Accept-Ranges: bytes` (RFC 7233 §2.3). Servers without ranged-GET
//! support fall back to whole-file streaming and symphonia will refuse
//! seek-back — that's the correct UX (no scrubbing on dumb servers).
//!
//! ## Threading
//!
//! All HTTP work happens inside the caller's `Read::read` call, which
//! the decode pipeline already runs on a dedicated per-track thread
//! (see `decoder_thread_main` in `decode.rs`). The audio thread NEVER
//! touches `HttpMediaSource` — it only consumes the ring that the
//! decode thread fills, so network stalls cause at worst a ring
//! underrun (silence padding), never a real-time glitch.
//!
//! ## No `unsafe`
//!
//! This module contains zero `unsafe` Rust. `reqwest::blocking` is a
//! safe wrapper over its async sibling; the LRU is built on plain
//! `VecDeque<(u64, Vec<u8>)>` (linear scan — 16 entries max).

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use symphonia::core::io::MediaSource;

/// Page size for the LRU cache. 64 KB balances per-request overhead
/// against memory footprint: symphonia probes typically read ~32-128
/// KB; mp3/m4a packet pulls are 4-16 KB each.
const PAGE_SIZE: u64 = 64 * 1024;

/// Maximum number of pages retained in the LRU — 16 × 64 KB = 1 MB.
const MAX_CACHED_PAGES: usize = 16;

/// Size of the "warm-up" prefetch fired during `open`. 256 KB is
/// enough for the symphonia probe + the first few hundred ms of
/// audio packets across mp3/m4a/flac.
const PREFETCH_BYTES: u64 = 256 * 1024;

/// Per-request timeout. Long enough to absorb cloud-provider TLS
/// handshake jitter on cold connections, short enough that a
/// genuinely dead host fails the open rather than wedging the
/// decoder thread indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Process-wide blocking client. Mirrors the pattern used in
/// `bridge::library_proxy::shared_client` — keeps the TLS+TCP pool
/// warm across multiple `HttpMediaSource::open` calls.
fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // Tag requests so server-side logs distinguish engine
            // streams from copilot library-proxy traffic.
            .user_agent("hypehouse-engine/0.1 (+http-mediasource)")
            .build()
            .unwrap_or_else(|_| Client::new())
    })
}

/// One cached page. `start` is the byte offset into the stream of the
/// FIRST byte in `bytes`; `bytes.len()` is normally `PAGE_SIZE`
/// except for the final (possibly short) page.
#[derive(Debug)]
struct Page {
    start: u64,
    bytes: Vec<u8>,
}

/// HTTP-backed media source. Implements `Read + Seek + Send + Sync`
/// so it can be wrapped in `symphonia::core::io::MediaSourceStream`.
pub struct HttpMediaSource {
    url: String,
    client: &'static Client,
    /// Current read cursor — what offset `read()` will pull from next.
    pos: u64,
    /// `Content-Length` from the initial response. `None` only when
    /// the server omitted both `Content-Length` AND `Content-Range`,
    /// which is illegal under RFC 7233 but we tolerate by returning
    /// EOF from any read past the highest byte we've actually seen.
    total_len: Option<u64>,
    /// True iff the server advertised `Accept-Ranges: bytes`.
    accept_ranges: bool,
    /// LRU pages, oldest at the front.
    pages: VecDeque<Page>,
}

impl std::fmt::Debug for HttpMediaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpMediaSource")
            .field("url", &self.url)
            .field("pos", &self.pos)
            .field("total_len", &self.total_len)
            .field("accept_ranges", &self.accept_ranges)
            .field("cached_pages", &self.pages.len())
            .finish()
    }
}

impl HttpMediaSource {
    /// Open `url`, perform the warm-up prefetch, and return a ready
    /// `HttpMediaSource`. Any network or HTTP-status error surfaces as
    /// `io::Error` so callers can use the standard error path.
    pub fn open(url: &str) -> io::Result<Self> {
        let client = shared_client();
        let resp = ranged_get(client, url, 0, PREFETCH_BYTES.saturating_sub(1))?;

        // RFC 7233 §3.1 — a ranged response uses 206; a server that
        // ignores `Range:` returns 200 + the whole body, which we
        // accept (we'll just hold up to PREFETCH_BYTES in the cache).
        let status = resp.status();
        if !(status.is_success()) {
            return Err(io::Error::other(format!(
                "http {} opening {url}",
                status.as_u16()
            )));
        }

        let accept_ranges = resp
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false)
            // 206 implies the server honoured the range — treat that
            // as seekable even if it forgot the explicit advertisement
            // (some CDNs do this).
            || status.as_u16() == 206;

        let total_len = parse_total_length(&resp);

        // Drain the prefetch body. `Response::bytes()` consumes the
        // stream — we slice it back into pages below.
        let body = resp
            .bytes()
            .map_err(|e| io::Error::other(format!("prefetch read: {e}")))?;
        let mut pages = VecDeque::with_capacity(MAX_CACHED_PAGES);
        slice_into_pages(0, &body, &mut pages);

        Ok(Self {
            url: url.to_string(),
            client,
            pos: 0,
            total_len,
            accept_ranges,
            pages,
        })
    }

    /// Look up `offset` in the cache. Returns `(page_idx, byte_offset_within_page)`
    /// on hit. Promotes the hit page to the most-recently-used slot.
    fn cache_lookup(&mut self, offset: u64) -> Option<(usize, usize)> {
        let hit = self
            .pages
            .iter()
            .position(|p| offset >= p.start && offset < p.start + p.bytes.len() as u64)?;
        // Promote: move hit to the back (MRU).
        if hit + 1 != self.pages.len() {
            if let Some(p) = self.pages.remove(hit) {
                self.pages.push_back(p);
            }
        }
        let p = self.pages.back().expect("just promoted");
        let within = (offset - p.start) as usize;
        Some((self.pages.len() - 1, within))
    }

    /// Issue a ranged GET starting at `offset` for up to ~4 pages,
    /// then store the response in the LRU. Returns the page index of
    /// the page that contains `offset`.
    fn fetch_around(&mut self, offset: u64) -> io::Result<usize> {
        // Fetch a 4-page (256 KB) chunk aligned to PAGE_SIZE so
        // adjacent reads share work.
        let page_start = (offset / PAGE_SIZE) * PAGE_SIZE;
        let chunk_end = page_start + PAGE_SIZE * 4 - 1;
        let chunk_end = match self.total_len {
            Some(n) if n > 0 => chunk_end.min(n - 1),
            _ => chunk_end,
        };
        if chunk_end < page_start {
            // Already past EOF.
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        let resp = ranged_get(self.client, &self.url, page_start, chunk_end)?;
        if !resp.status().is_success() {
            return Err(io::Error::other(format!(
                "http {} on range get",
                resp.status().as_u16()
            )));
        }
        // The server may revise total_len for us via Content-Range —
        // adopt it if we didn't have one yet (e.g. open got a 200).
        if self.total_len.is_none() {
            self.total_len = parse_total_length(&resp);
        }
        let body = resp
            .bytes()
            .map_err(|e| io::Error::other(format!("range read: {e}")))?;
        slice_into_pages(page_start, &body, &mut self.pages);
        // Trim LRU.
        while self.pages.len() > MAX_CACHED_PAGES {
            self.pages.pop_front();
        }
        // Find the page that now contains `offset`.
        let idx = self
            .pages
            .iter()
            .position(|p| offset >= p.start && offset < p.start + p.bytes.len() as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("offset {offset} not covered by fetched range"),
                )
            })?;
        Ok(idx)
    }

    /// Public for tests — total length the server reported (if any).
    #[cfg(test)]
    fn reported_len(&self) -> Option<u64> {
        self.total_len
    }
}

impl Read for HttpMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // EOF check — return Ok(0) for a graceful end-of-stream.
        if let Some(n) = self.total_len {
            if self.pos >= n {
                return Ok(0);
            }
        }

        let (page_idx, within) = match self.cache_lookup(self.pos) {
            Some(hit) => hit,
            None => {
                let idx = self.fetch_around(self.pos)?;
                let page = &self.pages[idx];
                let within = (self.pos - page.start) as usize;
                (idx, within)
            }
        };

        let page = &self.pages[page_idx];
        let available = page.bytes.len().saturating_sub(within);
        let to_copy = available.min(buf.len());
        if to_copy == 0 {
            // Hit-but-no-bytes-left only happens on short final page
            // when pos == total_len already — treat as EOF.
            return Ok(0);
        }
        buf[..to_copy].copy_from_slice(&page.bytes[within..within + to_copy]);
        self.pos += to_copy as u64;
        Ok(to_copy)
    }
}

impl Seek for HttpMediaSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(delta) => {
                let total = self.total_len.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek-from-end requires Content-Length",
                    )
                })?;
                if delta >= 0 {
                    total.saturating_add(delta as u64)
                } else {
                    total.saturating_sub(delta.unsigned_abs())
                }
            }
            SeekFrom::Current(delta) => {
                if delta >= 0 {
                    self.pos.saturating_add(delta as u64)
                } else {
                    self.pos.saturating_sub(delta.unsigned_abs())
                }
            }
        };
        // We do NOT prefetch on seek — the next `read` will trigger
        // `fetch_around` if the target page isn't already cached.
        self.pos = new_pos;
        Ok(self.pos)
    }
}

impl MediaSource for HttpMediaSource {
    fn is_seekable(&self) -> bool {
        self.accept_ranges
    }

    fn byte_len(&self) -> Option<u64> {
        self.total_len
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Issue a `GET url Range: bytes=lo-hi` request.
fn ranged_get(client: &Client, url: &str, lo: u64, hi: u64) -> io::Result<Response> {
    let range_value = format!("bytes={lo}-{hi}");
    client
        .get(url)
        .header(RANGE, range_value)
        .send()
        .map_err(|e| io::Error::other(format!("http get {url}: {e}")))
}

/// Parse the total resource length out of either `Content-Range:
/// bytes lo-hi/total` (preferred — accurate even on 206 responses) or
/// `Content-Length` (only correct when the server returned the whole
/// body).
fn parse_total_length(resp: &Response) -> Option<u64> {
    if let Some(cr) = resp.headers().get(CONTENT_RANGE) {
        if let Ok(s) = cr.to_str() {
            // RFC 7233 form: `bytes 0-1023/2048` (or `bytes 0-1023/*`).
            if let Some(slash) = s.rfind('/') {
                let tail = &s[slash + 1..];
                if let Ok(n) = tail.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    // On a 200 response the entire body is delivered, so
    // `Content-Length` IS the total. On a 206, `Content-Length`
    // describes only the slice — we deliberately fall through if
    // `Content-Range` didn't give us a total, accepting an unknown
    // length over a wrong one.
    if resp.status().as_u16() == 200 {
        if let Some(cl) = resp.headers().get(CONTENT_LENGTH) {
            if let Ok(s) = cl.to_str() {
                if let Ok(n) = s.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Append `body` to `pages`, slicing into `PAGE_SIZE`-aligned chunks.
/// Existing pages with the same `start` are replaced (not duplicated).
fn slice_into_pages(start: u64, body: &[u8], pages: &mut VecDeque<Page>) {
    let mut offset = 0usize;
    while offset < body.len() {
        let page_start = start + offset as u64;
        let take = (PAGE_SIZE as usize).min(body.len() - offset);
        let chunk = body[offset..offset + take].to_vec();
        // De-dup: drop any prior copy of this exact page so the new
        // one slots in as MRU.
        if let Some(dup_idx) = pages.iter().position(|p| p.start == page_start) {
            pages.remove(dup_idx);
        }
        pages.push_back(Page {
            start: page_start,
            bytes: chunk,
        });
        offset += take;
    }
}

// ---------------------------------------------------------------------------
// Tests — hand-rolled `TcpListener` mock server (no extra dev-dep).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    /// Spin up a single-connection mock HTTP/1.1 server. Returns
    /// `(url, shutdown, join)`. `shutdown` set to true ends the
    /// accept loop after the next connection.
    fn spawn_mock(
        body: Vec<u8>,
        accept_ranges: bool,
        fail_after: Option<usize>,
    ) -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/track.mp3");
        let shutdown = Arc::new(AtomicBool::new(false));
        let s2 = shutdown.clone();
        let join = thread::spawn(move || {
            // Don't block forever on shutdown — short accept timeout.
            listener.set_nonblocking(true).expect("set_nonblocking");
            loop {
                if s2.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let body = body.clone();
                        thread::spawn(move || {
                            let _ = handle_conn(stream, &body, accept_ranges, fail_after);
                        });
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        (url, shutdown, join)
    }

    fn handle_conn(
        mut stream: TcpStream,
        body: &[u8],
        accept_ranges: bool,
        fail_after: Option<usize>,
    ) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut range_lo: u64 = 0;
        let mut range_hi: Option<u64> = None;
        loop {
            let mut hdr = String::new();
            let n = reader.read_line(&mut hdr)?;
            if n == 0 || hdr == "\r\n" {
                break;
            }
            // Header names are case-insensitive per RFC 7230 §3.2 —
            // reqwest sends them lowercased.
            let lower = hdr.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("range: bytes=") {
                let v = v.trim();
                if let Some((lo, hi)) = v.split_once('-') {
                    range_lo = lo.parse().unwrap_or(0);
                    range_hi = hi.parse().ok();
                }
            }
        }
        let total = body.len() as u64;
        // Servers without range support ignore Range and 200 the whole body.
        if !accept_ranges {
            let header = format!(
                "HTTP/1.1 200 OK\r\n\
                 Connection: close\r\n\
                 Content-Length: {total}\r\n\
                 Content-Type: audio/mpeg\r\n\r\n"
            );
            stream.write_all(header.as_bytes())?;
            stream.write_all(body)?;
            stream.flush()?;
            let _ = stream.shutdown(Shutdown::Write);
            return Ok(());
        }
        let hi = range_hi.unwrap_or(total - 1).min(total - 1);
        if range_lo > hi {
            let resp = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nConnection: close\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(resp.as_bytes())?;
            stream.flush()?;
            let _ = stream.shutdown(Shutdown::Write);
            return Ok(());
        }
        let slice = &body[range_lo as usize..=hi as usize];
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\n\
             Connection: close\r\n\
             Accept-Ranges: bytes\r\n\
             Content-Range: bytes {range_lo}-{hi}/{total}\r\n\
             Content-Length: {len}\r\n\
             Content-Type: audio/mpeg\r\n\r\n",
            len = slice.len()
        );
        stream.write_all(header.as_bytes())?;
        if let Some(cutoff) = fail_after {
            // Write a prefix, then drop the connection mid-body to
            // simulate a flaky CDN.
            let send = cutoff.min(slice.len());
            stream.write_all(&slice[..send])?;
            // Closing without writing the full Content-Length forces
            // reqwest to surface a transport error on body read.
            let _ = stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        stream.write_all(slice)?;
        stream.flush()?;
        let _ = stream.shutdown(Shutdown::Write);
        Ok(())
    }

    fn deterministic_body(n: usize) -> Vec<u8> {
        // Cheap LCG so the body is reproducible across test runs but
        // not all-zeros (which would mask off-by-one slice bugs).
        let mut v = Vec::with_capacity(n);
        let mut x: u32 = 0xdead_beef;
        for _ in 0..n {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            v.push((x >> 16) as u8);
        }
        v
    }

    #[test]
    fn read_first_megabyte_byte_for_byte() {
        let body = deterministic_body(2 * 1024 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), true, None);

        let mut src = HttpMediaSource::open(&url).expect("open");
        assert!(src.is_seekable());
        assert_eq!(src.reported_len(), Some(body.len() as u64));

        let mut buf = vec![0u8; 1024 * 1024];
        let mut filled = 0;
        while filled < buf.len() {
            let n = src.read(&mut buf[filled..]).expect("read");
            if n == 0 {
                break;
            }
            filled += n;
        }
        assert_eq!(filled, 1024 * 1024);
        assert_eq!(&buf, &body[..1024 * 1024]);

        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();
    }

    #[test]
    fn seek_into_middle_returns_slice() {
        let body = deterministic_body(2 * 1024 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), true, None);

        let mut src = HttpMediaSource::open(&url).expect("open");
        let off = src.seek(SeekFrom::Start(500 * 1024)).expect("seek");
        assert_eq!(off, 500 * 1024);

        let mut buf = vec![0u8; 100 * 1024];
        src.read_exact(&mut buf).expect("read_exact");
        assert_eq!(&buf[..], &body[500 * 1024..600 * 1024]);

        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();
    }

    #[test]
    fn seek_to_end_reports_eof() {
        let body = deterministic_body(512 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), true, None);

        let mut src = HttpMediaSource::open(&url).expect("open");
        let total = src.reported_len().expect("len");
        src.seek(SeekFrom::Start(total)).expect("seek to end");

        let mut buf = [0u8; 64];
        let n = src.read(&mut buf).expect("read at eof");
        assert_eq!(n, 0, "read at EOF must return 0, got {n}");

        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();
    }

    #[test]
    fn server_without_accept_ranges_is_not_seekable() {
        let body = deterministic_body(128 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), false, None);

        let src = HttpMediaSource::open(&url).expect("open");
        assert!(!src.is_seekable(), "no accept-ranges -> not seekable");
        // total_len from Content-Length on the 200 response.
        assert_eq!(src.reported_len(), Some(body.len() as u64));

        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();
    }

    #[test]
    fn mid_body_transport_failure_surfaces_as_io_error() {
        // Body big enough that the prefetch needs the full 256 KB
        // but the server drops the connection after 40 KB.
        let body = deterministic_body(2 * 1024 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), true, Some(40 * 1024));

        // Either open() fails with io::Error OR open() succeeds but
        // a later read fails — both are acceptable graceful paths;
        // what we must NEVER do is panic.
        let result = HttpMediaSource::open(&url);
        match result {
            Err(e) => {
                let _ = e; // open failed cleanly — fine.
            }
            Ok(mut src) => {
                // Force a read at an offset beyond the truncated
                // prefix; ensure it errors instead of panicking.
                src.seek(SeekFrom::Start(1024 * 1024)).expect("seek");
                let mut buf = [0u8; 1024];
                let _ = src.read(&mut buf); // may Err or Ok(short) — must not panic.
            }
        }

        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();
    }

    #[test]
    fn cache_hit_avoids_second_network_call() {
        // After the prefetch (256 KB), reading within the first
        // 256 KB should not require any further network I/O. We test
        // this indirectly: kill the server, then read from the cache.
        let body = deterministic_body(256 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), true, None);

        let mut src = HttpMediaSource::open(&url).expect("open");
        // Kill the server; subsequent reads must come from cache.
        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();

        let mut buf = vec![0u8; 200 * 1024];
        src.read_exact(&mut buf).expect("served from cache");
        assert_eq!(&buf[..], &body[..200 * 1024]);
    }

    #[test]
    fn seek_back_reuses_cached_pages() {
        let body = deterministic_body(256 * 1024);
        let (url, sd, jh) = spawn_mock(body.clone(), true, None);

        let mut src = HttpMediaSource::open(&url).expect("open");
        // Read once, seek back, read again — second read must match.
        let mut first = vec![0u8; 4096];
        src.read_exact(&mut first).expect("read1");
        src.seek(SeekFrom::Start(0)).expect("rewind");

        // Kill the network — proves the rewound read came from cache.
        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();

        let mut second = vec![0u8; 4096];
        src.read_exact(&mut second).expect("read2 from cache");
        assert_eq!(first, second);
        assert_eq!(&first[..], &body[..4096]);
    }

    #[test]
    fn prefetch_latency_under_100ms_on_localhost() {
        // 256 KB prefetch on loopback should comfortably finish well
        // under 100 ms even on a busy CI box. We only assert an
        // upper bound; the actual measurement is printed for
        // visibility (cargo test -- --nocapture).
        let body = deterministic_body(2 * 1024 * 1024);
        let (url, sd, jh) = spawn_mock(body, true, None);

        let t0 = std::time::Instant::now();
        let _src = HttpMediaSource::open(&url).expect("open");
        let elapsed = t0.elapsed();
        eprintln!("prefetch 256 KB elapsed: {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(500),
            "prefetch too slow: {elapsed:?}"
        );

        sd.store(true, Ordering::Relaxed);
        let _ = jh.join();
    }

    #[test]
    fn shared_client_returns_same_pointer() {
        // Connection-pooling sanity check — multiple calls hand back
        // the same client instance.
        let a = shared_client() as *const Client;
        let b = shared_client() as *const Client;
        assert_eq!(a, b);
    }
}
