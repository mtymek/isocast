//! Linux desktop capture, Vulkan compositing and H.264 encoding.
#![cfg(target_os = "linux")]

mod audio;
mod browser;
mod camera;
mod encoder;
pub mod frames;
pub mod hotkeys;
mod media;
pub mod network;
mod nvenc;
mod pipewire_audio;
mod pipewire_video;
mod platform;
mod playback;
pub mod plugin;
mod portal;
mod pw_loop;
mod system;
mod video_encoder;
mod writer;

pub use platform::LinuxPlatform;
