const status = document.getElementById("status");
const startButton = document.getElementById("start-session");
const videoFrames = document.getElementById("video-frames");
const audioFrames = document.getElementById("audio-frames");
const byteCount = document.getElementById("bytes");
const lastPts = document.getElementById("last-pts");
const canvas = document.getElementById("video-canvas");
const ctx = canvas.getContext("2d");

let videoFrameCount = 0;
let audioFrameCount = 0;
let totalBytes = 0;
let videoDecoder = null;
let audioDecoder = null;
let audioContext = null;
let nextAudioTime = 0;

startButton.onclick = async () => {
    try {
        if (!("WebTransport" in window)) {
            throw new Error("WebTransport is not available in this browser");
        }
        if (!("VideoDecoder" in window)) {
            throw new Error("WebCodecs VideoDecoder is not available in this browser");
        }
        if (!("AudioDecoder" in window)) {
            throw new Error("WebCodecs AudioDecoder is not available in this browser");
        }

        startButton.disabled = true;
        status.textContent = "Loading config";
        await startAudioOutput();

        const config = await (await fetch("/webtransport-config")).json();
        if (config.error) {
            throw new Error(config.error);
        }

        const certHash = new Uint8Array(config.serverCertificateHash);
        const transport = new WebTransport(config.url, {
            serverCertificateHashes: [
                { algorithm: "sha-256", value: certHash.buffer },
            ],
        });

        status.textContent = "Connecting";
        await transport.ready;
        status.textContent = "Connected";

        transport.closed
            .then(() => {
                status.textContent = "Closed";
            })
            .catch((error) => {
                status.textContent = `Closed: ${error.message}`;
            });

        await readIncomingStreams(transport);
    } catch (error) {
        status.textContent = error.message;
        startButton.disabled = false;
    }
};

async function readIncomingStreams(transport) {
    const reader = transport.incomingUnidirectionalStreams.getReader();

    while (true) {
        const { value, done } = await reader.read();
        if (done) {
            return;
        }
        readMediaStream(value).catch((error) => {
            status.textContent = `Stream error: ${error.message}`;
        });
    }
}

async function readMediaStream(stream) {
    const reader = stream.getReader();
    let buffer = new Uint8Array(0);
    let magicSeen = false;

    while (true) {
        const { value, done } = await reader.read();
        if (done) {
            await flushDecoders();
            status.textContent = "Stream complete";
            return;
        }

        buffer = concat(buffer, value);
        if (!magicSeen) {
            if (buffer.length < 8) {
                continue;
            }
            const magic = new TextDecoder().decode(buffer.slice(0, 8));
            if (magic !== "WTMEDIA1") {
                throw new Error("invalid media stream header");
            }
            buffer = buffer.slice(8);
            magicSeen = true;
        }

        while (buffer.length >= 21) {
            const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
            const kind = view.getUint8(0);
            const ptsUs = Number(view.getBigInt64(1));
            const durationUs = Number(view.getBigInt64(9));
            const payloadLength = view.getUint32(17);
            const frameLength = 21 + payloadLength;
            if (buffer.length < frameLength) {
                break;
            }

            const payload = buffer.slice(21, frameLength);
            buffer = buffer.slice(frameLength);
            await handleFrame(kind, ptsUs, durationUs, payload);
        }
    }
}

async function handleFrame(kind, ptsUs, durationUs, payload) {
    updateStats(kind, ptsUs, payload.byteLength);

    if (kind === 0) {
        await configureDecoders(JSON.parse(new TextDecoder().decode(payload)));
    } else if (kind === 1 && videoDecoder) {
        decodeVideoChunk(ptsUs, durationUs, payload);
    } else if (kind === 2 && audioDecoder) {
        decodeAudioChunk(ptsUs, durationUs, payload);
    }
}

async function configureDecoders(metadata) {
    await configureVideoDecoder(metadata.video);
    await configureAudioDecoder(metadata.audio);
}

async function configureVideoDecoder(video) {
    if (!video || !video.width || !video.height) {
        return;
    }

    canvas.width = video.width;
    canvas.height = video.height;

    const config = {
        codec: video.codec,
        codedWidth: video.width,
        codedHeight: video.height,
        optimizeForLatency: true,
    };
    const support = await VideoDecoder.isConfigSupported(config);
    if (!support.supported) {
        throw new Error(`VideoDecoder does not support ${video.codec}`);
    }

    videoDecoder = new VideoDecoder({
        output: (frame) => {
            ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
            frame.close();
        },
        error: (error) => {
            status.textContent = `VideoDecoder: ${error.message}`;
        },
    });
    videoDecoder.configure(config);
}

async function configureAudioDecoder(audio) {
    if (!audio || !audio.sampleRate || !audio.channels) {
        return;
    }

    const config = {
        codec: audio.codec,
        sampleRate: audio.sampleRate,
        numberOfChannels: audio.channels,
    };
    const support = await AudioDecoder.isConfigSupported(config);
    if (!support.supported) {
        throw new Error(`AudioDecoder does not support ${audio.codec}`);
    }

    audioDecoder = new AudioDecoder({
        output: playAudioData,
        error: (error) => {
            status.textContent = `AudioDecoder: ${error.message}`;
        },
    });
    audioDecoder.configure(config);
}

function decodeVideoChunk(ptsUs, durationUs, payload) {
    const chunk = new EncodedVideoChunk({
        type: isH264Keyframe(payload) ? "key" : "delta",
        timestamp: Math.max(0, ptsUs),
        duration: Math.max(1, durationUs),
        data: payload,
    });
    videoDecoder.decode(chunk);
}

function decodeAudioChunk(ptsUs, durationUs, payload) {
    const chunk = new EncodedAudioChunk({
        type: "key",
        timestamp: Math.max(0, ptsUs),
        duration: Math.max(1, durationUs),
        data: payload,
    });
    audioDecoder.decode(chunk);
}

async function flushDecoders() {
    const flushes = [];
    if (videoDecoder) {
        flushes.push(videoDecoder.flush());
    }
    if (audioDecoder) {
        flushes.push(audioDecoder.flush());
    }
    await Promise.allSettled(flushes);
}

async function startAudioOutput() {
    if (!audioContext) {
        audioContext = new AudioContext({ sampleRate: 48000 });
    }
    if (audioContext.state !== "running") {
        await audioContext.resume();
    }
    nextAudioTime = Math.max(nextAudioTime, audioContext.currentTime + 0.08);
}

function playAudioData(audioData) {
    try {
        if (!audioContext) {
            return;
        }

        const buffer = audioContext.createBuffer(
            audioData.numberOfChannels,
            audioData.numberOfFrames,
            audioData.sampleRate,
        );

        for (let channel = 0; channel < audioData.numberOfChannels; channel += 1) {
            const target = buffer.getChannelData(channel);
            try {
                audioData.copyTo(target, { planeIndex: channel, format: "f32-planar" });
            } catch (_) {
                audioData.copyTo(target, { planeIndex: channel });
            }
        }

        const source = audioContext.createBufferSource();
        source.buffer = buffer;
        source.connect(audioContext.destination);

        const leadTime = 0.08;
        const startAt = Math.max(audioContext.currentTime + leadTime, nextAudioTime);
        source.start(startAt);
        nextAudioTime = startAt + buffer.duration;
    } finally {
        audioData.close();
    }
}

function isH264Keyframe(data) {
    for (const nalType of h264NalTypes(data)) {
        if (nalType === 5) {
            return true;
        }
    }
    return false;
}

function* h264NalTypes(data) {
    let i = 0;
    while (i + 4 < data.length) {
        let start = -1;
        let prefixLength = 0;
        for (; i + 4 < data.length; i += 1) {
            if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
                start = i + 3;
                prefixLength = 3;
                break;
            }
            if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 0 && data[i + 3] === 1) {
                start = i + 4;
                prefixLength = 4;
                break;
            }
        }
        if (start < 0) {
            return;
        }
        if (start < data.length) {
            yield data[start] & 0x1f;
        }
        i = start + prefixLength;
    }
}

function updateStats(kind, ptsUs, bytes) {
    if (kind === 1) {
        videoFrameCount += 1;
        videoFrames.textContent = String(videoFrameCount);
    } else if (kind === 2) {
        audioFrameCount += 1;
        audioFrames.textContent = String(audioFrameCount);
    }

    totalBytes += bytes;
    byteCount.textContent = String(totalBytes);
    lastPts.textContent = ptsUs >= 0 ? `${Math.round(ptsUs / 1000)} ms` : "-";
}

function concat(left, right) {
    const out = new Uint8Array(left.length + right.length);
    out.set(left, 0);
    out.set(right, left.length);
    return out;
}
