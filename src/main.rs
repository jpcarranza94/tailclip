//! tailclip - a shared clipboard for a tailnet.
//!
//! The tailnet is the authentication. WireGuard already authenticates every
//! device and encrypts every packet, so tailclip has no account, no password,
//! and no session. There is nothing to log in to again.

mod client;
mod server;

use std::net::SocketAddr;
use std::process::exit;

const USAGE: &str = "\
tailclip - a shared clipboard for a tailnet

USAGE
  tailclip serve [--bind IP] [--port N] [--allow-public-bind]
  tailclip sync  HOST[:PORT]
  tailclip get   HOST[:PORT]
  tailclip set   HOST[:PORT] [TEXT]
  tailclip pause
  tailclip resume
  tailclip selftest

With no --bind, serve looks for an address inside 100.64.0.0/10 and binds it.
That is the Tailscale range. The default port is 8757.

tailclip has no authentication. Run it on a tailnet only.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);

    let result = match cmd {
        "serve" => serve_cmd(rest),
        "sync" => host_of(rest).and_then(|h| client::sync(&h)),
        "get" => host_of(rest).and_then(|h| client::get(&h)),
        "set" => host_of(rest).and_then(|h| client::set(&h, rest.get(1).map(String::as_str))),
        "pause" => client::pause(),
        "resume" => client::resume(),
        "selftest" => selftest(),
        "-V" | "--version" | "version" => {
            println!("tailclip {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "-h" | "--help" | "help" | "" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprint!("tailclip: unknown command {other}\n\n{USAGE}");
            exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("tailclip: {e}");
        exit(1);
    }
}

fn host_of(rest: &[String]) -> Result<String, String> {
    match rest.first() {
        Some(h) => Ok(h.clone()),
        None => Err("this command needs a HOST[:PORT] argument".to_string()),
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn serve_cmd(args: &[String]) -> Result<(), String> {
    let allow_public = args.iter().any(|a| a == "--allow-public-bind");
    let bind = flag_value(args, "--bind");
    let port: u16 = match flag_value(args, "--port") {
        Some(p) => p
            .parse()
            .map_err(|_| format!("--port {p}: not a port number"))?,
        None => server::DEFAULT_PORT,
    };
    let ip = server::pick_bind(bind.as_deref(), allow_public)?;
    server::run(SocketAddr::new(ip, port))
}

// ------------------------------------------------------------------ selftest

macro_rules! check {
    ($cond:expr, $($msg:tt)*) => {
        if $cond {
            println!("  ok   {}", format!($($msg)*));
        } else {
            println!("  FAIL {}", format!($($msg)*));
            return Err(format!("selftest failed: {}", format!($($msg)*)));
        }
    };
}

/// Run every check of the project. Loopback only, on port 8759.
fn selftest() -> Result<(), String> {
    use client::{fetch, push, Payload, PNG_MIME};
    use server::{is_private, pick_bind, MAX_BODY, TEXT_MIME};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    let port = 8759u16;
    let url = format!("http://127.0.0.1:{port}/clip");
    let short = Duration::from_secs(30);

    // A short poll timeout keeps check 9 fast. Check 8 returns long before it.
    let addr = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port);
    let state = Arc::new(server::State::new(Duration::from_secs(2)));
    thread::spawn(move || {
        if let Err(e) = server::run_with(addr, state) {
            eprintln!("selftest server: {e}");
        }
    });

    // Wait for the listener.
    let up = Instant::now();
    while fetch(&url, None, Duration::from_secs(1)).is_err() {
        if up.elapsed() > Duration::from_secs(5) {
            return Err("the selftest server did not start".to_string());
        }
        thread::sleep(Duration::from_millis(50));
    }

    println!("tailclip selftest on {url}");

    // 1. Empty start reports version 0.
    let r = fetch(&url, None, short)?;
    check!(
        r.status == 200 && r.version == 0 && r.body.is_empty(),
        "1  empty start reports version 0"
    );

    // 2. A first POST bumps the version to 1.
    let r = push(&url, TEXT_MIME, b"hello", short)?;
    check!(
        r.status == 200 && r.version == 1,
        "2  first post bumps the version to 1"
    );

    // 3. A GET returns the same text.
    let r = fetch(&url, None, short)?;
    check!(r.body == b"hello", "3  get returns the same text");

    // 4. An identical POST does NOT bump the version.
    let r = push(&url, TEXT_MIME, b"hello", short)?;
    check!(
        r.version == 1,
        "4  an identical post does not bump the version"
    );

    // 5. A changed POST bumps the version to 2.
    let r = push(&url, TEXT_MIME, b"world", short)?;
    check!(r.version == 2, "5  a changed post bumps the version to 2");
    let r = fetch(&url, None, short)?;
    check!(r.body == b"world", "5b second round trip");

    // 6. A 1 KB clip round trips.
    let big = vec![b'x'; 1000];
    push(&url, TEXT_MIME, &big, short)?;
    let r = fetch(&url, None, short)?;
    check!(r.body == big, "6  a 1 KB clip round trips");

    // 7. A PNG body round trips, and the Content-Type survives.
    let png = tiny_png();
    let r = push(&url, PNG_MIME, &png, short)?;
    let v_png = r.version;
    let r = fetch(&url, None, short)?;
    check!(
        r.body == png && r.mime.starts_with(PNG_MIME),
        "7  a PNG round trips and keeps its content type"
    );
    check!(
        matches!(Payload::from_wire(&r.mime, r.body), Payload::Png(_)),
        "7b the client reads it back as an image"
    );

    // 8. A long poll blocks, then returns 200 when another thread posts.
    {
        let u = url.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let _ = push(&u, TEXT_MIME, b"pushed by the other thread", short);
        });
        let t0 = Instant::now();
        let r = fetch(&url, Some(v_png), short)?;
        let waited = t0.elapsed();
        check!(
            r.status == 200 && r.version == v_png + 1,
            "8  a long poll returns 200 after a new post"
        );
        check!(
            r.body == b"pushed by the other thread",
            "8b the long poll carries the new clip"
        );
        check!(
            waited >= Duration::from_millis(200),
            "8c the long poll blocked, it waited {}ms",
            waited.as_millis()
        );
    }

    // 9. A long poll that times out returns 204.
    {
        let now = fetch(&url, None, short)?.version;
        let t0 = Instant::now();
        let r = fetch(&url, Some(now), short)?;
        let waited = t0.elapsed();
        check!(
            r.status == 204 && r.body.is_empty(),
            "9  a long poll timeout returns 204"
        );
        check!(r.version == now, "9b the 204 carries the current version");
        check!(
            waited >= Duration::from_millis(1500),
            "9c the timeout waited {}ms",
            waited.as_millis()
        );
    }

    // 10. A body larger than 32 MB returns 413.
    {
        let too_big = vec![b'z'; MAX_BODY + 1];
        let r = push(&url, TEXT_MIME, &too_big, Duration::from_secs(120))?;
        check!(
            r.status == 413,
            "10 a body larger than 32 MB returns 413, got {}",
            r.status
        );
        let r = fetch(&url, None, short)?;
        check!(r.body != too_big, "10b the oversize clip was not stored");
    }

    // 11. The bind address rules.
    check!(
        is_private("100.110.80.23".parse::<IpAddr>().unwrap()),
        "11 the rules accept the Tailscale address 100.110.80.23"
    );
    check!(
        pick_bind(Some("100.110.80.23"), false).is_ok(),
        "11b --bind accepts a tailnet address"
    );
    check!(
        pick_bind(Some("8.8.8.8"), false).is_err(),
        "11c --bind rejects a public address"
    );
    check!(
        pick_bind(Some("8.8.8.8"), true).is_ok(),
        "11d --allow-public-bind overrides the rejection"
    );
    check!(
        pick_bind(Some("192.168.1.10"), false).is_ok(),
        "11e --bind accepts an RFC1918 address"
    );
    check!(
        pick_bind(Some("127.0.0.1"), false).is_ok(),
        "11f --bind accepts loopback"
    );
    check!(
        pick_bind(Some("not-an-ip"), false).is_err(),
        "11g --bind rejects a name"
    );
    check!(
        pick_bind(Some("0.0.0.0"), false).is_err(),
        "11h --bind rejects every interface"
    );
    check!(
        pick_bind(Some("::"), false).is_err(),
        "11i --bind rejects every interface, IPv6"
    );

    // 12. A since above the current version returns at once, not after a wait.
    {
        let now = fetch(&url, None, short)?.version;
        let t0 = Instant::now();
        let r = fetch(&url, Some(now + 500), short)?;
        check!(
            r.status == 200 && t0.elapsed() < Duration::from_secs(1),
            "12 a since above the version returns at once, after a server restart"
        );
        check!(
            r.version == now,
            "12b it carries the real version, so the client resets"
        );
    }

    println!("selftest OK");
    Ok(())
}

/// A 1x1 red PNG, so the selftest needs no file on disk.
fn tiny_png() -> Vec<u8> {
    client::encode_png(1, 1, &[255, 0, 0, 255]).expect("encode a 1x1 PNG")
}
