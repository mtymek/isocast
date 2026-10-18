//! Audio through PipeWire: microphones, what the computer plays (the
//! default output's monitor) and playback. Each stream runs its own
//! PipeWire main loop on a thread of its own; PipeWire converts to 48 kHz
//! float samples.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{AudioRender, AudioSink, SAMPLE_RATE, Time};
use isocast_protocol::devices::{AudioOutput, Microphone};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

/// A running stream. Dropping it stops the stream and its thread.
pub struct AudioStream {
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The microphone `device` names: "default", an id, a name, or part of a
/// name (the shortest name containing it wins).
pub fn find_microphone(device: &str) -> Result<Microphone> {
    let microphones = crate::devices::audio()?.microphones;
    let found = if device == "default" {
        microphones
            .iter()
            .find(|m| m.default)
            .or(microphones.first())
    } else {
        let wanted = device.to_lowercase();
        microphones
            .iter()
            .find(|m| m.id == device || m.name == device)
            .or_else(|| {
                microphones
                    .iter()
                    .filter(|m| m.name.to_lowercase().contains(&wanted))
                    .min_by_key(|m| m.name.len())
            })
    };
    found.cloned().with_context(|| {
        let valid: Vec<String> = microphones
            .iter()
            .map(|m| format!("{} ({})", m.id, m.name))
            .collect();
        format!(
            "device: no microphone matches {device:?}; valid options: default, {}",
            valid.join(", ")
        )
    })
}

/// The output `device` names, as for microphones.
pub fn find_output(device: &str) -> Result<AudioOutput> {
    let outputs = crate::devices::audio()?.outputs;
    let found = if device == "default" {
        outputs.iter().find(|o| o.default).or(outputs.first())
    } else {
        let wanted = device.to_lowercase();
        outputs
            .iter()
            .find(|o| o.id == device || o.name == device)
            .or_else(|| {
                outputs
                    .iter()
                    .filter(|o| o.name.to_lowercase().contains(&wanted))
                    .min_by_key(|o| o.name.len())
            })
    };
    found.cloned().with_context(|| {
        let valid: Vec<String> = outputs
            .iter()
            .map(|o| format!("{} ({})", o.id, o.name))
            .collect();
        format!(
            "device: no audio output matches {device:?}; valid options: default, {}",
            valid.join(", ")
        )
    })
}

/// The app's playback stream `app` names: its name or binary, or part of
/// the name (the shortest containing it wins).
pub fn find_app(app: &str) -> Result<crate::devices::AppStream> {
    let apps = crate::devices::audio()?.apps;
    let wanted = app.to_lowercase();
    apps.iter()
        .find(|a| a.name == app || a.binary == app)
        .or_else(|| {
            apps.iter()
                .filter(|a| a.name.to_lowercase().contains(&wanted))
                .min_by_key(|a| a.name.len())
        })
        .cloned()
        .with_context(|| {
            let playing: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
            format!(
                "app: no app playing audio matches {app:?}; playing now: {}",
                if playing.is_empty() {
                    "none".to_string()
                } else {
                    playing.join(", ")
                }
            )
        })
}

/// Which input channels to deliver (numbered from 1), of `available`; by
/// default the first two (or the only one). One is mono, two stereo.
pub fn pick_channels(available: u32, channels: Option<&[u32]>) -> Result<Vec<u32>> {
    let picked: Vec<u32> = match channels {
        Some(list) => list.to_vec(),
        None => (1..=available.min(2)).collect(),
    };
    if picked.is_empty() || picked.len() > 2 {
        bail!("channels: pick one channel (mono) or two (stereo)");
    }
    if let Some(bad) = picked.iter().find(|c| **c == 0 || **c > available) {
        bail!("channels: {bad} isn't an input channel; valid options: 1 to {available}");
    }
    Ok(picked)
}

/// The SPA format pod asking for 48 kHz F32 with `channels` channels.
fn format_pod(channels: u32) -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| anyhow!("audio format: {error:?}"))?
    .0
    .into_inner())
}

/// What to capture.
pub enum Source {
    /// A node, by `node.name`, with this many channels, delivering `pick`.
    Node {
        name: String,
        channels: u32,
        pick: Vec<u32>,
    },
    /// What a sink plays (its monitor), stereo.
    Monitor { sink: String },
}

/// Start capturing `source`, delivering interleaved samples (one or two
/// channels) with their capture times. Returns the channel count delivered.
pub fn capture(
    source: Source,
    sink: AudioSink,
    now: impl Fn() -> Time + Send + 'static,
) -> Result<(AudioStream, u32)> {
    let (target, stream_channels, pick, monitor) = match source {
        Source::Node {
            name,
            channels,
            pick,
        } => (name, channels, pick, false),
        Source::Monitor { sink } => (sink, 2, vec![1, 2], true),
    };
    let delivered = pick.len() as u32;
    let (stop, stopped) = pw::channel::channel::<()>();
    let (ready, started) = std::sync::mpsc::channel::<Result<()>>();
    let sink = Arc::new(sink);
    let thread = std::thread::Builder::new()
        .name(format!("audio {target}"))
        .spawn(move || {
            let run = || -> Result<()> {
                crate::devices::pipewire_init();
                let mainloop = pw::main_loop::MainLoopRc::new(None)?;
                let context = pw::context::ContextRc::new(&mainloop, None)?;
                let core = context.connect_rc(None)?;
                let mut props = pw::properties::properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Production",
                    *pw::keys::APP_NAME => "Isocast",
                    *pw::keys::NODE_NAME => "isocast-capture",
                    *pw::keys::TARGET_OBJECT => target.as_str(),
                };
                if monitor {
                    props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
                }
                let stream = pw::stream::StreamBox::new(&core, "Isocast capture", props)?;
                let sink_for_process = Arc::clone(&sink);
                let _listener = stream
                    .add_local_listener_with_user_data(())
                    .process(move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else {
                            return;
                        };
                        let datas = buffer.datas_mut();
                        let Some(data) = datas.first_mut() else {
                            return;
                        };
                        let bytes = data.chunk().size() as usize;
                        let offset = data.chunk().offset() as usize;
                        let Some(raw) = data.data() else {
                            return;
                        };
                        let raw = &raw[offset.min(raw.len())..(offset + bytes).min(raw.len())];
                        let frames = raw.len() / 4 / stream_channels as usize;
                        if frames == 0 {
                            return;
                        }
                        let mut samples = Vec::with_capacity(frames * pick.len());
                        for frame in 0..frames {
                            for channel in &pick {
                                let at = (frame * stream_channels as usize
                                    + (*channel as usize - 1))
                                    * 4;
                                samples
                                    .push(f32::from_le_bytes(raw[at..at + 4].try_into().unwrap()));
                            }
                        }
                        // The cycle's time, less what is still in flight and
                        // this buffer's own length: when its first sample
                        // was captured.
                        let duration = frames as i64 * 1_000_000_000 / i64::from(SAMPLE_RATE);
                        let (base, delay) = match stream.time() {
                            Ok(time) if time.now() > 0 => {
                                let rate = time.rate();
                                let delay = if rate.denom > 0 {
                                    time.delay() * i64::from(rate.num) * 1_000_000_000
                                        / i64::from(rate.denom)
                                } else {
                                    0
                                };
                                (time.now(), delay.max(0))
                            }
                            _ => (now().as_nanos(), 0),
                        };
                        let at = Time::from_nanos(base - delay - duration);
                        (sink_for_process.samples)(&samples, at);
                    })
                    .state_changed({
                        let sink = Arc::clone(&sink);
                        move |_, _, _, state| {
                            if let pw::stream::StreamState::Error(error) = state {
                                (sink.stopped)(format!("audio capture: {error}"));
                            }
                        }
                    })
                    .register()?;
                let pod = format_pod(stream_channels)?;
                let mut params = [Pod::from_bytes(&pod).context("audio format pod")?];
                stream.connect(
                    spa::utils::Direction::Input,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT
                        | pw::stream::StreamFlags::MAP_BUFFERS
                        | pw::stream::StreamFlags::RT_PROCESS,
                    &mut params,
                )?;
                let quit = mainloop.clone();
                let _stop = stopped.attach(mainloop.loop_(), move |_| quit.quit());
                let _ = ready.send(Ok(()));
                mainloop.run();
                Ok(())
            };
            if let Err(error) = run() {
                let _ = ready.send(Err(error));
            }
        })
        .context("couldn't start the audio thread")?;
    started
        .recv()
        .map_err(|_| anyhow!("the audio thread stopped while starting"))??;
    Ok((
        AudioStream {
            stop: Some(stop),
            thread: Some(thread),
        },
        delivered,
    ))
}

/// Start playing on `output` (a `node.name`): `render` fills each buffer
/// with interleaved stereo samples at 48 kHz.
pub fn play(output: &str, render: AudioRender) -> Result<AudioStream> {
    let target = output.to_string();
    let render = Arc::new(Mutex::new(render));
    let (stop, stopped) = pw::channel::channel::<()>();
    let (ready, started) = std::sync::mpsc::channel::<Result<()>>();
    let thread = std::thread::Builder::new()
        .name(format!("playback {target}"))
        .spawn(move || {
            let run = || -> Result<()> {
                crate::devices::pipewire_init();
                let mainloop = pw::main_loop::MainLoopRc::new(None)?;
                let context = pw::context::ContextRc::new(&mainloop, None)?;
                let core = context.connect_rc(None)?;
                let props = pw::properties::properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Playback",
                    *pw::keys::MEDIA_ROLE => "Production",
                    *pw::keys::APP_NAME => "Isocast",
                    *pw::keys::NODE_NAME => "isocast-monitor",
                    *pw::keys::TARGET_OBJECT => target.as_str(),
                };
                let stream = pw::stream::StreamBox::new(&core, "Isocast monitor", props)?;
                let _listener = stream
                    .add_local_listener_with_user_data(())
                    .process(move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else {
                            return;
                        };
                        let requested = buffer.requested() as usize;
                        let datas = buffer.datas_mut();
                        let Some(data) = datas.first_mut() else {
                            return;
                        };
                        let written = {
                            let Some(raw) = data.data() else {
                                return;
                            };
                            let mut frames = raw.len() / 8;
                            if requested > 0 {
                                frames = frames.min(requested);
                            }
                            let mut samples = vec![0.0f32; frames * 2];
                            if let Ok(mut render) = render.lock() {
                                render(&mut samples);
                            }
                            for (i, sample) in samples.iter().enumerate() {
                                raw[i * 4..i * 4 + 4].copy_from_slice(&sample.to_le_bytes());
                            }
                            frames * 8
                        };
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = 8;
                        *chunk.size_mut() = written as u32;
                    })
                    .register()?;
                let pod = format_pod(2)?;
                let mut params = [Pod::from_bytes(&pod).context("audio format pod")?];
                stream.connect(
                    spa::utils::Direction::Output,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT
                        | pw::stream::StreamFlags::MAP_BUFFERS
                        | pw::stream::StreamFlags::RT_PROCESS,
                    &mut params,
                )?;
                let quit = mainloop.clone();
                let _stop = stopped.attach(mainloop.loop_(), move |_| quit.quit());
                let _ = ready.send(Ok(()));
                mainloop.run();
                Ok(())
            };
            if let Err(error) = run() {
                let _ = ready.send(Err(error));
            }
        })
        .context("couldn't start the playback thread")?;
    started
        .recv()
        .map_err(|_| anyhow!("the playback thread stopped while starting"))??;
    Ok(AudioStream {
        stop: Some(stop),
        thread: Some(thread),
    })
}
