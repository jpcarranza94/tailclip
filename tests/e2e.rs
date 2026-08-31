//! End to end tests. Each test starts the real `tailclip` binary and talks to
//! it over loopback.
//!
//! The server picks its port with `--port 0` and prints the address it bound,
//! so the tests never collide on a fixed port.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tailclip");
const TEXT_MIME: &str = "text/plain; charset=utf-8";

// ------------------------------------------------------------------ harness

struct Serve {
    child: Child,
    addr: String,
    _out: BufReader<ChildStdout>,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start `tailclip serve` on a free loopback port and return its address.
fn serve() -> Serve {
    let mut child = Command::new(BIN)
        .args(["serve", "--bind", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("start tailclip serve");
    let mut out = BufReader::new(child.stdout.take().expect("the server has stdout"));
    let mut line = String::new();
    out.read_line(&mut line)
        .expect("the server prints its address");
    // "tailclip serving on http://127.0.0.1:54321/clip"
    let addr = line
        .trim()
        .rsplit(' ')
        .next()
        .expect("the line ends with the URL")
        .trim_start_matches("http://")
        .trim_end_matches("/clip")
        .to_string();
    assert!(
        addr.starts_with("127.0.0.1:"),
        "unexpected server line: {line}"
    );
    Serve {
        child,
        addr,
        _out: out,
    }
}

struct Run {
    ok: bool,
    code: i32,
    out: Vec<u8>,
    err: String,
}

fn run(args: &[&str]) -> Run {
    run_with(args, None, &[])
}

fn run_with(args: &[&str], stdin: Option<&[u8]>, env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(BIN);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().expect("start tailclip");
    if let Some(b) = stdin {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b)
            .expect("write stdin");
    }
    let o = child.wait_with_output().expect("wait for tailclip");
    Run {
        ok: o.status.success(),
        code: o.status.code().unwrap_or(-1),
        out: o.stdout,
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn version_of(r: &ureq::http::Response<ureq::Body>) -> u64 {
    r.headers()
        .get("X-Clip-Version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("every reply carries X-Clip-Version")
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tailclip-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("make a temporary HOME");
    d
}

// -------------------------------------------------------------------- tests

#[test]
fn text_round_trips_through_the_binaries() {
    let s = serve();
    let r = run(&["set", &s.addr, "hello from the test"]);
    assert!(r.ok, "set failed: {}", r.err);
    let r = run(&["get", &s.addr]);
    assert!(r.ok, "get failed: {}", r.err);
    assert_eq!(String::from_utf8_lossy(&r.out), "hello from the test");
}

#[test]
fn stdin_sets_the_clip() {
    let s = serve();
    let r = run_with(&["set", &s.addr], Some(b"clip from stdin"), &[]);
    assert!(r.ok, "set failed: {}", r.err);
    let r = run(&["get", &s.addr]);
    assert_eq!(String::from_utf8_lossy(&r.out), "clip from stdin");
}

#[test]
fn a_png_round_trips_and_keeps_its_content_type() {
    let s = serve();
    let png = red_dot_png();
    let r = run_with(&["set", &s.addr], Some(&png), &[]);
    assert!(r.ok, "set failed: {}", r.err);

    let r = run(&["get", &s.addr]);
    assert_eq!(r.out, png, "the bytes must survive the round trip");

    let url = format!("http://{}/clip", s.addr);
    let resp = agent().get(&url).call().expect("get the clip");
    let mime = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(mime.starts_with("image/png"), "content type was {mime}");
}

#[test]
fn an_identical_clip_does_not_bump_the_version() {
    let s = serve();
    let url = format!("http://{}/clip", s.addr);
    let a = agent();

    let r = a
        .post(&url)
        .header("Content-Type", TEXT_MIME)
        .send("same")
        .expect("post");
    assert_eq!(version_of(&r), 1);
    let r = a
        .post(&url)
        .header("Content-Type", TEXT_MIME)
        .send("same")
        .expect("post");
    assert_eq!(
        version_of(&r),
        1,
        "an identical body must not bump the version"
    );
    let r = a
        .post(&url)
        .header("Content-Type", TEXT_MIME)
        .send("other")
        .expect("post");
    assert_eq!(version_of(&r), 2, "a changed body must bump the version");
}

#[test]
fn the_same_bytes_with_a_new_mime_type_bump_the_version() {
    let s = serve();
    let url = format!("http://{}/clip", s.addr);
    let a = agent();
    let body = red_dot_png();

    let r = a
        .post(&url)
        .header("Content-Type", "image/png")
        .send(&body[..])
        .expect("post");
    assert_eq!(version_of(&r), 1);
    let r = a
        .post(&url)
        .header("Content-Type", TEXT_MIME)
        .send(&body[..])
        .expect("post");
    assert_eq!(version_of(&r), 2, "a new mime type must bump the version");
}

#[test]
fn the_long_poll_delivers_a_clip_from_another_process() {
    let s = serve();
    let url = format!("http://{}/clip", s.addr);
    let a = agent();

    let start = version_of(&a.get(&url).call().expect("first get"));
    let addr = s.addr.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        run(&["set", &addr, "delivered by the long poll"]);
    });

    let t0 = Instant::now();
    let mut r = a
        .get(&format!("{url}?since={start}"))
        .call()
        .expect("long poll");
    let waited = t0.elapsed();
    writer.join().expect("the writer thread");

    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(version_of(&r), start + 1);
    assert_eq!(
        r.body_mut().read_to_string().expect("read the body"),
        "delivered by the long poll"
    );
    assert!(
        waited >= Duration::from_millis(300),
        "the poll returned after {waited:?}"
    );
}

#[test]
fn a_clip_larger_than_the_cap_is_rejected() {
    let s = serve();
    let url = format!("http://{}/clip", s.addr);
    let a = agent();
    let too_big = vec![b'z'; 32 * 1024 * 1024 + 1];
    let r = a
        .post(&url)
        .header("Content-Type", TEXT_MIME)
        .send(&too_big[..])
        .expect("post");
    assert_eq!(r.status().as_u16(), 413);

    let mut r = a.get(&url).call().expect("get");
    assert_eq!(version_of(&r), 0, "the oversize clip must not be stored");
    assert!(r.body_mut().read_to_vec().expect("body").is_empty());
}

/// After a restart the server holds version 0, but a client still asks for the
/// version it saw before. The server must answer at once, not block.
#[test]
fn a_since_above_the_version_returns_at_once() {
    let s = serve();
    let url = format!("http://{}/clip", s.addr);
    let a = agent();
    a.post(&url)
        .header("Content-Type", TEXT_MIME)
        .send("one")
        .expect("post");

    let t0 = Instant::now();
    let mut r = a.get(&format!("{url}?since=9999")).call().expect("poll");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "the poll blocked for {:?}",
        t0.elapsed()
    );
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(version_of(&r), 1, "the reply carries the real version");
    assert_eq!(r.body_mut().read_to_string().expect("body"), "one");
}

#[test]
fn a_since_below_the_version_returns_at_once() {
    let s = serve();
    let url = format!("http://{}/clip", s.addr);
    let a = agent();
    a.post(&url)
        .header("Content-Type", TEXT_MIME)
        .send("one")
        .expect("post");
    a.post(&url)
        .header("Content-Type", TEXT_MIME)
        .send("two")
        .expect("post");

    let t0 = Instant::now();
    let r = a.get(&format!("{url}?since=1")).call().expect("poll");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "a client behind must not wait"
    );
    assert_eq!(version_of(&r), 2);
}

#[test]
fn an_unknown_path_is_not_found() {
    let s = serve();
    let r = agent()
        .get(&format!("http://{}/nope", s.addr))
        .call()
        .expect("get");
    assert_eq!(r.status().as_u16(), 404);
}

#[test]
fn serve_refuses_a_public_bind() {
    let r = run(&["serve", "--bind", "8.8.8.8"]);
    assert!(!r.ok, "serve must not start on a public address");
    assert_eq!(r.code, 1);
    assert!(
        r.err.contains("--allow-public-bind"),
        "stderr was: {}",
        r.err
    );
}

/// "Every interface" is not a private address. A container must ask for it.
#[test]
fn serve_refuses_to_bind_every_interface() {
    for a in ["0.0.0.0", "::"] {
        let r = run(&["serve", "--bind", a]);
        assert!(!r.ok, "serve must refuse --bind {a}");
        assert!(
            r.err.contains("--allow-public-bind"),
            "stderr was: {}",
            r.err
        );
    }
}

#[test]
fn serve_rejects_a_bind_that_is_not_an_address() {
    let r = run(&["serve", "--bind", "mini.local"]);
    assert!(!r.ok);
    assert!(r.err.contains("not an IP address"), "stderr was: {}", r.err);
}

#[test]
fn pause_and_resume_move_the_cache_file() {
    let home = tmp_home("pause");
    let h = home.to_str().unwrap();
    let flag = home.join(".cache/tailclip/paused");

    assert!(run_with(&["pause"], None, &[("HOME", h)]).ok);
    assert!(flag.exists(), "pause must create {}", flag.display());

    assert!(run_with(&["resume"], None, &[("HOME", h)]).ok);
    assert!(!flag.exists(), "resume must remove the file");

    // A second resume is not an error.
    assert!(run_with(&["resume"], None, &[("HOME", h)]).ok);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn help_and_unknown_commands_use_the_right_exit_codes() {
    let r = run(&["--help"]);
    assert!(r.ok);
    assert!(String::from_utf8_lossy(&r.out).contains("tailclip serve"));

    let r = run(&["frobnicate"]);
    assert_eq!(r.code, 2, "an unknown command exits 2");

    let r = run(&["--version"]);
    assert!(r.ok);
    assert!(String::from_utf8_lossy(&r.out).starts_with("tailclip "));
}

#[test]
fn get_and_set_need_a_host() {
    for cmd in ["get", "set", "sync"] {
        let r = run(&[cmd]);
        assert!(!r.ok, "{cmd} must fail with no host");
        assert!(r.err.contains("HOST"), "stderr was: {}", r.err);
    }
}

#[test]
fn the_selftest_passes() {
    let r = run(&["selftest"]);
    assert!(
        r.ok,
        "selftest failed:\n{}\n{}",
        String::from_utf8_lossy(&r.out),
        r.err
    );
    assert!(String::from_utf8_lossy(&r.out).contains("selftest OK"));
}

/// The full sync loop, with the real pasteboard of this host.
///
/// This test writes to the clipboard of the user who runs it, so it is
/// ignored by default. To run it, use `cargo test -- --ignored`.
#[test]
#[ignore]
fn the_sync_loop_moves_a_clip_to_the_pasteboard() {
    let s = serve();
    let home = tmp_home("sync");
    let h = home.to_str().unwrap().to_string();
    let mut sync = Command::new(BIN)
        .args(["sync", &s.addr])
        .env("HOME", &h)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start tailclip sync");
    std::thread::sleep(Duration::from_secs(1));

    let mut cb = arboard::Clipboard::new().expect("open the clipboard");
    let tag = std::process::id();

    // The pull direction: a remote clip reaches the pasteboard.
    let want = format!("sync test {tag}");
    assert!(run(&["set", &s.addr, &want]).ok);
    assert_eq!(
        wait_for(&mut cb, &want),
        want,
        "the sync loop must apply the remote clip"
    );

    // A pause stops the pull. The clip sent during the pause never arrives.
    assert!(run_with(&["pause"], None, &[("HOME", &h)]).ok);
    std::thread::sleep(Duration::from_millis(700));
    let hidden = format!("sent while paused {tag}");
    assert!(run(&["set", &s.addr, &hidden]).ok);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        cb.get_text().unwrap_or_default(),
        want,
        "a paused client must not apply a remote clip"
    );

    // A resume starts the pull again, and it does not replay the paused clip.
    assert!(run_with(&["resume"], None, &[("HOME", &h)]).ok);
    let after = format!("sent after resume {tag}");
    assert!(run(&["set", &s.addr, &after]).ok);
    assert_eq!(
        wait_for(&mut cb, &after),
        after,
        "a resumed client must apply a new clip"
    );

    let _ = sync.kill();
    let _ = sync.wait();
    let _ = std::fs::remove_dir_all(&home);
}

/// Read the clipboard until it holds `want`, or until 10 s pass.
fn wait_for(cb: &mut arboard::Clipboard, want: &str) -> String {
    let t0 = Instant::now();
    let mut got = String::new();
    while t0.elapsed() < Duration::from_secs(10) {
        got = cb.get_text().unwrap_or_default();
        if got == want {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    got
}

/// A 1x1 red PNG.
fn red_dot_png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 1, 1);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&[255, 0, 0, 255]).expect("png data");
        w.finish().expect("png finish");
    }
    out
}
