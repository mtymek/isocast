//! Linux back end for Isocast. Empty on other systems, so the workspace
//! still builds everywhere.
//!
//! Built up in steps (`PLAN_LINUX.md`, L.2 onwards). What isn't built yet
//! fails with a clear error, as the stub platform's does.

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod camera;
#[cfg(target_os = "linux")]
mod codec_config;
#[cfg(target_os = "linux")]
mod convert;
#[cfg(target_os = "linux")]
mod devices;
#[cfg(target_os = "linux")]
mod encode;
#[cfg(target_os = "linux")]
pub mod frames;
#[cfg(target_os = "linux")]
mod gpu;
#[cfg(target_os = "linux")]
mod jpeg;
#[cfg(target_os = "linux")]
mod media;
#[cfg(target_os = "linux")]
pub mod plugin;
#[cfg(target_os = "linux")]
mod screen;
#[cfg(target_os = "linux")]
mod system;
#[cfg(target_os = "linux")]
mod unsupported;
#[cfg(target_os = "linux")]
mod writer;

#[cfg(target_os = "linux")]
pub use platform::LinuxPlatform;

#[cfg(target_os = "linux")]
mod platform {
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::Result;
    use isocast_engine::gpu::Gpu;
    use isocast_engine::{
        AudioRender, AudioSink, BrowserControl, BrowserPage, CameraRequest, CaptureFormat,
        LiveAudioSpec, LiveVideoSpec, MediaInfo, MediaPart, Platform, Probe, ScreenTarget, Time,
        VideoSink, Volume, WriterSpec,
    };
    use isocast_platform_stub::StubPlatform;
    use isocast_protocol::api::Machine;
    use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
    use isocast_protocol::{Canvas, SourceKind};

    use crate::frames::{Frame, FramePool, Kind};
    use crate::gpu::Importer;
    use crate::system;
    use crate::unsupported::{Unsupported, unsupported};

    /// A running capture. Dropping it stops the capture.
    pub enum Capture {
        Camera(crate::camera::CameraCapture),
        Audio(crate::audio::AudioStream),
        Screen(crate::screen::ScreenCapture),
    }

    /// The Linux platform: a cheap handle, like macOS's.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct LinuxPlatform;

    const STUB: StubPlatform = StubPlatform;

    fn monotonic() -> libc::timespec {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: writes the clock's time into `time`.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
        time
    }

    impl Platform for LinuxPlatform {
        type Frame = Frame;
        type FramePool = FramePool;
        type Writer = crate::writer::Writer;
        type Capture = Capture;
        type Importer = Importer;
        type Media = Unsupported;
        type Playback = crate::audio::AudioStream;
        type VideoEncoder = Unsupported;
        type AudioEncoder = Unsupported;
        type SharedSurface = isocast_surfaces::Surface;

        /// `CLOCK_MONOTONIC`, the clock PipeWire and V4L2 stamp frames
        /// with, so their times need no mapping.
        fn now(&self) -> Time {
            let time = monotonic();
            Time::from_nanos(time.tv_sec * 1_000_000_000 + time.tv_nsec)
        }

        /// An absolute `CLOCK_MONOTONIC` deadline: no drift from computing
        /// a relative sleep, and no timer slack added by `nanosleep`'s
        /// relative form being rounded twice.
        fn sleep_until(&self, at: Time) {
            let nanos = at.as_nanos();
            let deadline = libc::timespec {
                tv_sec: nanos.div_euclid(1_000_000_000),
                tv_nsec: nanos.rem_euclid(1_000_000_000),
            };
            // SAFETY: an absolute sleep on the monotonic clock; EINTR
            // just sleeps again until the same deadline.
            while unsafe {
                libc::clock_nanosleep(
                    libc::CLOCK_MONOTONIC,
                    libc::TIMER_ABSTIME,
                    &deadline,
                    std::ptr::null_mut(),
                )
            } == libc::EINTR
            {}
        }

        /// A screen or window waits for the desktop's sharing dialog the
        /// first time (no kept token): give the person time to choose.
        fn open_timeout(&self, source: &SourceKind) -> std::time::Duration {
            match source {
                SourceKind::Display(_) | SourceKind::Window(_) => {
                    std::time::Duration::from_secs(180)
                }
                _ => isocast_engine::OPEN_TIMEOUT,
            }
        }

        fn devices(&self) -> Result<Devices> {
            crate::devices::devices()
        }

        fn permissions(&self) -> Result<PermissionsReport> {
            Ok(crate::devices::permissions_report())
        }

        /// Nothing to ask ahead on Linux: device access is the user's
        /// groups, and screens are asked for when capture starts.
        fn request_permissions(&self, _only: Option<PermissionKind>) -> Result<PermissionsReport> {
            Ok(crate::devices::permissions_report())
        }

        fn open_permission_settings(&self, permission: PermissionKind) -> Result<String> {
            anyhow::bail!(
                "{}: Linux has no settings page for this; `isocast permissions` says what to do",
                permission.label()
            )
        }

        fn media_info(&self, path: &Path) -> Result<MediaInfo> {
            crate::media::media_info(path)
        }

        fn open_media(&self, path: &Path, _part: MediaPart) -> Result<Unsupported> {
            Err(unsupported(&format!(
                "decoding media file {}",
                path.display()
            )))
        }

        fn frame_importer(&self, gpu: &Gpu) -> Result<Importer> {
            Importer::new(gpu)
        }

        fn machine(&self) -> Machine {
            system::machine()
        }

        fn probe(&self, source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
            match source {
                SourceKind::Camera(settings) => crate::camera::probe(
                    &settings.device,
                    CameraRequest {
                        size: settings.width.zip(settings.height),
                        fps: settings.fps,
                    },
                    canvas,
                )
                .map(Some),
                SourceKind::Microphone(settings) => {
                    let microphone = crate::audio::find_microphone(&settings.device)?;
                    let pick = crate::audio::pick_channels(
                        microphone.channels,
                        settings.channels.as_deref(),
                    )?;
                    Ok(Some(Probe {
                        device: microphone.name,
                        video: None,
                        channels: Some(pick.len() as u32),
                    }))
                }
                // Chosen in the desktop's dialog: the size is known once it
                // streams.
                SourceKind::Display(_) | SourceKind::Window(_) => Ok(Some(Probe {
                    device: "a screen chosen in the sharing dialog".into(),
                    video: None,
                    channels: None,
                })),
                SourceKind::ApplicationAudio(settings) => {
                    let app = crate::audio::find_app(&settings.app)?;
                    Ok(Some(Probe {
                        device: app.name,
                        video: None,
                        channels: Some(app.channels.clamp(1, 2)),
                    }))
                }
                SourceKind::SystemAudio(_) => {
                    let output = crate::audio::find_output("default")?;
                    Ok(Some(Probe {
                        device: output.name,
                        video: None,
                        channels: Some(2),
                    }))
                }
                other => STUB.probe(other, canvas),
            }
        }

        fn volume(&self, path: &Path) -> Result<Volume> {
            system::volume(path)
        }

        fn same_volume(&self, left: &Path, right: &Path) -> Result<bool> {
            use std::os::unix::fs::MetadataExt;
            Ok(std::fs::metadata(left)?.dev() == std::fs::metadata(right)?.dev())
        }

        fn process_cpu_seconds(&self) -> Option<f64> {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
            // SAFETY: getrusage fills only the correctly sized output structure.
            if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
                return None;
            }
            // SAFETY: filled in above.
            let usage = unsafe { usage.assume_init() };
            let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
        }

        fn write_latency(&self, probe: &Path) -> Option<std::time::Duration> {
            system::write_latency(probe).ok()
        }

        fn calibration_priority(&self) -> Option<&'static str> {
            // Calibration runs only in a disposable helper; on Linux a
            // thread's nice value is its own, so lower both.
            // SAFETY: changes only this process and the calling thread.
            let process = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 15) };
            let thread = unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as _, 15) };
            (process == 0 && thread == 0).then_some("nice 15")
        }

        fn thread_cpu_time(&self) -> Option<std::time::Duration> {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: writes the calling thread's CPU time into `time`.
            let status = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
            (status == 0).then(|| {
                std::time::Duration::new(time.tv_sec.max(0) as u64, time.tv_nsec.max(0) as u32)
            })
        }

        fn frame_pool(&self, width: u32, height: u32) -> Result<FramePool> {
            FramePool::new(Kind::Bgra, width, height)
        }

        fn ycbcr_frame_pool(&self, width: u32, height: u32) -> Result<FramePool> {
            FramePool::new(Kind::YCbCr, width, height)
        }

        fn frame_size(&self, frame: &Frame) -> (u32, u32) {
            (frame.width(), frame.height())
        }

        fn frame_is_ycbcr(&self, frame: &Frame) -> bool {
            frame.kind() == Kind::YCbCr
        }

        fn shared_surface(
            &self,
            _gpu: &Gpu,
            _width: u32,
            _height: u32,
        ) -> Result<(isocast_surfaces::Surface, wgpu::Texture)> {
            Err(unsupported("shared GPU surfaces"))
        }

        fn encode_jpeg(
            &self,
            frame: &Frame,
            max_width: Option<u32>,
            quality: f32,
        ) -> Result<(Vec<u8>, u32, u32)> {
            crate::jpeg::encode(frame, max_width, quality)
        }

        fn create_writer(&self, spec: &WriterSpec) -> Result<crate::writer::Writer> {
            crate::writer::Writer::create(spec)
        }

        fn recover_file(
            &self,
            folder: &Path,
            name: &std::ffi::OsStr,
        ) -> Result<Option<isocast_engine::RecoveredFile>> {
            crate::writer::recover(folder, name)
        }

        fn capture_screen(
            &self,
            target: ScreenTarget<'_>,
            fps: u32,
            ten_bit: bool,
            cursor: bool,
            sink: VideoSink<Frame>,
        ) -> Result<(Capture, CaptureFormat)> {
            if ten_bit {
                anyhow::bail!("10-bit screen capture isn't supported on Linux yet");
            }
            let platform = *self;
            let (screen, format) =
                crate::screen::capture(target, fps, cursor, sink, move || platform.now())?;
            Ok((Capture::Screen(screen), format))
        }

        fn capture_camera(
            &self,
            device: &str,
            request: CameraRequest,
            canvas: &Canvas,
            sink: VideoSink<Frame>,
        ) -> Result<(Capture, CaptureFormat)> {
            let platform = *self;
            let (camera, format) =
                crate::camera::capture(device, request, canvas, sink, move || platform.now())?;
            Ok((Capture::Camera(camera), format))
        }

        fn capture_microphone(
            &self,
            device: &str,
            channels: Option<&[u32]>,
            sink: AudioSink,
        ) -> Result<(Capture, u32)> {
            let microphone = crate::audio::find_microphone(device)?;
            let pick = crate::audio::pick_channels(microphone.channels, channels)?;
            let platform = *self;
            let (stream, count) = crate::audio::capture(
                crate::audio::Source::Node {
                    name: microphone.id,
                    channels: microphone.channels,
                    pick,
                },
                sink,
                move || platform.now(),
            )?;
            Ok((Capture::Audio(stream), count))
        }

        /// The default output's monitor. Leaving out apps or this process's
        /// own playback isn't supported on Linux yet.
        fn capture_system_audio(
            &self,
            exclude: &[String],
            sink: AudioSink,
            _exclude_own: bool,
        ) -> Result<(Capture, u32)> {
            if !exclude.is_empty() {
                anyhow::bail!(
                    "exclude: leaving apps out of system audio isn't supported on Linux yet"
                );
            }
            let output = crate::audio::find_output("default")?;
            let platform = *self;
            let (stream, count) = crate::audio::capture(
                crate::audio::Source::Monitor { sink: output.id },
                sink,
                move || platform.now(),
            )?;
            Ok((Capture::Audio(stream), count))
        }

        fn play_audio(
            &self,
            device: &str,
            render: AudioRender,
        ) -> Result<(crate::audio::AudioStream, String)> {
            let output = crate::audio::find_output(device)?;
            let stream = crate::audio::play(&output.id, render)?;
            Ok((stream, output.name))
        }

        fn capture_browser(
            &self,
            _page: &BrowserPage<'_>,
            _video: VideoSink<Frame>,
            _audio: Option<AudioSink>,
        ) -> Result<(Capture, CaptureFormat, Arc<dyn BrowserControl>)> {
            Err(unsupported("browser sources"))
        }

        /// The app's playback stream, as it plays (before the output's
        /// volume). A stream the app closes ends the capture.
        fn capture_application_audio(&self, app: &str, sink: AudioSink) -> Result<(Capture, u32)> {
            let app = crate::audio::find_app(app)?;
            let channels = app.channels.max(1);
            let pick = crate::audio::pick_channels(channels, None)?;
            let platform = *self;
            let (stream, count) = crate::audio::capture(
                crate::audio::Source::Node {
                    name: app.serial,
                    channels,
                    pick,
                },
                sink,
                move || platform.now(),
            )?;
            Ok((Capture::Audio(stream), count))
        }

        fn create_video_encoder(&self, _spec: &LiveVideoSpec) -> Result<Unsupported> {
            Err(unsupported("H.264 encoding"))
        }

        fn create_audio_encoder(&self, _spec: &LiveAudioSpec) -> Result<Unsupported> {
            Err(unsupported("AAC encoding"))
        }

        fn has_secret(&self, service: &str, account: &str) -> Result<bool> {
            STUB.has_secret(service, account)
        }

        fn secret(&self, service: &str, account: &str) -> Result<Option<String>> {
            STUB.secret(service, account)
        }

        fn set_secret(&self, service: &str, account: &str, secret: &str) -> Result<()> {
            STUB.set_secret(service, account, secret)
        }

        fn delete_secret(&self, service: &str, account: &str) -> Result<bool> {
            STUB.delete_secret(service, account)
        }
    }
}
