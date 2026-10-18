//! What media files hold, read with libavformat.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ffmpeg_next::{codec, format, media};
use isocast_engine::MediaInfo;

/// The picture size, whether there is sound, and the duration of `path`.
pub fn media_info(path: &Path) -> Result<MediaInfo> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    ffmpeg_next::init().context("couldn't start FFmpeg")?;
    let input = format::input(path).with_context(|| format!("couldn't open {}", path.display()))?;
    let video = match input.streams().best(media::Type::Video) {
        Some(stream) => {
            let decoder = codec::context::Context::from_parameters(stream.parameters())?
                .decoder()
                .video()?;
            Some((decoder.width(), decoder.height()))
        }
        None => None,
    };
    let audio = input.streams().best(media::Type::Audio).is_some();
    if video.is_none() && !audio {
        bail!("{} has no video or audio FFmpeg can read", path.display());
    }
    let micros = input.duration().max(0) as u64;
    Ok(MediaInfo {
        video,
        audio,
        duration: Duration::from_micros(micros),
    })
}
