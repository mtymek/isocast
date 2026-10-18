# Linux notes

Decisions, gotchas and environment facts for the Linux fork. The queue is
in `PLAN_LINUX.md`.

## Environment

- Dev machine (2026-10-18): Arch-based Omarchy, kernel 7.2, Hyprland
  (Wayland). Portal backend: xdg-desktop-portal-hyprland.
- GPUs: AMD Radeon RX 5700 XT (RADV) and Intel Arrow Lake iGPU (Mesa).
  System FFmpeg exposes `h264_vaapi`, `hevc_vaapi`, `av1_vaapi` and the
  NVENC encoders (no NVIDIA card here). Audio: PipeWire 1.6.9.
- Remotes: `origin` is the fork (`git@github.com:mtymek/isocast.git`),
  `upstream` is `git@github.com:R44VC0RP/isocast.git`. jj `trunk()` is
  `main@origin`.
- Linux binaries link only OpenSSL (`isocastd`) and fontconfig/freetype
  (`isocast-ui`) beyond libc.

## Waiting on the fork owner

- **Push to run Linux CI (blocks L.0b).** `jj bookmark move main --to <latest Linux commit>`
  (or a `linux/*` bookmark), then `jj git push -b main`. The fork also
  inherits upstream's macOS, release and benchmark workflows; disable them
  in the fork's Actions settings if you don't want them to run or fail on
  missing secrets.

## Decisions

- 2026-10-18: Linux notes live here instead of `docs/notes.md`, so upstream
  merges don't conflict in that file.
- 2026-10-18: Encoders through dynamically linked libavcodec (one API for
  VA-API, NVENC and software; same library reads media). Never build
  against a `--enable-gpl` FFmpeg: shipped binaries would become GPL.
- 2026-10-18: Wayland + xdg-desktop-portal first. Plain X11 without a
  portal is unsupported until someone needs it.

## Gotchas
