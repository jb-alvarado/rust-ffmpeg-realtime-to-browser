use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;
use wtransport::tls::Sha256DigestFmt;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::clock::MediaClock;
use crate::media::{EncodedBatch, EncodedSample, MediaReader};
use crate::webrtc_session::StaticHttpServer;

pub(crate) async fn run_webtransport_example(input: &str) -> Result<()> {
    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_digest = identity.certificate_chain().as_slice()[0].hash();
    let cert_hash = cert_digest.fmt(Sha256DigestFmt::BytesArray);

    let config = ServerConfig::builder()
        .with_bind_default(0)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
    let endpoint = Endpoint::server(config).context("failed to start WebTransport endpoint")?;
    let webtransport_port = endpoint.local_addr()?.port();
    let input = Arc::new(input.to_owned());

    let http_server = StaticHttpServer::webtransport(cert_hash, webtransport_port);
    tokio::spawn(async move {
        if let Err(error) = http_server.serve().await {
            eprintln!("static HTTP server stopped: {error:#}");
        }
    });

    eprintln!("Open http://127.0.0.1:3000/webtransport.html and click Start WebTransport.");

    loop {
        let incoming = endpoint.accept().await;
        if let Err(error) = handle_session(incoming, &input).await {
            eprintln!("WebTransport session failed: {error:#}");
        }
    }
}

async fn handle_session(
    incoming: wtransport::endpoint::IncomingSession,
    input: &str,
) -> Result<()> {
    let request = incoming.await?;
    eprintln!(
        "WebTransport request: authority={} path={}",
        request.authority(),
        request.path()
    );
    let connection = request.accept().await?;
    let mut stream = connection.open_uni().await?.await?;

    stream.write_all(b"WTMEDIA1").await?;
    stream_media_to_webtransport(input, &mut stream).await?;
    stream.finish().await?;
    connection.close(0_u32.into(), b"done");
    sleep(Duration::from_millis(100)).await;

    Ok(())
}

async fn stream_media_to_webtransport(
    input: &str,
    stream: &mut wtransport::SendStream,
) -> Result<()> {
    let mut reader = MediaReader::open(input)?;
    let mut clock = MediaClock::new();
    write_metadata(stream, reader.video_size()).await?;

    while let Some(batch) = reader.next_batch()? {
        write_batch(stream, &mut clock, batch).await?;
    }

    let (video_samples, audio_samples) = reader.finish()?;
    write_samples(stream, &mut clock, 1, video_samples).await?;
    write_samples(stream, &mut clock, 2, audio_samples).await?;

    Ok(())
}

async fn write_metadata(
    stream: &mut wtransport::SendStream,
    video_size: Option<(u32, u32)>,
) -> Result<()> {
    let (width, height) = video_size.unwrap_or((0, 0));
    let metadata = format!(
        r#"{{"video":{{"codec":"avc1.42E01F","width":{width},"height":{height}}},"audio":{{"codec":"opus","sampleRate":48000,"channels":2}}}}"#
    );
    let sample = EncodedSample {
        pts_us: Some(0),
        duration_us: 1,
        data: metadata.into_bytes(),
    };
    write_sample(stream, 0, &sample).await
}

async fn write_batch(
    stream: &mut wtransport::SendStream,
    clock: &mut MediaClock,
    batch: EncodedBatch,
) -> Result<()> {
    match batch {
        EncodedBatch::Video(samples) => write_samples(stream, clock, 1, samples).await,
        EncodedBatch::Audio(samples) => write_samples(stream, clock, 2, samples).await,
    }
}

async fn write_samples(
    stream: &mut wtransport::SendStream,
    clock: &mut MediaClock,
    kind: u8,
    samples: Vec<EncodedSample>,
) -> Result<()> {
    for sample in samples {
        clock.wait_until(sample.pts_us).await;
        write_sample(stream, kind, &sample).await?;
    }

    Ok(())
}

async fn write_sample(
    stream: &mut wtransport::SendStream,
    kind: u8,
    sample: &EncodedSample,
) -> Result<()> {
    let pts_us = sample.pts_us.unwrap_or(-1);
    let duration_us = sample.duration_us.max(1);
    let len = sample.data.len() as u32;
    let mut header = [0_u8; 21];
    header[0] = kind;
    header[1..9].copy_from_slice(&pts_us.to_be_bytes());
    header[9..17].copy_from_slice(&duration_us.to_be_bytes());
    header[17..21].copy_from_slice(&len.to_be_bytes());

    stream.write_all(&header).await?;
    stream.write_all(&sample.data).await?;
    Ok(())
}
