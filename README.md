# rtsp-player

Cross-platform desktop viewer for RTSP camera streams. Connections live in a
tree you organise into groups; opening one puts it on a video wall that grows
into a grid as you add more.

Credentials are optional per connection — cameras with anonymous access need
none, and cameras that want digest auth get it.

## Stack

Everything below RTSP is pure Rust. There is no ffmpeg, no GStreamer, no C
codec to link against.

| Layer | Crate |
|---|---|
| UI | [`gpui`](https://crates.io/crates/gpui) 0.2 + [`gpui-component`](https://crates.io/crates/gpui-component) 0.5 |
| RTSP / RTP | [`retina`](https://crates.io/crates/retina) 0.4 |
| H.264 decode | [`rusty_h264`](https://crates.io/crates/rusty_h264) 0.10 |
| H.265 decode | [`rust_h265`](https://crates.io/crates/rust_h265) 0.1 |

Decoding is software only, one thread per stream. Measured on this machine
against a local test server:

| Stream | Source rate | Decoded |
|---|---|---|
| 720p H.264 | 25 fps | 25.0 fps |
| 1080p H.264 | 25 fps | 25.0 fps |
| 480p H.265 | 15 fps | 14.7 fps |
| 1080p H.265 | 25 fps | 22.0 fps |

H.264 keeps up at 1080p with headroom. H.265 at 1080p runs about 12% behind
real time — `rust_h265` is young and unoptimised. Prefer a camera's H.264
substream if you plan to watch several 1080p feeds at once.

## Build

```sh
cargo build --release
```

Linux additionally needs the windowing and font development libraries that
gpui links against:

```sh
sudo apt install libxkbcommon-x11-dev libxcb1-dev libfontconfig-dev libfreetype6-dev libwayland-dev
```

macOS and Windows need no extra packages.

## Run

```sh
cargo run --release
```

Connections are stored as JSON in the platform config directory —
`~/.config/rtsp-player/library.json` on Linux, `%APPDATA%` on Windows,
`~/Library/Application Support` on macOS. Streams that were playing at exit
reopen on the next launch.

**Passwords are stored in plain text** in that file. It is not readable by
other users, but it is not encrypted either. Do not put a password there that
you use anywhere else.

## Using it

- **Add stream** — name, URL, optional username and password, TCP or UDP.
- **Group** — creates a folder. New items go into whatever is selected, so
  select a group first to add inside it.
- **Click a connection** — opens it on the wall. Click a group to fold it.
- **Edit / Delete** — act on the current selection; Edit renames a group or
  reopens the form for a connection.
- **✕ on a tile** — closes that stream.

TCP is the default transport and the right choice almost always. UDP is
lower latency on a quiet LAN but `retina` has no reorder buffer, so any
out-of-order packet is dropped.

## Checking a camera without the GUI

```sh
cargo run --release -- --probe rtsp://192.168.1.50:554/stream1 [username] [password]
```

Connects, decodes for a few seconds, and reports the resolution and frame
rate, or the error. Set `RTSP_PLAYER_DEBUG=1` for a trace of what arrives on
the wire.

## Notes and limits

- Video only. Audio streams are ignored.
- H.264 and H.265 only, 8-bit 4:2:0 (10-bit HEVC is downshifted to 8-bit).
  MJPEG cameras are not supported.
- Reconnects automatically with backoff up to 10 seconds.
- Some servers never flag a key frame. After 60 access units the player stops
  waiting and decodes anyway; parameter sets are also pulled from the SDP so
  the decoder is configured either way.
- **WSL:** gpui's Wayland client rejects WSLg's compositor version, so the app
  drops to XWayland automatically when it detects WSL.
