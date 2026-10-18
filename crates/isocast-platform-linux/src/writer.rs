//! Media files: libavcodec encoders into the crash-safe fragmented movie
//! writer (`isocast-mux`), one fragment every 500 ms.
//!
//! Video encodes on a thread of its own per file, fed through a short
//! queue, so a slow picture never holds up the engine's output thread (as
//! with macOS's asynchronous encoders). Audio is cheap and encodes on the
//! caller's thread. Both reach the movie through one lock.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{FileWriter, Finished, Time, WriterSpec};
use isocast_mux::{AudioConfig, Config, Framing, VideoCodec, VideoConfig, VideoPacket};
use isocast_protocol::{Codec, Container};

use crate::codec_config::{avc_config, hevc_config};
use crate::encode::{self, AudioEncoder, VideoEncoder};
use crate::frames::Frame;

/// Audio more than this far from where the track has got to (10 ms at
/// 48 kHz) is a real gap or overlap, not clock jitter.
const AUDIO_SLACK: u64 = 480;

const SAMPLE_RATE: i128 = 48_000;

/// Encoder ticks per frame: the timescale is `fps × FRAME_TICKS`.
const FRAME_TICKS: u32 = 1000;

/// Pictures waiting for the video encoder before new ones are dropped.
const QUEUE: usize = 4;

/// How long `append_video` waits for room in a full queue.
const WAIT: Duration = Duration::from_millis(8);

struct Audio {
    encoder: AudioEncoder,
    channels: usize,
    /// Samples handed to the encoder: where the track has got to.
    written: u64,
}

/// What the writer and its video thread share.
struct Shared {
    mux: Mutex<Option<isocast_mux::Writer>>,
    /// The video thread's first error, reported by the next call.
    failed: Mutex<Option<String>>,
    queued: AtomicUsize,
}

impl Shared {
    fn with_mux<T>(&self, f: impl FnOnce(&mut isocast_mux::Writer) -> Result<T>) -> Result<T> {
        let mut mux = self
            .mux
            .lock()
            .map_err(|_| anyhow!("the movie writer panicked"))?;
        f(mux.as_mut().context("the file is finished")?)
    }

    fn check(&self) -> Result<()> {
        match &*self
            .failed
            .lock()
            .map_err(|_| anyhow!("the video encoder panicked"))?
        {
            Some(error) => bail!("{error}"),
            None => Ok(()),
        }
    }
}

pub struct Writer {
    path: PathBuf,
    shared: Arc<Shared>,
    frames: Option<SyncSender<(Frame, i64)>>,
    video: Option<JoinHandle<()>>,
    audio: Vec<Audio>,
    start: Option<Time>,
    timescale: u32,
    last_tick: Option<i64>,
}

impl Writer {
    pub fn create(spec: &WriterSpec) -> Result<Self> {
        if !matches!(spec.container, Container::Mov | Container::Mp4) {
            bail!(
                "container: Linux records .mov and .mp4 only, not {:?}",
                spec.container
            );
        }
        let encoder = &spec.video.encoder;
        let (codec, mux_codec) = match encoder.codec {
            Codec::H264 => (encode::Codec::H264, VideoCodec::H264),
            Codec::Hevc => (encode::Codec::Hevc, VideoCodec::Hevc),
            other => bail!("encoder.codec: {other:?} isn't available on Linux; use h264 or hevc"),
        };
        if encoder.bit_depth != 8 {
            bail!("encoder.bit_depth: Linux records 8-bit video only so far");
        }
        let (width, height, fps) = (spec.video.width, spec.video.height, spec.video.fps.max(1));
        let timescale = fps * FRAME_TICKS;
        let video = VideoEncoder::new(
            codec,
            width,
            height,
            fps,
            timescale,
            encode::Rate::Quality(encoder.quality.value()),
        )?;
        let sets = video.parameter_sets()?;
        let nals: Vec<&[u8]> = sets.iter().map(Vec::as_slice).collect();
        let decoder_config = match codec {
            encode::Codec::H264 => avc_config(&nals),
            encode::Codec::Hevc => hevc_config(&nals),
        }
        .with_context(|| format!("{}'s parameter sets", video.name))?;
        let audio = spec
            .audio
            .iter()
            .map(|track| {
                Ok(Audio {
                    encoder: AudioEncoder::new(track.channels, None)?,
                    channels: track.channels as usize,
                    written: 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let audio_configs: Vec<AudioConfig> = audio
            .iter()
            .map(|a| AudioConfig {
                channels: a.channels as u16,
                priming: a.encoder.priming,
            })
            .collect();
        let mux = isocast_mux::Writer::create(
            &spec.path,
            spec.container,
            spec.created,
            VideoConfig {
                codec: mux_codec,
                width: u16::try_from(width).context("video width")?,
                height: u16::try_from(height).context("video height")?,
                timescale,
                frame_duration: FRAME_TICKS,
                decoder_config,
            },
            &audio_configs,
            Config::default(),
        )
        .with_context(|| format!("creating {}", spec.path.display()))?;
        let shared = Arc::new(Shared {
            mux: Mutex::new(Some(mux)),
            failed: Mutex::new(None),
            queued: AtomicUsize::new(0),
        });
        let (frames, queue) = mpsc::sync_channel(QUEUE);
        let thread_shared = Arc::clone(&shared);
        let name = spec
            .path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let video = std::thread::Builder::new()
            .name(format!("encode {name}"))
            .spawn(move || encode_video(video, queue, &thread_shared))
            .context("couldn't start the video encoder thread")?;
        Ok(Self {
            path: spec.path.clone(),
            shared,
            frames: Some(frames),
            video: Some(video),
            audio,
            start: None,
            timescale,
            last_tick: None,
        })
    }

    /// Nanoseconds since the file's start.
    fn since_start(&self, pts: Time) -> Result<i128> {
        let start = self.start.context("the writer hasn't started")?;
        Ok(i128::from(pts.as_nanos()) - i128::from(start.as_nanos()))
    }

    fn drain_audio(&mut self, track: usize) -> Result<()> {
        while let Some(packet) = self.audio[track].encoder.receive()? {
            self.shared
                .with_mux(|mux| mux.audio(track, packet.data, packet.time, packet.samples))?;
        }
        Ok(())
    }

    /// Stop the video thread once it has encoded everything queued.
    fn end_video(&mut self) -> Result<()> {
        drop(self.frames.take());
        if let Some(thread) = self.video.take() {
            thread
                .join()
                .map_err(|_| anyhow!("the video encoder thread panicked"))?;
        }
        self.shared.check()
    }
}

/// The video thread: encode queued pictures, then drain the encoder once
/// the queue closes.
fn encode_video(mut encoder: VideoEncoder, queue: mpsc::Receiver<(Frame, i64)>, shared: &Shared) {
    let mut run = || -> Result<()> {
        for (frame, tick) in queue.iter() {
            let sent = encoder.send(&frame, tick);
            drop(frame);
            shared.queued.fetch_sub(1, Ordering::Relaxed);
            sent?;
            deliver(&mut encoder, shared)?;
        }
        encoder.flush()?;
        deliver(&mut encoder, shared)
    };
    if let Err(error) = run() {
        if let Ok(mut failed) = shared.failed.lock() {
            failed.get_or_insert(format!("{} video encoder: {error:#}", encoder.name));
        }
        // Keep the queue draining so the writer never blocks on it.
        for _ in queue.iter() {
            shared.queued.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Hand finished pictures to the movie.
fn deliver(encoder: &mut VideoEncoder, shared: &Shared) -> Result<()> {
    while let Some(packet) = encoder.receive()? {
        let pts = u64::try_from(packet.pts).context("picture before the file's start")?;
        let dts = u64::try_from(packet.dts).context("picture before the file's start")?;
        shared.with_mux(|mux| {
            mux.video(VideoPacket {
                data: packet.data,
                pts,
                dts,
                sync: packet.key,
                framing: Framing::AnnexB,
            })
        })?;
    }
    Ok(())
}

impl FileWriter for Writer {
    type Frame = Frame;

    fn start(&mut self, at: Time) -> Result<()> {
        if self.start.is_some() {
            bail!("the writer has already started");
        }
        self.start = Some(at);
        Ok(())
    }

    fn append_video(&mut self, frame: &Frame, pts: Time) -> Result<bool> {
        self.shared.check()?;
        let nanos = self.since_start(pts)?;
        // Pictures sit on the frame grid (nearest slot; editors and the
        // contracts expect whole frames), strictly increasing: one that
        // lands on the previous picture's slot takes the next.
        let fps = i128::from(self.timescale / FRAME_TICKS);
        let slot = ((nanos * fps + 500_000_000).div_euclid(1_000_000_000)) as i64;
        let tick = slot * i64::from(FRAME_TICKS);
        let tick = match self.last_tick {
            Some(last) if tick <= last => last + i64::from(FRAME_TICKS),
            None if tick < 0 => 0,
            _ => tick,
        };
        let frames = self.frames.as_ref().context("the file is finished")?;
        let deadline = Instant::now() + WAIT;
        let mut item = (frame.clone(), tick);
        loop {
            match frames.try_send(item) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                    item = back;
                    std::thread::sleep(Duration::from_millis(1));
                }
                // The encoder is behind: this picture is dropped.
                Err(TrySendError::Full(_)) => return Ok(false),
                Err(TrySendError::Disconnected(_)) => {
                    self.shared.check()?;
                    bail!("the video encoder stopped");
                }
            }
        }
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        self.last_tick = Some(tick);
        Ok(true)
    }

    fn queue_depth(&self) -> usize {
        self.shared.queued.load(Ordering::Relaxed)
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> Result<bool> {
        let nanos = self.since_start(pts)?;
        let audio = self.audio.get_mut(track).context("no such audio track")?;
        let channels = audio.channels;
        if !samples.len().is_multiple_of(channels) {
            bail!(
                "audio: {} samples aren't whole {channels}-channel frames",
                samples.len()
            );
        }
        let at = nanos * SAMPLE_RATE / 1_000_000_000;
        let written = i128::from(audio.written);
        let mut samples = samples;
        if at > written + i128::from(AUDIO_SLACK) {
            // A gap: silence up to where these samples belong.
            let silence = vec![0.0; (at - written) as usize * channels];
            audio.encoder.send(&silence)?;
            audio.written += (at - written) as u64;
        } else if at + i128::from(AUDIO_SLACK) < written {
            // Overlap: drop what the track already has.
            let overlap = ((written - at) as usize * channels).min(samples.len());
            samples = &samples[overlap..];
        }
        audio.encoder.send(samples)?;
        audio.written += (samples.len() / channels) as u64;
        self.drain_audio(track)?;
        Ok(true)
    }

    fn finish(mut self, _end: Time) -> Result<Finished> {
        self.end_video()?;
        for track in 0..self.audio.len() {
            self.audio[track].encoder.flush()?;
            self.drain_audio(track)?;
        }
        let mux = self
            .shared
            .mux
            .lock()
            .map_err(|_| anyhow!("the movie writer panicked"))?
            .take()
            .context("the file is finished")?;
        mux.finish()
            .with_context(|| format!("finishing {}", self.path.display()))?;
        Ok(Finished::default())
    }

    fn cancel(mut self) {
        let _ = self.end_video();
        if let Ok(mut mux) = self.shared.mux.lock() {
            drop(mux.take());
        }
        let _ = std::fs::remove_file(&self.path);
    }

    fn crash_safe(&self) -> bool {
        true
    }

    fn commit(&mut self) -> Result<Option<Duration>> {
        self.shared.check()?;
        Ok(self.shared.with_mux(|mux| mux.commit())?.committed)
    }

    fn committed(&self) -> Option<Duration> {
        self.shared.mux.lock().ok()?.as_ref()?.progress().committed
    }
}

/// Finish `name` in `folder` from the fragments a killed recording left.
/// `None`: not a file this writer makes, or already finished. The engine
/// holds the folder's lock for the whole recovery pass.
pub fn recover(
    folder: &std::path::Path,
    name: &std::ffi::OsStr,
) -> Result<Option<isocast_engine::RecoveredFile>> {
    let directory = isocast_mux::file::Directory::for_repair_under_caller_lock(folder)?;
    let mut file = directory.open_for_repair(name)?;
    let Some(saved) = isocast_mux::mp4::recover(&mut file)? else {
        return Ok(None);
    };
    Ok(Some(isocast_engine::RecoveredFile {
        fragments: if saved.rebuilt { saved.fragments } else { 0 },
        frames: saved.rebuilt.then(|| saved.pictures()).flatten(),
        seconds: saved.rebuilt.then(|| saved.shortest()).flatten(),
        summary: if saved.rebuilt {
            saved.summary()
        } else {
            String::new()
        },
    }))
}
