# tailclip

A shared clipboard for a tailnet. Copy on one machine. Paste on another.

The tailnet is the authentication. Tailscale already authenticates every device
and encrypts every packet with WireGuard. tailclip adds no account, no password,
and no session. There is nothing to log in to again after a restart.

Text and images. One clip in RAM. No disk. About 30 crates.

## Quick start

You must have Tailscale on both machines first.

1. Install Rust, then build the binary:

       git clone https://github.com/jpcarranza94/tailclip
       cd tailclip
       cargo build --release
       sudo cp target/release/tailclip /usr/local/bin/

2. Check the build:

       tailclip selftest

3. Start the server on the machine that stays awake:

       tailclip serve

   With no `--bind`, the server looks for an address inside `100.64.0.0/10` and
   binds it. That range belongs to Tailscale. The server prints the address it
   bound. The default port is 8757.

4. Start the client on every other machine. Use the Tailscale address of the
   server:

       tailclip sync 100.110.80.23:8757

5. Copy some text on one machine. Paste it on another machine.

## Commands

| Command | What it does |
|---|---|
| `tailclip serve [--bind IP] [--port N] [--allow-public-bind]` | Hold the clip and answer the long poll |
| `tailclip sync HOST[:PORT]` | Two way sync of the local pasteboard |
| `tailclip get HOST[:PORT]` | Write the current clip to stdout |
| `tailclip set HOST[:PORT] [TEXT]` | Push text from the argument, or bytes from stdin |
| `tailclip pause` | Stop the push and the pull |
| `tailclip resume` | Start the push and the pull again |
| `tailclip selftest` | Run every check on loopback |

The machine that runs `serve` can also run `sync`. The server never touches a
pasteboard, so the two processes do not conflict.

`pause` writes `~/.cache/tailclip/paused`. `resume` removes that file. The sync
loop reads the file on each tick. Before you copy a password, run `tailclip
pause`.

A pause stops both directions, and it drops what happens during the pause. A
clip that you copy while paused never leaves the machine, not even after the
resume. A clip that another device sends while you are paused never reaches
your pasteboard. After the resume, only new clips move.

## Threat model

Read this before you run tailclip.

- Tailscale authenticates the devices. tailclip does not, and it never will. A
  second identity system on top of WireGuard adds work and adds no security.
- To remove a device, remove it from the Tailscale admin console. The key of
  that device stops working at once. tailclip needs no revocation list.
- `serve` refuses to bind a public address. Loopback, RFC1918, `100.64.0.0/10`,
  and IPv6 `fc00::/7` are allowed. `0.0.0.0` and `::` mean every interface, so
  the server refuses them too. `--allow-public-bind` overrides the refusal. Do
  not use that flag on a host with a public address. tailclip has no
  authentication, so a public bind gives the clipboard to the internet. Inside
  a container, the flag is correct, because the network namespace is the
  boundary.
- The server keeps one clip in RAM. It writes nothing to disk. If a thief takes
  the machine, the clip is gone.
- The transport is plain HTTP inside the WireGuard tunnel. Tailscale already
  encrypts the packets, so a second TLS layer adds only certificates to manage.

### Residual risk

A copied password travels to every connected device. This is the point of a
shared clipboard, and it is also the risk. The clip sits in the RAM of the
server and in the pasteboard of every client until the next copy replaces it.

If you copy a secret, run `tailclip pause` first. Run `tailclip resume` after.

## Protocol

The protocol is bytes and headers. There is no JSON.

    GET /clip?since=<v>
        Blocks up to 300 s until the version passes <v>.
        With no "since", the server answers at once.
        200  body = the clip, headers: X-Clip-Version, Content-Type
        204  timeout, nothing changed, header: X-Clip-Version

    POST /clip
        Body = the raw clip. Header Content-Type: text/plain; charset=utf-8
        or image/png.
        200  header: X-Clip-Version
        413  the body is larger than 32 MB

An identical body with an identical mime type does not bump the version. The
version starts at 0 with an empty clip.

You can drive the whole protocol with `curl`:

    curl -s http://100.110.80.23:8757/clip
    curl -s -X POST --data-binary 'hello' http://100.110.80.23:8757/clip

## Platforms

macOS and Linux are the release targets. Windows compiles, but nobody tests it.

Both targets are tested. The Linux tests run in a Debian container, with a
virtual X server for the clipboard checks:

    docker run --rm -v "$PWD":/src:ro rust:slim bash -c \
      "apt-get update -qq && apt-get install -y -qq pkg-config libwayland-dev \
       && cp -r /src /tmp/tc && cd /tmp/tc && cargo test"

There is no phone client. Android blocks background clipboard reads, so a
client cannot see a copy that happens in another app. Use `tailclip get` and
`tailclip set` from a script instead.

On macOS, the client reads `NSPasteboard.changeCount` to find a change. That
call costs nothing, so the client never reads a 24 MB image until the count
moves. On Linux, the client reads the text first, because text is cheap, and
falls back to the image. It hashes the result to find a change.

On X11, a process must stay alive to own the selection. The client is a long
running daemon, so a pasted clip stays available. If you stop `tailclip sync`,
the last clip that tailclip wrote disappears from the X11 selection.

## Run it at boot on macOS

The `launchd` directory holds two templates.

1. Install the server as a LaunchDaemon:

       sudo cp launchd/com.jpcar.tailclip-server.plist /Library/LaunchDaemons/
       sudo launchctl bootstrap system /Library/LaunchDaemons/com.jpcar.tailclip-server.plist

2. Set the address of the server in the client template. Replace
   `MINI-TAILSCALE-IP` with the Tailscale address of the server:

       sed -i '' 's/MINI-TAILSCALE-IP/100.110.80.23/' launchd/com.jpcar.tailclip-client.plist

3. Install the client as a LaunchAgent:

       cp launchd/com.jpcar.tailclip-client.plist ~/Library/LaunchAgents/
       launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.jpcar.tailclip-client.plist

The client must be a LaunchAgent, not a LaunchDaemon. Only an agent in the GUI
session can read the pasteboard.

If Tailscale is not up yet, the server exits and `launchd` starts it again after
10 s.

## Design

| Decision | Choice |
|---|---|
| Storage | RAM only, one clip, no disk |
| Transport | Long poll, 300 s timeout |
| Runtime | Blocking threads, `tiny_http` and `ureq` |
| Content | Text and images, as raw bytes plus `Content-Type` |
| Size cap | 32 MB |
| Auth | None. The tailnet authenticates |
| History | One clip. A local clipboard manager does history better |

The dependency tree is small on purpose: `tiny_http`, `ureq` with the TLS
features off, `arboard`, `png`, and `if-addrs`. On macOS, `NSPasteboard` carries
PNG bytes itself, so the `image` crate and its unused decoders stay out.

## Tests

    cargo test                  # 17 unit tests and 17 end to end tests
    cargo test -- --ignored     # also drive the real pasteboard of this host
    tailclip selftest           # the same protocol checks, from the binary

## License

Dual licensed under MIT or Apache-2.0, at your option.
