//! The tailclip server: one clip in RAM, a long poll, and the bind address rules.
//!
//! The server never touches a pasteboard. It only holds bytes.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Request, Response, Server};

/// A clip larger than this returns 413. Storage is RAM only, so peak memory
/// equals this cap.
pub const MAX_BODY: usize = 32 * 1024 * 1024;

/// How long a `GET /clip?since=<v>` blocks before it returns 204.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(300);

/// The live long poll timeout, in milliseconds. Only `selftest` changes it.
static POLL_MS: AtomicU64 = AtomicU64::new(300_000);

pub fn poll_timeout() -> Duration {
    Duration::from_millis(POLL_MS.load(Ordering::Relaxed))
}

pub fn set_poll_timeout(d: Duration) {
    POLL_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

pub const DEFAULT_PORT: u16 = 8757;

pub const TEXT_MIME: &str = "text/plain; charset=utf-8";

pub struct Clip {
    pub version: u64,
    pub body: Vec<u8>,
    pub mime: String,
}

pub struct State {
    clip: Mutex<Clip>,
    changed: Condvar,
}

impl State {
    pub fn new() -> State {
        State {
            clip: Mutex::new(Clip {
                version: 0,
                body: Vec::new(),
                mime: TEXT_MIME.to_string(),
            }),
            changed: Condvar::new(),
        }
    }
}

impl Default for State {
    fn default() -> State {
        State::new()
    }
}

// ---------------------------------------------------------------- bind rules

/// True if the address is loopback, RFC1918, `100.64.0.0/10`, or IPv6 `fc00::/7`.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            a.is_loopback() || a.is_private() || a.is_link_local() || is_cgnat(a)
        }
        IpAddr::V6(a) => {
            a.is_loopback() || (a.segments()[0] & 0xfe00) == 0xfc00 || a.is_unspecified()
        }
    }
}

/// True if the address is inside `100.64.0.0/10`, the Tailscale range.
pub fn is_cgnat(a: Ipv4Addr) -> bool {
    let o = a.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000
}

/// The first IPv4 address of this host inside `100.64.0.0/10`.
pub fn tailscale_ip() -> Option<IpAddr> {
    // ponytail: read the interfaces. Do not shell out to the tailscale binary,
    // because a non-interactive ssh session does not have it on PATH.
    for iface in if_addrs::get_if_addrs().ok()? {
        if let IpAddr::V4(a) = iface.ip() {
            if is_cgnat(a) {
                return Some(IpAddr::V4(a));
            }
        }
    }
    None
}

/// Apply the bind address rules and return the address to listen on.
pub fn pick_bind(explicit: Option<&str>, allow_public: bool) -> Result<IpAddr, String> {
    match explicit {
        Some(s) => {
            let ip: IpAddr = s
                .parse()
                .map_err(|_| format!("--bind {s}: not an IP address"))?;
            if allow_public || is_private(ip) {
                Ok(ip)
            } else {
                Err(format!(
                    "refusing to bind the public address {s}.\n\
                     tailclip has no authentication. The tailnet is the authentication.\n\
                     To override this, add --allow-public-bind."
                ))
            }
        }
        None => tailscale_ip().ok_or_else(|| {
            "no address inside 100.64.0.0/10 on this host.\n\
             tailclip binds the Tailscale address. Start Tailscale, then try again.\n\
             To use a different address, add --bind <ip>."
                .to_string()
        }),
    }
}

// ------------------------------------------------------------------- serving

/// Listen on `addr` and serve until the process stops.
pub fn run(addr: SocketAddr) -> Result<(), String> {
    let state = Arc::new(State::new());
    run_with(addr, state)
}

pub fn run_with(addr: SocketAddr, state: Arc<State>) -> Result<(), String> {
    let server = Server::http(addr).map_err(|e| format!("cannot bind {addr}: {e}"))?;
    let bound = server.server_addr().to_ip().unwrap_or(addr);
    println!("tailclip serving on http://{bound}/clip");
    // The end to end tests start the server with --port 0 and read this line.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    for req in server.incoming_requests() {
        let st = state.clone();
        // ponytail: one thread per request. A long poll holds a thread for
        // 300 s, so a fixed pool would starve. Add a pool above ~100 devices.
        thread::spawn(move || handle(req, &st));
    }
    Ok(())
}

fn version_header(v: u64) -> Header {
    Header::from_bytes(&b"X-Clip-Version"[..], v.to_string().as_bytes()).unwrap()
}

fn type_header(mime: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], mime.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..]).unwrap())
}

fn since_of(url: &str) -> Option<u64> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("since=") {
            return v.parse().ok();
        }
    }
    None
}

fn handle(req: Request, st: &State) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("");
    if path != "/clip" {
        let _ = req.respond(Response::empty(404));
        return;
    }
    match *req.method() {
        Method::Get => get_clip(req, st, since_of(&url)),
        Method::Post => post_clip(req, st),
        _ => {
            let _ = req.respond(Response::empty(405));
        }
    }
}

fn get_clip(req: Request, st: &State, since: Option<u64>) {
    let mut clip = st.clip.lock().unwrap();
    if let Some(s) = since {
        let deadline = Instant::now() + poll_timeout();
        while clip.version <= s {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let v = clip.version;
                drop(clip);
                let _ = req.respond(Response::empty(204).with_header(version_header(v)));
                return;
            }
            clip = st.changed.wait_timeout(clip, left).unwrap().0;
        }
    }
    let resp = Response::from_data(clip.body.clone())
        .with_header(version_header(clip.version))
        .with_header(type_header(&clip.mime));
    drop(clip);
    let _ = req.respond(resp);
}

fn post_clip(mut req: Request, st: &State) {
    let mime = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_else(|| TEXT_MIME.to_string());

    // ponytail: read twice the cap, then reject. This keeps the 413 reply on
    // the same connection, so the client reads a status and not a broken pipe.
    let mut body = Vec::new();
    if req
        .as_reader()
        .take(MAX_BODY as u64 * 2)
        .read_to_end(&mut body)
        .is_err()
    {
        let _ = req.respond(Response::empty(400));
        return;
    }
    if body.len() > MAX_BODY {
        let _ = req.respond(Response::empty(413));
        return;
    }

    let mut clip = st.clip.lock().unwrap();
    if clip.body != body || clip.mime != mime {
        clip.version += 1;
        clip.body = body;
        clip.mime = mime;
        st.changed.notify_all();
    }
    let v = clip.version;
    drop(clip);
    let _ = req.respond(Response::empty(200).with_header(version_header(v)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat_range_edges() {
        let cg = |s: &str| is_cgnat(s.parse().unwrap());
        assert!(!cg("100.63.255.255"), "just below the range");
        assert!(cg("100.64.0.0"), "the first address of the range");
        assert!(cg("100.110.80.23"), "a real Tailscale address");
        assert!(cg("100.127.255.255"), "the last address of the range");
        assert!(!cg("100.128.0.0"), "just above the range");
        assert!(!cg("99.64.0.1"), "the wrong first octet");
        assert!(!cg("101.64.0.1"), "the wrong first octet");
    }

    #[test]
    fn private_accepts_the_safe_ranges() {
        for a in ["127.0.0.1", "10.0.0.5", "172.16.9.9", "192.168.1.10", "100.110.80.23", "::1", "fd00::1"] {
            assert!(is_private(a.parse().unwrap()), "{a} must count as private");
        }
    }

    #[test]
    fn private_rejects_public_addresses() {
        for a in ["8.8.8.8", "1.1.1.1", "203.0.113.7", "2606:4700::1111"] {
            assert!(!is_private(a.parse().unwrap()), "{a} must count as public");
        }
    }

    #[test]
    fn pick_bind_follows_the_rules() {
        assert!(pick_bind(Some("100.110.80.23"), false).is_ok());
        assert!(pick_bind(Some("127.0.0.1"), false).is_ok());
        assert!(pick_bind(Some("8.8.8.8"), false).is_err());
        assert!(pick_bind(Some("8.8.8.8"), true).is_ok());
        assert!(pick_bind(Some("mini.local"), false).is_err(), "a name is not an address");
    }

    #[test]
    fn pick_bind_names_tailscale_in_the_error() {
        let msg = pick_bind(Some("8.8.8.8"), false).unwrap_err();
        assert!(msg.contains("--allow-public-bind"), "the error must name the override");
    }

    #[test]
    fn since_parses_from_the_query() {
        assert_eq!(since_of("/clip"), None);
        assert_eq!(since_of("/clip?since=0"), Some(0));
        assert_eq!(since_of("/clip?since=42"), Some(42));
        assert_eq!(since_of("/clip?a=1&since=7"), Some(7));
        assert_eq!(since_of("/clip?since=nine"), None);
    }

    #[test]
    fn a_new_state_starts_empty_at_version_zero() {
        let c = State::new().clip.into_inner().unwrap();
        assert_eq!(c.version, 0);
        assert!(c.body.is_empty());
        assert_eq!(c.mime, TEXT_MIME);
    }

    #[test]
    fn the_poll_timeout_is_five_minutes_by_default() {
        assert_eq!(POLL_TIMEOUT, Duration::from_secs(300));
    }

    #[test]
    fn the_cap_is_thirty_two_megabytes() {
        assert_eq!(MAX_BODY, 32 * 1024 * 1024);
    }
}
