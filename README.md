# rust-ffmpeg-realtime-to-browser

Minimal Rust inspired by
[`ashellunts/ffmpeg-to-webrtc`](https://github.com/ashellunts/ffmpeg-to-webrtc).

The Go original reads an H264 stream produced by FFmpeg and sends it to a browser
over WebRTC. This version accepts normal media files directly, decodes them with
`ffmpeg-next`, transcodes them to browser-friendly codecs, and sends them to the
browser in realtime over WebRTC or WebTransport:

- video input -> H264 baseline, Annex-B, 90 kHz WebRTC clock
- audio input -> Opus, 48 kHz, stereo

Files with only video or only audio are accepted too.

## Prerequisites

- Rust
- FFmpeg development libraries available to `ffmpeg-next`
- FFmpeg built with H264 and Opus encoders, preferably `libx264` and `libopus`
- A browser with WebRTC support
- For the QUIC browser demo: a browser with WebTransport support

On Debian/Ubuntu-like systems the native FFmpeg packages are typically:

```sh
sudo apt install pkg-config libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev
```

## Run

Optional: verify that your FFmpeg build can decode and transcode the file:

```sh
cargo run -- --dry-run input.mp4
```

1. Run the Rust process with a normal media file:

   ```sh
   cargo run -- input.mp4
   ```

   Other common containers such as `.mkv`, `.mov`, `.webm`, `.mp3`, or `.wav`
   can work as long as your FFmpeg build can decode them.

2. Open `http://127.0.0.1:3000` in your browser.
3. Click `Start session`.

The media should play in the browser.

Reloading the page is supported. To start a new WebRTC session after one has
already been negotiated, restart the Rust process; this demo keeps one
PeerConnection per process.

## WebTransport over QUIC

Browsers do not expose raw QUIC sockets to JavaScript. The browser-facing QUIC
API is WebTransport over HTTP/3. This repository includes a separate
WebTransport mode:

```sh
cargo run -- --webtransport input.mp4
```

Then open:

```text
http://127.0.0.1:3000/webtransport.html
```

Click `Start WebTransport`. The Rust process transcodes the same media to H264
and Opus, sends the encoded samples over WebTransport, and the browser decodes
H264 video with WebCodecs onto a canvas. Opus audio is decoded with WebCodecs
`AudioDecoder` and played through the Web Audio API.

## Notes

This example uses a tiny local HTTP signaling server on `127.0.0.1:3000`, so no
terminal SDP copy/paste is needed. It uses a small PTS-based clock, so encoded
samples are written at their media timestamps instead of as fast as the file can
be decoded.
