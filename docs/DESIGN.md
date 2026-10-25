# Sidestage design

Sidestage takes the audio of **several apps** (the *sources*: a music player, a podcast or video,
voice chat), moves each one to its own virtual position around the listener's head, mixes them
and plays that binaural mix to the headphones. The game's audio is never captured, routed or
processed, so it keeps its exact timing and positional cues.

> Music behind you, a podcast on your left, Discord on your right. Your game audio stays exactly
> as it is.

This document describes the code as it is. Where behaviour was only reasoned about and not measured,
it says so. [Testing](#testing) lists what ran on real hardware.

## Crates

| Crate | Role | Depends on |
|---|---|---|
| `sidestage-dsp` | One source's binaural renderer (`Spatializer`, with the voice leveller), the presets, the master bus for the mix (`MasterBus`), lock-free parameter hand-off. Pure Rust, no I/O. | serde |
| `sidestage-platform` | Per-OS capture, hiding and output for any number of sources behind the `Platform` / `Session` traits; the drift bridge; crash journals | `windows` (Windows), `objc2-core-audio` and friends (macOS), `pulseaudio` (Linux), rtrb, directories |
| `sidestage` | eframe/egui window (glow renderer) and command line; wraps one `Spatializer` per source and the `MasterBus` in `ProcessorFactory`s, and connects the sources for voice chat ducking (`VoiceBus`) | both of the above |

`sidestage-platform` does not depend on `sidestage-dsp`. The app hands the backend one
`ProcessorFactory` (`Fn(sample_rate) -> Box<dyn Processor>`) per source plus one for the master,
so each side builds and tests alone. `crates/sidestage/src/processor.rs` is the whole glue.

## Threads

```
 UI thread (egui / winit)        engine thread (sidestage)            backend threads (per OS)
 ┌──────────────────────┐ Desired ┌───────────────────────────┐ calls ┌──────────────────────────┐
 │ sources, radar,      ├────────►│ owns Box<dyn Platform>    ├──────►│ control / worker threads │
 │ presets, app picker, │◄────────┤ and the one Session;      │◄──────┤ (COM MTA, Core Audio,    │
 │ cards                │Snapshot │ polls apps 1 s, outputs + │status │ Pulse connections)       │
 │                      │         │ support 5 s, status 10 Hz │       └────────────┬─────────────┘
 │ Settings ────────────┼─── one SharedParams per source (seqlock) ──────► real-time audio thread:
 │ meters ◄─────────────┼─── one Meters per source (peak-hold atomics) ◄── every source's
 └──────────────────────┘                                                   Spatializer (+ VoiceBus),
                                                                           then the MasterBus
```

* **UI thread.** Never calls the platform. It sends what the user wants (`engine::Desired`: power,
  the switched-on sources with their ID, app and `SharedParams`, output, hide method, sidechain,
  the never-duck list) and draws a `Snapshot`. Sound settings skip the engine: the UI writes them
  straight into the source's `SharedParams`, which that source's renderer reads. A source keeps one
  `SharedParams` for as long as it exists; a new one under the same ID makes the engine remove and
  re-add the source (Undo relies on this).
* **Engine thread** (`crates/sidestage/src/engine.rs`). Creates the platform on its own thread,
  calls `recover()` once, then reconciles. It keeps **one session**: started as soon as one
  switched-on source's app appears in `list_apps()` (the other sources wait inside it as
  `WaitingForApp`), changed live with `set_sources` and `set_sidechain_ignore` when sources come
  and go or the never-duck list changes, restarted only when the output or the sidechain switch
  changes, and stopped (restoring every original) on power off, window close, `Drop` and when the
  engine thread panics. Restarting for a source change would un-hide every other app for a moment,
  so it never does. Source lists are compared by key (ID, app, hide method, `SharedParams`
  pointer) first, and processor factories are built only when something changed. A new hide method
  changes every key, so the backend restarts each source.
* **Retries.** A failed start, or a session that ends in `Error` or an unexpected `Stopped`, is
  stopped and retried after 5, 15 and 45 s. A single source that ends in `Error` inside a running
  session is removed and re-added after the same delays while the others keep playing; the card's
  Try again retries just that source. Attempts keep counting until a session or source has run for
  30 s, so a backend that fails right after every start is not restarted forever. An app has to be
  gone from the list for 3 s before it gets a fresh set of attempts. `support()` is re-read every
  5 s and after every failure, so a macOS permission denial shows up as the "Allow audio capture"
  card.
* **Backend threads.** Each backend does its OS work on threads it owns and keeps the caller's wait
  short (see the table below). One real-time thread runs every source's `Processor` and then the
  master's, one after another, for each block.

`Session::status()` hands out `capture_peak` and `sidechain_peak` as "peak since the last call" on
Windows and macOS, so exactly one caller polls it: the engine in the window, or the `run` loop on
the command line.

## Sources and the mix

A session has **one output and any number of sources**. For every source the backend captures only
that app, hides its original and runs the source's own processor (its `Spatializer`). It sums the
sources, runs the master processor on the sum and plays the result as one stream: one latency, one
device to follow, and no clipping when several sources peak together.

| | Windows | macOS | Linux |
|---|---|---|---|
| Per source | Process-loopback capture thread, drift bridge, gate, hush state | Process tap in the session's aggregate device | Null sink, capture connection, drift bridge |
| Mixer | The WASAPI render thread | The aggregate's one IOProc, in the same cycle as the capture | The playback stream's real-time callback |
| Sources added and removed | Lock-free command queue (rtrb); removed processors are freed off the real-time thread | A new engine is built next to the running one, then switched to | Lock-free SPSC queues; removed slots are freed on the session thread |
| Most sources | 64 | No fixed limit | 32 |

The app allows **8 sources** (`config::MAX_SOURCES`): each one is a capture, a renderer and a puck
on the radar, and more would crowd the radar long before they strained the CPU.

**`Session::set_sources`** replaces the source list while the session runs:

* Sources are matched by `SourceId`. A source whose ID, app and hide method are unchanged keeps
  running untouched: it is not unhidden, its processor is not rebuilt (the app changes its settings
  through its own `SharedParams`), and it has no gap. The exception is macOS, where it dips for a
  few tens of milliseconds whenever the set of captured apps changes (see the macOS section).
* A new ID is started; a missing ID is stopped and its original restored; a changed app or hide
  method counts as remove + add.
* No two sources may share an ID or name the same app (`AppKey::same_app`): an app can only be
  captured and hidden once. Such lists are refused with `SessionConfig::duplicate_error`, and the
  session stays as it was. Problems with one source (its app cannot be captured, its processor
  cannot be built or panics) appear in its own `SourceStatus` and never affect the others.
* It returns within about 200 ms. Windows returns once removed sources are restored and the new
  list is taken (7-15 ms measured); new sources attach right after. macOS and Linux return at
  once: macOS queues every list and applies them in order (the app restarts a failed source with a
  remove in one call and an add in the next), and Linux shows new sources as `Starting` before its
  session thread applies the list.

`Session::set_sidechain_ignore` replaces the never-duck list the same way (matched with
`AppKey::same_app`; apps that are not running are remembered for when they start).

### Voice chat ducking: the VoiceBus

Ducking a source under voice chat needs the sources to hear each other, but the backend knows
nothing about voice chat. So the app does it inside the processors (`processor.rs`):

* A source whose settings say `voice_chat` publishes its input peak for every block on a shared
  `VoiceBus`: a fixed array of 16 slots (twice the most sources) of atomics, with a counter bumped
  on every publish. No locks, no allocation.
* Every source's processor reads the loudest *other* live voice-chat slot and passes it to
  `Spatializer::process_with_voice`, so a source never ducks under its own voice. A slot whose
  writer has not published for 0.25 s counts as silent, and a removed voice source clears its
  slot when its processor is dropped.
* All of a session's processors run on one thread, one after another, so the voice level a source
  sees is at most one block old.
* The engine frees a slot only after the session has applied the removal (or before a fresh
  start), and freed slots are reused last, so a renderer the backend is still running never shares
  its slot with a new source.

The renderer ducks for the game when `Ducking::enabled` and for the voice when
`Ducking::under_voice`, whichever is louder, with the same threshold, depth and timing. The
sidechain meter always reports the raw game level, never the voice. The voice chat presets have
game ducking off and `under_voice` off; the music presets and Podcast left have `under_voice` on.

### The ducking sidechain

The sidechain peak is "everything except Sidestage's sources, the never-duck apps and Sidestage
itself": in practice, the game. Each backend measures it differently:

| | How the game is measured | Left out |
|---|---|---|
| Windows | One peak-only process-loopback capture (`INCLUDE` its tree, after its session volume) per app that counts; the level is the loudest app's latest 10 ms packet | Every source (also one that keeps its original), the never-duck apps, Sidestage's own process tree and its ancestors, the system sounds session |
| macOS | One unmuted global tap, in a tap-only aggregate whose IOProc only measures a peak | Every source's processes, the running never-duck apps' processes, Sidestage's own process object |
| Linux | A 144 Hz peak-meter record stream on every other sink-input of the output sink | Every source's app (whatever its state), the never-duck apps, Sidestage's own streams and process ID |

## The platform contract

`crates/sidestage-platform/src/lib.rs` is the contract. What each backend puts behind it:

| | Windows | macOS | Linux |
|---|---|---|---|
| `AppKey` | Lowercase full path of the app's root executable | Bundle ID of the outermost `.app` (covers helpers), else the executable path | `application.process.binary`, else `application.name`, else `pid:N` |
| `OutputDevice.id` | MMDevice endpoint ID | Device UID | Sink name |
| Factory sample rate | Output device's mix rate | Aggregate device's nominal rate (rebuilt on rate change) | Always 48000 |
| Factories called on | Session supervisor thread | Session control thread | The caller of `start()` for the master, then the session thread |
| Hide methods offered | Auto, KeepOriginal | Auto, KeepOriginal | Auto only (others fall back to Auto with a warning) |
| `ParkOn` | Refused as an invalid source list | Not supported: mutes and warns | Falls back to Auto with a warning |
| `list_apps` / `list_outputs` wait | 150 ms, then the previous answer (first call up to 3 s) | 150 ms, then the previous answer (first call up to 2 s) | Synchronous, 750 ms reply timeout |
| `start()` returns | Within 1.5 s; a slow start continues as `Starting` | At once, in `Starting` | After setup (connect, sweep, watchdog, event watch, output engine) |
| Output with no source to capture | Opens and plays silence | Not opened; the session reports `WaitingForApp` | Opens and plays silence |
| One session per process | Yes (the hush journal has one owner) | Yes | Yes |
| `recover()` | Restores journaled volumes, waits at most 3 s, also logs each line | Always empty: nothing outlives the process | Startup sweep plus the journal |
| Crash helper | Guardian (`windows-guardian`) | None | Watchdog (`linux-watchdog`) |
| Clock drift | One bridge per source with drift control | One clock, no bridge | One bridge per source with drift control |
| `SourceStatus.latency_ms` | Bridge fill + 10 ms capture packet + output buffer (about 62 ms) | The session's IO latency | Capture latency + bridge fill + output latency |

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
* **Session** (`windows/session.rs`). One supervisor thread (MTA) owns the output and, for every
  source, its app attachment, capture thread, drift bridge, output gate and hush state. Every
  processor factory call and every change to the OS happens there. Removed sources fade out of the
  mix (10 ms) and their capture stops *before* their volume is restored, so the makeup gain never
  meets full-level audio. New sources are journaled, hidden, captured and handed to the mixer.
  Kept sources are not touched. An app that restarts keeps its source's bridge and processor (the
  processor gets silence, so tails ring out); both are rebuilt only when the output reopens.
  A failed capture or processor build is retried every 2 s with the original audible meanwhile;
  after 5 failures in a row the source shows an error and its app is restored. A source naming
  Sidestage itself fails alone ("Sidestage cannot capture itself or the program that started it").
* **Capture** (`windows/capture.rs`), per source.
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
  every session of the app, on every endpoint, to the `windows-hush` journal first: one record
  holds every app the session hides. It then unmutes the sessions and sets them to 1e-6 (-120 dB),
  with a private event-context GUID. The capture is multiplied by `desired / read-back volume`,
  capped at 1e7. A session whose read-back is 0 is set again, then stepped to 1e-5 and 1e-4. A
  block that would come out above +12 dBFS is replaced by silence and triggers a re-hide.
* **Foreign volume changes.** `IAudioSessionEvents::OnSimpleVolumeChanged` with any other event
  context means the user moved the mixer slider or the app changed its own volume. That source's
  output gate closes at once (5 ms fade), the new value becomes its desired level, and after a
  150 ms quiet period (2 s for an app that keeps resetting its volume) the app is turned down again
  and the output fades back in over 20 ms. A slider drag is not mistaken for an app fighting back;
  four resets right after a hide raise the "keeps changing its own volume" warning.
* **Sessions that come and go.** New sessions of a source's app are hidden as soon as Windows
  reports them. Windows gives a new session the volume of the app's last session that closed
  (measured), so a session of *another* copy of the app that starts at exactly one of Sidestage's
  hush levels gets its volume back (volume only; a 0 the user set stays). `stop()` restores every
  live session of every source. If the attached copy has had no audio session for 2 s and another
  copy has one, the source switches to that copy.
* **Output and mixer** (`windows/render.rs`). Our own shared-mode, event-driven WASAPI render thread
  (not cpal). Every block it pulls each source's bridge, runs the source's processor, sums, runs
  the master and writes the headphones. The supervisor builds a source's bridge and processor and
  hands them over in a box through a lock-free queue; a removed box comes back on a second queue,
  so nothing is allocated or freed on the real-time thread. The slot list holds twice the most
  sources, so replaced slots can finish fading. A source processor that panics is caught: only its
  slot goes silent and leaves the mix, its panic payload is freed off the real-time thread, and the
  supervisor rebuilds it. The master is not isolated: a panic there ends the output thread, which
  is reopened like a lost device. The mixer entry is named "Sidestage" and opts out of
  communications ducking. The output follows the default device when no output is chosen and
  falls back to the default (with a warning) when the chosen one is missing; a device change or an
  output failure rebuilds every slot, and 5 output failures in a row end the session. Each
  source's drift bridge sits between its capture clock and the render clock, with a target fill of
  one capture packet plus one output period plus margin (at least 30 ms). The session reports the
  output buffer (22 ms on the test device) as its latency and each source about 62 ms.
* **Sidechain** (`windows/sidechain.rs`). Session peak meters do not tell what an app plays: a
  process that runs a process-loopback capture gets a session on the captured app's endpoint whose
  meter shows the *captured* audio. A probe that played silence while capturing a 0.01 test tone
  read exactly 0.0100 there, and a running Sidestage read the same as everything its capture heard
  (measured with `win_meters`). Counting meters would let a screen recorder or a voice chat that
  streams the screen feed Sidestage's own output, or a never-duck app, into ducking. So every app
  that counts gets its own peak-only process-loopback capture (`INCLUDE` its tree), which hears
  only what that tree renders, after its session volume and mute. The supervisor decides which
  apps count at every scan (once a second, and at once when the never-duck list changes); the
  render thread takes the loudest of up to 32 per-app slots itself, so the level cannot race
  between capture threads. Measured: a game tone was heard 41 ms after its stream appeared, at
  exactly its level; a sound a game starts in an open stream reaches the processors after a median
  65 ms, where a bare process-loopback capture of the game sees it after 56-67 ms, so the backend
  adds only a few ms. Each listened app costs one capture client and thread (about 0.3 % CPU in
  the backend review's measurement).
* **App list.** `list_apps` distrusts two processes whose meters show identical peaks (a capturing
  process mirrors what it captures, for example Discord streaming a game). Sidestage's own
  sessions are exempt, so a game the sidechain listens to keeps its meter in the picker.
* **Guardian** (`windows/guardian.rs`). When it first writes the journal, Sidestage starts
  `--sidestage-helper windows-guardian-launch`, which starts the guardian detached
  (`DETACHED_PROCESS`, a new process group, outside Sidestage's job object when the job allows it)
  and exits. The guardian is therefore not in Sidestage's process tree, and Task Manager's "End
  process tree" does not take it down with Sidestage. It knows Sidestage's exact identity (PID plus
  creation time) and never acts while that process is alive. When Sidestage exits, it restores
  every entry of the journal: with three hidden sources, all three were back 40 ms after a kill. If
  an app is not running, it waits until the volume is restored, another Sidestage takes the
  journal over, or 7 days pass. New audio sessions and new outputs wake it, with polling as a
  fallback. A failed guardian start is retried at the next journal write, at most every 30 s. It
  logs to `windows-guardian.log` in the state directory.
* **Journal safety.** Every read-and-rewrite of the journal takes a named mutex
  (`Local\Sidestage-<FNV hash of the journal path>`) shared by Sidestage and guardians; a wait is
  limited to 2 s. Only active, hiding sources own entries: a leftover entry for an app whose source
  gave up, or keeps its original audible, is restored as soon as the app plays. An entry for a
  program that is gone is dropped only if its drive is present and the file is definitely missing,
  so an app on an unplugged drive is restored when the drive returns. Entries that could not be
  restored for 30 days are dropped.
* **Platform worker.** List and recovery calls run on one MTA worker. While a journal has pending
  entries, it watches for new audio sessions and puts a leftover volume back the moment its app
  opens audio (measured 8 ms).
* **Status warnings** include: an app is muted or at 0 in the mixer, cannot be turned down without
  muting, or keeps changing its own volume; Windows communications ducking is active; the chosen
  output is missing; spatial sound (Windows Sonic, Dolby Atmos, DTS) is on; the headset is in
  hands-free mode; Sidestage hears nothing although the app plays (exclusive mode or protected
  content); more than one copy of an app is running; a source's audio processor stopped working;
  ducking cannot hear some apps (more than 32 count).

### macOS 14.2+: Core Audio process taps

* **Capture and hide.** One private aggregate device per session, clocked by the headphone output,
  holding one private `CATapDescription` per source over all of that app's process objects, with
  `muteBehavior = CATapMutedWhenTapped` (unmuted for KeepOriginal). macOS silences an original
  while its tap exists and un-silences it the moment the tap is destroyed, also when Sidestage
  crashes: private taps and aggregates belong to the process that created them. Nothing outlives
  the process, so there is no journal, no helper and nothing for `recover()` to do. That a
  SIGKILLed Sidestage leaves the apps un-muted follows from this; it has not been checked on a Mac
  (see [Crash safety](#crash-safety)).
* **Tap scope.** A tap is device-scoped to the output stream that holds the device's preferred
  stereo pair only when the app plays only to the chosen output *and* a misplaced tap would be
  detected: the device has no input streams, or none with that stream's channel count, and no
  other output stream with that count. Otherwise, or when the two channels sit in different
  streams, or when the created tap's channel count or rate does not match, it is a stereo mixdown
  tap. Devices with a stereo input (audio interfaces, headsets with a stereo microphone) therefore
  get mixdown taps.
* **Render** (`macos/render.rs`). The aggregate's one IOProc finds each source's left and right in
  its tap's buffers (any channel count, interleaved or split into several streams), runs that
  source's processor, sums, runs the master on the sum and writes the device's preferred stereo
  pair, in the same cycle. Everything runs on one clock, so there is no ring buffer, resampler or
  drift control; `drift_ppm` is always 0 and every source's latency is the session's. `latency_ms`
  is measured from the IO cycle's timestamps and excludes the device's own output latency. On the
  IO thread there is no allocation, lock, Objective-C call or panic path, and every index is
  checked.
* **Which buffers belong to which tap.** The IOProc gets the output device's own input streams (a
  headset microphone) first, then the taps' streams as the trailing buffers. For several taps,
  Apple documents only that an aggregate's list order sets its stream order (said of the
  sub-device list). So after creating the aggregate the engine reads the HAL's tap list back and
  orders the lanes by it (`plan::tap_order`), and the input stream configuration must split into
  exactly those taps' channel counts, with exactly the device's own inputs in front
  (`plan::input_layout`), or the build fails. Every cycle each lane re-checks its buffers' channel
  counts and reads silence when they changed; the poll then rebuilds. Two taps with the same
  channel count in swapped order, with the tap list reporting the wrong order too, would not be
  caught.
* **Changing the sources: build first, then switch** (`macos/engine.rs`). An aggregate's tap list
  is settable only before its IO starts; cpal and FineTune only put taps into the description they
  create the aggregate with; re-composing a running aggregate is a configuration change that Core
  Audio performs with IO stopped, when `CATapMutedWhenTapped` may let the kept apps through; and
  the IO thread would meet the new buffer layout before it had matching lanes. So whenever the set
  of taps has to change (a source added or removed, a source's app appearing, a re-scope, an output
  or rate change), the session builds a complete new engine next to the running one, with fresh
  taps for every source:
  1. Build the new engine and start it in standby: its taps are read, so the kept apps stay muted,
     but it plays nothing.
  2. Wait until its IO has run 2 cycles (up to 1.5 s). If it never runs, keep the old engine and
     count a failure.
  3. Pause the old engine: it fades out over its next cycle and gives back the processors.
  4. Activate the new engine, which fades in over 5 ms, then close the old one (IOProc, aggregate,
     taps).

  Processors live in shared cells that both engines can reach; one atomic swap hands a processor
  to one IO thread at a time. So kept sources keep their processors (reverb tails, smoothing) and
  stay hidden, but they dip where the two engines meet: one cycle fading out, up to one cycle of
  silence, 5 ms fading in, an estimated 10-25 ms at 512 frames and 48 kHz, plus a few repeated or
  skipped milliseconds at the new tap's start. FineTune switches devices the same way (a second tap
  and aggregate for the same process, a crossfade, then the first is destroyed). A build that would
  gain nothing or lose a playing source (a tap that failed) is dropped before its IOProc exists,
  unheard, unless the switch is forced (an output or rate change, a stale tap that must go, a
  retry after a failed build). Building blocks the control thread for up to about 4 s; the old
  engine keeps playing meanwhile.
* **Processes.** An app's helpers coming and going, or a relaunch, rewrite its tap's process list
  in place (`kAudioTapPropertyDescription`); only 3 failed rewrites in a row cause a re-tap. An app
  that quits keeps its now-silent tap, so its processor's tail rings out. When no source's app has
  had audio processes for 15 s, the engine closes and the session reports `WaitingForApp`. Removing
  a source whose app already quit causes no rebuild; its dead tap goes with the next build.
* **Control thread** (`macos/session.rs`). Owns every Core Audio object. `set_sources`,
  `set_sidechain_ignore` and the listener blocks (on a private serial dispatch queue) only post
  events to it, with a 1 s poll as a safety net. `create()` sets `kAudioHardwarePropertyRunLoop`
  to NULL so notifications do not depend on the main run loop. A tap that fails affects only its
  own source: it is retried with backoff and shows `Error` after 6 failures. An engine whose device
  is dead, or whose IO stalls for 2 polls, is closed. When building the device fails 6 times with
  no engine running, the session ends in an error. A source restarted with a remove and an add
  reports `Starting` from the call that adds it back, never the old instance's `Error`.
* **Permission.** `NSAudioCaptureUsageDescription` must be in the Info.plist, or macOS never asks
  and the tap delivers silence. `crates/sidestage/build.rs` embeds the plist into the executable
  (`-sectcreate __TEXT __info_plist`), so `cargo run` builds can ask too. The capture calls block
  while the consent prompt is open. After 1.5 s the status warns "macOS may be asking". If they take
  45 s or more the prompt timed out, and the session ends with "quit and reopen". Capture is
  confirmed with Chromium's probe (get and set `kAudioTapPropertyDescription` after
  `AudioDeviceStart`); one passing tap proves the permission. A failed probe on a device-scoped tap
  switches to mixdown taps; permission counts as denied only after two consecutive failures with no
  earlier success in this process. `support()` then reports `NeedsPermission`.
* **Picker.** `list_apps` and `list_outputs` run on a one-job-at-a-time worker. The caller waits 150
  ms (the first call up to 2 s), then gets the previous list; a late answer is handed to the next
  call. The list shows apps that are playing, user-facing apps, and apps that played in the last 15
  minutes.
* **Sidechain.** A second, unmuted global tap that excludes every source's processes, the running
  never-duck apps' processes and Sidestage's own process object, in a tap-only private aggregate
  whose IOProc only measures a peak. The exclusion list is sorted, compared, and rewritten in place
  only when it changes (on `set_sources`, `set_sidechain_ignore` and process changes); the tap is
  rebuilt when the render rate changes. A value older than 250 ms counts as 0.
* **Deployment target.** The process-tap symbols are linked strongly. `build.rs` raises
  `MACOSX_DEPLOYMENT_TARGET` to 14.2 for the `sidestage` binary; CI and `make-app.sh` export it for
  every crate. `Info.plist` sets `LSMinimumSystemVersion` to 14.2.

### Linux: PulseAudio native protocol (PulseAudio or pipewire-pulse)

* **Client.** A pure-Rust native-protocol client over the `pulseaudio` crate's types, with its own
  framing (`linux/conn.rs`). No libpulse at build or run time. The socket and cookie are found the
  way libpulse finds them; a `PULSE_SERVER` with only network entries is refused.
* **Capture and hide, per source.** Each source gets its own `module-null-sink` named
  `sidestage_music_<random hex>` (shown as "Sidestage Capture - <app>"). Its `.monitor` is
  recorded on a connection of its own with `no_move` (10 ms fragments, 48 kHz float stereo) into
  the source's drift bridge, and the app's sink-inputs are moved onto it. A new source is journaled
  before its sink is loaded, and its streams are moved only once recording has started, so an app
  is never parked with nobody listening. Every source gets its sink and capture as soon as it is
  added, even while its app is not running (`WaitingForApp`). The routing plan (`linux/routing.rs`)
  keeps each app on its own sink and moves anything else off Sidestage's sinks: another source's
  app that a saved route put there, an app whose source has no sink yet, Sidestage's own output. A
  brand-new stream of an app is heard for about 60 ms (event debounce) plus the move, unless a
  saved route already sends it there. If a capture sink becomes the system default, Sidestage sets
  the previous default back (at most once per 5 s).
* **Output** (`linux/audio.rs`, `linux/mixer.rs`). One playback stream to the chosen sink (or the
  default), with a 45 ms buffer. Its real-time callback pulls every source's bridge (40 ms target)
  through that source's processor, sums them, runs the master on the sum, and always answers the
  full request (silence when there is nothing to play). The slot list is reserved for 32 sources up
  front; sources come and go through lock-free SPSC queues, and removed slots are freed on the
  session thread. A panicking source processor is caught and muted, and that source is rebuilt on
  its own (its app is silent for about 300 ms meanwhile); a panicking master restarts the engine; a
  panicking factory counts as that source's (or the engine's) failure. The output opens at session
  start and plays silence while there are no sources. The audio thread asks for `SCHED_FIFO`
  priority 1 with `SCHED_RESET_ON_FORK`. That works only where `RLIMIT_RTPRIO` allows it; there is
  no rtkit fallback.
* **Feedback guard.** If the server puts Sidestage's own output on one of its capture sinks (the
  headphones were unplugged and only null sinks are left, or a null sink became the default), the
  output would loop through that source's capture. The output is then muted (every processor keeps
  running, silence is written) until it is moved off: the audio thread spots the move in the raw
  packet without allocating, and every listing checks it again. It is moved back to the chosen
  output if that sink exists, else to the default. With no real output left, the status says "No
  sound output is available, so Sidestage is muted until one is back."
* **Sidechain.** The output sink's monitor would contain Sidestage's own output, so instead every
  other sink-input on that sink gets a 144 Hz peak-meter record stream (`direct_on_input`,
  `PEAK_DETECT`), as pavucontrol does. Sources' apps, the never-duck apps (matched with
  `same_app`), Sidestage's own streams and its process ID are left out. The list is rebuilt on
  every reconcile. The game level is the loudest of them. Games playing to another sink are not
  heard.
* **Undo.** Moves persist in stream-restore or WirePlumber. Streams that were on the default sink
  are moved to the *current* default, which clears the saved route; others go back to their original
  sink. Then the module is unloaded. Unloading checks the module's own arguments, so another app's
  null sink is never removed.
* **Watchdog** (`linux/watchdog.rs`).
  `/proc/self/exe --sidestage-helper linux-watchdog <journal> <session id>` in its own process
  group. It holds the read end of a pipe whose only writer is Sidestage, so any exit (even SIGKILL)
  gives it EOF. It ignores SIGINT, SIGHUP and SIGTERM, so `pkill sidestage` or a logout does not
  kill both. It then restores every sink from the `linux-routing` journal, retrying 10 times over
  about 5 s. Its stdout and stderr go to `/dev/null`, so its log lines are lost.
* **Startup sweep.** `recover()` restores a dead run's journal and removes leftover
  `sidestage_music_*` modules that no live session owns. A second Sidestage process is refused while
  the journal's owner is alive.
* **Failures.** More than 5 failures a minute of the output engine end the session; of a source's
  sink or capture, they give up that source and hand its app back. A failure to open one source's
  capture connection (for example at PulseAudio's limit of 64 clients) counts only for that source.
  A vanished sound server is reconnected for up to 15 s.

## Real-time rules

* `Processor::process` / `Spatializer::process_with_voice` / `MasterBus::process`: no allocation,
  locks, syscalls, logging or panics. A counting-allocator test checks the renderer, the leveller
  and the master bus. Settings arrive through a seqlock of atomics (`SharedParams`). The audio
  thread tries a bounded number of times and otherwise keeps its previous settings, so it never
  waits. Meters go back as peak-hold atomics, and voice levels through the `VoiceBus`'s atomics.
* All of a session's processors run on one real-time thread: every source's, then the master's.
  Backends pass at most `MAX_BLOCK_FRAMES` (4096) frames per call. The renderer accepts any rate
  (its maths is clamped to 8-768 kHz) and any length, including odd and empty blocks.
* Processors are built off the real-time thread (backends call a factory when a source starts and
  whenever the sample rate changes, and may call it when the output device changes) and are handed
  to the real-time thread and back without allocating or freeing there. `Spatializer::new`
  allocates everything (about 2 MB at 768 kHz).
* Capture threads only copy into their source's bridge. The bridge (`bridge.rs`) is a lock-free
  SPSC ring read through a Catmull-Rom interpolator whose speed a proportional controller nudges
  (at most ±1000 ppm) to hold the fill level, like PulseAudio's module-loopback. In the first
  seconds after a source starts the correction briefly reads several hundred ppm, then settles.
* Windows COM: winit owns the UI thread as an STA. The backend does all COM work on its own MTA
  threads, never on the caller's thread.

## DSP graph (`sidestage-dsp`)

One `Spatializer` per source:

```
source L/R ─► clean (NaN → 0, clamp ±16) ─► 20 ms fade-in after creation or a fault reset
  ─► leveller (stereo-linked compressor with make-up gain; amount 0 is a bit-exact bypass)
  │
  ├─► room (shared, fed the levelled input)
  │     early reflections: fixed lateral/rear taps per room, per-ear delay and level difference
  │     late reverb: 8-line FDN, Householder matrix, 2x4 allpass diffusers, per-line loop filter
  │     ─► room damping low-pass ─► wet, normalized so reverb_db is relative to the source
  │
  └─► voice: L and R are two virtual speakers at az ∓ (width/2)·clamp(2·cos az, -1, 1)
        per speaker: rear cues   -5 dB @ 4.2 kHz, +1.5 dB @ 1.15 kHz, +3 dB @ 12.4 kHz
                                 (scaled by how far behind the speaker is)
                     height cues +3 dB @ 7.5 kHz (above), -4 dB high shelf @ 5 kHz (below)
                     ─► delay line ─► per ear: Woodworth ITD (3rd-order Lagrange read)
                                       ─► Brown-Duda head shadow (α crossfade, ears at ±90°)
        sum per ear ─► distance: "air" high shelf @ 10 kHz, -1.2 dB per doubling beyond 1 m
                    ─► distance gain 0.5/√r (-3 dB per doubling) ─► + room wet
                    ─► tone: high-pass, "muffle" low-pass (open at 20 kHz), 150 Hz low shelf,
                       3.2 kHz presence dip, 2.5 kHz clarity bell (Q 0.8)
  ─► ducker: follows max(game peak if ducking is on, voice peak if under_voice is on);
             3.2 kHz bell that deepens with the reduction, then broadband gain reduction
  ─► volume_db + 3 dB makeup ─► lookahead limiter (-1 dBFS, 1 ms) ─► bypass crossfade ─► clamp ±1
```

The backend sums every source's output, then the session's `MasterBus`:

```
sum ─► clean (NaN and denormals → 0, clamp ±64) ─► lookahead limiter (-1 dBFS, 2 ms lookahead,
       15 ms hold, 120 ms release) ─► clamp ±1
```

* **Head model.** Spherical head (Brown & Duda 1998), head radius 8.75 cm, so no HRTF dataset or
  licence is needed. The head shadow is written as `α + (1 - α)·LP` with a fixed one-pole low-pass
  at c/(πa) ≈ 1.25 kHz, so α can change every sample without clicks. Ears sit at ±90°, which keeps
  the model front/back symmetric; the explicit rear EQ does the front/back work. The cue bands
  follow Blauert's directional bands. The 10-15 kHz band is boosted behind, because cutting it pulls
  sound forward. That boost fades out at sample rates where the band no longer fits below Nyquist.
* **Width.** The spread follows `2·cos(az)`, so the pair narrows continuously to a point source
  exactly at ±90°. The source's left channel always stays on the listener's left, with no jump when
  it moves through the sides. The voice presets use width 0 (a point source).
* **Distance.** A high shelf, not a low-pass: it is exactly transparent at 1 m or closer and keeps
  the 10-15 kHz rear band. The direct gain follows r^-0.5 everywhere, so it is +3 dB at 0.5 m. The
  room send does not change with distance, so a source gets roomier as it moves away.
* **Leveller** (`leveler.rs`, `Settings::leveler` 0..1). Applied to the dry input before anything
  else hears it; the bypass crossfade still uses the untouched input. 6 ms RMS detector on the
  louder channel after a 100 Hz high-pass. Ratio 1 + 5·amount above a threshold (8 + 16·amount)
  dB below a reference of -19 dB detector level (a talker at about -20 LUFS), soft knee 8 dB;
  make-up up to +13.2 dB at 0.6 and +20 dB at 1. Attack 8 ms, release 250 ms. Nothing at or below
  -50 dBFS is lifted. The gain never rises below -45 dBFS, or within 12 dB of the input's own
  noise floor (the lowest detector level over the last 1.6 s, kept in 8 fixed segments), so steady
  noise does not swell up in the pauses between phrases. In a silent pause the gain holds,
  however long: a callout after 4-10 s of silence starts at the level the last phrase ended at
  (it used to start 3.5-4.5 dB low while the lift was re-earned). After a reset the gain holds
  for 60 ms while the detector warms up. Attack is never gated, so a loud talker is turned down
  at once.
* **Clarity** (`Settings::clarity_db`, -6..+6 dB). A bell at 2.5 kHz, Q 0.8, in the tone stage,
  smoothed and crossfaded like the other tone controls. Measured: +5.99 dB at 2.5 kHz for +6,
  +2.4 dB an octave either side, within 0.25 dB at 150-300 Hz and 11-14 kHz.
* **Ducker.** Only a peak level arrives, sampled once per `process` call, so there is no
  K-weighting. It is a 2:1 soft-knee (6 dB) curve above the threshold. Attack, hold and release
  smooth the reduction in dB. The reduction is still tracked (and metered) while bypassed. It
  follows the game when `Ducking::enabled` and the voice chat when `Ducking::under_voice`,
  whichever is louder; `process()` is bit-identical to `process_with_voice(.., 0.0)`.
* **Makeup.** +3 dB (`MAKEUP_DB`) for the level a stereo mix loses when its channels only partly add
  up at each ear. With music-like pink noise, Behind you then plays about 2.3 dB below its input.
  The loudness test keeps it within -4 to +1 dB of the input and the other music presets within
  4 dB of it (8 dB quieter allowed for Next room and Far away; Far away measures -4.5 dB). Loud
  masters engage the limiter more often.
* **Limiters.** Each source has a stereo-linked lookahead peak limiter at -1 dBFS with about 1 ms
  lookahead, built so it cannot overshoot, then a final clamp to ±1. The processed path is about
  1 ms plus 2 samples late. Its release decays the distance from unity multiplicatively, so after
  limiting the gain returns to an exact passthrough. Sources peaking together still add up past
  full scale (two by up to +5 dB, eight by up to +17 dB), so the `MasterBus` runs the same limiter
  on the sum with a 2 ms lookahead (95 frames at 48 kHz, `latency_frames()`), a 15 ms hold and a
  120 ms release. The hold gives a limited bass note one flat gain instead of one that recovers
  between its half-waves: a 50 Hz note 4 dB over the ceiling has harmonics at -147 dB, against
  -42 dB without the hold. Eight sources summing to +14 dBFS stay at or below -1 dBFS.
* **Smoothing.** Audio runs in sub-blocks of at most 64 frames. User parameters follow a 30 ms
  one-pole per sub-block; derived values (delays, α, gains, coefficients) glide linearly per sample.
  Filters are trapezoidal state-variable filters (Cytomic form), which stay stable while
  coefficients move. Jumps beyond 50° of direction, a doubling of distance, 50° of width, or large
  tone changes (6 dB of clarity, for example) crossfade over 50 ms between two voices instead of
  sweeping. A room change dips the wet signal out and in over 30 ms each. Bypass crossfades over
  20 ms and is bit-exact once complete. States are flushed below 1e-15, the reverb has a 1e-18
  denormal guard, and a non-finite state resets everything instead of going silent for good.

### Voice presets

| Preset | Position | Tone | Leveller | Ducking |
|---|---|---|---|---|
| Voice chat left / right | ∓70°, 1 m, point source, small room (-11 dB) | 100 Hz-12 kHz, bass -2 dB, clarity +3 dB, volume -8 dB | 0.6 | Off for the game and under voice chat |
| Over your shoulder | 140°, 10° up, 0.7 m | as above, clarity +4.5 dB, volume -9 dB | 0.6 | as above |
| Radio comms | -40°, 1 m, desk room (-14 dB) | 300 Hz-3.2 kHz, bass -6 dB, clarity +4 dB, volume -6 dB | 0.85 | as above |
| Podcast left | -90°, 1.5 m, point source, living room (-10 dB) | 80 Hz-14 kHz, clarity +2 dB, volume -5 dB | 0.4 | Gentle, for the game and under voice chat |

All but Podcast left set `voice_chat`; Podcast left is spoken word and is treated like music.
They were tuned by measurement with speech-shaped noise; this prints the table:

```text
cargo test -p sidestage-dsp --release --test voice -- --ignored --nocapture
```

For speech at -20 LUFS the voice chat presets play within +0.1 to +0.2 LU of Behind you on music,
and Podcast left within ±1.6 LU for spoken word at -15 to -20 LUFS. Two talkers 12 dB apart come
out 4.1 dB apart (Radio comms 3.2 dB, Podcast left 5.5 dB). The direct-to-reverb ratio is 7-14 dB
and the interaural coherence 0.70-0.89: a little room, so the voice sounds outside the head but
still close. Radio comms is 12.5-12.9 dB below the mids at 80-250 Hz and 5-10 kHz. Peaks for
-14 LUFS speech stay around -6 dBFS, so the source limiter does not engage.

**Footstep-safe** (`make_footstep_safe`) is idempotent for every preset. On music it sets the
presence dip to -6 dB or deeper, the low-pass to 8 kHz or lower, the bass to -3 dB or lower and
Competitive ducking. On a voice chat source it only takes the dip to -3 dB and the low-pass to
10 kHz and leaves ducking, clarity, leveller and bass alone. Weighted by the band importance of
speech intelligibility, that costs about 1.15 dB, against 2.3 dB with the music rules, which would
also duck the voice by 12 dB for the game.

## Crash safety

Anything that outlives the process is journaled before it changes (`journal.rs`: JSON in the state
directory, written to a temp file and renamed). It is undone by:

1. a normal stop or `Drop` of the session, which restores every source (the engine stops it on
   power off, window close, exit and engine panic; Ctrl+C and SIGTERM close the window normally);
   removing a source restores just that one;
2. the helper process: the Windows guardian or the Linux watchdog, which restore every journaled
   app or sink;
3. `Platform::recover()` at the next start, or `sidestage restore`.

With several sources:

* **Windows.** One `windows-hush` record holds every app the session hides, and the guardian
  restores every entry. A source's entries are written before its first volume change and removed
  only after its restore. Measured on the merged code: three hidden sources were all restored 40 ms
  after a kill, and a session killed at 12 different moments while it added and removed sources
  (14 tones hidden at the kills in all) had every tone back within 59 ms of each kill and its
  journal cleared.
* **Linux.** The `linux-routing` journal holds one record per sink
  (`{pid, session, sinks: [{sink_name, module_index, streams}]}`). It is written before each sink
  load and before each batch of moves. A sink being removed stays in it until its undo has
  finished, and a failed undo is kept as an orphan and retried. The watchdog, teardown and
  `recover()` undo every sink. A 0.1 single-sink journal is still read and undone.
* **macOS.** Nothing is journaled: the OS un-mutes an app when its tap dies. This rests on the
  premise that private taps and aggregates die with the process. FineTune 1.9.0, whose aggregates
  are private and auto-start their taps as Sidestage's do, ships an orphaned-aggregate cleanup and
  claims that a `kill -9` leaves aggregates with muted taps behind. That must be checked on a Mac;
  if it is true, `recover()` needs a sweep of aggregates with Sidestage's UID prefix and a dead PID.

The window, `run` and `restore` hold an exclusive lock on `sidestage.lock` in the state directory,
so two instances never hide the same app or run `recover()` against each other's live changes. The
OS releases the lock when the process ends.

## Files on disk

| | Windows | macOS | Linux |
|---|---|---|---|
| Settings (`config.json`) | `%APPDATA%\Sidestage\config\` | `~/Library/Application Support/Sidestage/` | `$XDG_CONFIG_HOME/sidestage/` (`~/.config/sidestage/`) |
| State directory | `%LOCALAPPDATA%\Sidestage\data\` | `~/Library/Application Support/Sidestage/` | `$XDG_STATE_HOME/sidestage/` (`~/.local/state/sidestage/`) |
| In the state directory | `sidestage.log`, `sidestage.prev.log`, `sidestage.lock`, `windows-hush.json`, `windows-guardian.log` | `sidestage.log`, `sidestage.prev.log`, `sidestage.lock` | `sidestage.log`, `sidestage.prev.log`, `sidestage.lock`, `linux-routing.json` |

* `SIDESTAGE_STATE_DIR` moves the state directory elsewhere (logs, the instance lock, journals,
  guardians and their journal locks included), and `SIDESTAGE_CONFIG_DIR` the settings folder, so
  test runs never meet a Sidestage the user is running.
* The window writes its log file and keeps the previous run's; command-line commands log to stderr
  only. `RUST_LOG` overrides the default filter (Sidestage's crates at `info`, the rest at `warn`).
* Settings (version 2: a list of sources, each with its own app, colour, on/off state and sound
  settings, plus the never-duck list) are saved 1.5 s after the last change and on close, on a
  background writer thread. A version 0.1 file (one app, one set of settings) becomes one source,
  and the original is kept as `config.json.v1`. A file with unreadable values keeps every field
  that still reads, and the original is kept as `config.json.broken`. A file from a newer version
  is kept as `config.json.newer`, because the options this version does not know are lost when it
  saves. Source IDs above 2^30 are renumbered, and empty app names are rebuilt from the app's key.
* Nothing is sent over the network: there is no network code in the workspace.

## Known limitations and leaks

* **General.** Headphones only. Front/back confusions are possible with any generic head model,
  especially without head tracking. No listening tests have been done; the cue amounts are
  heuristics checked by spectral measurements only. There is no HRTF-dataset mode.
* **Voice.** The voice presets were tuned with synthetic speech-shaped signals, not real Discord or
  podcast recordings. Radio comms band-limits with 2nd-order filters (12 dB/octave), not a steep
  telephone band. Spotify-normalized music (-14 LUFS) plays about 4-6 dB above levelled voice chat
  until it ducks under the voice; only the sources' volumes change that. Footstep-safe treats
  Podcast left like music.
* **Leveller.** Any steady signal counts as noise: a held tone, or continuous music through a voice
  preset, is turned down by the attack but never lifted. Speech less than about 12 dB above a
  steady noise floor is not lifted either. For about 1.6 s after a stretch of digital silence (a
  voice-activity gate) the floor window still holds that silence, so noise right after the gate
  opens can still rise at the normal release. The very first phrase after the leveller starts
  has no history, so it ramps up over about a second; after that, pauses hold the gain. A
  louder new talker after a pause is turned down within the 8 ms attack.
* **Windows leaks.** After a foreign volume change the original plays unprocessed at the new level
  for about 150-200 ms (debounce plus the volume change reaching the capture, measured 41-77 ms). A
  new session of an app is audible for about 50-200 ms before it is turned down. While the volume
  is set again, that source's output is silent for about 350 ms. After the +12 dBFS safety cut,
  the output can stay silent up to 150 ms longer. After an output reopen every source is silent for
  about 300 ms (each waits the 200 ms settle time again, although its hush did not change).
* **Windows persistence.** Windows saves an app's session volume. If Sidestage, its guardian and the
  next start all miss a restore, the apps stay at -120 dB. This happens after a power loss, a forced
  reboot or a logoff while hiding: the guardian dies too, and winit does not pass `WM_ENDSESSION`
  on, so the window does not stop the session. The apps are restored the next time Sidestage runs
  while they have audio. After a crash while an app was closed, the app starts silent and is put
  back the moment it opens audio, if Sidestage or the guardian is running. The first hide in a
  process waits up to 2 s for the guardian launcher before it writes the journal; if the launcher
  is slower, a kill in that window can leave an app hushed until the next `recover()`.
* **Windows, several sources.** Two sources whose process trees nest (a launcher and a game it
  started) are not handled: the outer source's capture would also play the inner app's hushed
  audio. A source's capture start blocks the supervisor (up to 8 s; process-loopback activation
  up to 5 s), so if the audio service hangs, managing the other sources waits too (their audio
  keeps playing). A processor that panics on the render thread still allocates its panic payload
  there, and the default panic hook prints from that thread. Right after a source starts, its
  drift correction can read up to ±1000 ppm for a few seconds.
* **Windows sidechain.** An ancestor of Sidestage (Explorer, or the terminal or launcher that
  started it) is never listened to, since capturing its tree would capture Sidestage. A never-duck
  app or KeepOriginal source that runs inside a counted app's process tree is heard through that
  app's capture. Capturing elevated or anti-cheat-protected games was not verified. Apps whose
  sessions are merely inactive are listened to as well. `list_apps` still distrusts two other apps
  with identical meter peaks (Discord mirroring a game it streams).
* **Windows other.** An app that was muted in the mixer is unmuted while hidden (Sidestage plays it)
  and muted again on stop. Another copy of an app that started at a hush level gets the hidden
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
* **macOS.** Nothing has run on a Mac. Changing the set of taps is not gapless: kept sources dip
  for an estimated 10-25 ms (not measured), and that they stay muted through the switch relies on
  FineTune's precedent (two `MutedWhenTapped` taps on one process). In a forced build (output, rate
  or default-output change), a kept source whose new tap fails is dropped and audible unprocessed
  until its retry. The lane order for several taps is inferred (see above). Whether rewriting a
  dead tap's process list after a relaunch works is unverified. After the 15 s idle close, an app
  that dropped its process objects while paused is heard unprocessed for the build time (about
  0.1-2 s) when it resumes. Devices with stereo inputs lose multichannel layout (mixdown taps).
  The stall watchdog could rebuild once after a slow wake from sleep. No internal resampler: a
  mixdown tap whose rate differs from the output's may play at the wrong speed on macOS 14 (only a
  warning). User-made aggregate or multi-output devices are not split into their sub-devices;
  using one as the output fails with an explanation. AirPlay is untested. If the user revokes
  permission mid-run, the session eventually ends with a generic "could not start" error instead of
  the permission text. The consent hint appears whenever the capture calls take 1.5 s, whatever
  the cause. On a same-rate output change, kept processors are reused without a factory call. A
  processor panic on the IO thread is not caught.
* **Linux.** Nothing has run on a Linux desktop. No KeepOriginal. Every source's null sink shows up
  in the sound settings, and its monitor is recorded (silence) while its app is not running.
  Removing a playing source cuts its processed output without a fade. Each source uses one
  connection to the sound server (PulseAudio allows 64 in all; 8 sources use about 13). If every
  stream of an app ends while it is captured, its saved route points at the dead random sink and
  replaces an explicit per-app route the user had set; later streams go to the default. Restoring a
  stream that was on the default clears an explicit route to that same device. On PulseAudio 13 and
  older, moving a stream back to the default pins it to that device (only logged). Whether
  pipewire-pulse sends `PLAYBACK_STREAM_MOVED` (needed for the instant feedback mute; otherwise the
  next listing, about 60-120 ms later, mutes) and whether it meters a stream after its volume, as
  PulseAudio does, are unverified. If the chosen output is missing when the engine starts, the
  session follows the default for the rest of its life. `start()` blocks its caller (the engine
  thread) during setup, up to the 5 s reply timeout on a hung server. A 0.1 binary cannot read the
  new journal layout. The watchdog's log lines are lost, and re-running `/proc/self/exe` is
  untested under AppImage/FUSE. Flatpak and Snap apps (sandboxed PIDs and binary names) are
  untested, and an app keyed by `pid:N` gets a new key at every launch.
* **App.** A second window that is blocked by the instance lock shows default settings and does not
  take over when the first instance exits. If the window cannot be created (no OpenGL 2), only
  Windows shows a message box; elsewhere it is only logged. Window position is not saved. accesskit
  is off, although custom widgets set accessible names. Which preset tab (Music or Voice) a source
  shows is remembered only for the current run. Voice chat apps are recognized by whole words of
  their name, so an app called "Zoom Player" would get a voice preset. When three or more pucks sit
  on exactly the same spot, only the top one can be clicked on the radar (the others can be
  selected in the list). At the 760x520 minimum window size the source list scrolls once cards
  appear.

## Testing

### Automated tests: `cargo test --workspace`

* **`sidestage-dsp`.** Unit tests check the filters against the RBJ cookbook and their stability
  while coefficients glide, the Lagrange delay, the head-model values, that the limiter never
  exceeds its threshold and returns to exact unity, the leveller (curve, bypass, smooth gain, no
  swelling of steady noise in pauses), the master bus (no overshoot at 8-768 kHz and odd call
  sizes, sustained bass without distortion, hostile input), the ducker curve and timing, the
  `SharedParams` round trip and the presets' ranges, kinds and Footstep-safe. Integration tests in
  `tests/`:
  * `spatial`: level and time differences at ±90° (12.5-15 dB at 4 kHz, 0.63-0.69 ms) at 44.1, 48
    and 96 kHz, the behind-versus-front spectral cues, the elevation cue, the left channel staying
    left for every preset, and an azimuth sweep without zipper noise (step and curvature measures).
  * `rooms`: each room's T60 within 15% of nominal, room off is dry, room changes do not click.
  * `ducking`: off never reduces, quiet games do not duck, standard ducking engages, holds and
    releases; `process_with_voice` (game only, voice only, the louder of both, the meter always
    shows the game).
  * `loudness`: the music presets are loudness-matched on pink noise, the voice presets in LUFS on
    speech-like noise, and loud and quiet talkers come out closer together.
  * `voice`: the voice presets are close, clear and outside the head and sit on their side; Radio
    comms is band-limited without distortion; the clarity bell and its sweep; the leveller never
    lifts noise or silence, does not swell steady noise in pauses and does not distort low voices;
    Footstep-safe keeps voice chat intelligible. Its ignored `tuning_report` prints the tuning
    table.
  * `session_mix`: music, a podcast and voice chat through the master bus with voice ducking, and
    eight loud sources staying under the ceiling.
  * `robustness`: random and hostile settings and samples (NaN, inf, 1e30, denormals) stay finite
    and within ±1, also through `process_with_voice` and the master bus; unusual sample rates work,
    bypass is bit-exact, music and voice preset changes and a renderer starting mid-song do not
    click, and v0.1 settings JSON still loads.
  * `no_alloc`: a counting global allocator sees zero allocations in `process()` across rates,
    presets, block lengths, leveller and clarity extremes, `process_with_voice` and the master bus.
  * `perf`: ignored in debug builds. With `cargo test -p sidestage-dsp --release`, 10 s of 48 kHz
    stereo must render in under 300 ms for every preset (measured 69-146 ms, depending on how busy
    the machine was), and 10 s of eight of the heaviest sources plus the master bus in under 2 s
    (measured 0.63-0.79 s).
* **`sidestage-platform`.** The bridge (equal, fast and slow capture clocks, underrun, overrun,
  resync), the journal, the level helpers and the source-list check. The OS-independent logic of
  the Linux backend (`env`, `mixer`, `modargs`, `pcm`, `routing`, `sources`) and the macOS backend
  (`group`, `plan`, `render`, `sources`, `sys`, `worker`) is compiled for tests on every OS
  (`#[cfg(any(target_os = ..., test))]` in `lib.rs`), so those tests also run on Windows: for
  example the Linux mixer and routing plan for several sources, and the macOS source diff, switch
  verdicts and the hand-over of a kept processor between two engines. The Windows tests (MTA
  worker, journal lock, session matching, stale entries, Store-app match keys, the list-cache
  fallback, the gain fader, channel mapping, the mixer isolating a panicking processor, the
  sidechain level, mirrored meters, invalid source lists) run on Windows only.
* **`sidestage`.** CLI parsing (several sources and presets) and app matching, config load, save,
  partial recovery and the 0.1 migration, the save debounce, source-list diffing, the default
  preset for a new source and voice chat app detection, the `VoiceBus` (publishing, staleness, slot
  reuse) and the music ducking under voice chat through the real renderer, the engine lifecycle
  against a fake backend (start and stop, failed start with backoff, no endless restart loop,
  per-source retries, source changes that never restart the session, panicking or missing
  backend), the instance lock, the processor factory, radar geometry and headless egui tests of the
  radar (select, place, drag, reset), the source cards and text formatting.

CI (`.github/workflows/ci.yml`) runs `cargo clippy -D warnings`, the tests and a release build on
Windows (MSVC), macOS (aarch64) and Ubuntu 22.04, `cargo fmt --check` on the Linux job, and bundles
`Sidestage.app` on macOS. From the Windows dev box,
`cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`,
`--target aarch64-apple-darwin` and `--target x86_64-apple-darwin` type-check the other backends
without linking.

### Windows live harness (`crates/sidestage-platform/examples`)

These drive the real backend through the public `Platform` API. They change the volume of no app
except the test tones they start themselves (copies of `win_tone` at 0.01 or less), and the
sidechain checks put every app that was already playing on the never-duck list, so it is not even
listened to. `win_e2e` refuses to run unless `SIDESTAGE_STATE_DIR` names a folder of its own.

```text
cargo build -p sidestage-platform --examples
set SIDESTAGE_STATE_DIR=%TEMP%\sidestage-e2e
target\debug\examples\win_e2e.exe                              # all checks
target\debug\examples\win_e2e.exe --multi                      # only the multi-source checks (8-14)
target\debug\examples\win_e2e.exe --soak 120 [--default-output] # three sources for 120 s
target\debug\examples\win_e2e.exe --sidechain                  # sidechain checks only
target\debug\examples\win_e2e.exe --churn 20                   # kill a session 20 times mid-change
target\debug\examples\win_e2e.exe --reopen                     # the output dies and is reopened
target\debug\examples\win_e2e.exe --ramp 5                     # how fast a volume change lands
cargo run -p sidestage-platform --example win_sessions         # read-only session listing
cargo run -p sidestage-platform --example win_meters -- 3      # read-only meter trace
cargo run -p sidestage-platform --example win_probe -- sidestage 5 [output id]
```

* `win_tone` plays a steady tone through cpal: at 0.01 on "Steam Streaming Speakers" (a virtual
  output nobody hears) when that device exists, otherwise at 0.001 (-60 dBFS) on the default
  output. It prints one `TONE pid=...` line. `--freq` and `--amp` set the tone, `--delay-ms` delays
  opening the stream (an app that starts playing later), and `--start-at` opens it at once but
  stays silent until a given time (for latency measurements).
* `win_e2e` checks, with one source as in v0.1: (1) `list_apps` finds the tone, and how fast;
  (2) KeepOriginal captures at the right level and leaves the volume alone; (3) Auto hides at 1e-6
  with the captured level within 1 dB and restores on `stop()` and on `Drop`; (4) a stream that
  appears after the start is hidden; (5) a foreign volume change is adopted and hidden again,
  slider drags versus an app fighting back, re-attaching after the tone restarts; (6) another copy
  of the app; (7) killing a hiding child together with its whole process tree, after which the
  guardian (not in that tree) must restore, `recover()` then finds nothing, and a crash while the
  app is closed, with and without `recover()`. With several sources: (8) Sidestage itself, a
  processor that cannot be built and one that panics while playing each fail only their own
  source; (9) three tones at different levels through pass-through processors of different gains:
  each captured at its own level, and the right mix at the master; (10) `set_sources` adds a source
  and removes one while the untouched sources' volumes are read every few ms and their audio is
  checked for gaps and processor rebuilds; duplicates are refused; (11) a 40 s soak with three
  sources, and a source removed while its app is closed is restored when it plays again; (12) a
  crash with three hidden sources, then the churn test: a child session that keeps adding and
  removing sources is killed at 12 different moments; (13) the sidechain: a game tone counts at
  its level, a never-duck tone, sources (also one that keeps its original) and Sidestage's own
  output do not, ignore-list changes apply live, the picker still trusts the game's meter, and the
  onset latency; (14) the output dies while three sources play and is reopened with no app
  unhidden.
* `win_sessions` prints every audio session (volume and mute) on every active output plus the
  backend's apps, outputs and hide methods. It was run before and after live tests to confirm the
  user's own apps were untouched. `win_meters` traces every session's peak meter next to its
  endpoint's; it is the measurement behind the sidechain design. `win_probe` measures an app's
  left and right RMS through KeepOriginal without changing it, for example Sidestage's own output
  to check that a front-right preset is louder on the right.

### What ran on real hardware

| Part | Verified on real hardware | Only compiled, cross-checked or unit-tested |
|---|---|---|
| DSP | Offline measurements in the tests: spectra, ILD/ITD, T60, loudness, voice tuning, clicks, allocations, speed. Listened to with real music: Spotify spatialized while playing Valorant on Windows 11 (v0.1, the demo video in the README) | Formal listening tests across listeners and headphones; the voice presets with real voice chat and podcasts |
| Windows backend | Windows 11 build 26200. v0.1, one source: Spotify (Microsoft Store build) captured, hidden and spatialized while Valorant played. The merged multi-source code: the full `win_e2e` run passes with test tones on Steam Streaming Speakers (numbers below), and every v0.1 check still passes (hidden 320 ms after `start()`, a foreign change re-hidden after 162 ms, guardian restore 22 ms after a tree kill, a closed app restored 8 ms after it plays again) | Real apps as several sources at once (for example Spotify, a podcast app and Discord); a real default-device change or unplug (the reopen path was driven by killing the output thread); browsers and other multi-process apps; nested process trees; elevated or anti-cheat games in the sidechain; exclusive-mode or protected streams; Windows 10; the MSVC toolchain; the communications-ducking, spatial-sound and hands-free warnings; `start()`'s slow path |
| App (window and CLI) | v0.1, on Windows with the real backend: picking Spotify, presets, the radar and Footstep-safe while gaming. Against the demo backend and a scripted test backend: the multi-source window (sources, presets, voice chat, ducking, never-duck list, cards), screenshots at 1000x660 and 760x520 in both themes, `run` with several apps, Ctrl+C / Ctrl+Break for the window and `run`, console attach of the release exe, the single-instance lock | The multi-source window and `run` against a real backend; Linux and macOS with real audio; following the OS theme live |
| macOS backend and packaging | Built, linked and unit-tested in CI on GitHub's macOS runner, which also assembles the universal `Sidestage.app` with `make-app.sh`. The multi-source backend was cross-compiled and linted for both Mac targets, and its pure logic is unit-tested on every OS | Running it: no Mac with real audio has tried it yet |
| Linux backend | Built, linked and unit-tested in CI on GitHub's Ubuntu runner. The multi-source backend's pure logic is unit-tested on every OS | Running it: no Linux desktop with PulseAudio or PipeWire has tried it yet. The protocol code was read against the `pulseaudio` crate and the PulseAudio and pipewire-pulse sources |

**Multi-source live numbers** (`win_e2e` on the merged code, Windows 11 build 26200, test tones on
Steam Streaming Speakers at 48 kHz, 22 ms output buffer):

| Check | Result |
|---|---|
| Three sources at 0.01 / 0.006 / 0.004 with gains 0.4 / 0.3 / 0.2 | Each captured within ±0.00 dB. Mix at the master 0.004000 / 0.001800 / 0.000775 against 0.004000 / 0.001800 / 0.000800 expected, while the third source's clock correction was still settling; exact for all three later in the run |
| `start()` with three sources | Returned in 83 ms; all three hidden 299 ms after `start()` |
| `set_sources`: add one source, remove one | Returned in 14.5 ms; the removed app's volume was back after 8 ms; the new source hidden after 243 ms |
| Untouched sources during that change | Stayed at 1e-6 over 497 volume readings each, with 0 gaps, 0 processor rebuilds and 0 dropouts |
| Duplicate app or ID | Refused; the session is unchanged |
| 40 s soak, three sources | 0 dropouts, 62 ms added latency per source, 2.5 % CPU of one core |
| Crash with three hidden sources | The guardian restored all three 40 ms after the kill |
| 12 kills while sources are added and removed | 14 hidden tones in all; every tone back within 59 ms of each kill, journal cleared each time |
| The output dies with three sources playing | Reopened 42 ms later, sources audible again after 294-314 ms, every app still at 1e-6 over 4441 readings (originals never unhidden) |
| One source's problems (Sidestage itself, a processor that cannot be built, one that panics) | Each fails only its own source; the others keep playing |
| Never-duck list changed live | Taking an app off the list took effect in 62 ms, putting it back in 21 ms |
| What the sidechain hears | A hidden source, a KeepOriginal source, a never-duck tone and Sidestage's own output: 0 (sources never count). A 0.004 game tone: exactly 0.00400, 41 ms after its stream appeared; quiet again 20 ms after the game closed |
| Game onset to the processors | Median 65 ms (65-68 ms); a bare process-loopback capture of the game sees it after 56-67 ms |

**First hardware test, Linux** (PulseAudio 16/17 and PipeWire 1.x): start, list apps, capture
Spotify and Firefox as two sources, check `pactl list short sinks` shows one `sidestage_music_*`
per source, add and remove a source while the other plays, stop (the apps go back and the sinks
are gone), `kill -9` and `pkill sidestage` mid-session (the watchdog should restore every sink
within about 1 s), relaunch (recover and sweep), unload a null sink by hand, restart pipewire-pulse
or pulseaudio mid-session, unplug the headphones (the feedback mute), and check ducking and the
never-duck list with a stream that is not on a plain sink.

**First hardware test, macOS**: two stereo apps at clearly different positions (each must be heard
at its own position: tap order); add and remove a source while others play (the kept ones may only
dip, never be heard unprocessed); add an app whose tap fails (the others must not dip); Discord or
Chrome with helper processes coming and going; quit and relaunch a source's app, within and after
the 15 s idle window; remove a source whose app already quit; change the default output and the
sample rate; built-in output, Bluetooth headphones and AirPods (A2DP and hands-free), a USB
interface and a headset with a stereo microphone (expect mixdown taps); the consent prompt (allow,
deny, ignore); the never-duck list with a browser video; SIGKILL during a session with sources
muted (the apps must be un-muted); `make-app.sh` signing and the binary's minimum OS version.
