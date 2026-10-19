# Isocast Linux plan

The task queue for the Linux fork of Isocast. Upstream `PLAN.md` stays the
plan for macOS and Windows; this file covers only the Linux port. Agents
follow the loop in `AGENTS.md`, reading this file in place of `PLAN.md`.

Linux notes (decisions, gotchas, environment facts, what's waiting on the
fork owner) go in `docs/notes-linux.md`, not `docs/notes.md`, so merges from
upstream don't conflict in the 8 000-line notes file.

## Goals

1. Isocast records on Linux with the same guarantees as on macOS: crash-safe
   fragmented files, one clock for program and ISOs, honest drop counters.
2. The engine, protocol, CLI, MCP and app stay shared with upstream. Linux
   code lives in `isocast-platform-linux` and in small `cfg(target_os =
   "linux")` seams, so upstream merges stay cheap.
3. GPU-resident media path: capture as DMA-BUF, composite in Vulkan, encode
   in hardware. No hot-path CPU pixel readback.

## Ground rules

- Every upstream ground rule in `PLAN.md` and `AGENTS.md` applies
  (clean-room, one method table, ids not names, never lose a recording).
- **macOS and Windows behavior never changes** because of Linux work. A
  shared-code change keeps macOS output byte-identical.
- **Merge upstream `main` between milestones.** Keep Linux diffs to shared
  files small and mechanical.
- **No pushing, publishing or packaging releases** without the fork owner.
- Hosted CI has no GPU. Lavapipe (Mesa software Vulkan) and software
  encoders are allowed only as CI stand-ins; never claim hardware
  qualification from them.

## Starting point (2026-10-18)

- `cargo check --workspace --exclude isocast-app --all-targets` passes on
  Linux. The daemon selects `StubPlatform` on every non-macOS OS
  (`crates/isocast-daemon/src/platform.rs`), which records nothing.
- IPC and manifest I/O already use the Unix paths (`cfg(unix)`).
- `isocast-mux` (upstream W.2) is a portable crash-safe BMFF writer that
  takes encoded packets.
- `isocast-platform-contracts` (upstream W.3a) is the shared qualification
  suite every native backend must pass.

## Technology choices

| Job | Linux |
|---|---|
| Screen and window capture | xdg-desktop-portal ScreenCast + PipeWire (Wayland and X11) |
| Screen permission | Portal dialog; persist the portal `restore_token` |
| Cameras, capture cards | V4L2 (UVC) |
| Mic, system audio, output | PipeWire capture, monitor and playback nodes |
| GPU | wgpu Vulkan; DMA-BUF import via `VK_EXT_external_memory_dma_buf` |
| Encode | libavcodec, dynamically linked: VA-API, NVENC, then an LGPL software encoder |
| File writing | `isocast-mux` + `fsync`, `renameat2(RENAME_NOREPLACE)`, `O_NOFOLLOW`, `flock` |
| Media reading | libavformat / libavcodec |
| Clock | `CLOCK_MONOTONIC` (PipeWire timestamps use it too) |
| Shared surfaces | DMA-BUF fds over the Unix socket (`SCM_RIGHTS`) + sync_file fences |
| Global hotkeys | Portal GlobalShortcuts |
| Secrets | Secret Service over D-Bus |
| Plugin sandbox | Landlock + seccomp |
| Paths | XDG base directories |
| Crash reports | `coredumpctl` |
| Virtual camera | PipeWire video source node; v4l2loopback as fallback |

## Task queue

### L. Linux port

- [x] **L.0 Linux build baseline.**
  - [x] **L.0a Local gates.** `cargo build`, `cargo clippy --all-targets
    -- -D warnings` and `cargo test` for every crate that builds on Linux
    (the set upstream builds for Windows, plus `isocast-surfaces`). Fix
    Linux-only failures without changing macOS code paths. Record the
    exact crate set and any tests that need a GPU or display.
  - [x] **L.0b Linux CI workflow.** `.github/workflows/linux.yml` running
    L.0a's gates on `ubuntu-latest`. Done when a hosted run on the fork is
    green (needs the fork owner to push).
- [x] **L.1 Portable paths and services.** Same output on macOS, Linux
  defaults on Linux.
  - [x] **L.1a XDG paths.** One helper for data, log and preferences
    directories; replace the `~/Library` paths in `isocast-protocol`
    (`api.rs`, `obs.rs`), `isocast-diagnostics` and `isocast-ui/prefs.rs`.
  - [x] **L.1b Daemon launch.** The CLI finds and spawns `isocastd` on
    Linux without the signed dev app; `isocast status` and `isocast
    restart` work against the stub.
  - [x] **L.1c Diagnostics.** Crash reports from `coredumpctl` in the bug
    report bundle; process memory from `/proc` in developer mode. Machine
    facts (chip, RAM, CPU time, volumes) are `Platform` methods and move to
    L.2.
  - [x] **L.1d Plugin sandbox.** Landlock (ABI 10) for process plugins,
    reported honestly when the kernel lacks it. Plugin peer and process
    identity from `SO_PEERCRED` and `/proc`.
  - [x] **L.1e Linux recordings folder.** Default to `~/Videos/Isocast`
    instead of `~/Movies/Isocast` on Linux, in new setups and profiles and
    the OBS-import fallback. Help and MCP text that names `~/Movies` moves
    to the L.6 wording pass.
- [x] **L.2 Linux synthetic platform.** `isocast-platform-linux` with the
  monotonic clock, machine facts, volumes, a Vulkan compositor and
  `isocast-mux` writing with Linux durability. Done when a synthetic
  program plus ISOs produce decodable, aligned files here and under
  lavapipe, and the core contract suite passes.
  - [x] **L.2a Platform skeleton.** `LinuxPlatform` selected by the daemon
    and the contract harness: `CLOCK_MONOTONIC` clock, absolute
    `clock_nanosleep` deadlines, machine facts from `/proc` and
    `/etc/os-release`, `statfs` volumes, CPU times, write latency. The rest
    delegates to the stub until replaced.
  - [x] **L.2b Vulkan compositing.** wgpu `vulkan` on Linux; a frame
    importer for CPU frames (upload) and Y'CbCr pools, so the engine
    composites; JPEG monitors. `gpu` contract passes; `isocast frame`
    saves the program.
  - [x] **L.2c Encode and write.** libavcodec H.264/HEVC on VA-API (libx264
    fallback for H.264) and AAC into `isocast-mux`, video on a thread per
    file. A synthetic program and ISOs record complete, decodable, aligned
    files when compositing keeps up (see L.2e).
  - [x] **L.2d Recovery and contracts.** `recover_file` through the mux's
    repair on Linux (its recovery module is Windows-only today), launch
    recovery of killed takes, and the `normal`, `pressure`, `commit`,
    `ownership` and `recover` contracts.
  - [x] **L.2e Discrete GPU frames.** On the RX 5700 XT the compositor
    drawing into and sampling CPU-memory frames over PCIe holds the GPU at
    99% busy and drops about half the frames of a 1080p60 program plus
    ISOs.
    - [x] **L.2e1 Prefer the integrated GPU.** On Linux the engine asks
      wgpu for the low-power adapter, so machines with an iGPU composite
      where CPU memory is local.
    - [x] **L.2e2 GPU-memory frames.** On a discrete GPU, Y'CbCr output
      frames get shadow textures in GPU memory that the compositor draws
      into; `Frame::sync` copies them down once when a CPU consumer (the
      encoder, JPEG monitors) needs the pixels. DMA-BUF hand-over to VA-API
      without the copy down is left to L.3.
- [ ] **L.3 Encoders.** Program priority and ISO shedding within
  detected session limits; clear errors for unsupported codecs.
  - [x] **L.3a VA-API and libx264.** Done in L.2c: H.264/HEVC on VA-API,
    libx264 for H.264 without it.
  - [ ] **L.3b NVENC.** `h264_nvenc`/`hevc_nvenc` when an NVIDIA GPU is
    present. Needs NVIDIA hardware to qualify (none here).
  - [ ] **L.3c Zero-copy VA-API input.** Shadow frames handed to VA-API on
    the same GPU as DMA-BUF, without the copy down and upload. An
    optimisation: the copy path records 1080p60 + ISOs with 0 drops.
  - [ ] **L.3e Capacity table.** Doctor's and tune's encoder capacity
    estimates assume Apple media engines ("1 engine(s); assumed"); give
    VA-API and NVENC their own entries and calibrate them.
  - [ ] **L.3d LGPL software encoder.** OpenH264 for distribution builds
    whose FFmpeg has no libx264.
- [ ] **L.4 Capture and audio.** Done when a setup with a camera, the
  screen, a microphone and system audio records program and ISOs with
  aligned A/V.
  - [x] **L.4a Devices and permissions.** `devices()` lists V4L2 cameras
    with their formats, PipeWire audio inputs and outputs, and screens
    (through the portal); `permissions()` reports camera device access,
    PipeWire reachability and the ScreenCast portal.
  - [x] **L.4b Cameras.** V4L2 capture (MJPEG, YUYV, NV12) into frames,
    timestamps on `CLOCK_MONOTONIC`.
  - [x] **L.4c Audio.** PipeWire microphone, system audio (monitor of the
    default sink) and playback.
  - [x] **L.4c2 Application audio.** One app's audio (its PipeWire
    stream node); apps with audio listed in `isocast devices`.
  - [ ] **L.4c4 System audio exclusions.** System audio leaving out apps
    or Isocast's own monitoring (capture each other app's stream and mix,
    or a filtered PipeWire graph).
  - [x] **L.4c3 Measured A/V offset.** A clap in front of a camera with its
    microphone: offset between the clap frame and its sound.
  - [x] **L.4d Screens and windows.** xdg-desktop-portal ScreenCast +
    PipeWire (shared-memory BGRx/RGBx copied into frames), restore token
    kept per target, up to 3 minutes for the first dialog.
  - [ ] **L.4d2 DMA-BUF screen frames.** Import the portal's DMA-BUFs into
    Vulkan instead of copying shared memory (4K60 copies cost CPU).
  - [x] **L.4d3 Choose windows in the desktop's dialog.** `screen_pick`
    (CLI `isocast screen pick [--display]`, MCP `pick_screen`) shows the
    portal dialog and keeps the restore token under a new `picked:<id>`
    value, which the source takes as its `app` or `display`, so each
    source keeps its own choice. The app's Window capture form offers
    "Choose a window…" (and opens the dialog as the source is added); the
    Display form offers "Choose a screen…"; the title row is hidden.
  - [ ] **L.4d4 Waiting-for-a-window status.** While a screen source's
    dialog is open, the sources list says it waits for a choice instead
    of showing "opening" over a black frame.
  - [x] **L.4e Hotplug and reconnects.**
- [x] **L.5 Native contract qualification.** Full
  `isocast-platform-contracts` native suite passes on Linux hardware. A
  stub or lavapipe run does not count.
- [ ] **L.6 App and shared surfaces.**
  - [x] **L.6a Shared surfaces.** DMA-BUF surfaces handed over with
    `SCM_RIGHTS`; the app's preview and `isocast frames --shared` work.
  - [x] **L.6b Monitors and projectors.** Every place the app shows
    frames (source monitors, projectors, multiview) checked on Linux.
  - [x] **L.6c Global shortcuts.** The portal's GlobalShortcuts for
    hotkeys.
  - [x] **L.6d Secret Service.** Stream keys and other secrets in the
    desktop's keyring over D-Bus.
  - [x] **L.6e1 Runtime wording.** Doctor, tune, capacity, status, stream
    key, setup init and obs-websocket messages say "this computer", "the
    keyring" and "the desktop asks" on Linux (`isocast_protocol::wording`;
    macOS text unchanged).
  - [ ] **L.6e2 Help, schema and app wording.** CLI help, MCP tool
    descriptions, the JSON Schema's descriptions and the app's `.slint`
    copy; best done with upstream's Windows wording pass (W.6), since the
    same text serves every platform.
- [ ] **L.7 Media, streaming, virtual camera.**
  - [x] **L.7a Live encoders and streaming.** H.264 at a bitrate (CBR or
    average, keyframe interval, low latency) and AAC for streams and the
    H.264 monitors.
  - [x] **L.7b Media reader.** libav decoding for media sources, stingers
    and the editor (`open_media`).
  - [x] **L.7c Virtual camera.** A PipeWire video source node other apps
    can open as a camera.
  - [ ] **L.7d v4l2loopback camera.** For apps that read only V4L2
    devices (Zoom, Discord, OBS): write frames to a v4l2loopback device
    when one is present; installing the module needs root.
  - [ ] **L.7e Browser source.** Web pages as sources need an embedded
    web engine (WebKitGTK or CEF); a decision for the fork owner.
- [ ] **L.8 Packaging.** Flatpak and an AUR package; install, upgrade and
  uninstall verified locally. Publishing waits for the fork owner.
- [ ] **L.9 e2e, stress and alpha.** Port the synthetic e2e and stress
  gates; map `docs/alpha-test.md` to Linux evidence.

## Log

- L.0a (2026-10-18): Linux builds, lints and tests clean with no code
  changes. `cargo fmt --all --check`, `cargo clippy --workspace --exclude
  isocast-app --all-targets -- -D warnings`, `cargo test --workspace
  --exclude isocast-app` (219 tests, 0 failures) and `cargo build` all pass
  on the dev machine. The daemon runs on `StubPlatform`, so nothing records.
- L.0b (2026-10-18, partial): `.github/workflows/linux.yml` written; YAML
  parses and its locked build passes locally. Not yet run on GitHub: needs
  a push to the fork.
- L.1a (2026-10-18): Linux uses XDG folders: data and socket in
  `$XDG_DATA_HOME/isocast`, logs in `$XDG_STATE_HOME/isocast/logs`, UI
  prefs in `$XDG_CONFIG_HOME/isocast`, OBS import from
  `$XDG_CONFIG_HOME/obs-studio`. macOS and Windows paths unchanged.
  Verified with XDG variables pointed at `target/scratch/xdg`: the daemon
  created `isocastd.sock`, `.lock` and `.log` there and the CLI logged to
  `state/isocast/logs`. Protocol, diagnostics and UI clippy and tests pass.
- L.1b (2026-10-18): no code needed. `isocast doctor` launches the adjacent
  `target/debug/isocastd` on Linux, `isocast restart` restarts it, and
  `isocast devices` exits 1 with the stub's "not supported on linux" error.
- L.1c (2026-10-18): bug reports include this user's systemd-coredump
  records for Isocast executables (`linux-crashes/`, newest ten, with the
  stack trace). Hostname, user, machine id, boot id and session-unit lines
  are dropped; the redactor rewrites home paths. Verified by aborting a
  scratch daemon (SIGABRT) and reading the entry back with `isocast
  report-file` from `isocast report --preview`. Developer mode reports
  `VmRSS`: `isocast debug status --json` gave 43 900 928 bytes against
  `VmRSS: 42956 kB` in `/proc` a moment later.
- L.1d (2026-10-18): new crate `isocast-platform-linux` (`plugin` module).
  Process plugins run under Landlock ABI 10 when the kernel has it: reads
  only in system folders, their own folders and granted files; writes only
  in their data folder; Unix sockets only in their own folder; outbound
  TCP/UDP only with a network grant; abstract sockets and signals scoped.
  Recordings `write` grants read only (Landlock can't allow create-only),
  and `plugin add` says so. `peer_pid` (`SO_PEERCRED`) and `process_info`
  (`/proc/<pid>/stat`) replace `None` on Linux, so main-socket connections
  from unsandboxed plugins are held to their grant. Verified with a scratch
  probe plugin: home, `/tmp`, D-Bus, PipeWire, Wayland, outbound TCP/UDP,
  `/dev/video0`, `/dev/snd/controlC0` and signals to isocastd were denied;
  its data folder, `status` on its socket, serving on 127.0.0.1, `/dev/null`
  and `/usr/bin/true` were allowed; with `network` granted TCP and UDP
  connected. The pid file's start time matched `/proc/<pid>/stat`.
- L.1e (2026-10-18): `DEFAULT_FOLDER` is `~/Videos/Isocast` on Linux, and
  an OBS import without a recordings folder falls back to `~/Videos`.
  Verified: `isocast profile new` on the scratch daemon exported
  `folder = "~/Videos/Isocast"`. Protocol clippy and 88 tests pass.
- L.2a (2026-10-18): `LinuxPlatform` replaces the stub on Linux in the
  daemon and contract harness. `isocast doctor` prints `Intel(R) Core(TM)
  Ultra 7 270K Plus · 31 GB · 24 cores · Omarchy, Linux
  7.2.8-7-omarchy-bore`. Native `clock` and `pools` contracts pass (1 ms
  `sleep_until` overshoot 58 µs); `gpu` panics with no wgpu backend, which
  L.2b adds.
- L.2b (2026-10-18): wgpu `vulkan` on Linux. The engine opens the Vulkan
  device with `VK_EXT_external_memory_host` when the adapter has it (Linux
  only; other platforms unchanged). Linux frames are page-aligned,
  `MADV_DONTFORK` CPU memory imported once per buffer; each plane is a
  linear Vulkan image bound to it, with the row pitch the GPU chose, so
  the GPU draws into and samples frames without copies. Own BGRA and
  Y'CbCr pools, importer (cached and uncached), JPEG monitors. Verified on
  the RX 5700 XT (RADV): native `gpu` contract passes (exact pixels through
  import, target and cached paths; pools refill after texture drop), and
  `isocast frame` on a synthetic setup (two test patterns and a text
  source) saved a correct 1920×1080 program at frame 24, T+0.400 s.
- L.2c (2026-10-18): Linux records. `LinuxPlatform::create_writer` opens
  VA-API H.264/HEVC on the first working render node (libx264 for H.264
  without it) and FFmpeg's AAC, and writes through `isocast-mux`
  (fragments every 500 ms, crash-safe commit). The avcC/hvcC comes from a
  twin encoder's first keyframe, because VA-API's extradata SPS/PPS differ
  from the in-band ones. Video encodes on a thread per file behind a
  4-picture queue. Verified with the release daemon and the synthetic
  setup, compositing on the Intel iGPU (`VK_DRIVER_FILES=.../intel_icd.json`)
  and encoding on the RX 5700 XT: 10 s take, 0 dropped frames, program
  and BARS 600 frames, SMALL 300 frames, AAC 10.0 s each, full decode
  clean, beep onset 0.700021 s against flash 0.700 s. Compositing on the
  RX 5700 XT drops about half (L.2e). `doctor` lists `H.264 (VA-API,
  /dev/dri/renderD129), HEVC (VA-API, /dev/dri/renderD129)`.
- L.2d (2026-10-18): killed takes are finished at the next launch.
  `recover_file` repairs through `isocast-mux`'s `mp4::recover` using a new
  `Directory::for_repair_under_caller_lock` (the engine already holds the
  folder's lock exclusively for the pass). `media_info` reads files with
  libavformat. The encoder converts BGRA frames to NV12 with libswscale,
  skips draining an encoder that never got a picture (that crashed in
  libavcodec 63), and pictures sit on the frame grid. Verified: a take
  killed with `kill -9` after 4 s came back at the next launch with all
  three files finished from 8 fragments (209/209/104 pictures, 3.48 s),
  full decode clean, manifest `interrupted` with probed durations;
  `scripts/platform-contracts.py --backend native --suite media --hardware`
  passes clock, pools, gpu, normal, pressure, ownership and recovery on the
  RX 5700 XT; a 6 s synthetic take stored exactly 360/360/180 frames,
  6.000000 s, 0 dropped.
- L.2e1 (2026-10-18): the Linux engine prefers the low-power (integrated)
  adapter. Verified: with no override the daemon logged `GPU: Intel(R)
  Graphics (ARL)` and a 10 s synthetic take recorded 0 dropped frames,
  encoding on the RX 5700 XT's VA-API.
- L.0b (2026-10-18): Linux CI is green on the fork (run 39122298521,
  15m44s): fmt, build, clippy and tests for the workspace without
  `isocast-ui`, then clippy on `isocast-ui` alone. Building `isocast-ui`
  peaks at 13.0 GB in one rustc (clippy 8.3 GB), which killed the 16 GB
  runner (exit 143, "lost communication"); its 13 tests don't run on CI.
  The job frees disk and adds 10 GB of swap first.
- L.2e2 (2026-10-18): an experiment on the RX 5700 XT isolated the cost:
  with Y'CbCr render targets in GPU memory the GPU fell from 99% to 22%
  busy and drops from 496 to 0; sampling CPU-memory sources cost nothing
  measurable. So on a discrete GPU, Y'CbCr pool frames get shadow textures
  in GPU memory, and consumers call `Frame::sync` to copy them down once.
  Verified with the compositor forced onto the RX 5700 XT
  (`VK_DRIVER_FILES=.../radeon_icd.json`): 10 s synthetic take, 0 dropped
  (was 851), 38% busy, 600/600/300 frames, clean decode, correct picture,
  beep 0.883354 s against flash 0.883333 s. The default iGPU path still
  records 0 dropped. The native media contract suite passes on both GPUs.
  L.2 is done.
- L.4a (2026-10-18): `isocast devices` lists V4L2 cameras (by-id ids,
  every format and frame rate; metadata nodes left out) and PipeWire
  microphones and outputs with the defaults marked; `isocast permissions`
  reports camera node access, PipeWire, and that screens go through the
  portal (not built). Verified here: the Logitech BRIO (and its IR node),
  the BRIO and ACE microphones and the ACE output, defaults matching
  `wpctl status`.
- L.4b (2026-10-18): V4L2 cameras record. Format choice follows macOS's
  (rate up to the canvas, largest fitting size, orientation, then NV12 >
  YUYV > MJPEG); MJPEG decodes with libavcodec and YUYV/MJPEG convert to
  Rec. 709 video-range NV12 with libswscale into CPU-written frames;
  buffer timestamps are `CLOCK_MONOTONIC`. Verified with the Logitech BRIO
  (with the owner's go-ahead): doctor chose 1920×1080 @ 30 MJPEG on a
  1080p30 canvas; a 20 s take recorded program 600 frames and FACECAM 579
  frames (30.0 fps over 19.3 s, start offset 0.7 s in the manifest), 0
  dropped, full decode clean, correct picture.
- L.4c (2026-10-18): microphones and system audio record through PipeWire
  streams (48 kHz float, the device's own channels, the engine picks one
  or two), timed from the stream's graph clock (`CLOCK_MONOTONIC` now, less
  delay and buffer length); monitors play through a PipeWire output
  stream. Verified: 8 s take with the BRIO camera, BRIO microphone and
  system audio, 0 dropped; a -20 dB 1 kHz tone played through the default
  output during the take is in the program's track from 2.84 s to 3.88 s;
  the BRIO microphone ISO track's level (mean -81.3 dB) matches a
  `pw-record` of the same input (-80.8 dB).

- L.4d (2026-10-18): screens record through the ScreenCast portal and
  PipeWire. The first take shows the desktop's sharing dialog; its restore
  token is kept in `screencast-tokens.json` in the data folder, so later
  takes start without it. `Platform::open_timeout` (new, default 8 s)
  gives screens 3 minutes on Linux for the dialog. Verified with the owner
  picking in Hyprland's picker: 10 s take, 300/300 frames, 0 dropped,
  correct picture (the chosen 1730×2080 area, letterboxed in the
  program); a later take restored without the dialog (`start` returned in
  0.6 s), 150/150 frames over 5.0 s.
- L.4c3 (2026-10-18): clap test with the Logitech BRIO camera and its
  microphone (1080p30 MJPEG): the sound starts 0.740 s and 1.272 s into
  the camera ISO; the hands first touch on the frames at about 0.833 s and
  1.367 s (apart one frame earlier). The picture trails the sound by about
  60–95 ms: V4L2 stamps buffers when they arrive, after the camera's own
  exposure and MJPEG pipeline. Not compensated in code; a setup lines them
  up with `sources[].audio.offset_ms = 80` on the microphone, as in OBS.
- L.4c2 (2026-10-18): `application_audio` sources capture the app's
  PipeWire playback stream (by `object.serial`); `isocast devices` lists
  apps playing audio. Verified with `pw-play` playing a 440 Hz tone: a 6 s
  take's track measures -54.1 dB RMS each second, the same as the file
  played.
- L.4e (2026-10-18): unplugging a device stops its capture with a clear
  reason and the engine reopens it (every 5 s while idle; a take in
  progress keeps the file ended early, as on macOS). Named microphones and
  app streams are `node.dont-reconnect`, so PipeWire can't move them to
  another input; system audio still follows the default output. Verified
  by unplugging the BRIO 3.5 s into a 20 s take: camera ("No such device")
  and microphone ("target not found") stopped in the same millisecond, the
  program recorded all 20 s, both reopened afterwards; an app stream
  ending (`pw-play`) did the same.
  Audio timing: capture times now follow the samples, nudged at most
  200 ppm toward PipeWire's graph time. PipeWire's rate estimate runs up
  to 1.2% off for the first second of a stream, which had shown as 2–6
  audio dropouts in the first take after the engine started. Six 10 s
  BRIO takes (three right after starting the engine) then had 0 dropouts.
- L.5 (2026-10-18): `scripts/platform-contracts.py --backend native
  --suite full --hardware` passes every case (clock, pools, gpu, normal,
  pressure, ownership, recovery, capture) on this machine, compositing on
  the Intel iGPU and again forced onto the RX 5700 XT. The capture case
  restored the kept ScreenCast token without a dialog: 6 frames, none
  after drop, presentation 1–3 ms before arrival.
- L.6a (2026-10-18): shared surfaces on Linux. The daemon draws monitors
  into linear RGBA Vulkan images exported as DMA-BUFs; a socket in
  `$XDG_RUNTIME_DIR` (same user only) hands a ring's descriptors to the
  client holding its token, once, with `SCM_RIGHTS`; clients import them
  with wgpu-hal's `texture_from_dmabuf_fd` (devices ask for
  `VULKAN_EXTERNAL_MEMORY_DMA_BUF`). Transport `dmabuf-socket`. Verified:
  `isocast frames program --shared --check --seconds 5` received 154
  frames through 4 shared 1920×1080 surfaces, all 154 read back drawn (0
  black, 0 undrawn), 34.9 ms median after their time; `isocast-ui` shows
  the live program preview at 60 fps (0.4% CPU), where before it said
  shared frames were macOS only.
- L.6d (2026-10-18): secrets live in the desktop keyring through the
  Secret Service (`secret-service` crate, encrypted session), items
  labelled `Isocast: <service> (<account>)` with attributes
  `application=isocast`, `service`, `account`. Verified against GNOME
  Keyring: `isocast stream key set --service twitch` stored a test key
  `secret-tool` read back, and `stream key clear` removed it.
- L.6e1 (2026-10-18): `isocast_protocol::wording` names the computer,
  keyring and permission asker per platform; 27 runtime messages use it
  (macOS output byte-identical). Verified: doctor on Linux reads "this
  computer's encoders…"; an audit confirmed every placeholder sits in a
  formatting macro; workspace tests pass (225).
- L.6c (2026-10-18): global hotkeys through the GlobalShortcuts portal.
  Isocast registers as host app `dev.isocast.Isocast`, which the portal
  only accepts with a matching desktop entry
  (`scripts/linux-dev-install.sh` installs one for the dev build; the
  owner approved installing it here). Each binding is a shortcut with its
  chord as the preferred trigger; a binding the desktop gave no keys says
  how to assign them. Verified on Hyprland: `hyprctl globalshortcuts`
  listed `dev.isocast.Isocast:isotest`, and `hyprctl dispatch
  'hl.dsp.global("dev.isocast.Isocast:isotest")'` produced `hotkey_fired`
  (studio.toggle, source global, press).
- L.6b (2026-10-18): source and scene monitors (what the app's monitors
  and projectors use) share frames on Linux: `isocast frames small` and
  `scene:main` with `--shared --check` each received 95 frames through
  `dmabuf-socket`, all read back drawn.
- L.7a (2026-10-18): live encoders: libavcodec H.264 with bitrate rate
  control (VA-API CBR/VBR, libx264 nal-hrd CBR, zerolatency/async_depth 1
  for monitors), parameter sets in-band and split out on keyframes, and
  AAC at the stream's bitrate. Verified: `isocast frames program` saved
  149 frames of 1920×1080 High-profile H.264 that decode cleanly; a stream
  to a local RTMP listener (`ffmpeg -listen 1`) delivered 347 H.264 frames
  and 272 AAC frames (5.8 s at 60 fps), 0 dropped, 0 reconnects; the test
  key was removed from the keyring after.
- L.7b (2026-10-18): `open_media` decodes with libav: video into CPU
  frames (NV12, or BGRA for alpha or odd sizes) through libswscale with
  the file's colour space and range, audio resampled to 48 kHz stereo
  float; frame-threaded decoding. Verified: a media source looping an
  earlier synthetic ISO recorded a 5.0 s program (300 frames, 5.0 s AAC)
  with the right picture, and the clip's beeps and burnt-in clock agree
  (both 0.483 s from program time).
- L.7c (2026-10-18): the virtual camera is a PipeWire node
  `isocast-virtual-camera` ("Isocast Virtual Camera", `Video/Source`,
  role Camera), driving its own clock and taking BGRx at the camera's size
  and rate; frames reach its loop thread through a 2-deep queue (full: the
  frame counts as dropped). Negotiation needed what `pipewire-vircam`
  does: no `ALLOC_BUFFERS`, no `dataType` in the Buffers reply, a Header
  meta. Verified: GStreamer `pipewiresrc target-object=isocast-virtual-
  camera` read 30 frames of the live program (correct picture, 60 fps
  counter); 462 frames sent, 0 dropped; stopping removes the node.
- L.4d3 (2026-10-19): window and display sources are chosen in the
  desktop's sharing dialog from the CLI, MCP and the app's form.
  Verified: `isocast screen pick --json` returned `picked:7d7761049850`
  with `remembered: true`; `isocast record --source
  window:picked:7d7761049850 --seconds 5` then recorded without a dialog
  (3840x2160 HEVC at 60 fps, 5.000 s, 0 dropped). The app's form was
  built but not clicked through.
