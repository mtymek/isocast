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

- [ ] **L.0 Linux build baseline.**
  - [x] **L.0a Local gates.** `cargo build`, `cargo clippy --all-targets
    -- -D warnings` and `cargo test` for every crate that builds on Linux
    (the set upstream builds for Windows, plus `isocast-surfaces`). Fix
    Linux-only failures without changing macOS code paths. Record the
    exact crate set and any tests that need a GPU or display.
  - [ ] **L.0b Linux CI workflow.** `.github/workflows/linux.yml` running
    L.0a's gates on `ubuntu-latest`. Done when a hosted run on the fork is
    green (needs the fork owner to push).
- [ ] **L.1 Portable paths and services.** Same output on macOS, Linux
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
  - [ ] **L.1d Plugin sandbox.** Landlock + seccomp for process plugins,
    reported honestly when the kernel lacks Landlock.
- [ ] **L.2 Linux synthetic platform.** `isocast-platform-linux` with the
  monotonic clock, machine facts from `/proc` and `/sys`, volumes via
  `statvfs`, bounded frame pools, wgpu Vulkan backend, frame importer,
  synthetic sources and `isocast-mux` writing with Linux durability. Done
  when a synthetic program plus ISOs produce decodable, aligned files here
  and under lavapipe, and the core contract suite passes.
- [ ] **L.3 Encoders.** libavcodec VA-API and NVENC, LGPL software last;
  DMA-BUF input where the driver allows; program priority and ISO shedding
  within detected session limits; clear errors for unsupported codecs.
- [ ] **L.4 Capture and audio.** Portal/PipeWire screen and window capture
  as DMA-BUF, V4L2 cameras, PipeWire mic/loopback/output; `permissions()`
  and `request_permissions()` mapped onto the portal; hotplug and
  reconnects.
- [ ] **L.5 Native contract qualification.** Full
  `isocast-platform-contracts` native suite passes on Linux hardware. A
  stub or lavapipe run does not count.
- [ ] **L.6 App and shared surfaces.** Slint on Wayland and X11, DMA-BUF
  previews and monitors, GlobalShortcuts, Secret Service, Linux wording in
  onboarding, settings, errors and shortcut labels.
- [ ] **L.7 Media, streaming, virtual camera.** libav media reader for
  media sources and the editor, streaming through the existing path,
  PipeWire virtual camera.
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
