# Sidestage design

Sidestage takes the audio of **one app** (usually the music player), moves it to a virtual position
around the listener's head (by default straight behind, 1.5 m away, in a small room) and plays that
binaural version to the headphones. The game's audio is never captured, routed or processed, so it
keeps its exact timing and positional cues.

> Music moves behind you. Your game audio stays exactly as it is.

This document describes the code as it is. Where behaviour was only reasoned about and not measured,
it says so. [Testing](#testing) lists what ran on real hardware.

## Crates

| Crate | Role | Depends on |
|---|---|---|
| `sidestage-dsp` | Binaural renderer, presets, lock-free parameter hand-off. Pure Rust, no I/O. | serde |
| `sidestage-platform` | Per-OS capture, hiding and output behind the `Platform` / `Session` traits; the drift bridge; crash journals | `windows` (Windows), `objc2-core-audio` and friends (macOS), `pulseaudio` (Linux), rtrb, directories |
| `sidestage` | eframe/egui window (glow renderer) and command line; wraps a `Spatializer` in a `ProcessorFactory` | both of the above |

`sidestage-platform` does not depend on `sidestage-dsp`. The app hands the backend a
`ProcessorFactory` (`Fn(sample_rate) -> Box<dyn Processor>`), so each side builds and tests alone.
`crates/sidestage/src/processor.rs` is the whole glue.

## Threads

```
 UI thread (egui / winit)        engine thread (sidestage)            backend threads (per OS)
 ┌──────────────────────┐ Desired ┌───────────────────────────┐ calls ┌──────────────────────────┐
 │ radar, presets,      ├────────►│ owns Box<dyn Platform>    ├──────►│ control / worker threads │
 │ app picker, cards    │◄────────┤ and the running Session   │◄──────┤ (COM MTA, Core Audio,    │
 │                      │Snapshot │ polls apps 1 s, outputs + │status │ Pulse connections)       │
 │                      │         │ support 5 s, status 10 Hz │       └────────────┬─────────────┘
 │ Settings ────────────┼─── SharedParams (seqlock of atomics) ───────► real-time audio thread:
 │ meters ◄─────────────┼─── Meters (peak-hold atomics) ◄────────────── Processor::process
 └──────────────────────┘                                              (sidestage_dsp::Spatializer)
```

* **UI thread.** Never calls the platform. It sends what the user wants (`engine::Desired`: power,
  app, output, hide method, sidechain) and draws a `Snapshot`. Sound settings skip the engine: the
  UI writes them straight into the `SharedParams` the renderer reads.
* **Engine thread** (`crates/sidestage/src/engine.rs`). Creates the platform on its own thread,
  calls `recover()` once, then reconciles: it starts the session when the chosen app appears in
  `list_apps()`, restarts it when the output, hide method or sidechain changes, and stops it
  (restoring the original) on power off, window close, `Drop` and when the engine thread panics. A
  failed start, or a session that ends in `Error` or an unexpected `Stopped`, is stopped and retried
  after 5, 15 and 45 s. After that the error stays until the user retries or changes something.
  Attempts keep counting until a session has run for 30 s, so a backend that fails right after every
  start is not restarted forever. An app has to be gone from the list for 3 s before it gets a fresh
  set of attempts. `support()` is re-read every 5 s and after every failure, so a macOS permission
  denial shows up as the "Allow audio capture" card.
* **Backend threads.** Each backend does its OS work on threads it owns and keeps the caller's wait
  short (see the table below). The real-time thread runs the `Processor`.

`Session::status()` hands out `capture_peak` and `sidechain_peak` as "peak since the last call" on
Windows and macOS, so exactly one caller polls it: the engine in the window, or the `run` loop on
the command line.

## The platform contract

`crates/sidestage-platform/src/lib.rs` is the contract. What each backend puts behind it:

| | Windows | macOS | Linux |
|---|---|---|---|
| `AppKey` | Lowercase full path of the app's root executable | Bundle ID of the outermost `.app` (covers helpers), else the executable path | `application.process.binary`, else `application.name`, else `pid:N` |
| `OutputDevice.id` | MMDevice endpoint ID | Device UID | Sink name |
| Factory sample rate | Output device's mix rate | Aggregate device's nominal rate (rebuilt on rate change) | Always 48000 |
| Factory called on | Session supervisor thread | Session control thread | The caller of `start()`, then the session thread on restarts |
| Hide methods offered | Auto, KeepOriginal | Auto, KeepOriginal | Auto only (others fall back to Auto with a warning) |
| `ParkOn` | Refused by `start()` | Not supported: mutes and warns | Falls back to Auto with a warning |
| `list_apps` / `list_outputs` wait | 150 ms, then the previous answer (first call up to 3 s) | 150 ms, then the previous answer (first call up to 2 s) | Synchronous, 750 ms reply timeout |
| `start()` returns | Within 1.5 s; a slow start continues as `Starting` | At once, in `Starting` | After setup (connect, sweep, load the sink, wait up to 2 s for it, engine) |
| `recover()` | Restores journaled volumes, waits at most 3 s, also logs each line | Always empty: nothing outlives the process | Startup sweep plus the journal |
| Crash helper | Guardian (`windows-guardian`) | None | Watchdog (`linux-watchdog`) |
| Clock drift | Bridge with drift control | One clock, no bridge | Bridge with drift control |

`ParkOn` (sending the app to an output nobody listens to) is in the contract but no backend offers
it. On Windows it would need the undocumented `IAudioPolicyConfig`, and resetting the route does not
move streams the app opened while parked (measured on build 26200), so the app could keep playing to
the unlistened output after Sidestage exits.

Helpers are this same executable started as `sidestage --sidestage-helper <kind> ...`. `main` calls
`sidestage_platform::run_helper_if_requested()` before it parses arguments, sets up logging or takes
the instance lock.

## Per-OS strategy

### Windows 10 2004+ / 11: WASAPI process loopback

* **Support.** Builds below 19041 are refused. Builds 19041 to 20347 (all of Windows 10) run with a
  warning that capturing one app is not officially supported before build 20348.
* **Capture** (`windows/capture.rs`).
  `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK)` with
  `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` on the app's **root** process, the highest
  ancestor with the same executable, so helper processes such as Chromium's audio service are
  included. The format is ours to choose: float32 stereo at the output device's rate, with
  `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`. `GetMixFormat`, `GetDevicePeriod` and `GetCurrentPadding`
  return `E_NOTIMPL`. Packets are always 10 ms, the queue holds only about 3, and a late reader
  loses audio silently, so the capture thread (MTA, MMCSS "Pro Audio") only copies into the bridge
  and fills gaps it sees in the QPC timestamps with silence. The stream never ends; it delivers
  zeros when the app is silent or gone.
* **Hiding the original ("hush", `windows/hush.rs`).** Process loopback taps the app *after* its
  session volume, and that volume is an exact linear gain (measured down to 1e-7). Sidestage writes
  every session of the app, on every endpoint, to the `windows-hush` journal first. It then unmutes
  them and sets them to 1e-6 (-120 dB), with a private event-context GUID. The capture is multiplied
  by `desired / read-back volume`, capped at 1e7. A session whose read-back is 0 is set again, then
  stepped to 1e-5 and 1e-4. A block that would come out above +12 dBFS is replaced by silence and
  triggers a re-hide.
* **Foreign volume changes.** `IAudioSessionEvents::OnSimpleVolumeChanged` with any other event
  context means the user moved the mixer slider or the app changed its own volume. The output gate
  closes at once (5 ms fade), the new value becomes the desired music level, and after a 150 ms
  quiet period (2 s for an app that keeps resetting its volume) the app is turned down again and the
  output fades back in over 20 ms. A slider drag is not mistaken for an app fighting back; four
  resets right after a hide raise the "keeps changing its own volume" warning.
* **Sessions that come and go.** New sessions of the app are hidden as soon as Windows reports them.
  Windows gives a new session the volume of the app's last session that closed (measured), so a
  session of *another* copy of the app that starts at exactly one of Sidestage's hush levels gets
  its volume back (volume only; a 0 the user set stays). `stop()` restores every live session. If
  the attached copy has had no audio session for 2 s and another copy has one, the session switches
  to that copy. Only one hidden session per process is allowed; the journal has one owner.
* **Output** (`windows/render.rs`). Our own shared-mode, event-driven WASAPI render thread (not
  cpal). Its mixer entry is named "Sidestage" and opts out of communications ducking. It follows the
  default device when no output is chosen and falls back to the default (with a warning) when the
  chosen one is missing. The drift bridge sits between the capture and render clocks; its target
  fill is one capture packet plus one output period plus margin (at least 30 ms). In the soak tests
  the status reported about 62 ms of added latency.
* **Sidechain.** A second process-loopback client in `EXCLUDE` mode on Sidestage's own process tree
  hears everything except Sidestage. The hidden music contributes nothing audible (it is at -120
  dB). With KeepOriginal it also hears the music, and the status says so.
* **Guardian** (`windows/guardian.rs`). When it first writes the journal, Sidestage starts
  `--sidestage-helper windows-guardian-launch`, which starts the guardian detached
  (`DETACHED_PROCESS`, a new process group, outside Sidestage's job object when the job allows it)
  and exits. The guardian is therefore not in Sidestage's process tree, and Task Manager's "End
  process tree" does not take it down with Sidestage. It knows Sidestage's exact identity (PID plus
  creation time) and never acts while that process is alive. When Sidestage exits, it restores the
  journal (measured 28-29 ms after a tree kill). If the music app is not running, it waits until the
  volume is restored, another Sidestage takes the journal over, or 7 days pass. New audio sessions
  and new outputs wake it, with polling as a fallback. A failed guardian start is retried at the
  next journal write, at most every 30 s. It logs to `windows-guardian.log` in the state directory.
* **Journal safety.** Every read-and-rewrite of the journal takes a named mutex
  (`Local\Sidestage-<FNV hash of the journal path>`) shared by Sidestage and guardians; a wait is
  limited to 2 s. An entry for a program that is gone is dropped only if its drive is present and
  the file is definitely missing, so an app on an unplugged drive is restored when the drive
  returns. Entries that could not be restored for 30 days are dropped.
* **Platform worker.** List and recovery calls run on one MTA worker. While a journal has pending
  entries, it watches for new audio sessions and puts a leftover volume back the moment its app
  opens audio (measured 4-5 ms).
* **Status warnings** include: the app is muted or at 0 in the mixer, cannot be turned down without
  muting, keeps changing its own volume, Windows communications ducking is active, the chosen output
  is missing, spatial sound (Windows Sonic, Dolby Atmos, DTS) is on, the headset is in hands-free
  mode, Sidestage hears nothing although the app plays (exclusive mode or protected content), and
  more than one copy of the app is running.

### macOS 14.2+: Core Audio process taps

* **Capture and hide.** A private `CATapDescription` over all of the app's process objects with
  `muteBehavior = CATapMutedWhenTapped` (unmuted for KeepOriginal). macOS silences the original
  while the tap exists and un-silences it the moment the tap is destroyed, also when Sidestage
  crashes: private taps and aggregates belong to the process that created them. Nothing outlives the
  process, so there is no journal, no helper and nothing for `recover()` to do. That a SIGKILLed
  Sidestage leaves the app un-muted follows from this; it has not been checked on a Mac.
* **Tap scope.** When the app plays only to the chosen output, the tap is device-scoped to the
  output stream that holds the device's preferred stereo pair. Otherwise, or when the two channels
  sit in different streams, or when the created tap's channel count or rate does not match, it is a
  stereo mixdown tap.
* **Output.** A private aggregate device whose main (clock) sub-device is the headphone output and
  whose tap list holds the tap. One IOProc reads the tap (the trailing input buffers, counted from
  the end) and writes the rendered audio in the same cycle. Both sides run on one clock, so there is
  no ring buffer, resampler or drift control; `drift_ppm` is always 0. `latency_ms` is measured from
  the IO cycle's timestamps and excludes the device's own output latency.
* **Control thread** (`macos/session.rs`). Owns every Core Audio object. Listener blocks on a
  private serial dispatch queue only post events to it, with a 1 s poll as a safety net. `create()`
  sets `kAudioHardwarePropertyRunLoop` to NULL so notifications do not depend on the main run loop.
  The engine is rebuilt when the device, the output or aggregate rate, or the default output
  changes. Rebuilds tear down before building, so the original is briefly audible. A change of the
  tap's own rate only updates a warning. Six failures end the session.
* **Permission.** `NSAudioCaptureUsageDescription` must be in the Info.plist, or macOS never asks
  and the tap delivers silence. `crates/sidestage/build.rs` embeds the plist into the executable
  (`-sectcreate __TEXT __info_plist`), so `cargo run` builds can ask too. The capture calls block
  while the consent prompt is open. After 1.5 s the status warns "macOS may be asking". If they take
  45 s or more the prompt timed out, and the session ends with "quit and reopen". Capture is
  confirmed with Chromium's probe (get and set `kAudioTapPropertyDescription` after
  `AudioDeviceStart`). A failed probe on a device-scoped tap switches the session to mixdown taps;
  permission counts as denied only after two consecutive failures with no earlier success in this
  process. `support()` then reports `NeedsPermission`.
* **Picker.** `list_apps` and `list_outputs` run on a one-job-at-a-time worker. The caller waits 150
  ms (the first call up to 2 s), then gets the previous list; a late answer is handed to the next
  call. The list shows apps that are playing, user-facing apps, and apps that played in the last 15
  minutes.
* **Sidechain.** A second, unmuted global tap that excludes the music app and Sidestage's own
  process object, in a tap-only private aggregate whose IOProc only measures a peak. A value older
  than 250 ms counts as 0.
* **Deployment target.** The process-tap symbols are linked strongly. `build.rs` raises
  `MACOSX_DEPLOYMENT_TARGET` to 14.2 for the `sidestage` binary; CI and `make-app.sh` export it for
  every crate. `Info.plist` sets `LSMinimumSystemVersion` to 14.2.

### Linux: PulseAudio native protocol (PulseAudio or pipewire-pulse)

* **Client.** A pure-Rust native-protocol client over the `pulseaudio` crate's types, with its own
  framing (`linux/conn.rs`). No libpulse at build or run time. The socket and cookie are found the
  way libpulse finds them; a `PULSE_SERVER` with only network entries is refused.
* **Capture and hide.** Load a `module-null-sink` named `sidestage_music_<random hex>` (shown as
  "Sidestage Music Capture"), move the app's sink-inputs onto it, and record its `.monitor` with
  `no_move` (10 ms fragments, 48 kHz float stereo). The supervisor keeps the app's streams on the
  sink and everything else off it. A brand-new stream of the app is heard for about 60 ms (event
  debounce) plus the move, unless a saved route already sends it there. If the capture sink becomes
  the system default, Sidestage sets the previous default back (at most once per 5 s).
* **Output.** A playback stream to the chosen sink (or the default), with a 45 ms buffer, fed from
  the drift bridge (40 ms target). The audio thread asks for `SCHED_FIFO` priority 1 with
  `SCHED_RESET_ON_FORK`. That works only where `RLIMIT_RTPRIO` allows it; there is no rtkit
  fallback.
* **Sidechain.** The output sink's monitor would contain Sidestage's own output, so instead every
  other sink-input on that sink gets a 144 Hz peak-meter record stream (`direct_on_input`,
  `PEAK_DETECT`), as pavucontrol does. The game level is the loudest of them. Games playing to
  another sink are not heard.
* **Undo.** Moves persist in stream-restore or WirePlumber. Streams that were on the default sink
  are moved to the *current* default, which clears the saved route; others go back to their original
  sink. Then the module is unloaded. Unloading checks the module's own arguments, so another app's
  null sink is never removed.
* **Watchdog** (`linux/watchdog.rs`).
  `/proc/self/exe --sidestage-helper linux-watchdog <journal> <session id>` in its own process
  group. It holds the read end of a pipe whose only writer is Sidestage, so any exit (even SIGKILL)
  gives it EOF. It ignores SIGINT, SIGHUP and SIGTERM, so `pkill sidestage` or a logout does not
  kill both. It then restores from the `linux-routing` journal, retrying 10 times over about 5 s.
  Its stdout and stderr go to `/dev/null`, so its log lines are lost.
* **Startup sweep.** `recover()` restores a dead run's journal and removes leftover
  `sidestage_music_*` modules that no live session owns. A second Sidestage process is refused while
  the journal's owner is alive.
* **Failures.** More than 5 engine failures or capture-sink losses per minute end the session. A
  vanished sound server is reconnected for up to 15 s.

## Real-time rules

* `Processor::process` / `Spatializer::process`: no allocation, locks, syscalls, logging or panics.
  A counting-allocator test checks the renderer. Settings arrive through a seqlock of atomics
  (`SharedParams`). The audio thread tries a bounded number of times and otherwise keeps its
  previous settings, so it never waits. Meters go back as peak-hold atomics.
* Backends pass at most `MAX_BLOCK_FRAMES` (4096) frames per call. The renderer accepts any rate
  (its maths is clamped to 8-768 kHz) and any length, including odd and empty blocks.
* Capture threads only copy into the bridge. The bridge (`bridge.rs`) is a lock-free SPSC ring read
  through a Catmull-Rom interpolator whose speed a proportional controller nudges (at most ±1000
  ppm) to hold the fill level, like PulseAudio's module-loopback. In the first seconds after a start
  the correction briefly reads several hundred ppm, then settles.
* Windows COM: winit owns the UI thread as an STA. The backend does all COM work on its own MTA
  threads, never on the caller's thread.
* `Spatializer::new` allocates everything (about 2 MB at 768 kHz) and runs off the real-time thread:
  backends call the factory on stream (re)open, device change and rate change.

## DSP graph (`sidestage-dsp`)

```
music L/R ─► clean (NaN → 0, clamp ±16) ─► 20 ms fade-in after creation or a fault reset
  │
  ├─► room (shared, fed the unprocessed music)
  │     early reflections: fixed lateral/rear taps per room, per-ear delay and level difference
  │     late reverb: 8-line FDN, Householder matrix, 2x4 allpass diffusers, per-line loop filter
  │     ─► room damping low-pass ─► wet, normalized so reverb_db is relative to the music
  │
  └─► voice: music L and R are two virtual speakers at az ∓ (width/2)·clamp(2·cos az, -1, 1)
        per speaker: rear cues   -5 dB @ 4.2 kHz, +1.5 dB @ 1.15 kHz, +3 dB @ 12.4 kHz
                                 (scaled by how far behind the speaker is)
                     height cues +3 dB @ 7.5 kHz (above), -4 dB high shelf @ 5 kHz (below)
                     ─► delay line ─► per ear: Woodworth ITD (3rd-order Lagrange read)
                                       ─► Brown-Duda head shadow (α crossfade, ears at ±90°)
        sum per ear ─► distance: "air" high shelf @ 10 kHz, -1.2 dB per doubling beyond 1 m
                    ─► distance gain 0.5/√r (-3 dB per doubling) ─► + room wet
                    ─► tone: high-pass, "muffle" low-pass (open at 20 kHz), 150 Hz low shelf,
                       3.2 kHz presence dip
  ─► ducker: 3.2 kHz bell that deepens with the reduction, then broadband gain reduction
  ─► volume_db + 3 dB makeup ─► lookahead limiter (-1 dBFS, 1 ms) ─► bypass crossfade ─► clamp ±1
```

* **Head model.** Spherical head (Brown & Duda 1998), head radius 8.75 cm, so no HRTF dataset or
  licence is needed. The head shadow is written as `α + (1 - α)·LP` with a fixed one-pole low-pass
  at c/(πa) ≈ 1.25 kHz, so α can change every sample without clicks. Ears sit at ±90°, which keeps
  the model front/back symmetric; the explicit rear EQ does the front/back work. The cue bands
  follow Blauert's directional bands. The 10-15 kHz band is boosted behind, because cutting it pulls
  sound forward. That boost fades out at sample rates where the band no longer fits below Nyquist.
* **Width.** The spread follows `2·cos(az)`, so the pair narrows continuously to a point source
  exactly at ±90°. The music's left channel always stays on the listener's left, with no jump when
  the music moves through the sides.
* **Distance.** A high shelf, not a low-pass: it is exactly transparent at 1 m or closer and keeps
  the 10-15 kHz rear band. The direct gain follows r^-0.5 everywhere, so it is +3 dB at 0.5 m. The
  room send does not change with distance, so the music gets roomier as it moves away.
* **Ducker.** Only a peak level arrives, sampled once per `process` call, so there is no
  K-weighting. It is a 2:1 soft-knee (6 dB) curve above the threshold. Attack, hold and release
  smooth the reduction in dB. The reduction is still tracked (and metered) while bypassed.
* **Makeup.** +3 dB (`MAKEUP_DB`) for the level a stereo mix loses when its channels only partly add
  up at each ear. With music-like pink noise, Behind you then plays about 2.3 dB below its input.
  The loudness test keeps it within -4 to +1 dB of the input and the other presets within 4 dB of it
  (8 dB quieter allowed for Next room and Far away; Far away measures -4.5 dB). Loud masters engage
  the limiter more often.
* **Limiter.** Stereo-linked lookahead peak limiter at -1 dBFS with about 1 ms lookahead, built so
  it cannot overshoot, then a final clamp to ±1. The processed path is about 1 ms plus 2 samples
  late.
* **Smoothing.** Audio runs in sub-blocks of at most 64 frames. User parameters follow a 30 ms
  one-pole per sub-block; derived values (delays, α, gains, coefficients) glide linearly per sample.
  Filters are trapezoidal state-variable filters (Cytomic form), which stay stable while
  coefficients move. Jumps beyond 50° of direction, a doubling of distance, 50° of width, or large
  tone changes crossfade over 50 ms between two voices instead of sweeping. A room change dips the
  wet signal out and in over 30 ms each. Bypass crossfades over 20 ms and is bit-exact once
  complete. States are flushed below 1e-15, the reverb has a 1e-18 denormal guard, and a non-finite
  state resets everything instead of going silent for good.

## Crash safety

Anything that outlives the process is journaled before it changes (`journal.rs`: JSON in the state
directory, written to a temp file and renamed). It is undone by:

1. a normal stop or `Drop` of the session (the engine stops it on power off, window close, exit and
   engine panic; Ctrl+C and SIGTERM close the window normally);
2. the helper process: the Windows guardian or the Linux watchdog;
3. `Platform::recover()` at the next start, or `sidestage restore`.

macOS needs none of this: the OS un-mutes the app when the tap dies.

The window, `run` and `restore` hold an exclusive lock on `sidestage.lock` in the state directory,
so two instances never hide the same app or run `recover()` against each other's live changes. The
OS releases the lock when the process ends.

## Files on disk

| | Windows | macOS | Linux |
|---|---|---|---|
| Settings (`config.json`) | `%APPDATA%\Sidestage\config\` | `~/Library/Application Support/Sidestage/` | `$XDG_CONFIG_HOME/sidestage/` (`~/.config/sidestage/`) |
| State directory | `%LOCALAPPDATA%\Sidestage\data\` | `~/Library/Application Support/Sidestage/` | `$XDG_STATE_HOME/sidestage/` (`~/.local/state/sidestage/`) |
| In the state directory | `sidestage.log`, `sidestage.prev.log`, `sidestage.lock`, `windows-hush.json`, `windows-guardian.log` | `sidestage.log`, `sidestage.prev.log`, `sidestage.lock` | `sidestage.log`, `sidestage.prev.log`, `sidestage.lock`, `linux-routing.json` |

* The window writes its log file and keeps the previous run's; command-line commands log to stderr
  only. `RUST_LOG` overrides the default filter (Sidestage's crates at `info`, the rest at `warn`).
* Settings are saved 1.5 s after the last change and on close, on a background writer thread. A file
  with unreadable values keeps every field that still reads, and the original is kept as
  `config.json.broken`.
* Nothing is sent over the network: there is no network code in the workspace.

## Known limitations and leaks

* **General.** Headphones only. Front/back confusions are possible with any generic head model,
  especially without head tracking. No listening tests have been done; the cue amounts are
  heuristics checked by spectral measurements only. There is no HRTF-dataset mode.
* **Windows leaks.** After a foreign volume change the original plays unprocessed at the new level
  for about 150-200 ms (debounce plus the volume change reaching the capture, measured 41-77 ms). A
  new session of the app is audible for about 50-200 ms before it is turned down. While the volume
  is set again, Sidestage's own output is silent for about 350 ms. After the +12 dBFS safety cut,
  the output can stay silent up to 150 ms longer.
* **Windows persistence.** Windows saves an app's session volume. If Sidestage, its guardian and the
  next start all miss a restore, the app stays at -120 dB. This happens after a power loss, a forced
  reboot or a logoff while hiding: the guardian dies too, and winit does not pass `WM_ENDSESSION`
  on, so the window does not stop the session. The app is restored the next time Sidestage runs
  while the app has audio. After a crash while the app was closed, the app starts silent and is put
  back the moment it opens audio, if Sidestage or the guardian is running.
* **Windows other.** An app that was muted in the mixer is unmuted while hidden (Sidestage plays it)
  and muted again on stop. Another copy of the app that started at a hush level gets the hidden
  copy's saved volume, not its own. `Session::stop` can block for up to 10 s if the session thread
  is stuck (the journal then stays for the guardian), and dropping the platform joins its worker, so
  a hung audio service could hang app exit.
* **Windows `AppKey` and app updates.** `list_apps()` reports the full, versioned executable path.
  Store and Squirrel apps (Spotify, Discord) move to a new versioned folder with every update, so
  keys are compared with `AppKey::same_app` (backed by `procs::match_key`, which drops the version
  folder), never with `==`. Installers that version their folders differently still need the app
  to be picked again after an update.
* **Windows command line.** Release builds are GUI-subsystem executables. CLI commands attach to the
  parent console, but cmd and PowerShell do not wait for them, so the prompt comes back while `run`
  keeps printing. If stdout is redirected and stderr is not, error text is lost (the exit code is
  still right).
* **macOS.** No internal resampler: a mixdown tap whose rate differs from the output's may play at
  the wrong speed on macOS 14 (only a warning). User-made aggregate or multi-output devices are not
  split into their sub-devices; using one as the output fails with an explanation. AirPlay is
  untested. If the user revokes permission mid-run, the session eventually ends with a generic
  "could not start" error instead of the permission text. The consent hint appears whenever the
  capture calls take 1.5 s, whatever the cause. Rebuilds are not counted in `dropouts`.
* **Linux.** No KeepOriginal. If every stream of the app ends while captured, its saved route points
  at the dead random sink and replaces an explicit per-app route the user had set; later streams go
  to the default. Restoring a stream that was on the default clears an explicit route to that same
  device. On PulseAudio 13 and older, moving a stream back to the default pins it to that device
  (only logged). Whether pipewire-pulse meters a stream after its volume, as PulseAudio does, is
  unverified. `start()` blocks its caller (the engine thread) during setup. The watchdog's log lines
  are lost, and re-running `/proc/self/exe` is untested under AppImage/FUSE. Flatpak and Snap apps
  (sandboxed PIDs and binary names) are untested.
* **App.** A second window that is blocked by the instance lock shows default settings and does not
  take over when the first instance exits. If the window cannot be created (no OpenGL 2), only
  Windows shows a message box; elsewhere it is only logged. Window position is not saved. accesskit
  is off, although custom widgets set accessible names.

## Testing

### Automated tests: `cargo test --workspace`

* **`sidestage-dsp`.** Unit tests check the filters against the RBJ cookbook and their stability
  while coefficients glide, the Lagrange delay, the head-model values, that the limiter never
  exceeds its threshold, the ducker curve and timing, the `SharedParams` round trip and the presets'
  ranges. Integration tests in `tests/`:
  * `spatial`: level and time differences at ±90° (12.5-15 dB at 4 kHz, 0.63-0.69 ms) at 44.1, 48
    and 96 kHz, the behind-versus-front spectral cues, the elevation cue, the left channel staying
    left for every preset, and an azimuth sweep without zipper noise (step and curvature measures).
  * `rooms`: each room's T60 within 15% of nominal, room off is dry, room changes do not click.
  * `ducking`: off never reduces, quiet games do not duck, standard ducking engages, holds and
    releases.
  * `loudness`: the presets are loudness-matched.
  * `robustness`: random and hostile settings and samples (NaN, inf, 1e30, denormals) stay finite
    and within ±1, unusual sample rates work, bypass is bit-exact, preset changes and a renderer
    starting mid-song do not click.
  * `no_alloc`: a counting global allocator sees zero allocations in `process()` across rates,
    presets and block lengths.
  * `perf`: ignored in debug builds. With `cargo test -p sidestage-dsp --release`, 10 s of 48 kHz
    stereo must render in under 300 ms (measured 73-104 ms).
* **`sidestage-platform`.** The bridge (equal, fast and slow capture clocks, underrun, overrun,
  resync), the journal and the level helpers. The OS-independent logic of the Linux backend (`env`,
  `modargs`, `pcm`, `routing`) and the macOS backend (`group`, `plan`, `render`, `sys`, `worker`) is
  compiled for tests on every OS (`#[cfg(any(target_os = ..., test))]` in `lib.rs`), so those tests
  also run on Windows. The Windows tests (MTA worker, journal lock, session matching, stale entries,
  Store-app match keys, the list-cache fallback, the gain fader, channel mapping) run on Windows
  only.
* **`sidestage`.** CLI parsing and app matching, config load, save and partial recovery, the save
  debounce, the engine lifecycle against a fake backend (start and stop, failed start with backoff,
  no endless restart loop, panicking or missing backend), the instance lock, the processor factory,
  radar geometry and text formatting.

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy -D warnings`, the tests and
a release build on Windows (MSVC), macOS (aarch64) and Ubuntu, and bundles `Sidestage.app` on macOS.
From the Windows dev box,
`cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`,
`--target aarch64-apple-darwin` and `--target x86_64-apple-darwin` type-check the other backends
without linking.

### Windows live harness (`crates/sidestage-platform/examples`)

These drive the real backend through the public `Platform` API. They change the volume of no app
except the test tone they start themselves.

```text
cargo build -p sidestage-platform --examples
target\debug\examples\win_e2e.exe                              # all checks, about 75 s
target\debug\examples\win_e2e.exe --soak 120 [--default-output] # hide for 120 s: drift, dropouts
target\debug\examples\win_e2e.exe --sidechain 600              # sidechain only, waits for quiet
target\debug\examples\win_e2e.exe --ramp 4                     # how fast a volume change lands
cargo run -p sidestage-platform --example win_sessions         # read-only session listing
cargo run -p sidestage-platform --example win_probe -- sidestage 5 [output id]
```

* `win_tone` plays a steady 997 Hz tone through cpal: at 0.01 on "Steam Streaming Speakers" (a
  virtual output nobody hears) when that device exists, otherwise at 0.001 (-60 dBFS) on the default
  output. It prints one `TONE pid=...` line. `--delay-ms` delays opening the stream, to test an app
  that starts playing later.
* `win_e2e` starts tone copies and checks: `list_apps` finds the tone, and how fast; KeepOriginal
  captures at the right level and leaves the volume alone; Auto hides at 1e-6 with the captured
  level within 1 dB and restores on `stop()` and on `Drop`; a stream that appears after the start is
  hidden; a foreign volume change is adopted and hidden again; slider drags versus an app fighting
  back; re-attaching after the tone restarts; another copy of the app; killing a hiding child
  together with its whole process tree, after which the guardian (not in that tree) must restore;
  `recover()` then finds nothing; a crash while the app is closed, with and without `recover()`. The
  sidechain checks start a "music" and a "game" tone outside Sidestage's process tree: the hidden
  music and Sidestage's own output must not reach the sidechain, and the game must, at its level.
* `win_sessions` prints every audio session (volume and mute) on every active output plus the
  backend's apps, outputs and hide methods. It was run before and after every live test to confirm
  the user's own apps were untouched.
* `win_probe` measures an app's left and right RMS through KeepOriginal without changing it, for
  example Sidestage's own output to check that a front-right preset is louder on the right.

### What ran on real hardware

| Part | Verified on real hardware | Only compiled, cross-checked or unit-tested |
|---|---|---|
| DSP | Offline measurements in the tests: spectra, ILD/ITD, T60, loudness, clicks, allocations, speed | No listening tests; never heard with real music |
| Windows backend | Windows 11 build 26200, test tone on Steam Streaming Speakers, Sidestage's output on the same device or on a USB headset: `win_e2e` passes (hidden 291-300 ms after `start()`, foreign change re-hidden in 155-169 ms, guardian restore 28-29 ms after a tree kill, a closed app restored 4-5 ms after it opens audio), 60-120 s soaks with 0 dropouts at about 62 ms latency, the sidechain checks, volume changes reaching the capture in 41-77 ms | Default-device changes, the chosen output disappearing, `AUDCLNT_E_DEVICE_INVALIDATED`, real music apps (Spotify, browsers, multi-process apps), exclusive-mode or protected streams, Windows 10, the MSVC toolchain, the communications-ducking, spatial-sound and hands-free warnings, `start()`'s slow path |
| App (window and CLI) | On Windows against the demo backend: screenshots at 1000x660 and 760x520 in both themes, Ctrl+C / Ctrl+Break for the window and `run`, console attach of the release exe, the single-instance lock. The component reviews never ran it with the real backends. | The merged app with a real music app; Linux and macOS (compile and clippy only); following the OS theme live |
| macOS backend and packaging | Nothing. It has not even been linked: no Mac was available. | Everything: `cargo check`, clippy and rustdoc for both Apple targets, the pure-logic tests on every OS, `make-app.sh` syntax and plist parsing |
| Linux backend | Nothing. It has not even been linked: no Linux machine was available. | Everything: `cargo check` and clippy for x86_64-unknown-linux-gnu, the pure-logic tests on every OS, protocol code read against the `pulseaudio` crate and the PulseAudio and pipewire-pulse sources |

**First hardware test, Linux** (PulseAudio 16/17 and PipeWire 1.x): start, list apps, capture
Spotify or Firefox, check `pactl list short sinks` shows `sidestage_music_*`, stop (the app goes
back and the sink is gone), `kill -9` and `pkill sidestage` mid-session (the watchdog should restore
within about 1 s), relaunch (recover and sweep), unload the null sink by hand, restart
pipewire-pulse or pulseaudio mid-session, unplug the headphones, and check ducking with a stream
that is not on a plain sink.

**First hardware test, macOS**: Spotify or Music, Safari and Chrome; built-in output, Bluetooth
headphones and AirPods (A2DP and hands-free), a USB interface, a headset with a microphone; the
consent prompt (allow, deny, ignore); SIGKILL during a session (the app must be un-muted);
`make-app.sh` signing and the binary's minimum OS version.
