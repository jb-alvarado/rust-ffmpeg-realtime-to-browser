use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use ffmpeg::{
    Dictionary, Packet, Rational, Rescale, codec, encoder, filter, format, frame, media, picture,
    rescale,
    software::scaling,
    util::{
        channel_layout::ChannelLayout,
        format::{
            pixel::Pixel,
            sample::{Sample as SampleFormat, Type as SampleType},
        },
    },
};
use ffmpeg_next as ffmpeg;
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::clock::MediaClock;

pub(crate) const VIDEO_CLOCK_RATE: i32 = 90_000;
pub(crate) const AUDIO_SAMPLE_RATE: i32 = 48_000;

const DEFAULT_VIDEO_FRAME_DURATION_US: i64 = 33_333;
const DEFAULT_AUDIO_PACKET_DURATION_US: i64 = 20_000;

pub(crate) async fn stream_media_file(
    input: &str,
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
) -> Result<()> {
    let mut reader = MediaReader::open(input)?;
    let mut clock = MediaClock::new();

    while let Some(batch) = reader.next_batch()? {
        match batch {
            EncodedBatch::Video(samples) => {
                write_samples(&mut clock, &video_track, samples).await?
            }
            EncodedBatch::Audio(samples) => {
                write_samples(&mut clock, &audio_track, samples).await?
            }
        }
    }

    let (video_samples, audio_samples) = reader.finish()?;
    write_samples(&mut clock, &video_track, video_samples).await?;
    write_samples(&mut clock, &audio_track, audio_samples).await?;

    Ok(())
}

pub(crate) fn dry_run_media_file(input: &str) -> Result<()> {
    let mut reader = MediaReader::open(input)?;
    let mut video_count = 0_usize;
    let mut audio_count = 0_usize;

    while let Some(batch) = reader.next_batch()? {
        match batch {
            EncodedBatch::Video(samples) => video_count += samples.len(),
            EncodedBatch::Audio(samples) => audio_count += samples.len(),
        }
    }

    let (video_samples, audio_samples) = reader.finish()?;
    video_count += video_samples.len();
    audio_count += audio_samples.len();

    eprintln!("dry-run ok: encoded {video_count} video samples, {audio_count} audio samples");
    Ok(())
}

pub(crate) struct MediaReader {
    ictx: format::context::Input,
    video: Option<VideoTranscoder>,
    audio: Option<AudioTranscoder>,
    video_index: Option<usize>,
    audio_index: Option<usize>,
}

impl MediaReader {
    pub(crate) fn open(input: &str) -> Result<Self> {
        let ictx = format::input(input).with_context(|| format!("failed to open {input}"))?;
        let mut reader = Self {
            ictx,
            video: None,
            audio: None,
            video_index: None,
            audio_index: None,
        };

        if let Some(stream) = reader.ictx.streams().best(media::Type::Video) {
            let transcoder = VideoTranscoder::new(&stream)?;
            reader.video_index = Some(transcoder.stream_index);
            reader.video = Some(transcoder);
        }
        if let Some(stream) = reader.ictx.streams().best(media::Type::Audio) {
            let transcoder = AudioTranscoder::new(&stream)?;
            reader.audio_index = Some(transcoder.stream_index);
            reader.audio = Some(transcoder);
        }
        if reader.video.is_none() && reader.audio.is_none() {
            bail!("{input} contains no audio or video stream");
        }

        Ok(reader)
    }

    pub(crate) fn next_batch(&mut self) -> Result<Option<EncodedBatch>> {
        loop {
            let Some((stream, mut packet)) = self.ictx.packets().next() else {
                return Ok(None);
            };
            if Some(stream.index()) == self.video_index {
                if let Some(video) = self.video.as_mut() {
                    packet.rescale_ts(stream.time_base(), video.decoder_time_base);
                    return Ok(Some(EncodedBatch::Video(video.push_packet(&packet)?)));
                }
            } else if Some(stream.index()) == self.audio_index
                && let Some(audio) = self.audio.as_mut()
            {
                packet.rescale_ts(stream.time_base(), audio.decoder_time_base);
                return Ok(Some(EncodedBatch::Audio(audio.push_packet(&packet)?)));
            }
        }
    }

    pub(crate) fn finish(&mut self) -> Result<(Vec<EncodedSample>, Vec<EncodedSample>)> {
        let video_samples = match self.video.as_mut() {
            Some(video) => video.finish()?,
            None => Vec::new(),
        };
        let audio_samples = match self.audio.as_mut() {
            Some(audio) => audio.finish()?,
            None => Vec::new(),
        };

        Ok((video_samples, audio_samples))
    }

    pub(crate) fn video_size(&self) -> Option<(u32, u32)> {
        self.video
            .as_ref()
            .map(|video| (video.encoder.width(), video.encoder.height()))
    }
}

pub(crate) enum EncodedBatch {
    Video(Vec<EncodedSample>),
    Audio(Vec<EncodedSample>),
}

async fn write_samples(
    clock: &mut MediaClock,
    track: &TrackLocalStaticSample,
    samples: Vec<EncodedSample>,
) -> Result<()> {
    for sample in samples {
        clock.wait_until(sample.pts_us).await;
        track
            .write_sample(&Sample {
                data: Bytes::from(sample.data),
                duration: Duration::from_micros(sample.duration_us.max(1) as u64),
                ..Default::default()
            })
            .await
            .context("failed to write WebRTC sample")?;
    }

    Ok(())
}

pub(crate) struct EncodedSample {
    pub(crate) pts_us: Option<i64>,
    pub(crate) duration_us: i64,
    pub(crate) data: Vec<u8>,
}

struct VideoTranscoder {
    stream_index: usize,
    decoder: codec::decoder::Video,
    decoder_time_base: Rational,
    encoder: codec::encoder::video::Encoder,
    encoder_time_base: Rational,
    scaler: scaling::Context,
    next_pts: i64,
    frame_duration: i64,
}

impl VideoTranscoder {
    fn new(stream: &format::stream::Stream) -> Result<Self> {
        let decoder = codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .video()
            .context("failed to open video decoder")?;

        let width = make_even(decoder.width());
        let height = make_even(decoder.height());
        let codec = encoder::find_by_name("libx264")
            .or_else(|| encoder::find(codec::Id::H264))
            .ok_or_else(|| anyhow!("no H264 encoder found; install FFmpeg with libx264 support"))?;
        let mut encoder = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;

        let encoder_time_base = Rational(1, VIDEO_CLOCK_RATE);
        let frame_rate = usable_rate(stream.avg_frame_rate())
            .or_else(|| usable_rate(stream.rate()))
            .or_else(|| decoder.frame_rate().and_then(usable_rate))
            .unwrap_or(Rational(30, 1));
        let frame_duration = (i64::from(VIDEO_CLOCK_RATE) * i64::from(frame_rate.denominator())
            / i64::from(frame_rate.numerator()))
        .max(1);

        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(Pixel::YUV420P);
        encoder.set_time_base(encoder_time_base);
        encoder.set_frame_rate(Some(frame_rate));
        encoder.set_bit_rate(2_000_000);
        encoder.set_max_b_frames(0);

        let mut options = Dictionary::new();
        options.set("preset", "ultrafast");
        options.set("tune", "zerolatency");
        options.set("profile", "baseline");
        options.set("level", "3.1");
        options.set("repeat-headers", "1");
        options.set("annexb", "1");

        let encoder = encoder
            .open_with(options)
            .context("failed to open H264 encoder")?;
        let scaler = scaling::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::YUV420P,
            width,
            height,
            scaling::flag::Flags::BILINEAR,
        )
        .context("failed to create video scaler")?;

        Ok(Self {
            stream_index: stream.index(),
            decoder,
            decoder_time_base: stream.time_base(),
            encoder,
            encoder_time_base,
            scaler,
            next_pts: 0,
            frame_duration,
        })
    }

    fn push_packet(&mut self, packet: &Packet) -> Result<Vec<EncodedSample>> {
        self.decoder.send_packet(packet)?;
        self.receive_frames()
    }

    fn finish(&mut self) -> Result<Vec<EncodedSample>> {
        self.decoder.send_eof()?;
        let mut samples = self.receive_frames()?;
        self.encoder.send_eof()?;
        samples.extend(self.receive_packets()?);
        Ok(samples)
    }

    fn receive_frames(&mut self) -> Result<Vec<EncodedSample>> {
        let mut samples = Vec::new();
        let mut decoded = frame::Video::empty();

        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let pts = decoded
                .timestamp()
                .or_else(|| decoded.pts())
                .map(|pts| pts.rescale(self.decoder_time_base, self.encoder_time_base))
                .unwrap_or(self.next_pts);
            self.next_pts = pts + self.frame_duration;

            let mut scaled =
                frame::Video::new(Pixel::YUV420P, self.encoder.width(), self.encoder.height());
            self.scaler.run(&decoded, &mut scaled)?;
            scaled.set_pts(Some(pts));
            scaled.set_kind(picture::Type::None);

            self.encoder.send_frame(&scaled)?;
            samples.extend(self.receive_packets()?);
        }

        Ok(samples)
    }

    fn receive_packets(&mut self) -> Result<Vec<EncodedSample>> {
        let mut samples = Vec::new();
        let mut packet = Packet::empty();

        while self.encoder.receive_packet(&mut packet).is_ok() {
            samples.push(packet_to_sample(
                &packet,
                self.encoder_time_base,
                DEFAULT_VIDEO_FRAME_DURATION_US,
            ));
        }

        Ok(samples)
    }
}

struct AudioTranscoder {
    stream_index: usize,
    decoder: codec::decoder::Audio,
    decoder_time_base: Rational,
    encoder: codec::encoder::audio::Encoder,
    encoder_time_base: Rational,
    filter: filter::Graph,
}

impl AudioTranscoder {
    fn new(stream: &format::stream::Stream) -> Result<Self> {
        let mut decoder = codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()
            .context("failed to open audio decoder")?;
        decoder.set_parameters(stream.parameters())?;

        let audio_codec = encoder::find_by_name("libopus")
            .or_else(|| encoder::find(codec::Id::OPUS))
            .ok_or_else(|| anyhow!("no Opus encoder found; install FFmpeg with libopus support"))?
            .audio()?;
        let mut encoder = codec::context::Context::new_with_codec(*audio_codec)
            .encoder()
            .audio()?;

        encoder.set_rate(AUDIO_SAMPLE_RATE);
        encoder.set_channel_layout(ChannelLayout::STEREO);
        let sample_format = choose_opus_sample_format(&audio_codec)?;
        encoder.set_format(sample_format);
        encoder.set_bit_rate(96_000);
        encoder.set_time_base((1, AUDIO_SAMPLE_RATE));

        let mut options = Dictionary::new();
        options.set("application", "audio");
        options.set("frame_duration", "20");

        let encoder = encoder
            .open_as_with(audio_codec, options)
            .context("failed to open Opus encoder")?;
        let encoder_time_base = Rational(1, AUDIO_SAMPLE_RATE);
        let filter = create_audio_filter(&decoder, &encoder, sample_format)?;

        Ok(Self {
            stream_index: stream.index(),
            decoder,
            decoder_time_base: stream.time_base(),
            encoder,
            encoder_time_base,
            filter,
        })
    }

    fn push_packet(&mut self, packet: &Packet) -> Result<Vec<EncodedSample>> {
        self.decoder.send_packet(packet)?;
        self.receive_frames()
    }

    fn finish(&mut self) -> Result<Vec<EncodedSample>> {
        self.decoder.send_eof()?;
        let mut samples = self.receive_frames()?;
        self.filter.get("in").unwrap().source().flush()?;
        samples.extend(self.receive_filtered_frames()?);
        self.encoder.send_eof()?;
        samples.extend(self.receive_packets()?);
        Ok(samples)
    }

    fn receive_frames(&mut self) -> Result<Vec<EncodedSample>> {
        let mut samples = Vec::new();
        let mut decoded = frame::Audio::empty();

        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = decoded.timestamp();
            decoded.set_pts(timestamp);
            self.filter.get("in").unwrap().source().add(&decoded)?;
            samples.extend(self.receive_filtered_frames()?);
        }

        Ok(samples)
    }

    fn receive_filtered_frames(&mut self) -> Result<Vec<EncodedSample>> {
        let mut samples = Vec::new();
        let mut filtered = frame::Audio::empty();

        while self
            .filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut filtered)
            .is_ok()
        {
            self.encoder.send_frame(&filtered)?;
            samples.extend(self.receive_packets()?);
        }

        Ok(samples)
    }

    fn receive_packets(&mut self) -> Result<Vec<EncodedSample>> {
        let mut samples = Vec::new();
        let mut packet = Packet::empty();

        while self.encoder.receive_packet(&mut packet).is_ok() {
            samples.push(packet_to_sample(
                &packet,
                self.encoder_time_base,
                DEFAULT_AUDIO_PACKET_DURATION_US,
            ));
        }

        Ok(samples)
    }
}

fn create_audio_filter(
    decoder: &codec::decoder::Audio,
    encoder: &codec::encoder::Audio,
    sample_format: SampleFormat,
) -> Result<filter::Graph> {
    let mut graph = filter::Graph::new();
    let input_channel_layout = if decoder.channel_layout().bits() == 0 {
        ChannelLayout::default(i32::from(decoder.channels()))
    } else {
        decoder.channel_layout()
    };
    let input_args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.time_base(),
        decoder.rate(),
        decoder.format().name(),
        input_channel_layout.bits()
    );

    graph.add(&filter::find("abuffer").unwrap(), "in", &input_args)?;
    graph.add(&filter::find("abuffersink").unwrap(), "out", "")?;

    graph
        .output("in", 0)?
        .input("out", 0)?
        .parse(&format!(
            "aresample=48000,aformat=channel_layouts=stereo,pan=stereo|c0<FL+FR|c1<FL+FR,volume=0.5,aformat=sample_fmts={}:channel_layouts=stereo",
            sample_format.name()
        ))?;
    graph.validate()?;

    if let Some(codec) = encoder.codec()
        && !codec
            .capabilities()
            .contains(codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE)
    {
        graph
            .get("out")
            .unwrap()
            .sink()
            .set_frame_size(encoder.frame_size());
    }

    Ok(graph)
}

fn choose_opus_sample_format(codec: &codec::audio::Audio) -> Result<SampleFormat> {
    let formats = codec
        .formats()
        .ok_or_else(|| anyhow!("Opus encoder reports no supported sample formats"))?;
    let formats: Vec<_> = formats.collect();

    [
        SampleFormat::I16(SampleType::Packed),
        SampleFormat::F32(SampleType::Packed),
        SampleFormat::F32(SampleType::Planar),
    ]
    .into_iter()
    .find(|preferred| formats.contains(preferred))
    .or_else(|| formats.first().copied())
    .ok_or_else(|| anyhow!("Opus encoder reports no supported sample formats"))
}

fn packet_to_sample(
    packet: &Packet,
    time_base: Rational,
    default_duration_us: i64,
) -> EncodedSample {
    let pts_us = packet
        .pts()
        .or_else(|| packet.dts())
        .map(|pts| pts.rescale(time_base, rescale::TIME_BASE));
    let duration_us = if packet.duration() > 0 {
        packet.duration().rescale(time_base, rescale::TIME_BASE)
    } else {
        default_duration_us
    };

    EncodedSample {
        pts_us,
        duration_us,
        data: packet.data().map_or_else(Vec::new, ToOwned::to_owned),
    }
}

fn make_even(value: u32) -> u32 {
    value.max(2) & !1
}

fn usable_rate(rate: Rational) -> Option<Rational> {
    (rate.numerator() > 0 && rate.denominator() > 0).then_some(rate)
}
