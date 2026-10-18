//! Linux back end for Isocast. Empty on other systems, so the workspace
//! still builds everywhere.
//!
//! Built up in steps (`PLAN_LINUX.md`, L.2 onwards). What isn't Linux's own
//! yet is the stub platform's: CPU BGRA frames, and a clear error for
//! everything it can't do.

#[cfg(target_os = "linux")]
pub mod plugin;
#[cfg(target_os = "linux")]
mod system;

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
    use isocast_platform_stub::{Frame, FramePool, StubPlatform, Unsupported};
    use isocast_protocol::api::Machine;
    use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
    use isocast_protocol::{Canvas, SourceKind};

    use crate::system;

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
        type Writer = Unsupported;
        type Capture = Unsupported;
        type Importer = Unsupported;
        type Media = Unsupported;
        type Playback = Unsupported;
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

        fn devices(&self) -> Result<Devices> {
            STUB.devices()
        }

        fn permissions(&self) -> Result<PermissionsReport> {
            STUB.permissions()
        }

        fn request_permissions(&self, only: Option<PermissionKind>) -> Result<PermissionsReport> {
            STUB.request_permissions(only)
        }

        fn open_permission_settings(&self, permission: PermissionKind) -> Result<String> {
            STUB.open_permission_settings(permission)
        }

        fn media_info(&self, path: &Path) -> Result<MediaInfo> {
            STUB.media_info(path)
        }

        fn open_media(&self, path: &Path, part: MediaPart) -> Result<Unsupported> {
            STUB.open_media(path, part)
        }

        fn frame_importer(&self, gpu: &Gpu) -> Result<Unsupported> {
            STUB.frame_importer(gpu)
        }

        fn machine(&self) -> Machine {
            system::machine()
        }

        fn probe(&self, source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
            STUB.probe(source, canvas)
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
            STUB.frame_pool(width, height)
        }

        fn ycbcr_frame_pool(&self, width: u32, height: u32) -> Result<FramePool> {
            STUB.ycbcr_frame_pool(width, height)
        }

        fn frame_size(&self, frame: &Frame) -> (u32, u32) {
            STUB.frame_size(frame)
        }

        fn frame_is_ycbcr(&self, frame: &Frame) -> bool {
            STUB.frame_is_ycbcr(frame)
        }

        fn shared_surface(
            &self,
            gpu: &Gpu,
            width: u32,
            height: u32,
        ) -> Result<(isocast_surfaces::Surface, wgpu::Texture)> {
            STUB.shared_surface(gpu, width, height)
        }

        fn encode_jpeg(
            &self,
            frame: &Frame,
            max_width: Option<u32>,
            quality: f32,
        ) -> Result<(Vec<u8>, u32, u32)> {
            STUB.encode_jpeg(frame, max_width, quality)
        }

        fn create_writer(&self, spec: &WriterSpec) -> Result<Unsupported> {
            STUB.create_writer(spec)
        }

        fn recover_file(
            &self,
            folder: &Path,
            name: &std::ffi::OsStr,
        ) -> Result<Option<isocast_engine::RecoveredFile>> {
            STUB.recover_file(folder, name)
        }

        fn capture_screen(
            &self,
            target: ScreenTarget<'_>,
            fps: u32,
            ten_bit: bool,
            cursor: bool,
            sink: VideoSink<Frame>,
        ) -> Result<(Unsupported, CaptureFormat)> {
            STUB.capture_screen(target, fps, ten_bit, cursor, sink)
        }

        fn capture_camera(
            &self,
            device: &str,
            request: CameraRequest,
            canvas: &Canvas,
            sink: VideoSink<Frame>,
        ) -> Result<(Unsupported, CaptureFormat)> {
            STUB.capture_camera(device, request, canvas, sink)
        }

        fn capture_microphone(
            &self,
            device: &str,
            channels: Option<&[u32]>,
            sink: AudioSink,
        ) -> Result<(Unsupported, u32)> {
            STUB.capture_microphone(device, channels, sink)
        }

        fn capture_system_audio(
            &self,
            exclude: &[String],
            sink: AudioSink,
            exclude_own: bool,
        ) -> Result<(Unsupported, u32)> {
            STUB.capture_system_audio(exclude, sink, exclude_own)
        }

        fn play_audio(&self, device: &str, render: AudioRender) -> Result<(Unsupported, String)> {
            STUB.play_audio(device, render)
        }

        fn capture_browser(
            &self,
            page: &BrowserPage<'_>,
            video: VideoSink<Frame>,
            audio: Option<AudioSink>,
        ) -> Result<(Unsupported, CaptureFormat, Arc<dyn BrowserControl>)> {
            STUB.capture_browser(page, video, audio)
        }

        fn capture_application_audio(
            &self,
            app: &str,
            sink: AudioSink,
        ) -> Result<(Unsupported, u32)> {
            STUB.capture_application_audio(app, sink)
        }

        fn create_video_encoder(&self, spec: &LiveVideoSpec) -> Result<Unsupported> {
            STUB.create_video_encoder(spec)
        }

        fn create_audio_encoder(&self, spec: &LiveAudioSpec) -> Result<Unsupported> {
            STUB.create_audio_encoder(spec)
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
