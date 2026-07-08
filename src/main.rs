use anyhow::{Context, Result, anyhow};
use ffmpeg_next as ffmpeg;

mod clock;
mod media;
mod webrtc_session;
mod webtransport_session;

use media::{dry_run_media_file, stream_media_file};
use webrtc_session::WebRtcSession;
use webtransport_session::run_webtransport_example;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;

    ffmpeg::init().context("failed to initialize ffmpeg")?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);

    if args.dry_run {
        dry_run_media_file(&args.input)?;
        return Ok(());
    }

    if args.webtransport {
        run_webtransport_example(&args.input).await?;
        return Ok(());
    }

    let mut session = WebRtcSession::new().await?;
    session.complete_http_signaling().await?;
    session.wait_until_connected().await?;

    eprintln!("WebRTC connected. Transcoding and streaming {}", args.input);
    stream_media_file(
        &args.input,
        session.video_track.clone(),
        session.audio_track.clone(),
    )
    .await?;

    session.peer_connection.close().await?;
    Ok(())
}

struct Args {
    input: String,
    dry_run: bool,
    webtransport: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let first_arg = args
            .next()
            .ok_or_else(|| anyhow!("usage: cargo run -- [--dry-run] <input-media-file>"))?;

        if first_arg == "--dry-run" {
            let input = args
                .next()
                .ok_or_else(|| anyhow!("usage: cargo run -- --dry-run <input-media-file>"))?;
            return Ok(Self {
                input,
                dry_run: true,
                webtransport: false,
            });
        }

        if first_arg == "--webtransport" {
            let input = args
                .next()
                .ok_or_else(|| anyhow!("usage: cargo run -- --webtransport <input-media-file>"))?;
            return Ok(Self {
                input,
                dry_run: false,
                webtransport: true,
            });
        }

        Ok(Self {
            input: first_arg,
            dry_run: false,
            webtransport: false,
        })
    }
}
