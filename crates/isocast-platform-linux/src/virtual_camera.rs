//! The virtual camera on Linux: a PipeWire video source node ("Isocast
//! Virtual Camera", `media.role` Camera) that apps reading cameras through
//! PipeWire (browsers, OBS, the camera portal, `pw-v4l2`) can choose. Apps
//! that only open V4L2 devices need v4l2loopback, which this doesn't provide.
//!
//! The node drives its own clock and takes BGRx at the camera's size and
//! rate. Like capture and monitoring, every PipeWire object stays on one
//! owned worker running its main loop; the stream has no RT_PROCESS, so its
//! callbacks run there too. The engine hands frames over through a two-deep
//! queue and never waits for a camera app: each one is copied into the next
//! buffer, or replaced by a newer one while no app reads the node.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::Time;
use isocast_engine::record::CameraSink;
use pipewire::{self as pw, properties::properties, spa};
use spa::param::video::VideoFormat;
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

use crate::frames::Frame;
use crate::pw_loop::WorkerLoop;

/// Frames on their way to the node before new ones are dropped.
const QUEUE: usize = 2;
const START_TIMEOUT: Duration = Duration::from_secs(2);
const LOOP_WAIT: Duration = Duration::from_millis(20);

/// A live camera node; closing or dropping it removes the node.
pub struct PipeWireCamera {
    frames: pw::channel::Sender<Frame>,
    /// Frames sent and not yet taken by the worker.
    queued: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    /// Why the worker ended on its own, set before it ends.
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
}

impl PipeWireCamera {
    /// Make the node and return once it is connecting, without waiting for
    /// an app to read it.
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        if width == 0 || height == 0 || fps == 0 {
            bail!("virtual_camera: {width}x{height} at {fps} fps must be at least 1x1 at 1 fps");
        }
        let (frames, receiver) = pw::channel::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let (ready, started) = mpsc::sync_channel(1);
        let (worker_queued, worker_cancel, worker_failure) =
            (queued.clone(), cancel.clone(), failure.clone());
        let thread = thread::Builder::new()
            .name("pipewire-virtual-camera".into())
            .spawn(move || {
                let mut running = false;
                let result = run(
                    (width, height, fps),
                    receiver,
                    worker_queued,
                    &worker_cancel,
                    &ready,
                    &mut running,
                );
                if let Err(error) = result {
                    if running {
                        if let Ok(mut failure) = worker_failure.lock() {
                            *failure = Some(format!("{error:#}"));
                        }
                    } else {
                        let _ = ready.send(Err(error));
                    }
                }
            })
            .context("starting the virtual camera's PipeWire worker")?;
        let camera = Self {
            frames,
            queued,
            cancel,
            failure,
            thread: Some(thread),
            width,
            height,
        };
        // On either error `camera` drops, cancels and joins the worker.
        started
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| {
                anyhow!("virtual_camera: the PipeWire node didn't start within 2 seconds")
            })?
            .context("virtual_camera: making the PipeWire camera node")?;
        Ok(camera)
    }
}

impl CameraSink<Frame> for PipeWireCamera {
    fn send(&mut self, frame: &Frame, _pts: Time) -> Result<bool> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            bail!(
                "virtual_camera: a {}x{} frame for a {}x{} camera",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            );
        }
        if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
            let failure = self
                .failure
                .lock()
                .ok()
                .and_then(|mut failure| failure.take());
            bail!(
                "virtual_camera: the PipeWire node stopped: {}",
                failure.as_deref().unwrap_or("its worker ended")
            );
        }
        if self.queued.load(Ordering::Acquire) >= QUEUE {
            return Ok(false);
        }
        self.queued.fetch_add(1, Ordering::AcqRel);
        if self.frames.send(frame.clone()).is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            bail!("virtual_camera: the PipeWire node stopped");
        }
        Ok(true)
    }

    fn close(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("virtual_camera: the PipeWire worker panicked"))?;
        }
        Ok(())
    }

    fn transport(&self) -> &'static str {
        "pipewire-node"
    }
}

impl Drop for PipeWireCamera {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct State {
    width: u32,
    height: u32,
    /// The newest frame not yet copied into a buffer.
    latest: Rc<RefCell<Option<Frame>>>,
    failure: Rc<RefCell<Option<String>>>,
}

fn run(
    (width, height, fps): (u32, u32, u32),
    frames: pw::channel::Receiver<Frame>,
    queued: Arc<AtomicUsize>,
    cancel: &AtomicBool,
    ready: &mpsc::SyncSender<Result<()>>,
    running: &mut bool,
) -> Result<()> {
    pw::init();
    let mainloop = WorkerLoop::new()?;
    let context = pw::context::ContextRc::new(mainloop.main_loop(), None)?;
    let core = context
        .connect_rc(None)
        .context("connecting the virtual camera to PipeWire")?;
    let failure: Rc<RefCell<Option<String>>> = Rc::default();
    let core_failure = failure.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, code, message| {
            core_failure
                .borrow_mut()
                .get_or_insert_with(|| format!("remote error on object {id} ({code}): {message}"));
        })
        .register();
    let stream = mainloop.stream(pw::stream::StreamBox::new(
        &core,
        "Isocast Virtual Camera",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Camera",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => "isocast-virtual-camera",
            *pw::keys::NODE_DESCRIPTION => "Isocast Virtual Camera",
            *pw::keys::APP_NAME => "Isocast",
        },
    )?);
    let latest: Rc<RefCell<Option<Frame>>> = Rc::default();
    let state = State {
        width,
        height,
        latest: latest.clone(),
        failure: failure.clone(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, state, old, new| {
            let reason = match new {
                pw::stream::StreamState::Error(reason) => Some(reason),
                pw::stream::StreamState::Unconnected
                    if old != pw::stream::StreamState::Unconnected =>
                {
                    Some("camera stream disconnected".into())
                }
                _ => None,
            };
            if let Some(reason) = reason {
                state.failure.borrow_mut().get_or_insert(reason);
            }
        })
        .param_changed(|stream, state, id, param| {
            if param.is_none() || id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Err(error) = configure(stream, state.width, state.height) {
                state
                    .failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("negotiating camera buffers: {error:#}"));
            }
        })
        .process(|stream, state| {
            if let Err(error) = fill(stream, state) {
                state
                    .failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("sending a camera frame: {error:#}"));
            }
        })
        .register()?;
    let format = serialize(format_offer(width, height, fps))?;
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [Pod::from_bytes(&format).context("invalid camera format pod")?],
    )?;
    // A frame arrived: keep the newest and run a graph cycle for it once
    // this iteration's sources are dispatched.
    let pending = Rc::new(Cell::new(false));
    let arrived = pending.clone();
    let _frames = frames.attach(mainloop.main_loop().loop_(), move |frame| {
        queued.fetch_sub(1, Ordering::AcqRel);
        *latest.borrow_mut() = Some(frame);
        arrived.set(true);
    });
    ready
        .send(Ok(()))
        .context("the virtual camera's caller disconnected")?;
    *running = true;
    while !cancel.load(Ordering::Acquire) {
        mainloop.iterate(LOOP_WAIT)?;
        if let Some(reason) = failure.borrow_mut().take() {
            bail!(reason);
        }
        // Without an app reading the node there is no cycle to run; the
        // frame waits in `latest` until a newer one replaces it.
        if pending.replace(false) {
            let _ = stream.trigger_process();
        }
    }
    // Locals drop in order: receiver, listener, stream (disconnected, which
    // removes the node, and its queued callbacks flushed on this thread),
    // core listener, core, context, loop.
    Ok(())
}

/// The node's only format: BGRx at the camera's size and rate.
fn format_offer(width: u32, height: u32, fps: u32) -> Object {
    use spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    use spa::pod::{object, property};
    use spa::utils::{Fraction, Rectangle, SpaTypes};
    object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(F::MediaType, Id, MediaType::Video),
        property!(F::MediaSubtype, Id, MediaSubtype::Raw),
        property!(F::VideoFormat, Id, VideoFormat::BGRx),
        property!(F::VideoSize, Rectangle, Rectangle { width, height }),
        property!(F::VideoFramerate, Fraction, Fraction { num: fps, denom: 1 }),
    )
}

/// Once a format is chosen: one packed BGRx plane per buffer (PipeWire
/// allocates them; any memory type), with a Header meta as cameras send.
fn configure(stream: &pw::stream::Stream, width: u32, height: u32) -> Result<()> {
    let stride = (width * 4) as i32;
    let int = |key, value| Property::new(key, Value::Int(value));
    let buffers = serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 8,
                    },
                ))),
            ),
            int(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
            int(spa::sys::SPA_PARAM_BUFFERS_size, stride * height as i32),
            int(spa::sys::SPA_PARAM_BUFFERS_stride, stride),
        ],
    })?;
    let header = serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    })?;
    stream.update_params(&mut [
        Pod::from_bytes(&buffers).context("invalid buffer pod")?,
        Pod::from_bytes(&header).context("invalid metadata pod")?,
    ])?;
    Ok(())
}

/// Copy the newest frame into the next free buffer. With no frame or no
/// free buffer, nothing is sent.
fn fill(stream: &pw::stream::Stream, state: &mut State) -> Result<()> {
    let Some(frame) = state.latest.borrow_mut().take() else {
        return Ok(());
    };
    // Read before dequeuing, so a failed readback never sends a buffer.
    let pixels = frame.read_bgra()?;
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return Ok(());
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        return Ok(());
    };
    let stride = state.width as usize * 4;
    // An unmapped buffer goes back empty, never with an old chunk's size.
    let written = match data.data() {
        Some(bytes) => {
            let len = (state.height as usize).min(bytes.len() / stride) * stride;
            bytes[..len].copy_from_slice(&pixels[..len]);
            len
        }
        None => 0,
    };
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = written as u32;
    Ok(())
}

fn serialize(object: Object) -> Result<Vec<u8>> {
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map_err(|error| anyhow!("serializing PipeWire pod: {error:?}"))?
    .0
    .into_inner())
}
