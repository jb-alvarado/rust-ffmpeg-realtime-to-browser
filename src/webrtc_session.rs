use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::time::{Duration, timeout};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::media::{AUDIO_SAMPLE_RATE, VIDEO_CLOCK_RATE};

pub(crate) struct StaticHttpServer {
    webtransport_config: Option<String>,
}

impl StaticHttpServer {
    pub(crate) fn webtransport(cert_hash: String, port: u16) -> Self {
        Self {
            webtransport_config: Some(format!(
                r#"{{"url":"https://127.0.0.1:{port}/media","serverCertificateHash":{cert_hash}}}"#
            )),
        }
    }

    pub(crate) async fn serve(self) -> Result<()> {
        let listener = bind_signaling_listener()
            .await
            .context("failed to bind HTTP server on 127.0.0.1:3000")?;
        let config = Arc::new(self.webtransport_config);

        loop {
            let (stream, _) = listener.accept().await?;
            handle_static_http_request(stream, Arc::clone(&config)).await?;
        }
    }
}

pub(crate) struct WebRtcSession {
    pub(crate) peer_connection: Arc<RTCPeerConnection>,
    pub(crate) video_track: Arc<TrackLocalStaticSample>,
    pub(crate) audio_track: Arc<TrackLocalStaticSample>,
    state_rx: watch::Receiver<RTCPeerConnectionState>,
}

impl WebRtcSession {
    pub(crate) async fn new() -> Result<Self> {
        let (state_tx, state_rx) = watch::channel(RTCPeerConnectionState::Unspecified);
        let peer_connection = create_peer_connection(state_tx).await?;
        let video_track = create_video_track();
        let audio_track = create_audio_track();

        peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("failed to add video track")?;
        peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("failed to add audio track")?;

        Ok(Self {
            peer_connection,
            video_track,
            audio_track,
            state_rx,
        })
    }

    pub(crate) async fn complete_http_signaling(&self) -> Result<()> {
        let listener = bind_signaling_listener()
            .await
            .context("failed to bind signaling server on 127.0.0.1:3000")?;
        eprintln!("Open http://127.0.0.1:3000 and click Start session.");

        let (offer_tx, offer_rx) = oneshot::channel();
        let peer_connection = Arc::clone(&self.peer_connection);
        tokio::spawn(async move {
            if let Err(error) = serve_http_signaling(listener, peer_connection, offer_tx).await {
                eprintln!("signaling server stopped: {error:#}");
            }
        });

        offer_rx
            .await
            .context("signaling server stopped before receiving an offer")
    }

    pub(crate) async fn wait_until_connected(&mut self) -> Result<()> {
        eprintln!("Waiting for the browser to apply the answer and connect...");

        loop {
            match *self.state_rx.borrow_and_update() {
                RTCPeerConnectionState::Connected => return Ok(()),
                RTCPeerConnectionState::Failed => bail!("peer connection failed"),
                RTCPeerConnectionState::Closed => {
                    bail!("peer connection closed before streaming");
                }
                _ => {}
            }

            self.state_rx
                .changed()
                .await
                .context("peer connection state channel closed")?;
        }
    }
}

async fn bind_signaling_listener() -> Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind("127.0.0.1:3000".parse()?)?;
    Ok(socket.listen(128)?)
}

async fn serve_http_signaling(
    listener: TcpListener,
    peer_connection: Arc<RTCPeerConnection>,
    offer_tx: oneshot::Sender<()>,
) -> Result<()> {
    let answered = Arc::new(AtomicBool::new(false));
    let mut offer_tx = Some(offer_tx);

    loop {
        let (stream, _) = listener.accept().await?;
        let completed_offer =
            handle_http_request(stream, Arc::clone(&peer_connection), Arc::clone(&answered))
                .await?;

        if completed_offer && let Some(offer_tx) = offer_tx.take() {
            let _ = offer_tx.send(());
        }
    }
}

async fn handle_http_request(
    mut stream: TcpStream,
    peer_connection: Arc<RTCPeerConnection>,
    answered: Arc<AtomicBool>,
) -> Result<bool> {
    let request = read_http_request(&mut stream).await?;

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", _) => {
            write_static_response(&mut stream, &request.path, None).await?;
            Ok(false)
        }
        ("POST", "/offer") => {
            if answered.load(Ordering::SeqCst) {
                write_http_response(
                    &mut stream,
                    "409 Conflict",
                    "text/plain",
                    "this demo process already has an active WebRTC session; restart cargo run to create a new session",
                )
                .await?;
                return Ok(false);
            }

            match answer_offer(&peer_connection, &request.body).await {
                Ok(answer) => {
                    answered.store(true, Ordering::SeqCst);
                    write_http_response(&mut stream, "200 OK", "application/json", &answer).await?;
                    Ok(true)
                }
                Err(error) => {
                    let body = format!("failed to answer offer: {error:#}");
                    write_http_response(
                        &mut stream,
                        "500 Internal Server Error",
                        "text/plain",
                        &body,
                    )
                    .await?;
                    Ok(false)
                }
            }
        }
        _ => {
            write_http_response(&mut stream, "404 Not Found", "text/plain", "not found").await?;
            Ok(false)
        }
    }
}

async fn handle_static_http_request(
    mut stream: TcpStream,
    webtransport_config: Arc<Option<String>>,
) -> Result<()> {
    let request = read_http_request(&mut stream).await?;

    match request.method.as_str() {
        "GET" => {
            write_static_response(
                &mut stream,
                &request.path,
                webtransport_config.as_ref().as_ref(),
            )
            .await?;
        }
        _ => {
            write_http_response(
                &mut stream,
                "405 Method Not Allowed",
                "text/plain",
                "method not allowed",
            )
            .await?;
        }
    }

    Ok(())
}

async fn write_static_response(
    stream: &mut TcpStream,
    path: &str,
    webtransport_config: Option<&String>,
) -> Result<()> {
    match path {
        "/" | "/index.html" => {
            write_http_response(stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML).await
        }
        "/webtransport.html" => {
            write_http_response(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                WEBTRANSPORT_HTML,
            )
            .await
        }
        "/css/main.css" => {
            write_http_response(stream, "200 OK", "text/css; charset=utf-8", MAIN_CSS).await
        }
        "/js/webrtc.js" => {
            write_http_response(
                stream,
                "200 OK",
                "application/javascript; charset=utf-8",
                MAIN_JS,
            )
            .await
        }
        "/js/webtransport.js" => {
            write_http_response(
                stream,
                "200 OK",
                "application/javascript; charset=utf-8",
                WEBTRANSPORT_JS,
            )
            .await
        }
        "/webtransport-config" => {
            let body = webtransport_config
                .map(String::as_str)
                .unwrap_or(r#"{"error":"webtransport mode is not running"}"#);
            write_http_response(stream, "200 OK", "application/json", body).await
        }
        _ => write_http_response(stream, "404 Not Found", "text/plain", "not found").await,
    }
}

async fn answer_offer(peer_connection: &RTCPeerConnection, body: &[u8]) -> Result<String> {
    let offer: RTCSessionDescription =
        serde_json::from_slice(body).context("failed to parse browser offer JSON")?;

    peer_connection
        .set_remote_description(offer)
        .await
        .context("failed to set remote description")?;

    let answer = peer_connection
        .create_answer(None)
        .await
        .context("failed to create answer")?;
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    peer_connection
        .set_local_description(answer)
        .await
        .context("failed to set local description")?;

    let _ = timeout(Duration::from_millis(1000), gather_complete.recv()).await;

    let local_description = peer_connection
        .local_description()
        .await
        .ok_or_else(|| anyhow!("local description was not set"))?;

    Ok(serde_json::to_string(&local_description)?)
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let header_end;

    loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("client closed connection before sending a complete HTTP request");
        }
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(position) = find_header_end(&buffer) {
            header_end = position;
            break;
        }

        if buffer.len() > 128 * 1024 {
            bail!("HTTP request headers are too large");
        }
    }

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing HTTP request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP method"))?
        .to_owned();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP path"))?
        .to_owned();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let mut chunk = vec![0_u8; body_start + content_length - buffer.len()];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("client closed connection before sending complete HTTP body");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Ok(HttpRequest {
        method,
        path,
        body: buffer[body_start..body_start + content_length].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn create_peer_connection(
    state_tx: watch::Sender<RTCPeerConnectionState>,
) -> Result<Arc<RTCPeerConnection>> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .context("failed to register default codecs")?;

    let api = APIBuilder::new().with_media_engine(media_engine).build();
    let config = RTCConfiguration::default();

    let peer_connection = Arc::new(
        api.new_peer_connection(config)
            .await
            .context("failed to create peer connection")?,
    );

    peer_connection.on_peer_connection_state_change(Box::new(
        move |state: RTCPeerConnectionState| {
            eprintln!("PeerConnection state: {state}");
            let _ = state_tx.send(state);
            Box::pin(async {})
        },
    ));

    Ok(peer_connection)
}

fn create_video_track() -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: VIDEO_CLOCK_RATE as u32,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        "video".to_owned(),
        "rust-ffmpeg-next-video".to_owned(),
    ))
}

fn create_audio_track() -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: AUDIO_SAMPLE_RATE as u32,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        "audio".to_owned(),
        "rust-ffmpeg-next-audio".to_owned(),
    ))
}

const INDEX_HTML: &str = include_str!("../public/index.html");
const MAIN_CSS: &str = include_str!("../public/css/main.css");
const MAIN_JS: &str = include_str!("../public/js/webrtc.js");
const WEBTRANSPORT_HTML: &str = include_str!("../public/webtransport.html");
const WEBTRANSPORT_JS: &str = include_str!("../public/js/webtransport.js");
