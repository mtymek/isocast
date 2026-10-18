//! What can be captured: V4L2 cameras and PipeWire audio devices. Screens
//! and windows are chosen in the ScreenCast portal's dialog (L.4d), so
//! none are listed here yet.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use isocast_protocol::devices::{
    AudioOutput, Camera, CameraKind, Devices, Microphone, Permission, Permissions, VideoFormat,
};
use v4l::video::Capture;

/// Cameras, microphones and audio outputs, with what reading them needs.
pub fn devices() -> Result<Devices> {
    let mut notes = Vec::new();
    let cameras = cameras();
    let audio = match audio() {
        Ok(audio) => Some(audio),
        Err(error) => {
            notes.push(format!("couldn't list audio devices: {error:#}"));
            None
        }
    };
    let (microphones, audio_outputs) = audio.clone().unwrap_or_default();
    notes.push(
        "screens and windows are chosen in the desktop's screen-sharing dialog, which Linux \
         recording doesn't support yet"
            .into(),
    );
    Ok(Devices {
        displays: Vec::new(),
        windows: Vec::new(),
        cameras,
        microphones,
        audio_outputs,
        audio_apps: Vec::new(),
        permissions: permissions(audio.is_some()),
        notes,
    })
}

/// What each kind of capture needs: camera device nodes this user can
/// open, and a running PipeWire for audio. Screens go through the portal's
/// dialog each time (not built yet).
pub fn permissions(pipewire: bool) -> Permissions {
    let audio = if pipewire {
        Permission::Granted
    } else {
        Permission::Denied
    };
    Permissions {
        screen_recording: Permission::NotDetermined,
        camera: camera_access(),
        microphone: audio,
        system_audio: audio,
    }
}

/// Whether every camera node can be opened for reading and writing.
fn camera_access() -> Permission {
    let nodes = video_nodes();
    if nodes.is_empty() {
        return Permission::NotDetermined;
    }
    let open = nodes.iter().all(|node| {
        let Ok(path) = std::ffi::CString::new(node.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        // SAFETY: checks access to a path; no side effects.
        unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
    });
    if open {
        Permission::Granted
    } else {
        Permission::Denied
    }
}

fn video_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("video"))
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    nodes.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str()?.strip_prefix("video")?.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    nodes
}

/// A stable id for a camera node: its `/dev/v4l/by-id` name if it has one
/// (survives replugging), else its `/dev` name.
fn stable_id(node: &Path) -> String {
    let by_id = std::fs::read_dir("/dev/v4l/by-id")
        .ok()
        .and_then(|entries| {
            entries.filter_map(|entry| entry.ok()).find_map(|entry| {
                let target = std::fs::canonicalize(entry.path()).ok()?;
                (target == node).then(|| entry.file_name().to_string_lossy().into_owned())
            })
        });
    by_id.unwrap_or_else(|| {
        node.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    })
}

/// The capture nodes and their formats. Nodes without video capture
/// formats (UVC metadata nodes, encoders, radio) are left out. Opening a
/// node to read its formats doesn't start the camera.
pub(crate) fn cameras() -> Vec<Camera> {
    let mut cameras: Vec<Camera> = Vec::new();
    for node in video_nodes() {
        let Ok(device) = v4l::Device::with_path(&node) else {
            continue;
        };
        let Ok(caps) = device.query_caps() else {
            continue;
        };
        let formats = formats(&device);
        if formats.is_empty() {
            continue;
        }
        let kind = if caps.driver.contains("loopback") {
            CameraKind::Virtual
        } else if caps.card.to_lowercase().contains("integrated") {
            CameraKind::BuiltIn
        } else if caps.bus.starts_with("usb") || caps.bus.starts_with("pci") {
            CameraKind::External
        } else {
            CameraKind::Other
        };
        let mut name = caps.card.clone();
        // A second capture node of the same device (an IR camera, say).
        let same = cameras.iter().filter(|c| c.model == caps.card).count();
        if same > 0 {
            name = format!("{name} ({})", same + 1);
        }
        cameras.push(Camera {
            id: stable_id(&node),
            name,
            model: caps.card.clone(),
            manufacturer: caps.driver.clone(),
            kind,
            formats,
            suspended: false,
        });
    }
    cameras
}

fn formats(device: &v4l::Device) -> Vec<VideoFormat> {
    let mut out = Vec::new();
    for description in device.enum_formats().unwrap_or_default() {
        let fourcc = description.fourcc;
        let pixel_format = fourcc.str().unwrap_or("????").trim().to_string();
        for size in device.enum_framesizes(fourcc).unwrap_or_default() {
            for discrete in size.size.to_discrete() {
                let (width, height) = (discrete.width, discrete.height);
                let mut frame_rates: Vec<f64> = device
                    .enum_frameintervals(fourcc, width, height)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|interval| match interval.interval {
                        v4l::frameinterval::FrameIntervalEnum::Discrete(fraction)
                            if fraction.numerator > 0 =>
                        {
                            Some(f64::from(fraction.denominator) / f64::from(fraction.numerator))
                        }
                        _ => None,
                    })
                    .collect();
                frame_rates.sort_by(|a, b| b.total_cmp(a));
                frame_rates.dedup();
                out.push(VideoFormat {
                    width,
                    height,
                    pixel_format: pixel_format.clone(),
                    frame_rates,
                    frame_rate_ranges: Vec::new(),
                });
            }
        }
    }
    out
}

fn pipewire_init() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(pipewire::init);
}

/// One PipeWire node of interest.
#[derive(Debug, Clone)]
struct Node {
    name: String,
    description: String,
    class: String,
    channels: u32,
    rate: f64,
    product: String,
    vendor: String,
}

/// PipeWire's audio sources (microphones) and sinks (outputs), the
/// defaults marked.
fn audio() -> Result<(Vec<Microphone>, Vec<AudioOutput>)> {
    pipewire_init();
    let mainloop = pipewire::main_loop::MainLoopRc::new(None).context("PipeWire main loop")?;
    let context = pipewire::context::ContextRc::new(&mainloop, None).context("PipeWire context")?;
    let core = context
        .connect_rc(None)
        .map_err(|error| anyhow!("couldn't connect to PipeWire: {error}"))?;
    let registry = Rc::new(core.get_registry_rc().context("PipeWire registry")?);
    let nodes: Rc<RefCell<Vec<Node>>> = Rc::default();
    let defaults: Rc<RefCell<(Option<String>, Option<String>)>> = Rc::default();
    let metadata: Rc<
        RefCell<
            Vec<(
                pipewire::metadata::Metadata,
                pipewire::metadata::MetadataListener,
            )>,
        >,
    > = Rc::default();
    let _registry_listener = {
        let (nodes, defaults, metadata, registry_ref) = (
            Rc::clone(&nodes),
            Rc::clone(&defaults),
            Rc::clone(&metadata),
            Rc::downgrade(&registry),
        );
        registry
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else {
                    return;
                };
                match global.type_ {
                    pipewire::types::ObjectType::Node => {
                        let class = props.get("media.class").unwrap_or_default();
                        if class != "Audio/Source" && class != "Audio/Sink" {
                            return;
                        }
                        let get = |key: &str| props.get(key).unwrap_or_default().to_string();
                        nodes.borrow_mut().push(Node {
                            name: get("node.name"),
                            description: props
                                .get("node.description")
                                .or_else(|| props.get("node.nick"))
                                .unwrap_or_default()
                                .to_string(),
                            class: class.to_string(),
                            channels: get("audio.channels").parse().unwrap_or(2),
                            rate: get("audio.rate").parse().unwrap_or(48_000.0),
                            product: get("device.product.name"),
                            vendor: get("device.vendor.name"),
                        });
                    }
                    pipewire::types::ObjectType::Metadata
                        if props.get("metadata.name") == Some("default") =>
                    {
                        let Some(registry) = registry_ref.upgrade() else {
                            return;
                        };
                        let Ok(bound) = registry.bind::<pipewire::metadata::Metadata, _>(global)
                        else {
                            return;
                        };
                        let defaults = Rc::clone(&defaults);
                        let listener = bound
                            .add_listener_local()
                            .property(move |_, key, _, value| {
                                // Values look like {"name":"alsa_output…"}.
                                let name = value.and_then(|value| {
                                    serde_json::from_str::<serde_json::Value>(value)
                                        .ok()?
                                        .get("name")?
                                        .as_str()
                                        .map(str::to_string)
                                });
                                let mut defaults = defaults.borrow_mut();
                                match key {
                                    Some("default.audio.source") => defaults.0 = name,
                                    Some("default.audio.sink") => defaults.1 = name,
                                    _ => {}
                                }
                                0
                            })
                            .register();
                        metadata.borrow_mut().push((bound, listener));
                    }
                    _ => {}
                }
            })
            .register()
    };
    // Two round trips: the first lists the globals, the second delivers the
    // properties of the metadata bound during the first.
    for _ in 0..2 {
        let done = Rc::new(std::cell::Cell::new(false));
        let pending = core.sync(0).context("PipeWire sync")?;
        let _core_listener = {
            let (done, mainloop) = (Rc::clone(&done), mainloop.clone());
            core.add_listener_local()
                .done(move |id, seq| {
                    if id == pipewire::core::PW_ID_CORE && seq == pending {
                        done.set(true);
                        mainloop.quit();
                    }
                })
                .register()
        };
        while !done.get() {
            mainloop.run();
        }
    }
    let (default_source, default_sink) = defaults.borrow().clone();
    let nodes = nodes.borrow();
    let microphones = nodes
        .iter()
        .filter(|node| node.class == "Audio/Source")
        .map(|node| Microphone {
            id: node.name.clone(),
            name: node.description.clone(),
            model: node.product.clone(),
            manufacturer: node.vendor.clone(),
            default: default_source.as_deref() == Some(node.name.as_str()),
            sample_rates: vec![node.rate],
            channels: node.channels,
            channel_names: Vec::new(),
        })
        .collect();
    let outputs = nodes
        .iter()
        .filter(|node| node.class == "Audio/Sink")
        .map(|node| AudioOutput {
            id: node.name.clone(),
            name: node.description.clone(),
            default: default_sink.as_deref() == Some(node.name.as_str()),
            sample_rate: node.rate,
            channels: node.channels,
        })
        .collect();
    Ok((microphones, outputs))
}

/// The permissions and the next step for each one that isn't granted.
pub fn permissions_report() -> isocast_protocol::devices::PermissionsReport {
    use isocast_protocol::devices::{
        PermissionAction, PermissionDetail, PermissionKind, PermissionsReport,
    };
    let pipewire = audio().is_ok();
    let permissions = permissions(pipewire);
    let details = PermissionKind::ALL
        .into_iter()
        .map(|kind| {
            let state = permissions.get(kind);
            let (needed_for, next_step) = match kind {
                PermissionKind::ScreenRecording => (
                    "displays and windows",
                    "the desktop asks which screen or window to share when recording starts \
                     (not supported on Linux yet)",
                ),
                PermissionKind::Camera => (
                    "cameras and capture cards",
                    "give your user access to /dev/video*: add it to the `video` group \
                     (`sudo usermod -aG video $USER`) and log in again",
                ),
                PermissionKind::Microphone => (
                    "microphones",
                    "start PipeWire for your session (`systemctl --user start pipewire \
                     wireplumber`)",
                ),
                PermissionKind::SystemAudio => (
                    "system and app audio",
                    "start PipeWire for your session (`systemctl --user start pipewire \
                     wireplumber`)",
                ),
            };
            PermissionDetail {
                permission: kind,
                state,
                needed_for: needed_for.into(),
                action: match state {
                    isocast_protocol::devices::Permission::Granted => PermissionAction::None,
                    _ if kind == PermissionKind::ScreenRecording => PermissionAction::Request,
                    _ => PermissionAction::AskAdministrator,
                },
                settings_url: String::new(),
                next_step: (state != isocast_protocol::devices::Permission::Granted)
                    .then(|| next_step.to_string()),
                restart_after_granting: false,
                app: String::new(),
                app_path: None,
                denied_without_prompt: false,
            }
        })
        .collect();
    PermissionsReport {
        permissions,
        app: "isocastd".into(),
        details,
    }
}
