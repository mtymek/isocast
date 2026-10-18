//! The virtual camera on Linux: a PipeWire video source node ("Isocast
//! Virtual Camera", `media.role` Camera) that apps reading cameras through
//! PipeWire (browsers, the camera portal) can choose. Apps that only read
//! V4L2 devices need v4l2loopback, which this doesn't provide.
//!
//! The stream drives itself: each frame the engine sends is handed to the
//! node's loop thread and copied into the next buffer, never waiting for a
//! camera app.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::Time;
use isocast_engine::record::CameraSink;
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use crate::frames::{Frame, Kind};

/// Frames on their way to the node before new ones are refused.
const QUEUE: usize = 2;

enum Message {
    Frame(Frame),
}

pub struct PipeWireCamera {
    sender: Option<pw::channel::Sender<Message>>,
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    queued: Arc<AtomicUsize>,
    width: u32,
    height: u32,
}

impl PipeWireCamera {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("virtual_camera: size {width}x{height} must be at least 1x1");
        }
        let (sender, frames) = pw::channel::channel::<Message>();
        let (stop, stopped) = pw::channel::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel::<Result<()>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let node_queued = Arc::clone(&queued);
        let thread = std::thread::Builder::new()
            .name("virtual camera node".into())
            .spawn(move || {
                let fail = ready.clone();
                if let Err(error) = run(width, height, fps, frames, stopped, node_queued, ready) {
                    let _ = fail.send(Err(error));
                }
            })
            .context("couldn't start the virtual camera thread")?;
        started
            .recv()
            .map_err(|_| anyhow!("virtual_camera: the PipeWire node stopped while starting"))??;
        Ok(Self {
            sender: Some(sender),
            stop: Some(stop),
            thread: Some(thread),
            queued,
            width,
            height,
        })
    }
}

impl CameraSink<Frame> for PipeWireCamera {
    fn send(&mut self, frame: &Frame, _pts: Time) -> Result<bool> {
        if frame.kind() != Kind::Bgra
            || (frame.width(), frame.height()) != (self.width, self.height)
        {
            bail!(
                "virtual_camera: a {}x{} {:?} frame for a {}x{} BGRA camera",
                frame.width(),
                frame.height(),
                frame.kind(),
                self.width,
                self.height
            );
        }
        if self.queued.load(Ordering::Acquire) >= QUEUE {
            return Ok(false);
        }
        let sender = self
            .sender
            .as_ref()
            .context("virtual_camera: the camera is closed")?;
        self.queued.fetch_add(1, Ordering::AcqRel);
        if sender.send(Message::Frame(frame.clone())).is_err() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            bail!("virtual_camera: the PipeWire node stopped");
        }
        Ok(true)
    }

    fn close(&mut self) -> Result<()> {
        drop(self.sender.take());
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("virtual_camera: the PipeWire node panicked"))?;
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

/// The node's format: BGRx at the camera's size and rate.
fn format_pod(width: u32, height: u32, fps: u32) -> Result<Vec<u8>> {
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
            Id,
            spa::param::video::VideoFormat::BGRx
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle { width, height }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: fps, denom: 1 }
        ),
    );
    serialize(spa::pod::Value::Object(object))
}

/// The buffers the consumer gets: one BGRx plane.
fn buffers_pod(width: u32, height: u32) -> Result<Vec<u8>> {
    use spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let stride = (width * 4) as i32;
    let int = |key: u32, value: i32| Property {
        key,
        flags: PropertyFlags::empty(),
        value: Value::Int(value),
    };
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 8,
                    },
                ))),
            },
            int(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
            int(spa::sys::SPA_PARAM_BUFFERS_size, stride * height as i32),
            int(spa::sys::SPA_PARAM_BUFFERS_stride, stride),
        ],
    };
    serialize(Value::Object(object))
}

/// A Header meta (sequence and time) with each buffer, as cameras send.
fn meta_pod() -> Result<Vec<u8>> {
    use spa::pod::{Property, PropertyFlags, Value};
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            },
            Property {
                key: spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            },
        ],
    };
    serialize(Value::Object(object))
}

fn serialize(value: spa::pod::Value) -> Result<Vec<u8>> {
    Ok(
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
            .map_err(|error| anyhow!("virtual_camera: pod: {error:?}"))?
            .0
            .into_inner(),
    )
}

fn run(
    width: u32,
    height: u32,
    fps: u32,
    frames: pw::channel::Receiver<Message>,
    stopped: pw::channel::Receiver<()>,
    queued: Arc<AtomicUsize>,
    ready: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    crate::devices::pipewire_init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Camera",
        *pw::keys::MEDIA_CLASS => "Video/Source",
        *pw::keys::NODE_NAME => "isocast-virtual-camera",
        *pw::keys::NODE_DESCRIPTION => "Isocast Virtual Camera",
        *pw::keys::APP_NAME => "Isocast",
    };
    let stream = pw::stream::StreamRc::new(core.clone(), "Isocast Virtual Camera", props)?;
    // The newest frame not yet copied into a buffer.
    let latest: Arc<Mutex<Option<Frame>>> = Arc::default();
    let for_process = Arc::clone(&latest);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .param_changed(move |stream, _, id, param| {
            if param.is_none() || id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let (Ok(buffers), Ok(meta)) = (buffers_pod(width, height), meta_pod())
                && let (Some(buffers), Some(meta)) =
                    (Pod::from_bytes(&buffers), Pod::from_bytes(&meta))
            {
                let _ = stream.update_params(&mut [buffers, meta]);
            }
        })
        .process(move |stream, _| {
            let Some(frame) = for_process.lock().ok().and_then(|mut l| l.take()) else {
                return;
            };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let stride = (width * 4) as usize;
            let written = {
                let Some(raw) = data.data() else {
                    return;
                };
                let pitch = frame.layout().planes[0].row_pitch;
                let pixels = frame.plane(0);
                let rows = (height as usize).min(raw.len() / stride);
                for row in 0..rows {
                    raw[row * stride..(row + 1) * stride]
                        .copy_from_slice(&pixels[row * pitch..row * pitch + stride]);
                }
                rows * stride
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = written as u32;
        })
        .register()?;
    let format = format_pod(width, height, fps)?;
    let mut params = [Pod::from_bytes(&format).context("virtual camera format pod")?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let triggering = stream.clone();
    let _frames = frames.attach(mainloop.loop_(), move |message| match message {
        Message::Frame(frame) => {
            queued.fetch_sub(1, Ordering::AcqRel);
            if let Ok(mut latest) = latest.lock() {
                *latest = Some(frame);
            }
            let _ = triggering.trigger_process();
        }
    });
    let quit = mainloop.clone();
    let _stop = stopped.attach(mainloop.loop_(), move |_| quit.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}
