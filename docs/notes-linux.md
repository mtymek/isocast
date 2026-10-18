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
- 2026-10-18: The socket stays in `$XDG_DATA_HOME/isocast`, not
  `$XDG_RUNTIME_DIR`. `lock_path()` and `setup_path()` derive from the
  socket path, and the runtime dir is tmpfs, so moving the socket would
  lose `setup.toml` on reboot. Revisit only if a socket outside the data
  folder is needed (Flatpak may want it).

- 2026-10-18: Plugin sandbox is Landlock only, no seccomp. It needs ABI 10
  (Linux 7.0): ABI 9 adds `RESOLVE_UNIX` (pathname socket connects) and
  ABI 10 adds UDP. On an older kernel plugins run unsandboxed with the
  doctor warning, same as a Mac without `sandbox-exec`. Programs anywhere
  in the system folders may run: the Landlock domain and `no_new_privs`
  are inherited, so running another program gains nothing. Known gaps:
  ICMP ping sockets, and recordings `write` reduced to read.
- 2026-10-18: Linux platform code lives in `isocast-platform-linux`
  (empty on other OSes, so the workspace still builds everywhere).

## Gotchas

- `isocast doctor` on the stub reports `x86_64 (unknown chip) · 0 GB`:
  machine facts are not read on Linux yet (L.1c).
- An external `kill -SEGV` does not crash `isocastd`: wasmtime installs a
  SIGSEGV handler for its traps. Use `kill -ABRT` to produce a test
  coredump.
- `pkill -f 'isocastd ...'` inside a shell command also matches that
  shell's own command line. Kill daemons by pid.
- `cargo clippy` after a small edit can finish in under a second; that is
  incremental checking, not a skipped crate.
