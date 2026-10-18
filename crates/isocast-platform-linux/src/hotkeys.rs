//! Global hotkeys through the desktop's GlobalShortcuts portal. Isocast
//! registers each binding as a shortcut with its preferred keys; the
//! desktop decides the keys (GNOME and KDE ask the person; Hyprland binds
//! them in its own config: `bind = MODS, KEY, global, :<binding>`) and
//! reports presses and releases.

use std::sync::Mutex;

use futures_util::StreamExt;
use isocast_engine::hotkeys::{Edge, GlobalHotkeys, Key, OnKey};
use isocast_protocol::hotkeys::{Chord, Phase, Registration};

/// The portal session of the current registrations; replaced as a whole.
struct Running {
    session: ashpd::desktop::Session<ashpd::desktop::global_shortcuts::GlobalShortcuts>,
    listeners: Vec<tokio::task::JoinHandle<()>>,
}

/// The app id Isocast registers as with the portals (host apps must, for
/// GlobalShortcuts).
const APP_ID: &str = "dev.isocast.Isocast";

#[derive(Default)]
pub struct LinuxHotkeys {
    running: Mutex<Option<Running>>,
}

/// One runtime for every portal session (ashpd keeps its D-Bus connection
/// for the process, so it must outlive them all).
fn runtime() -> anyhow::Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("hotkeys")
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))
}

impl Running {
    fn close(self) {
        for listener in &self.listeners {
            listener.abort();
        }
        if let Ok(runtime) = runtime() {
            let session = self.session;
            let _ = runtime.block_on(async move { session.close().await });
        }
    }
}

impl LinuxHotkeys {
    /// The process's hotkeys (one portal session at a time).
    pub fn shared() -> &'static LinuxHotkeys {
        static SHARED: std::sync::OnceLock<LinuxHotkeys> = std::sync::OnceLock::new();
        SHARED.get_or_init(LinuxHotkeys::default)
    }
}

/// A chord in the shortcuts specification's form: `CTRL+ALT+r`.
fn trigger(chord: &Chord) -> String {
    let mut parts = Vec::new();
    for (bit, name) in [(1, "CTRL"), (2, "ALT"), (4, "SHIFT"), (8, "LOGO")] {
        if chord.modifiers & bit != 0 {
            parts.push(name.to_string());
        }
    }
    parts.push(chord.key.clone());
    parts.join("+")
}

impl GlobalHotkeys for LinuxHotkeys {
    fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
        let Ok(mut running) = self.running.lock() else {
            return Vec::new();
        };
        if let Some(previous) = running.take() {
            previous.close();
        }
        if keys.is_empty() {
            return Vec::new();
        }
        match register(&keys, generation, on_key) {
            Ok((started, triggers)) => {
                *running = Some(started);
                keys.into_iter()
                    .map(|key| {
                        let assigned = triggers
                            .iter()
                            .find(|(id, _)| *id == key.binding)
                            .map(|(_, trigger)| trigger.clone())
                            .unwrap_or_default();
                        Registration {
                            warning: assigned.is_empty().then(|| {
                                format!(
                                    "registered with the desktop, which has no keys for it yet: \
                                     assign them in its shortcut settings (Hyprland: bind = \
                                     MODS, KEY, global, {APP_ID}:{})",
                                    key.binding
                                )
                            }),
                            binding: key.binding,
                            global: true,
                        }
                    })
                    .collect()
            }
            Err(error) => keys
                .into_iter()
                .map(|key| Registration {
                    binding: key.binding,
                    global: false,
                    warning: Some(format!(
                        "the desktop's GlobalShortcuts portal isn't available ({error:#}); use \
                         focused input"
                    )),
                })
                .collect(),
        }
    }
}

/// Open a session, bind `keys`, and deliver their edges. Returns the
/// running session and the triggers the desktop assigned, by binding.
fn register(
    keys: &[Key],
    generation: u64,
    on_key: OnKey,
) -> anyhow::Result<(Running, Vec<(String, String)>)> {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    let runtime = runtime()?;
    let shortcuts: Vec<NewShortcut> = keys
        .iter()
        .map(|key| {
            let preferred = trigger(&key.chord);
            NewShortcut::new(key.binding.clone(), format!("Isocast: {}", key.binding))
                .preferred_trigger(preferred.as_str())
        })
        .collect();
    let (session, triggers, listeners) = runtime.block_on(async {
        static REGISTERED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        REGISTERED
            .get_or_try_init(|| async {
                ashpd::register_host_app(ashpd::AppID::try_from(APP_ID)?).await
            })
            .await?;
        let proxy = GlobalShortcuts::new().await?;
        let session = proxy.create_session(Default::default()).await?;
        let bound = proxy
            .bind_shortcuts(&session, &shortcuts, None, Default::default())
            .await?
            .response()?;
        let triggers: Vec<(String, String)> = bound
            .shortcuts()
            .iter()
            .map(|s| (s.id().to_string(), s.trigger_description().to_string()))
            .collect();
        // The session's object path (ashpd keeps `path` private; it
        // serializes as the path), to keep only this session's events.
        let path: String = serde_json::from_value(serde_json::to_value(&session)?)?;
        let mut activated = proxy.receive_activated().await?;
        let mut deactivated = proxy.receive_deactivated().await?;
        let (press, release) = (on_key.clone(), on_key);
        let mine = path.clone();
        let pressed = tokio::spawn(async move {
            while let Some(event) = activated.next().await {
                if event.session_handle().as_str() == mine {
                    press(Edge {
                        binding: event.shortcut_id().to_string(),
                        generation,
                        phase: Phase::Press,
                    });
                }
            }
        });
        let released = tokio::spawn(async move {
            while let Some(event) = deactivated.next().await {
                if event.session_handle().as_str() == path {
                    release(Edge {
                        binding: event.shortcut_id().to_string(),
                        generation,
                        phase: Phase::Release,
                    });
                }
            }
        });
        anyhow::Ok((session, triggers, vec![pressed, released]))
    })?;
    Ok((Running { session, listeners }, triggers))
}
