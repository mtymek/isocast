# Linux notes

Decisions, gotchas and environment facts for the Linux fork. The queue is
in `PLAN_LINUX.md`.

## Environment

- Dev machine (2026-10-18): Arch-based Omarchy, kernel 7.2, Hyprland
  (Wayland). Portal backend: xdg-desktop-portal-hyprland.

## Waiting on the fork owner

## Decisions

- 2026-10-18: Linux notes live here instead of `docs/notes.md`, so upstream
  merges don't conflict in that file.
- 2026-10-18: Encoders through dynamically linked libavcodec (one API for
  VA-API, NVENC and software; same library reads media). Never build
  against a `--enable-gpl` FFmpeg: shipped binaries would become GPL.
- 2026-10-18: Wayland + xdg-desktop-portal first. Plain X11 without a
  portal is unsupported until someone needs it.

## Gotchas
