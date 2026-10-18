//! Cameras and capture cards through V4L2: choosing a format, and a
//! capture thread converting each picture into a frame.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::{CameraRequest, CaptureFormat, Probe, Time, VideoSink};
use isocast_protocol::Canvas;
use isocast_protocol::devices::Camera;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;

use crate::convert::Converter;
use crate::frames::{Frame, FramePool, Kind};

/// The camera `device` names: an id, a name, or part of a name (the
/// shortest name containing it wins).
pub fn find(device: &str) -> Result<Camera> {
    let cameras = crate::devices::cameras();
    if let Some(camera) = cameras.iter().find(|c| c.id == device || c.name == device) {
        return Ok(camera.clone());
    }
    let wanted = device.to_lowercase();
    cameras
        .iter()
        .filter(|c| c.name.to_lowercase().contains(&wanted))
        .min_by_key(|c| c.name.len())
        .cloned()
        .with_context(|| {
            let valid: Vec<String> = cameras
                .iter()
                .map(|c| format!("{} ({})", c.id, c.name))
                .collect();
            format!(
                "device: no camera matches {device:?}; valid options: {}",
                if valid.is_empty() {
                    "none connected".to_string()
                } else {
                    valid.join(", ")
                }
            )
        })
}

/// The node to open for a camera id.
fn node(id: &str) -> PathBuf {
    let by_id = PathBuf::from("/dev/v4l/by-id").join(id);
    if by_id.exists() {
        by_id
    } else {
        PathBuf::from("/dev").join(id)
    }
}

/// A format to capture in.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: String,
}

/// Pixel formats the converter takes, best first (no decoding).
const SUPPORTED: [&str; 3] = ["NV12", "YUYV", "MJPG"];

/// The format to capture in, as on macOS: without a request, the highest
/// frame rate up to the canvas rate, then the largest size that fits on the
/// canvas either way round (else the smallest larger one), turned as the
/// canvas is if it can be, then a pixel format that needs no decoding.
pub fn choose(camera: &Camera, request: CameraRequest, canvas: &Canvas) -> Result<Choice> {
    let target = request.fps.unwrap_or(canvas.fps);
    // Frame rate, fits, size, turned as the canvas, pixel format.
    type Score = (u32, bool, u64, bool, usize);
    let mut best: Option<(Score, Choice)> = None;
    let mut offered = Vec::new();
    for format in &camera.formats {
        let Some(rank) = SUPPORTED.iter().position(|f| *f == format.pixel_format) else {
            continue;
        };
        let (width, height) = (format.width, format.height);
        if width % 2 != 0 || height % 2 != 0 {
            continue;
        }
        for &rate in &format.frame_rates {
            offered.push(format!("{width}×{height} @ {rate}"));
            if request.size.is_some_and(|wanted| wanted != (width, height)) {
                continue;
            }
            let fps = rate.round() as u32;
            if request.fps.is_some_and(|wanted| wanted != fps) {
                continue;
            }
            let area = u64::from(width) * u64::from(height);
            let fits = width.max(height) <= canvas.width.max(canvas.height)
                && width.min(height) <= canvas.width.min(canvas.height);
            let score = (
                fps.min(target),
                fits,
                if fits { area } else { u64::MAX - area },
                (width >= height) == (canvas.width >= canvas.height),
                SUPPORTED.len() - rank,
            );
            if best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((
                    score,
                    Choice {
                        width,
                        height,
                        fps,
                        pixel_format: format.pixel_format.clone(),
                    },
                ));
            }
        }
    }
    offered.dedup();
    best.map(|(_, choice)| choice).with_context(|| {
        format!(
            "{}: no format matches the request ({}); valid options: {}",
            camera.name,
            match (request.size, request.fps) {
                (Some((w, h)), Some(fps)) => format!("{w}×{h} @ {fps}"),
                (Some((w, h)), None) => format!("{w}×{h}"),
                (None, Some(fps)) => format!("{fps} fps"),
                (None, None) => "any".into(),
            },
            offered.join(", ")
        )
    })
}

pub fn probe(device: &str, request: CameraRequest, canvas: &Canvas) -> Result<Probe> {
    let camera = find(device)?;
    let choice = choose(&camera, request, canvas)?;
    Ok(Probe {
        device: camera.name,
        video: Some(CaptureFormat {
            width: choice.width,
            height: choice.height,
            fps: choice.fps,
        }),
        channels: None,
    })
}

/// A running camera. Dropping it stops the capture.
pub struct CameraCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start capturing `device` in the chosen format, delivering frames to
/// `sink` with their capture times on `CLOCK_MONOTONIC`.
pub fn capture(
    device: &str,
    request: CameraRequest,
    canvas: &Canvas,
    sink: VideoSink<Frame>,
    now: impl Fn() -> Time + Send + 'static,
) -> Result<(CameraCapture, CaptureFormat)> {
    let camera = find(device)?;
    let choice = choose(&camera, request, canvas)?;
    let path = node(&camera.id);
    let device = v4l::Device::with_path(&path)
        .with_context(|| format!("opening {} ({})", camera.name, path.display()))?;
    let fourcc = v4l::FourCC::new(
        choice
            .pixel_format
            .as_bytes()
            .try_into()
            .context("pixel format")?,
    );
    let set = device
        .set_format(&v4l::Format::new(choice.width, choice.height, fourcc))
        .with_context(|| format!("{}: setting the format", camera.name))?;
    if (set.width, set.height, set.fourcc) != (choice.width, choice.height, fourcc) {
        bail!(
            "{}: asked for {}×{} {}, the camera chose {}×{} {}",
            camera.name,
            choice.width,
            choice.height,
            choice.pixel_format,
            set.width,
            set.height,
            set.fourcc
        );
    }
    device
        .set_params(&v4l::video::capture::Parameters::with_fps(choice.fps))
        .with_context(|| format!("{}: setting {} fps", camera.name, choice.fps))?;
    let mut stream =
        v4l::io::mmap::Stream::with_buffers(&device, v4l::buffer::Type::VideoCapture, 4)
            .with_context(|| format!("{}: starting the stream", camera.name))?;
    stream.set_timeout(Duration::from_millis(500));
    let mut pool = FramePool::cpu(Kind::YCbCr, choice.width, choice.height)?;
    let mut converter = Converter::new(choice.width, choice.height);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let name = camera.name.clone();
    let pixel_format = choice.pixel_format.clone();
    let thread = std::thread::Builder::new()
        .name(format!("camera {name}"))
        .spawn(move || {
            // The device must outlive its stream.
            let _device = device;
            while !stopping.load(Ordering::Relaxed) {
                let (data, meta) = match stream.next() {
                    Ok(next) => next,
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
                    Err(error) => {
                        (sink.stopped)(format!("{name}: {error}"));
                        return;
                    }
                };
                let used = (meta.bytesused as usize).min(data.len());
                // `time_t` is narrower on 32-bit systems.
                #[allow(clippy::useless_conversion)]
                let at = timestamp(
                    i64::from(meta.timestamp.sec),
                    i64::from(meta.timestamp.usec),
                    now(),
                );
                let filled = pool.fill(|planes, layout| {
                    converter.convert(&pixel_format, &data[..used], planes, layout)
                });
                match filled {
                    Ok(Some(frame)) => (sink.frame)(frame, at),
                    // Every frame is still in use downstream: skip this one.
                    Ok(None) => {}
                    // A corrupt picture (a USB hiccup): skip it.
                    Err(error) => {
                        isocast_diagnostics::diagnostic!("warn"; "{name}: {error:#}");
                    }
                }
            }
        })
        .context("couldn't start the camera thread")?;
    Ok((
        CameraCapture {
            stop,
            thread: Some(thread),
        },
        CaptureFormat {
            width: choice.width,
            height: choice.height,
            fps: choice.fps,
        },
    ))
}

/// A V4L2 buffer time on the host clock. UVC drivers stamp buffers with
/// `CLOCK_MONOTONIC`, the host clock; a driver stamping another clock
/// (more than a second away) gets the arrival time instead.
fn timestamp(sec: i64, usec: i64, now: Time) -> Time {
    let stamped = Time::from_nanos(sec * 1_000_000_000 + usec * 1000);
    if (stamped.as_nanos() - now.as_nanos()).abs() < 1_000_000_000 {
        stamped
    } else {
        now
    }
}
