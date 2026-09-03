//! The tailclip client: the HTTP calls, the pasteboard, and the sync loop.
//!
//! All pasteboard code lives here. The server never touches a pasteboard.

use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::server::{DEFAULT_PORT, MAX_BODY, POLL_TIMEOUT, TEXT_MIME};

pub const PNG_MIME: &str = "image/png";

/// How often the watcher thread looks for a local pasteboard change.
const WATCH_TICK: Duration = Duration::from_millis(500);

/// How long the client waits after a failed call before it tries again.
const RETRY_WAIT: Duration = Duration::from_secs(2);

// ------------------------------------------------------------------- payload

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Payload {
    Text(String),
    Png(Vec<u8>),
}

impl Payload {
    pub fn mime(&self) -> &'static str {
        match self {
            Payload::Text(_) => TEXT_MIME,
            Payload::Png(_) => PNG_MIME,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Payload::Text(t) => t.as_bytes(),
            Payload::Png(b) => b,
        }
    }

    /// Build a payload from a body and its mime type.
    pub fn from_wire(mime: &str, body: Vec<u8>) -> Payload {
        if mime.starts_with(PNG_MIME) {
            Payload::Png(body)
        } else {
            Payload::Text(String::from_utf8_lossy(&body).into_owned())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }

    /// Used for change detection on Linux, where there is no changeCount.
    #[allow(dead_code)]
    fn hash64(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.mime().hash(&mut h);
        self.bytes().hash(&mut h);
        h.finish()
    }
}

/// True if the bytes start with the PNG signature.
pub fn looks_like_png(b: &[u8]) -> bool {
    b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
}

// ---------------------------------------------------------------------- HTTP

pub struct Resp {
    pub status: u16,
    pub version: u64,
    pub mime: String,
    pub body: Vec<u8>,
}

/// Turn `HOST` or `HOST:PORT` into the clip URL.
pub fn clip_url(host: &str) -> String {
    if host
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .is_some()
    {
        format!("http://{host}/clip")
    } else {
        format!("http://{host}:{DEFAULT_PORT}/clip")
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn read_resp(r: ureq::http::Response<ureq::Body>) -> Result<Resp, String> {
    let status = r.status().as_u16();
    let head = |k: &str| -> Option<String> {
        r.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let version = head("X-Clip-Version")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mime = head("Content-Type").unwrap_or_else(|| TEXT_MIME.to_string());
    let mut r = r;
    let body = r
        .body_mut()
        .with_config()
        .limit(MAX_BODY as u64)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok(Resp {
        status,
        version,
        mime,
        body,
    })
}

/// `GET /clip`. If `since` is present, the server blocks until the version passes it.
pub fn fetch(url: &str, since: Option<u64>, timeout: Duration) -> Result<Resp, String> {
    let full = match since {
        Some(v) => format!("{url}?since={v}"),
        None => url.to_string(),
    };
    let r = agent(timeout)
        .get(&full)
        .call()
        .map_err(|e| e.to_string())?;
    read_resp(r)
}

/// `POST /clip`.
pub fn push(url: &str, mime: &str, body: &[u8], timeout: Duration) -> Result<Resp, String> {
    let r = agent(timeout)
        .post(url)
        .header("Content-Type", mime)
        .send(body)
        .map_err(|e| e.to_string())?;
    read_resp(r)
}

// ----------------------------------------------------------------- PNG codec

/// Encode 8 bit RGBA pixels as a PNG.
pub fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width as u32, height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(rgba).map_err(|e| e.to_string())?;
        w.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Decode a PNG into width, height, and 8 bit RGBA pixels.
///
/// macOS moves PNG bytes straight through NSPasteboard, so only Linux and the
/// tests call this.
#[allow(dead_code)]
pub fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("the PNG is too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "unsupported PNG: {:?} {:?}",
            info.color_type, info.bit_depth
        ));
    }
    buf.truncate(info.buffer_size());
    Ok((info.width as usize, info.height as usize, buf))
}

// -------------------------------------------------- image on the pasteboard

#[cfg(target_os = "macos")]
fn read_image(_cb: &mut arboard::Clipboard) -> Option<Vec<u8>> {
    // NSPasteboard already holds PNG bytes, so there is no pixel conversion.
    let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
    let data = unsafe { pb.dataForType(objc2_app_kit::NSPasteboardTypePNG) }?;
    Some(data.to_vec())
}

#[cfg(target_os = "macos")]
fn write_image(_cb: &mut arboard::Clipboard, png_bytes: &[u8]) -> Result<(), String> {
    let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
    let data = objc2_foundation::NSData::with_bytes(png_bytes);
    unsafe {
        pb.clearContents();
        if pb.setData_forType(Some(&data), objc2_app_kit::NSPasteboardTypePNG) {
            Ok(())
        } else {
            Err("NSPasteboard refused the image".to_string())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn read_image(cb: &mut arboard::Clipboard) -> Option<Vec<u8>> {
    let img = cb.get_image().ok()?;
    encode_png(img.width, img.height, &img.bytes).ok()
}

#[cfg(not(target_os = "macos"))]
fn write_image(cb: &mut arboard::Clipboard, png_bytes: &[u8]) -> Result<(), String> {
    let (width, height, rgba) = decode_png(png_bytes)?;
    let img = arboard::ImageData {
        width,
        height,
        bytes: rgba.into(),
    };
    cb.set_image(img).map_err(|e| e.to_string())
}

// ----------------------------------------------------------------- clipboard

fn read_clipboard(cb: &mut arboard::Clipboard) -> Option<Payload> {
    // Text is cheap, so try text first.
    if let Ok(t) = cb.get_text() {
        if !t.is_empty() {
            return Some(Payload::Text(t));
        }
    }
    read_image(cb).map(Payload::Png)
}

fn write_clipboard(cb: &mut arboard::Clipboard, p: &Payload) -> Result<(), String> {
    match p {
        Payload::Text(t) => cb.set_text(t.clone()).map_err(|e| e.to_string()),
        Payload::Png(b) => write_image(cb, b),
    }
}

// ------------------------------------------------------- change detection

#[cfg(target_os = "macos")]
fn change_count() -> u64 {
    // NSPasteboard.changeCount is an integer that macOS increments on every
    // clipboard change. Reading it costs nothing, so a big image never gets
    // read until the count moves.
    objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u64
}

/// Tracks the last clipboard state this process knows about.
///
/// The poller calls `mark_written` the moment it applies a remote clip. The
/// watcher then does not treat that write as a local change, so the two hosts
/// never push to each other forever.
pub struct Watcher {
    last: u64,
}

impl Watcher {
    pub fn new(cb: &mut arboard::Clipboard) -> Watcher {
        let mut w = Watcher { last: 0 };
        w.prime(cb);
        w
    }

    #[cfg(target_os = "macos")]
    fn prime(&mut self, _cb: &mut arboard::Clipboard) {
        self.last = change_count();
    }

    #[cfg(not(target_os = "macos"))]
    fn prime(&mut self, cb: &mut arboard::Clipboard) {
        self.last = read_clipboard(cb).map(|p| p.hash64()).unwrap_or(0);
    }

    /// Return the local clip if it changed since the last call.
    #[cfg(target_os = "macos")]
    pub fn poll(&mut self, cb: &mut arboard::Clipboard) -> Option<Payload> {
        let c = change_count();
        if c == self.last {
            return None;
        }
        self.last = c;
        read_clipboard(cb)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn poll(&mut self, cb: &mut arboard::Clipboard) -> Option<Payload> {
        let p = read_clipboard(cb)?;
        let h = p.hash64();
        if h == self.last {
            return None;
        }
        self.last = h;
        Some(p)
    }

    /// Record a clip that this process wrote, so the watcher ignores it.
    #[cfg(target_os = "macos")]
    pub fn mark_written(&mut self, _p: &Payload) {
        self.last = change_count();
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mark_written(&mut self, p: &Payload) {
        self.last = p.hash64();
    }
}

// ---------------------------------------------------------------------- pause

pub fn pause_file() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cache/tailclip/paused")
}

pub fn is_paused() -> bool {
    pause_file().exists()
}

pub fn pause() -> Result<(), String> {
    let f = pause_file();
    if let Some(d) = f.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&f, b"").map_err(|e| e.to_string())?;
    println!("tailclip paused: {}", f.display());
    Ok(())
}

pub fn resume() -> Result<(), String> {
    let f = pause_file();
    match std::fs::remove_file(&f) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.to_string()),
    }
    println!("tailclip resumed");
    Ok(())
}

// ------------------------------------------------------------- one shot verbs

/// Print the current clip to stdout.
pub fn get(host: &str) -> Result<(), String> {
    let r = fetch(&clip_url(host), None, Duration::from_secs(10))?;
    use std::io::Write;
    std::io::stdout()
        .write_all(&r.body)
        .map_err(|e| e.to_string())
}

/// Push text from the argument, or bytes from stdin.
pub fn set(host: &str, text: Option<&str>) -> Result<(), String> {
    let (mime, body) = match text {
        Some(t) => (TEXT_MIME.to_string(), t.as_bytes().to_vec()),
        None => {
            let mut b = Vec::new();
            std::io::stdin()
                .read_to_end(&mut b)
                .map_err(|e| e.to_string())?;
            let m = if looks_like_png(&b) {
                PNG_MIME
            } else {
                TEXT_MIME
            };
            (m.to_string(), b)
        }
    };
    if body.len() > MAX_BODY {
        return Err(format!(
            "clip is {} bytes. The cap is {MAX_BODY} bytes.",
            body.len()
        ));
    }
    let r = push(&clip_url(host), &mime, &body, Duration::from_secs(30))?;
    if r.status == 413 {
        return Err("the server rejected the clip: too large".to_string());
    }
    Ok(())
}

// ------------------------------------------------------------------ sync loop

/// Run the two way sync loop until the process stops.
pub fn sync(host: &str) -> Result<(), String> {
    let url = clip_url(host);
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("no clipboard: {e}"))?;
    let watcher = Arc::new(Mutex::new(Watcher::new(&mut cb)));

    println!("tailclip sync with {url}");

    let poll_url = url.clone();
    let poll_watcher = watcher.clone();
    thread::spawn(move || poll_loop(&poll_url, poll_watcher));
    watch_loop(&url, watcher, cb);
    Ok(())
}

fn watch_loop(url: &str, watcher: Arc<Mutex<Watcher>>, mut cb: arboard::Clipboard) {
    loop {
        thread::sleep(WATCH_TICK);
        // Read the pasteboard even when paused. The read moves the change
        // token, so a clip copied during a pause never gets pushed later.
        let local = {
            let mut w = watcher.lock().unwrap();
            w.poll(&mut cb)
        };
        let Some(p) = local else { continue };
        if is_paused() || p.is_empty() || p.bytes().len() > MAX_BODY {
            continue;
        }
        if let Err(e) = push(url, p.mime(), p.bytes(), Duration::from_secs(30)) {
            eprintln!("tailclip: push failed: {e}");
        }
    }
}

fn poll_loop(url: &str, watcher: Arc<Mutex<Watcher>>) {
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tailclip: no clipboard: {e}");
            return;
        }
    };
    // The first call reads the current version. It does not apply the clip,
    // so a start up never overwrites the local clipboard.
    let mut seen = loop {
        match fetch(url, None, Duration::from_secs(10)) {
            Ok(r) => break r.version,
            Err(e) => {
                eprintln!("tailclip: server not reachable: {e}");
                thread::sleep(RETRY_WAIT);
            }
        }
    };
    loop {
        let r = match fetch(url, Some(seen), POLL_TIMEOUT + Duration::from_secs(15)) {
            Ok(r) => r,
            Err(_) => {
                thread::sleep(RETRY_WAIT);
                continue;
            }
        };
        // The long poll blocks for up to 300 s, so the pause file can appear
        // while this thread waits. Check it here, not before the poll. The
        // version still moves, so a resume does not apply an old clip.
        seen = r.version;
        if is_paused() || r.status != 200 || r.body.is_empty() {
            continue;
        }
        let p = Payload::from_wire(&r.mime, r.body);
        let mut w = watcher.lock().unwrap();
        match write_clipboard(&mut cb, &p) {
            // Record the new token at once, so the watcher does not push this
            // clip straight back to the server.
            Ok(()) => w.mark_written(&p),
            Err(e) => eprintln!("tailclip: cannot apply clip: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_gets_the_default_port() {
        assert_eq!(clip_url("100.64.0.10"), "http://100.64.0.10:8757/clip");
    }

    #[test]
    fn url_keeps_an_explicit_port() {
        assert_eq!(clip_url("mini.local:9000"), "http://mini.local:9000/clip");
    }

    #[test]
    fn png_signature_check() {
        assert!(looks_like_png(&[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0
        ]));
        assert!(!looks_like_png(b"hello"));
    }

    #[test]
    fn wire_mime_picks_the_payload_kind() {
        assert_eq!(
            Payload::from_wire(TEXT_MIME, b"hi".to_vec()),
            Payload::Text("hi".into())
        );
        match Payload::from_wire(PNG_MIME, vec![1, 2, 3]) {
            Payload::Png(b) => assert_eq!(b, vec![1, 2, 3]),
            other => panic!("expected a PNG, got {other:?}"),
        }
    }

    #[test]
    fn hash_separates_text_from_an_image() {
        let a = Payload::Text("x".into());
        let b = Payload::Png(b"x".to_vec());
        assert_ne!(a.hash64(), b.hash64());
    }

    #[test]
    fn png_round_trip_keeps_the_pixels() {
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
        ];
        let png = encode_png(2, 2, &rgba).expect("encode");
        assert!(looks_like_png(&png));
        let (w, h, back) = decode_png(&png).expect("decode");
        assert_eq!((w, h), (2, 2));
        assert_eq!(back, rgba);
    }

    #[test]
    fn decode_rejects_bytes_that_are_not_a_png() {
        assert!(decode_png(b"not a png at all").is_err());
    }
}
