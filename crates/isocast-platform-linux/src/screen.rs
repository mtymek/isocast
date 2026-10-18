//! Screens and windows through the desktop's ScreenCast portal: the
//! desktop asks which screen or window to share (once per target: the
//! portal's restore token is kept), then PipeWire delivers its frames.
//!
//! Frames arrive in shared memory (BGRx/RGBx) and are copied into BGRA
//! frames; importing the portal's DMA-BUFs directly is a later step.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use isocast_engine::{CaptureFormat, ScreenTarget, Time, VideoSink};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use crate::frames::{Frame, FramePool, Kind};

/// How long the desktop's dialog may wait for a choice.
const DIALOG: Duration = Duration::from_secs(180);

/// The key a target's restore token is kept under.
fn key(target: &ScreenTarget<'_>) -> String {
    match target {
        ScreenTarget::Display(display) => format!("display:{display}"),
        ScreenTarget::Window { app, title } => {
            format!("window:{app}:{}", title.unwrap_or_default())
        }
        ScreenTarget::Application { app, display } => format!("application:{app}:{display}"),
    }
}

fn tokens_path() -> PathBuf {
    isocast_protocol::api::support_folder().join("screencast-tokens.json")
}

fn tokens() -> BTreeMap<String, String> {
    std::fs::read_to_string(tokens_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_token(key: &str, token: Option<&str>) {
    let mut all = tokens();
    match token {
        Some(token) => all.insert(key.to_string(), token.to_string()),
        None => all.remove(key),
    };
    if let Ok(text) = serde_json::to_string_pretty(&all) {
        let path = tokens_path();
        let temporary = path.with_extension("json.partial");
        if std::fs::write(&temporary, text).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    }
}

/// Whether a screen was shared before (a restore token is kept), so
/// recording won't show the dialog again.
pub fn remembered() -> bool {
    !tokens().is_empty()
}

/// The portal session, kept open while the capture runs.
struct Portal {
    runtime: tokio::runtime::Runtime,
    session: ashpd::desktop::Session<Screencast>,
}

impl Portal {
    fn close(self) {
        let Portal { runtime, session } = self;
        let _ = runtime.block_on(async move { session.close().await });
    }
}

/// Open a session for `target`, letting the desktop ask what to share
/// unless a kept token restores it. Returns the PipeWire remote, its node
/// and the stream size the portal gave.
fn open(target: &ScreenTarget<'_>, cursor: bool) -> Result<Opened> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("starting the portal connection")?;
    let key = key(target);
    let token = tokens().get(&key).cloned();
    let sources: enumflags2::BitFlags<SourceType> = match target {
        ScreenTarget::Display(_) => SourceType::Monitor.into(),
        _ => SourceType::Window.into(),
    };
    let (session, fd, node, size, new_token) = runtime.block_on(async {
        let proxy = Screencast::new()
            .await
            .context("the desktop has no ScreenCast portal (xdg-desktop-portal)")?;
        let session = proxy.create_session(Default::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(if cursor {
                        CursorMode::Embedded
                    } else {
                        CursorMode::Hidden
                    })
                    .set_sources(sources)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?;
        let started = tokio::time::timeout(DIALOG, async {
            proxy
                .start(&session, None, Default::default())
                .await?
                .response()
        })
        .await
        .map_err(|_| anyhow!("no screen was chosen in the sharing dialog within 3 minutes"))?
        .context("screen sharing was cancelled or refused")?;
        let stream = started
            .streams()
            .first()
            .context("the portal shared nothing")?
            .clone();
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        anyhow::Ok((
            session,
            fd,
            stream.pipe_wire_node_id(),
            stream.size(),
            started.restore_token().map(str::to_string),
        ))
    })?;
    save_token(&key, new_token.as_deref());
    Ok(Opened {
        portal: Portal { runtime, session },
        fd,
        node,
        size,
    })
}

/// An open portal session and the PipeWire stream it shares.
struct Opened {
    portal: Portal,
    fd: std::os::fd::OwnedFd,
    node: u32,
    size: Option<(i32, i32)>,
}

/// The SPA format pod: 8-bit RGB with or without alpha, any size, up to
/// `fps`.
fn format_pod(size: Option<(i32, i32)>, fps: u32) -> Result<Vec<u8>> {
    let (width, height) = size.unwrap_or((1920, 1080));
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: width.max(1) as u32,
                height: height.max(1) as u32
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: fps, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| anyhow!("video format: {error:?}"))?
    .0
    .into_inner())
}

/// What the stream negotiated.
#[derive(Clone, Copy)]
struct Negotiated {
    width: u32,
    height: u32,
    /// Red first (RGBx/RGBA) rather than blue.
    rgb: bool,
}

/// A running screen capture. Dropping it stops it and closes the portal
/// session.
pub struct ScreenCapture {
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start capturing `target` at up to `fps`. Blocks while the desktop asks
/// what to share (the first time), then until the stream's size is known.
pub fn capture(
    target: ScreenTarget<'_>,
    fps: u32,
    cursor: bool,
    sink: VideoSink<Frame>,
    now: impl Fn() -> Time + Send + Sync + 'static,
) -> Result<(ScreenCapture, CaptureFormat)> {
    let Opened {
        portal,
        fd,
        node,
        size,
    } = open(&target, cursor)?;
    let (stop, stopped) = pw::channel::channel::<()>();
    let (ready, negotiated) = mpsc::channel::<Result<Negotiated>>();
    let sink = Arc::new(sink);
    let thread = std::thread::Builder::new()
        .name("screen capture".into())
        .spawn(move || {
            let ready_for_error = ready.clone();
            let run = || -> Result<()> {
                crate::devices::pipewire_init();
                let mainloop = pw::main_loop::MainLoopRc::new(None)?;
                let context = pw::context::ContextRc::new(&mainloop, None)?;
                let core = context.connect_fd_rc(fd, None)?;
                let props = pw::properties::properties! {
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Screen",
                    *pw::keys::APP_NAME => "Isocast",
                };
                let stream = pw::stream::StreamBox::new(&core, "Isocast screen", props)?;
                let state: Arc<Mutex<Option<(Negotiated, FramePool)>>> = Arc::default();
                let params_state = Arc::clone(&state);
                let ready_once = Mutex::new(Some(ready));
                let process_sink = Arc::clone(&sink);
                let state_sink = Arc::clone(&sink);
                let _listener = stream
                    .add_local_listener_with_user_data(())
                    .param_changed(move |_, _, id, param| {
                        let Some(param) = param else {
                            return;
                        };
                        if id != spa::param::ParamType::Format.as_raw() {
                            return;
                        }
                        let mut info = spa::param::video::VideoInfoRaw::new();
                        if info.parse(param).is_err() {
                            return;
                        }
                        let format = info.format();
                        let negotiated = Negotiated {
                            width: info.size().width,
                            height: info.size().height,
                            rgb: matches!(
                                format,
                                spa::param::video::VideoFormat::RGBx
                                    | spa::param::video::VideoFormat::RGBA
                            ),
                        };
                        let pool = FramePool::cpu(Kind::Bgra, negotiated.width, negotiated.height);
                        let result = match pool {
                            Ok(pool) => {
                                if let Ok(mut state) = params_state.lock() {
                                    *state = Some((negotiated, pool));
                                }
                                Ok(negotiated)
                            }
                            Err(error) => Err(error),
                        };
                        if let Some(ready) = ready_once.lock().ok().and_then(|mut r| r.take()) {
                            let _ = ready.send(result);
                        }
                    })
                    .process(move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else {
                            return;
                        };
                        let at = now();
                        let datas = buffer.datas_mut();
                        let Some(data) = datas.first_mut() else {
                            return;
                        };
                        let stride = data.chunk().stride().max(0) as usize;
                        let offset = data.chunk().offset() as usize;
                        let Some(raw) = data.data() else {
                            return;
                        };
                        let Ok(mut state) = state.lock() else {
                            return;
                        };
                        let Some((negotiated, pool)) = state.as_mut() else {
                            return;
                        };
                        let (width, height) =
                            (negotiated.width as usize, negotiated.height as usize);
                        let stride = if stride == 0 { width * 4 } else { stride };
                        if raw.len() < offset + stride * (height - 1) + width * 4 {
                            return;
                        }
                        let rgb = negotiated.rgb;
                        let filled = pool.fill(|planes, layout| {
                            let pitch = layout[0].row_pitch;
                            for row in 0..height {
                                let source = &raw[offset + row * stride..][..width * 4];
                                let dest = &mut planes[0][row * pitch..][..width * 4];
                                if rgb {
                                    let (dest, _) = dest.as_chunks_mut::<4>();
                                    let (source, _) = source.as_chunks::<4>();
                                    for (d, s) in dest.iter_mut().zip(source) {
                                        *d = [s[2], s[1], s[0], 255];
                                    }
                                } else {
                                    dest.copy_from_slice(source);
                                    for pixel in dest.as_chunks_mut::<4>().0 {
                                        pixel[3] = 255;
                                    }
                                }
                            }
                            Ok(())
                        });
                        if let Ok(Some(frame)) = filled {
                            (process_sink.frame)(frame, at);
                        }
                    })
                    .state_changed(move |_, _, _, state| {
                        if let pw::stream::StreamState::Error(error) = state {
                            (state_sink.stopped)(format!("screen capture: {error}"));
                        }
                    })
                    .register()?;
                let pod = format_pod(size, fps)?;
                let mut params = [Pod::from_bytes(&pod).context("video format pod")?];
                stream.connect(
                    spa::utils::Direction::Input,
                    Some(node),
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )?;
                let quit = mainloop.clone();
                let _stop = stopped.attach(mainloop.loop_(), move |_| quit.quit());
                mainloop.run();
                Ok(())
            };
            if let Err(error) = run() {
                let _ = ready_for_error.send(Err(error));
            }
            portal.close();
        })
        .context("couldn't start the screen capture thread")?;
    let capture = ScreenCapture {
        stop: Some(stop),
        thread: Some(thread),
    };
    let negotiated = negotiated
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow!("the shared screen sent no picture format within 10 s"))??;
    if negotiated.width == 0 || negotiated.height == 0 {
        bail!("the shared screen has no size");
    }
    Ok((
        capture,
        CaptureFormat {
            width: negotiated.width,
            height: negotiated.height,
            fps,
        },
    ))
}
