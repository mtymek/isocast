//! A single user-selected portal stream per display source. The portal owns
//! consent and cursor compositing; discovery never opens a share dialog.
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use futures_util::StreamExt;
use isocast_engine::{CaptureFormat, ScreenTarget, VideoSink};
use isocast_protocol::api::{ScreenPickKind, ScreenPicked};
use isocast_protocol::devices::Display;
use serde::{Deserialize, Serialize};

use crate::{frames::Frame, pipewire_video};

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
static DECLINED: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());
const MAX_SETTINGS: u64 = 64 * 1024;
const MAX_SAVED: usize = 32;

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    token: Option<String>,
    width: u32,
    height: u32,
}

fn settings() -> std::path::PathBuf {
    isocast_protocol::api::setup_path().with_extension("linux-capture.json")
}

fn load() -> Result<BTreeMap<String, Saved>> {
    let file = match std::fs::File::open(settings()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.len() <= MAX_SETTINGS,
        "Linux capture settings exceed 64 KiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_SETTINGS as usize,
        "Linux capture settings exceed 64 KiB"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid Linux capture settings: {}", settings().display()))
}

fn save(key: &str, saved: Saved) -> Result<()> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let mut all = load()?;
    ensure!(
        all.contains_key(key) || all.len() < MAX_SAVED,
        "at most {MAX_SAVED} saved portal selections are supported"
    );
    all.insert(key.to_owned(), saved);
    let bytes = serde_json::to_vec(&all)?;
    ensure!(
        bytes.len() <= MAX_SETTINGS as usize,
        "Linux capture settings exceed 64 KiB"
    );
    let path = settings();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .context("create Linux capture settings temporary file")?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub fn displays() -> Result<Vec<Display>> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let mut saved = load()?;
    saved.entry("main".into()).or_default();
    Ok(saved
        .into_iter()
        .filter(|(id, _)| !id.starts_with(WINDOW_KEY))
        .map(|(id, info)| Display {
            name: display_name(&id),
            main: id == "main",
            id,
            built_in: false,
            width: info.width,
            height: info.height,
            refresh_rate: 0.0,
        })
        .collect())
}

fn display_name(id: &str) -> String {
    if id == "main" {
        "Choose a screen (desktop portal)".into()
    } else if id.starts_with(PICKED) {
        "Screen chosen in the desktop portal".into()
    } else {
        format!("Portal screen {id}")
    }
}

pub fn allow_prompt() {
    DECLINED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

pub struct Capture {
    // Stop PipeWire before closing its portal session.
    video: Option<pipewire_video::Capture>,
    stopping: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Disconnecting PipeWire can itself make the compositor close the
        // session. That is our shutdown, not a revoked grant.
        self.stopping.store(true, Ordering::Release);
        drop(self.video.take());
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Saved window selections are kept under `window:` and the source's app
/// (and title, if it gives one), apart from displays.
const WINDOW_KEY: &str = "window:";

/// The key a target's portal selection is saved under, and whether the
/// portal should offer windows instead of monitors.
fn target_key(target: &ScreenTarget<'_>) -> Result<(String, bool)> {
    let (key, window) = match *target {
        ScreenTarget::Display(id) if !id.is_empty() && !id.starts_with(WINDOW_KEY) => {
            (id.to_owned(), false)
        }
        ScreenTarget::Window { app, title } if !app.is_empty() => (
            match title {
                Some(title) => format!("{WINDOW_KEY}{app}:{title}"),
                None => format!("{WINDOW_KEY}{app}"),
            },
            true,
        ),
        _ => bail!(
            "screen target: Linux shares one screen or one window chosen in the desktop portal; use a display or window source"
        ),
    };
    ensure!(
        key.len() <= 128,
        "screen target: a display id or window app and title must be at most 128 bytes together"
    );
    Ok((key, window))
}

/// The size the portal last gave `target`, if it was shared before.
pub fn saved_size(target: &ScreenTarget<'_>) -> Option<(u32, u32)> {
    let (key, _) = target_key(target).ok()?;
    let _lock = SETTINGS_LOCK.lock();
    let saved = load().ok()?.remove(&key)?;
    (saved.width > 0 && saved.height > 0).then_some((saved.width, saved.height))
}

type Session = ashpd::desktop::Session<Screencast>;

/// The ScreenCast portal on a connection of its own, and a new session.
async fn connect() -> Result<(Screencast, Session)> {
    // A dedicated connection belongs to the caller's runtime. ASHPD's
    // process-global connection would outlive the first runtime.
    tokio::time::timeout(Duration::from_secs(10), async {
        let connection = zbus::Connection::session().await?;
        let portal = Screencast::with_connection(connection)
            .await
            .context("connect to desktop ScreenCast portal")?;
        let session = portal.create_session(Default::default()).await?;
        Ok::<_, anyhow::Error>((portal, session))
    })
    .await
    .context("desktop portal connection timed out")?
}

/// The one stream the portal started, and whether it gave a restore token.
struct Selected {
    node: u32,
    width: u32,
    height: u32,
    remembered: bool,
}

/// Ask the portal for one screen (or, with `window`, one window), which
/// `token` may grant without its dialog, and save the grant under `key`.
async fn select(
    portal: &Screencast,
    session: &Session,
    key: &str,
    window: bool,
    cursor: Option<CursorMode>,
    token: Option<&str>,
) -> Result<Selected> {
    let source = if window {
        SourceType::Window
    } else {
        SourceType::Monitor
    };
    let options = SelectSourcesOptions::default()
        .set_sources(Some(source.into()))
        .set_multiple(false)
        .set_cursor_mode(cursor)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(token);
    portal.select_sources(session, options).await?.response()?;
    let response = portal
        .start(session, None, Default::default())
        .await?
        .response()?;
    ensure!(
        response.streams().len() == 1,
        "desktop portal must return exactly one screen per source"
    );
    let stream = &response.streams()[0];
    let (width, height) = stream.size().unwrap_or((0, 0));
    let token = response.restore_token().map(str::to_owned);
    let selected = Selected {
        node: stream.pipe_wire_node_id(),
        width: width.max(0) as u32,
        height: height.max(0) as u32,
        remembered: token.is_some(),
    };
    save(
        key,
        Saved {
            token,
            width: selected.width,
            height: selected.height,
        },
    )?;
    Ok(selected)
}

/// What `screen_pick` gives a source: `picked:` and 12 hex digits.
const PICKED: &str = "picked:";

fn pick_value(id: u64) -> String {
    format!("{PICKED}{:012x}", id & 0xffff_ffff_ffff)
}

/// A `picked:` value no saved selection has yet, if one more fits.
fn new_pick() -> Result<String> {
    use std::hash::BuildHasher;
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let saved = load()?;
    ensure!(
        saved.len() < MAX_SAVED,
        "screen_pick: {MAX_SAVED} portal selections are saved already, the most kept; remove \
         ones no source uses from {}",
        settings().display()
    );
    loop {
        let seed = (std::time::SystemTime::now(), std::process::id());
        let value = pick_value(std::collections::hash_map::RandomState::new().hash_one(seed));
        if !saved.contains_key(&value) && !saved.contains_key(&format!("{WINDOW_KEY}{value}")) {
            return Ok(value);
        }
    }
}

/// Show the portal's dialog for a window or a screen and save the grant
/// under a new `picked:` value, which a source then gives as its `app` or
/// `display` to reopen exactly that choice: each source keeps its own.
pub fn pick(kind: ScreenPickKind) -> Result<ScreenPicked> {
    let value = new_pick()?;
    let (target, field, what) = match kind {
        ScreenPickKind::Window => (
            ScreenTarget::Window {
                app: &value,
                title: None,
            },
            "app",
            "window",
        ),
        ScreenPickKind::Display => (ScreenTarget::Display(&value), "display", "screen"),
    };
    let (key, window) = target_key(&target)?;
    let selected = std::thread::Builder::new()
        .name("linux-screen-pick".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (portal, session) = connect().await?;
                let selected = tokio::time::timeout(
                    Duration::from_secs(180),
                    select(&portal, &session, &key, window, None, None),
                )
                .await
                .context("nothing was chosen within 3 minutes")
                .and_then(|result| result);
                let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
                selected
            })
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("screen_pick: the portal thread panicked"))?
        .with_context(|| format!("screen_pick: choose a {what} in the desktop's sharing dialog"))?;
    Ok(ScreenPicked {
        field: field.into(),
        value,
        width: selected.width,
        height: selected.height,
        remembered: selected.remembered,
    })
}

pub fn capture(
    target: ScreenTarget<'_>,
    fps: u32,
    ten_bit: bool,
    cursor: bool,
    sink: VideoSink<Frame>,
) -> Result<(Capture, CaptureFormat)> {
    ensure!(
        !ten_bit,
        "Linux screen capture: 10-bit capture is not supported; select 8-bit ISO recording"
    );
    let (key, window) = target_key(&target)?;
    {
        let mut declined = DECLINED.lock().unwrap_or_else(|error| error.into_inner());
        declined.retain(|_, when| when.elapsed() < Duration::from_secs(600));
        ensure!(
            !declined.contains_key(&key),
            "screen sharing was declined or stopped; request screen permission to try again, or wait ten minutes"
        );
    }
    let on_stopped = Arc::new(sink.stopped);
    let portal_stopped = on_stopped.clone();
    let sink = VideoSink {
        frame: sink.frame,
        stopped: Box::new(move |reason| on_stopped(reason)),
    };
    let token = {
        let _lock = SETTINGS_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
        load()?.remove(&key).and_then(|saved| saved.token)
    };
    let (send, receive) = mpsc::sync_channel(1);
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let portal_stopping = stopping.clone();
    let thread = std::thread::Builder::new().name("linux-screen-portal".into()).spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async {
                let (portal, session) = tokio::select! {
                    _ = &mut stopped => return Ok(()),
                    result = connect() => result?,
                };
                let mut closed = tokio::time::timeout(Duration::from_secs(5), session.receive_closed())
                    .await.context("desktop portal signal subscription timed out")??;
                let opening = async {
                    let available = portal.available_cursor_modes().await?;
                    let requested = if cursor { CursorMode::Embedded } else { CursorMode::Hidden };
                    ensure!(available.contains(requested), "desktop portal cannot provide the requested cursor mode");
                    let selected = select(&portal, &session, &key, window, Some(requested), token.as_deref()).await?;
                    let fd = portal.open_pipe_wire_remote(&session, Default::default()).await?;
                    Ok::<_, anyhow::Error>((fd, selected.node))
                };
                let opened = tokio::select! {
                    _ = &mut stopped => Err(anyhow::anyhow!("screen sharing cancelled")),
                    result = tokio::time::timeout(Duration::from_secs(70), opening) => result
                        .context("screen sharing timed out; approve the desktop portal dialog").and_then(|result| result),
                };
                if let Err(error) = &opened
                    && error.chain().any(|cause| matches!(cause.downcast_ref::<ashpd::Error>(), Some(ashpd::Error::Response(_)))) {
                        let mut declined = DECLINED.lock().unwrap_or_else(|error| error.into_inner());
                        if declined.len() < 32 { declined.insert(key.clone(), Instant::now()); }
                }
                let succeeded = opened.is_ok();
                if send.send(opened).is_ok() && succeeded {
                    tokio::select! {
                        biased;
                        _ = stopped => {},
                        _ = closed.next() => {
                            if !portal_stopping.load(Ordering::Acquire) {
                            let mut declined = DECLINED.lock().unwrap_or_else(|error| error.into_inner());
                            if declined.len() < 32 { declined.insert(key.clone(), Instant::now()); }
                            drop(declined);
                            portal_stopped("desktop portal stopped screen sharing; request screen permission to try again, or wait ten minutes".into());
                            }
                        },
                    }
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
                Ok(())
            })
        })();
        if let Err(error) = result { let _ = send.send(Err(error)); }
    })?;
    let mut capture = Capture {
        video: None,
        stopping,
        stop: Some(stop),
        thread: Some(thread),
    };
    // 10s connect + 5s subscription + 70s consent + 15s PipeWire = 100s;
    // even the caller's 90s portal watchdog + 15s fits the 120s open budget.
    let (remote, node) = receive
        .recv_timeout(Duration::from_secs(90))
        .context("desktop portal did not respond")??;
    let (video, format) = pipewire_video::start(remote, node, fps, sink)?;
    capture.video = Some(video);
    Ok((capture, format))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_and_displays_are_saved_apart() {
        assert_eq!(
            target_key(&ScreenTarget::Display("main")).unwrap(),
            ("main".into(), false)
        );
        assert_eq!(
            target_key(&ScreenTarget::Window {
                app: "firefox",
                title: None
            })
            .unwrap(),
            ("window:firefox".into(), true)
        );
        assert_eq!(
            target_key(&ScreenTarget::Window {
                app: "firefox",
                title: Some("Docs")
            })
            .unwrap(),
            ("window:firefox:Docs".into(), true)
        );
        for refused in [
            ScreenTarget::Display(""),
            ScreenTarget::Display("window:firefox"),
            ScreenTarget::Window {
                app: "",
                title: None,
            },
            ScreenTarget::Application {
                app: "firefox",
                display: "main",
            },
        ] {
            assert!(target_key(&refused).is_err());
        }
        let long = "x".repeat(130);
        assert!(target_key(&ScreenTarget::Display(&long)).is_err());
    }

    #[test]
    fn picks_are_saved_under_their_own_value() {
        let value = pick_value(u64::MAX);
        assert_eq!(value, "picked:ffffffffffff");
        assert_eq!(pick_value(0xabc), "picked:000000000abc");
        assert_eq!(
            target_key(&ScreenTarget::Display(&value)).unwrap(),
            (value.clone(), false)
        );
        assert_eq!(
            target_key(&ScreenTarget::Window {
                app: &value,
                title: None
            })
            .unwrap(),
            (format!("window:{value}"), true)
        );
        assert_eq!(display_name("main"), "Choose a screen (desktop portal)");
        assert_eq!(display_name(&value), "Screen chosen in the desktop portal");
        assert_eq!(display_name("left"), "Portal screen left");
    }
}
