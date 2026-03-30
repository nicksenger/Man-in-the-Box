mod program;
mod yuv;

use matroska_demuxer::{Frame, MatroskaFile, TrackEntry, TrackType};
use opus_decoder::OpusDecoder;
use re_rav1d::dav1d::{
    Decoder as Av1Decoder, Error as Av1DecodeError, Picture, PixelLayout, PlanarImageComponent,
};
use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};
use std::fs::File;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use symphonia::core::audio::{AudioBuffer, Channels, Layout, Signal, SignalSpec};
use symphonia::core::codecs::{CODEC_TYPE_OPUS, CodecParameters};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub use program::Program;
use yuv::Renderable;
pub use yuv::{Format, Size, Yuv};

const DEFAULT_MEDIA_FILE_NAME: &str = "mitb.mkv";
const DEFAULT_FRAME_DURATION_NS: u64 = 33_333_333;
const DEFAULT_AUDIO_SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone)]
pub enum AvEvent {
    VideoFrame(Yuv),
    PlaybackEnded,
    PlaybackError(String),
}

#[derive(Debug, Error)]
pub enum AvError {
    #[error("missing HOME for AV media path: {0}")]
    MissingHome(std::env::VarError),
    #[error("failed opening media file `{path}`: {source}")]
    OpenMedia {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("matroska demux error: {0}")]
    Demux(String),
    #[error("missing AV1 video track in media file")]
    MissingVideoTrack,
    #[error("failed to initialize AV1 decoder: {0}")]
    VideoDecoder(String),
    #[error("failed to initialize Opus decoder: {0}")]
    AudioDecoder(String),
    #[error("audio output channel count is invalid: {0}")]
    AudioChannels(usize),
    #[error("audio output sample rate is invalid: {0}")]
    AudioSampleRate(u32),
    #[error("failed initializing audio output: {0}")]
    AudioOutput(String),
}

#[derive(Debug)]
struct SelectedTracks {
    timestamp_scale_ns: u64,
    video_track: u64,
    video_default_duration_ns: Option<u64>,
    audio: Option<AudioTrack>,
}

#[derive(Debug)]
struct AudioTrack {
    track: u64,
    channels: usize,
    sample_rate: u32,
    pre_skip: usize,
}

#[derive(Debug, Clone, Copy)]
struct OpusHead {
    channels: usize,
    pre_skip: usize,
    sample_rate: u32,
    mapping_family: u8,
}

struct SymphoniaOpusDecoder {
    opus: OpusDecoder,
    codec_params: CodecParameters,
    buffer: AudioBuffer<f32>,
    scratch: Vec<f32>,
    channels: usize,
}

struct AudioOutput {
    _sink: MixerDeviceSink,
    player: Player,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
}

struct PlaybackClock {
    start: Instant,
    base_timestamp_ns: Option<u64>,
    synthetic_timestamp_ns: u64,
}

impl PlaybackClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            base_timestamp_ns: None,
            synthetic_timestamp_ns: 0,
        }
    }

    fn wait_until(&mut self, timestamp_ns: Option<u64>, frame_duration_ns: u64) {
        let target_ns = match timestamp_ns {
            Some(ts) => {
                self.synthetic_timestamp_ns = ts;
                ts
            }
            None => {
                self.synthetic_timestamp_ns = self
                    .synthetic_timestamp_ns
                    .saturating_add(frame_duration_ns.max(1));
                self.synthetic_timestamp_ns
            }
        };

        let Some(base) = self.base_timestamp_ns else {
            self.base_timestamp_ns = Some(target_ns);
            return;
        };

        let relative_ns = target_ns.saturating_sub(base);
        let target = self.start + Duration::from_nanos(relative_ns);
        if let Some(remaining) = target.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        }
    }
}

impl SymphoniaOpusDecoder {
    fn new(track: &AudioTrack) -> Result<Self, AvError> {
        let mut codec_params = CodecParameters::new();
        codec_params
            .for_codec(CODEC_TYPE_OPUS)
            .with_sample_rate(track.sample_rate)
            .with_channels(match track.channels {
                1 => Layout::Mono.into_channels(),
                _ => Layout::Stereo.into_channels(),
            })
            .with_time_base(symphonia::core::units::TimeBase::new(1, track.sample_rate));

        let opus = OpusDecoder::new(track.sample_rate, track.channels)
            .map_err(|error| AvError::AudioDecoder(error.to_string()))?;

        let spec = SignalSpec::new(
            track.sample_rate,
            match track.channels {
                1 => Channels::FRONT_LEFT,
                _ => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            },
        );
        let max_frame = opus.max_frame_size_per_channel() as u64;
        let buffer = AudioBuffer::<f32>::new(max_frame, spec);
        let scratch = vec![0.0_f32; opus.max_frame_size_per_channel() * track.channels];

        Ok(Self {
            opus,
            codec_params,
            buffer,
            scratch,
            channels: track.channels,
        })
    }

    fn decode_packet(&mut self, packet: &[u8]) -> Result<&AudioBuffer<f32>, AvError> {
        let max_samples = self.opus.max_frame_size_per_channel() * self.channels;
        if self.scratch.len() < max_samples {
            self.scratch.resize(max_samples, 0.0);
        }

        let samples_per_channel = self
            .opus
            .decode_float(packet, &mut self.scratch[..max_samples], false)
            .map_err(|error| AvError::AudioDecoder(error.to_string()))?;

        self.buffer.clear();
        self.buffer.render_reserved(Some(samples_per_channel));

        for channel in 0..self.channels {
            let channel_samples = self.buffer.chan_mut(channel);
            for (frame_index, sample) in channel_samples
                .iter_mut()
                .enumerate()
                .take(samples_per_channel)
            {
                let source_index = frame_index * self.channels + channel;
                *sample = self.scratch[source_index];
            }
        }

        Ok(&self.buffer)
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.codec_params
    }
}

impl AudioOutput {
    fn new(channels: usize, sample_rate: u32) -> Result<Self, AvError> {
        let channels = NonZeroU16::new(channels as u16).ok_or(AvError::AudioChannels(channels))?;
        let sample_rate =
            NonZeroU32::new(sample_rate).ok_or(AvError::AudioSampleRate(sample_rate))?;

        let sink = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| AvError::AudioOutput(error.to_string()))?;
        let player = Player::connect_new(sink.mixer());

        Ok(Self {
            _sink: sink,
            player,
            channels,
            sample_rate,
        })
    }

    fn append(&self, samples: Vec<f32>) {
        if samples.is_empty() {
            return;
        }

        let source = SamplesBuffer::new(self.channels, self.sample_rate, samples);
        self.player.append(source);
    }
}

pub fn spawn_default() -> Result<mpsc::UnboundedReceiver<AvEvent>, AvError> {
    let media_path = default_media_path()?;
    Ok(spawn(media_path))
}

pub fn spawn(media_path: PathBuf) -> mpsc::UnboundedReceiver<AvEvent> {
    let (tx, rx) = mpsc::unbounded_channel();

    thread::Builder::new()
        .name(String::from("mitb-av-playback"))
        .spawn(move || run_worker(media_path, tx))
        .map_err(|error| {
            warn!(%error, "failed to spawn AV worker thread");
        })
        .ok();

    rx
}

fn run_worker(media_path: PathBuf, tx: mpsc::UnboundedSender<AvEvent>) {
    debug!(path = %media_path.display(), "started AV worker");
    if !media_path.exists() {
        debug!(path = %media_path.display(), "AV media file not found; worker exiting");
        let _ = tx.send(AvEvent::PlaybackEnded);
        return;
    }

    let mut loop_index: u64 = 0;
    loop {
        loop_index = loop_index.saturating_add(1);
        debug!(
            path = %media_path.display(),
            loop_index,
            "starting AV playback loop"
        );

        match play_media_file(media_path.as_path(), &tx) {
            Ok(()) => {
                if tx.is_closed() {
                    break;
                }

                debug!(
                    path = %media_path.display(),
                    loop_index,
                    "completed AV playback loop"
                );
            }
            Err(error) => {
                warn!(%error, loop_index, "AV playback failed");
                let _ = tx.send(AvEvent::PlaybackError(error.to_string()));
                thread::sleep(Duration::from_secs(1));
            }
        }

        if tx.is_closed() {
            break;
        }
    }

    let _ = tx.send(AvEvent::PlaybackEnded);
}

fn play_media_file(path: &Path, tx: &mpsc::UnboundedSender<AvEvent>) -> Result<(), AvError> {
    let file = File::open(path).map_err(|source| AvError::OpenMedia {
        path: path.to_path_buf(),
        source,
    })?;

    let mut matroska =
        MatroskaFile::open(file).map_err(|error| AvError::Demux(error.to_string()))?;
    let tracks = select_tracks(&matroska)?;

    debug!(
        path = %path.display(),
        video_track = tracks.video_track,
        has_audio = tracks.audio.is_some(),
        "starting AV decode"
    );

    let mut video_decoder =
        Av1Decoder::new().map_err(|error| AvError::VideoDecoder(error.to_string()))?;

    let mut audio_decoder = match tracks.audio.as_ref() {
        Some(audio_track) => Some(SymphoniaOpusDecoder::new(audio_track)?),
        None => None,
    };

    if let Some(decoder) = audio_decoder.as_ref() {
        let params = decoder.codec_params();
        debug!(
            codec = %params.codec,
            sample_rate = params.sample_rate.unwrap_or(DEFAULT_AUDIO_SAMPLE_RATE),
            "configured symphonia-backed Opus decode pipeline"
        );
    }

    let mut audio_output = match tracks.audio.as_ref() {
        Some(audio_track) => {
            match AudioOutput::new(audio_track.channels, audio_track.sample_rate) {
                Ok(output) => Some(output),
                Err(error) => {
                    warn!(%error, "audio output unavailable; continuing video-only playback");
                    None
                }
            }
        }
        None => None,
    };

    let mut remaining_pre_skip = tracks.audio.as_ref().map_or(0, |audio| audio.pre_skip);
    let mut frame = Frame::default();
    let mut playback_clock = PlaybackClock::new();

    while matroska
        .next_frame(&mut frame)
        .map_err(|error| AvError::Demux(error.to_string()))?
    {
        if tx.is_closed() {
            return Ok(());
        }

        if frame.track == tracks.video_track {
            let timestamp_ns = frame.timestamp.saturating_mul(tracks.timestamp_scale_ns);
            let frame_duration_ns = frame
                .duration
                .map(|duration| duration.saturating_mul(tracks.timestamp_scale_ns))
                .or(tracks.video_default_duration_ns)
                .unwrap_or(DEFAULT_FRAME_DURATION_NS);

            submit_video_packet(
                &mut video_decoder,
                frame.data.clone(),
                timestamp_ns,
                frame_duration_ns,
                tx,
                &mut playback_clock,
            )?;
            continue;
        }

        if let Some(audio_track) = tracks.audio.as_ref()
            && frame.track == audio_track.track
            && let (Some(decoder), Some(output)) = (audio_decoder.as_mut(), audio_output.as_ref())
        {
            match decoder.decode_packet(frame.data.as_slice()) {
                Ok(buffer) => {
                    let samples = interleave_audio_buffer(
                        buffer,
                        audio_track.channels,
                        &mut remaining_pre_skip,
                    );
                    output.append(samples);
                }
                Err(error) => {
                    warn!(%error, "failed decoding Opus audio packet");
                }
            }
        }
    }

    drain_video_decoder(
        &mut video_decoder,
        tx,
        &mut playback_clock,
        tracks
            .video_default_duration_ns
            .unwrap_or(DEFAULT_FRAME_DURATION_NS),
    )?;

    if let Some(output) = audio_output.take() {
        output.player.sleep_until_end();
    }

    Ok(())
}

fn select_tracks(matroska: &MatroskaFile<File>) -> Result<SelectedTracks, AvError> {
    let timestamp_scale_ns = matroska.info().timestamp_scale().get();

    let mut video_track = None;
    let mut video_default_duration_ns = None;
    let mut audio_track = None;

    for track in matroska.tracks() {
        match track.track_type() {
            TrackType::Video if track.codec_id() == "V_AV1" => {
                if video_track.is_none() {
                    video_track = Some(track.track_number().get());
                    video_default_duration_ns =
                        track.default_duration().map(|duration| duration.get());
                }
            }
            TrackType::Audio if track.codec_id() == "A_OPUS" => {
                if audio_track.is_none() {
                    audio_track = parse_audio_track(track);
                }
            }
            _ => {}
        }
    }

    let video_track = video_track.ok_or(AvError::MissingVideoTrack)?;

    Ok(SelectedTracks {
        timestamp_scale_ns,
        video_track,
        video_default_duration_ns,
        audio: audio_track,
    })
}

fn parse_audio_track(track: &TrackEntry) -> Option<AudioTrack> {
    let audio = track.audio()?;

    let mut channels = audio.channels().get() as usize;
    let mut pre_skip = 0_usize;
    let mut sample_rate = audio
        .output_sampling_frequency()
        .or_else(|| Some(audio.sampling_frequency()))
        .unwrap_or(DEFAULT_AUDIO_SAMPLE_RATE as f64) as u32;

    if let Some(opus_head) = parse_opus_head(track.codec_private()) {
        channels = opus_head.channels.max(1);
        pre_skip = opus_head.pre_skip;
        if opus_head.sample_rate > 0 {
            sample_rate = opus_head.sample_rate;
        }

        if opus_head.mapping_family != 0 && channels > 2 {
            warn!(
                mapping_family = opus_head.mapping_family,
                channels, "unsupported Opus mapping family for this build; skipping audio playback"
            );
            return None;
        }
    }

    if channels == 0 || channels > 2 {
        warn!(
            channels,
            "unsupported Opus channel count for this build; skipping audio playback"
        );
        return None;
    }

    Some(AudioTrack {
        track: track.track_number().get(),
        channels,
        sample_rate: sample_rate.max(DEFAULT_AUDIO_SAMPLE_RATE),
        pre_skip,
    })
}

fn parse_opus_head(codec_private: Option<&[u8]>) -> Option<OpusHead> {
    let data = codec_private?;
    if data.len() < 19 || &data[..8] != b"OpusHead" {
        return None;
    }

    let channels = data.get(9).copied().unwrap_or(1) as usize;
    let pre_skip = u16::from_le_bytes([data[10], data[11]]) as usize;
    let sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let mapping_family = data[18];

    Some(OpusHead {
        channels,
        pre_skip,
        sample_rate,
        mapping_family,
    })
}

fn interleave_audio_buffer(
    buffer: &AudioBuffer<f32>,
    channels: usize,
    remaining_pre_skip: &mut usize,
) -> Vec<f32> {
    let frames = buffer.frames();
    if frames == 0 {
        return Vec::new();
    }

    let skip_frames = (*remaining_pre_skip).min(frames);
    *remaining_pre_skip = remaining_pre_skip.saturating_sub(skip_frames);

    if skip_frames >= frames {
        return Vec::new();
    }

    let output_frames = frames - skip_frames;
    let mut output = vec![0.0_f32; output_frames * channels];

    for frame_index in skip_frames..frames {
        let output_index = frame_index - skip_frames;
        for channel in 0..channels {
            if let Some(sample) = buffer.chan(channel).get(frame_index) {
                output[output_index * channels + channel] = *sample;
            }
        }
    }

    output
}

fn submit_video_packet(
    decoder: &mut Av1Decoder,
    data: Vec<u8>,
    timestamp_ns: u64,
    frame_duration_ns: u64,
    tx: &mpsc::UnboundedSender<AvEvent>,
    playback_clock: &mut PlaybackClock,
) -> Result<(), AvError> {
    match decoder.send_data(
        data,
        None,
        Some(timestamp_ns_to_i64(timestamp_ns)),
        Some(timestamp_ns_to_i64(frame_duration_ns)),
    ) {
        Ok(()) | Err(Av1DecodeError::Again) => {}
        Err(error) => {
            return Err(AvError::VideoDecoder(error.to_string()));
        }
    }

    drain_video_decoder(decoder, tx, playback_clock, frame_duration_ns)?;

    loop {
        match decoder.send_pending_data() {
            Ok(()) => break,
            Err(Av1DecodeError::Again) => {
                drain_video_decoder(decoder, tx, playback_clock, frame_duration_ns)?;
            }
            Err(error) => {
                return Err(AvError::VideoDecoder(error.to_string()));
            }
        }
    }

    drain_video_decoder(decoder, tx, playback_clock, frame_duration_ns)?;

    Ok(())
}

fn drain_video_decoder(
    decoder: &mut Av1Decoder,
    tx: &mpsc::UnboundedSender<AvEvent>,
    playback_clock: &mut PlaybackClock,
    default_frame_duration_ns: u64,
) -> Result<(), AvError> {
    loop {
        let picture = match decoder.get_picture() {
            Ok(picture) => picture,
            Err(Av1DecodeError::Again) => break,
            Err(error) => return Err(AvError::VideoDecoder(error.to_string())),
        };

        let frame_duration_ns = if picture.duration() > 0 {
            picture.duration() as u64
        } else {
            default_frame_duration_ns
        };

        let timestamp_ns = picture
            .timestamp()
            .and_then(|timestamp| u64::try_from(timestamp).ok());
        playback_clock.wait_until(timestamp_ns, frame_duration_ns);

        if let Some(yuv) = picture_to_yuv(&picture)
            && tx.send(AvEvent::VideoFrame(yuv)).is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

fn picture_to_yuv(picture: &Picture) -> Option<Yuv> {
    if picture.bit_depth() != 8 {
        warn!(
            bit_depth = picture.bit_depth(),
            "unsupported AV1 bit depth for overlay"
        );
        return None;
    }

    let width = picture.width();
    let height = picture.height();
    let dimensions = (width, height).into();

    match picture.pixel_layout() {
        PixelLayout::I420 => {
            let y = copy_plane(
                picture.plane(PlanarImageComponent::Y).as_ref(),
                picture.stride(PlanarImageComponent::Y),
                width,
                height,
            )?;
            let chroma_width = width.div_ceil(2);
            let chroma_height = height.div_ceil(2);
            let u = copy_plane(
                picture.plane(PlanarImageComponent::U).as_ref(),
                picture.stride(PlanarImageComponent::U),
                chroma_width,
                chroma_height,
            )?;
            let v = copy_plane(
                picture.plane(PlanarImageComponent::V).as_ref(),
                picture.stride(PlanarImageComponent::V),
                chroma_width,
                chroma_height,
            )?;

            let mut data = Vec::with_capacity(y.len() + u.len() + v.len());
            data.extend_from_slice(y.as_slice());
            data.extend_from_slice(u.as_slice());
            data.extend_from_slice(v.as_slice());

            Some(Yuv {
                format: Format::I420,
                data,
                dimensions,
            })
        }
        PixelLayout::I444 => {
            let y = copy_plane(
                picture.plane(PlanarImageComponent::Y).as_ref(),
                picture.stride(PlanarImageComponent::Y),
                width,
                height,
            )?;
            let u = copy_plane(
                picture.plane(PlanarImageComponent::U).as_ref(),
                picture.stride(PlanarImageComponent::U),
                width,
                height,
            )?;
            let v = copy_plane(
                picture.plane(PlanarImageComponent::V).as_ref(),
                picture.stride(PlanarImageComponent::V),
                width,
                height,
            )?;

            let mut data = Vec::with_capacity(y.len() + u.len() + v.len());
            data.extend_from_slice(y.as_slice());
            data.extend_from_slice(u.as_slice());
            data.extend_from_slice(v.as_slice());

            Some(Yuv {
                format: Format::Y444,
                data,
                dimensions,
            })
        }
        layout => {
            warn!(?layout, "unsupported AV1 pixel layout for overlay");
            None
        }
    }
}

fn copy_plane(plane: &[u8], stride: u32, width: u32, height: u32) -> Option<Vec<u8>> {
    let stride = usize::try_from(stride).ok()?;
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;

    if stride < width {
        return None;
    }

    let required = stride.checked_mul(height)?;
    if plane.len() < required {
        return None;
    }

    let mut output = Vec::with_capacity(width.checked_mul(height)?);
    for row in 0..height {
        let start = row.checked_mul(stride)?;
        let end = start.checked_add(width)?;
        output.extend_from_slice(plane.get(start..end)?);
    }

    Some(output)
}

fn timestamp_ns_to_i64(value: u64) -> i64 {
    if value > i64::MAX as u64 {
        i64::MAX
    } else {
        value as i64
    }
}

pub fn default_media_path() -> Result<PathBuf, AvError> {
    let home = std::env::var("HOME").map_err(AvError::MissingHome)?;
    let mut path = PathBuf::from(home);
    path.push(".mitb");
    path.push(DEFAULT_MEDIA_FILE_NAME);
    Ok(path)
}
