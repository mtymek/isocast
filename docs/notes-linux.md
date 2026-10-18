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

- **Intel VA-API driver (optional).** VA-API fails to initialise on the
  Intel iGPU (`/dev/dri/renderD128`), most likely because
  `intel-media-driver` isn't installed. Encoding uses the AMD GPU, so
  nothing is blocked. To enable it: `sudo pacman -S intel-media-driver`.

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

- 2026-10-18: Linux recordings default to the literal `~/Videos/Isocast`,
  not `$XDG_VIDEOS_DIR` from `user-dirs.dirs`: the default is a constant
  written into setup files, and a localized folder name would make setups
  differ between machines.

- 2026-10-18: `LinuxPlatform` reuses the stub's CPU `Frame`, `FramePool`
  and `Unsupported` types and delegates unbuilt methods to it, so each L.2
  step replaces one piece. The clock is `CLOCK_MONOTONIC` since boot, not a
  process origin, so PipeWire and V4L2 timestamps map 1:1.
- 2026-10-18: `real_time_thread` is a no-op on Linux for now. Raising
  priority needs rtkit over D-Bus or `CAP_SYS_NICE`; revisit when L.4
  measures capture jitter.
- 2026-10-18: FUSE volumes count as local (ntfs-3g and sshfs share one
  magic number); the write-speed test still catches slow ones.

- 2026-10-18: Linux frames are CPU memory the GPU imports with
  `VK_EXT_external_memory_host` (RADV, ANV, NVIDIA and lavapipe have it),
  the closest match to IOSurface: synthetic sources draw on the CPU, the
  compositor draws into the same memory, CPU encoders read it. On a
  discrete GPU the compositor's writes cross PCIe. DMA-BUF frames
  (PipeWire capture, VA-API) come in L.3/L.4 as another frame kind.
- 2026-10-18: Frame images start `UNINITIALIZED` in wgpu (layout
  `UNDEFINED`), which the Vulkan spec lets a driver discard. Mesa keeps
  linear contents; the `gpu` contract's exact-pixel checks catch a driver
  that doesn't.

- 2026-10-18: The software H.264 fallback is libx264 (GPL), opened by
  name at run time from the system FFmpeg. Packaging (L.8) must ship
  against an LGPL FFmpeg or rely on the system's.
- 2026-10-18: Video quality maps Isocast's 1–100 scale to constant QP
  `round(40 - q × 0.25)` (70 → 22) for VA-API CQP and libx264 CRF.

- 2026-10-18: Discrete GPUs composite Y'CbCr outputs into shadow textures
  in GPU memory, copied down on demand (`Frame::sync`). Shader writes into
  CPU memory over PCIe were the bottleneck; GPU copy-engine transfers and
  sampling CPU-memory sources are cheap. The engine still prefers the
  integrated GPU on Linux (fewer copies); a discrete-only machine records
  with 0 drops too.

- 2026-10-18: Cameras come from V4L2 (`v4l` crate), audio devices from
  PipeWire's registry (`pipewire` crate); their ids are the
  `/dev/v4l/by-id` name and PipeWire's `node.name`, both stable across
  replugging. The default source and sink come from the `default`
  metadata object.
- 2026-10-18: Capture tests with the BRIO camera, a microphone or the
  screen record the person at the desk, so they wait for the fork owner's
  go-ahead (synthetic sources cover everything else).

- 2026-10-18: Each PipeWire capture or playback stream runs its own main
  loop on its own thread, stopped through a `pipewire::channel`. The
  `pipewire` crate's `v0_3_50` feature is needed for `TARGET_OBJECT` and
  `pw_stream_get_time_n` (Ubuntu 24.04 ships PipeWire 1.0).

- 2026-10-18: Screens come from the ScreenCast portal (`ashpd` on a
  one-worker tokio runtime kept for the session) with PipeWire frames in
  shared memory. The portal decides what is shared; the setup's display
  name and window app can't be matched, so each target key keeps its own
  restore token.

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
- Run the contract harness as `target/debug/isocast-platform-contracts
  native <case> <folder>`, where `<folder>` holds a `.contract-owner` file
  containing `isocast-platform-contracts`.
- A `wgpu::Device` inside a frame type overflows the trait solver when
  isocastd proves `Send`/`Sync` (`E0275` on `naga::ir::ImageClass: Sync`).
  `gpu::Owner` wraps it with explicit `Send`/`Sync`.
- `target/scratch/setups/synthetic.toml` is a permission-free setup (two
  test patterns, text, tone) for compositing and recording checks. Apply
  it to a scratch daemon with `isocast apply --revision 0 <file>`.
- FFmpeg's VA-API H.264 extradata (global header) has different SPS VUI
  and PPS bytes from the parameter sets sent in-band with the first IDR;
  the mux refuses the mismatch (`video parameter sets changed`). The writer
  takes the in-band sets from a twin encoder's first picture.
- With frames in CPU memory, a discrete GPU compositing 1080p60 plus ISOs
  sits at 99% busy (`/sys/class/drm/card2/device/gpu_busy_percent`) and
  drops about half the frames. `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/intel_icd.json`
  makes wgpu use the Intel iGPU instead, which records with 0 drops.
- `isocast debug on` refuses until a live setup exists; apply one first.
- `avcodec_send_frame(NULL)` on a VA-API encoder that never received a
  picture segfaults in libavcodec 63 (FFmpeg 9.0.2). `VideoEncoder::flush`
  skips draining when nothing was sent.
- The media contracts need stored picture times on the exact 1/fps grid;
  the writer snaps each picture to its nearest frame slot.
- Run the native contracts with
  `python3 scripts/platform-contracts.py --bin target/release/isocast-platform-contracts --work <new dir> --backend native --suite media --hardware`.
- `pgrep -f`/`pkill -f` with a path in the pattern also matches the shell
  running the command. Use `pgrep -x isocastd`.
- Building `isocast-ui` peaks at 13.0 GB of memory in one rustc (debug,
  no debug info); clippy on it at 8.3 GB; isocastd and isocast 1.5 GB each.
  On a 16 GB GitHub runner, building it beside other crates kills the
  runner (exit 143 or "lost communication").
- `jj new -m ...` run right after other jj commands sometimes left an
  empty undescribed change in the chain, and `jj git push` refuses those.
  Check `jj log -r 'main@origin::'` before pushing.

- Hyprland's screen-sharing picker gives a restore token only when its
  "Allow a restore token" option is ticked; without it every take shows
  the dialog.
- The engine gave every source 8 s to open, which the first-time portal
  dialog can exceed; `Platform::open_timeout` now lets Linux give screens
  3 minutes.
- A UVC camera's picture trails its microphone by about 60–95 ms (Logitech
  BRIO, 1080p30 MJPEG, clap test): V4L2 timestamps mark buffer arrival.
  Use `audio.offset_ms` on the microphone (about 80) to line them up.
