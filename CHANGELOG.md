# Changelog

Each `## v<version>` section is also the text of that version's GitHub release.

## v0.2.0 - 2026-10-25

Several apps at once: your music behind you, a podcast on your left and Discord on your right,
each in its own place with its own preset. Your game's audio is still never captured or
processed, and voice chat now has presets of its own.

### Downloads

| File | System |
|---|---|
| `Sidestage-0.2.0-windows-x86_64.zip` | Windows 11 (Windows 10 2004+ may work) |
| `Sidestage-0.2.0-macos-universal.zip` | macOS 14.2+, Apple silicon and Intel |
| `Sidestage-0.2.0-linux-x86_64.tar.gz` | Linux with PipeWire or PulseAudio, glibc 2.35+ |
| `SHA256SUMS.txt` | Checksums of the files above |

* **Windows**: unzip and run `sidestage.exe`. The app is not code-signed yet, so SmartScreen may
  warn you the first time: choose **More info** > **Run anyway**.
* **macOS**: unzip and move `Sidestage.app` to Applications. It is not notarized yet, so macOS
  blocks the first start: open it once, then allow it in System Settings > Privacy & Security
  > **Open Anyway**. On first use macOS asks for audio-capture permission; choose Allow.
* **Linux**: `tar -xzf Sidestage-0.2.0-linux-x86_64.tar.gz` and run `./sidestage`.

Upgrading from 0.1: your settings become your first source, and the 0.1 settings file is kept as
`config.json.v1` in case you go back.

### What's new

* **Up to 8 sources at once.** **+ Add source** picks another app. Each source has its own spot on
  the radar, its own preset, volume and ducking, and can be switched off or removed (with Undo).
  Adding or removing a source never interrupts the others.
* **Voice presets for Discord and other voice chat:** Voice chat left, Voice chat right, Over your
  shoulder and Radio comms, plus Podcast left for spoken word. They come with two new controls:
  **Leveler**, which evens out loud and quiet friends (a talker 12 dB quieter comes out within
  about 4 dB) and keeps its level through pauses so every callout starts clear, and **Clarity**.
  A voice-chat app you add goes to the next free voice position.
* **Voice chat is never turned down for the game.** Instead your music and podcasts turn down
  while someone talks (**Turn down while voice chat talks**, on by default).
* **Never duck for these apps:** sounds from the apps on this list, such as a video in your
  browser, never turn your sources down.
* **A limiter on the mix of all sources**, so several loud sources playing together never clip.
* **Command line:** `sidestage run` takes several apps, each with an optional preset
  (`sidestage run spotify discord=voice-right`), and `sidestage presets` lists the presets.
* `SIDESTAGE_CONFIG_DIR` and `SIDESTAGE_STATE_DIR` move the settings and the crash-recovery state
  to folders of your choice, for portable setups and test runs.

### Status

| System | Tested |
|---|---|
| Windows 11 | Version 0.1 was tested with Spotify while playing Valorant. The multi-source engine passed its live tests on real hardware with test tones: several sources captured at their exact levels, adding and removing sources while others play (no gaps, originals stayed hidden), 40 s with three sources and no dropouts, crash recovery with three hidden apps in 40 ms, and the never-duck list. Several real apps at once have not been tried yet. |
| macOS | Builds and passes its tests in CI, but has not been run on a Mac yet. |
| Linux | Builds and passes its tests in CI, but has not been run on a Linux desktop yet. |

Please report what works and what doesn't on your system:
<https://github.com/pixityy/Sidestage/issues>.

### Known limitations

* Headphones only: the effect does not work on speakers.
* An app can be only one source at a time.
* Windows: the Volume mixer shows every source's app at 0 while Sidestage runs; that is how the
  originals are hidden. If an app changes its own volume, its original sound is audible for about
  0.2 s.
* macOS: adding or removing a source dips the other sources for a few tens of milliseconds.
* Windows: shutting down or logging off while Sidestage runs can leave your apps turned down until
  Sidestage starts again (or run `sidestage restore`).

## v0.1.0 - 2026-10-15

The first release. Sidestage takes the sound of one app, usually your music player, and plays it
in 3D around your head on headphones: behind you by default, or in the next room, far away or
overhead. Your game's audio is never captured or processed.

### Downloads

| File | System |
|---|---|
| `Sidestage-0.1.0-windows-x86_64.zip` | Windows 11 (Windows 10 2004+ may work) |
| `Sidestage-0.1.0-macos-universal.zip` | macOS 14.2+, Apple silicon and Intel |
| `Sidestage-0.1.0-linux-x86_64.tar.gz` | Linux with PipeWire or PulseAudio, glibc 2.35+ |
| `SHA256SUMS.txt` | Checksums of the files above |

* **Windows**: unzip and run `sidestage.exe`. The app is not code-signed yet, so SmartScreen may
  warn you the first time: choose **More info** > **Run anyway**.
* **macOS**: unzip and move `Sidestage.app` to Applications. It is not notarized yet, so macOS
  blocks the first start: open it once, then allow it in System Settings > Privacy & Security
  > **Open Anyway**. On first use macOS asks for audio-capture permission; choose Allow.
* **Linux**: `tar -xzf Sidestage-0.1.0-linux-x86_64.tar.gz` and run `./sidestage`.

### What's in it

* Pick the app that plays your music; Sidestage remembers it and starts whenever it plays.
* A radar to drag the music around your head (direction, distance and height), plus six presets:
  Behind you, Next room, Far away, Above, Desk speaker and Lounge.
* Footstep-safe mode, which keeps the music out of the band where footsteps live.
* Ducking that turns the music down while the game is loud.
* Hides the app's normal sound so you don't hear it twice, and puts it back when Sidestage stops,
  including after a crash.
* A command line: `sidestage list`, `outputs`, `run <app>`, `restore`, and `--demo` to try the
  window without capturing anything.

### Status

| System | Tested |
|---|---|
| Windows 11 | The audio engine was tested on real hardware with test tones: capture, hiding the original, crash recovery and 2-minute soak runs without dropouts. Not yet tested with real music apps. |
| macOS | Builds and passes its tests in CI, but has not been run on a Mac yet. |
| Linux | Builds and passes its tests in CI, but has not been run on a Linux desktop yet. |

Please report what works and what doesn't on your system:
<https://github.com/pixityy/Sidestage/issues>.

### Known limitations

* Headphones only: the effect does not work on speakers.
* Generic head models sometimes place "behind" sounds in front of you. Moving the music a little to
  one side, or adding distance and room, helps.
* Windows: the Volume mixer shows the music app at 0 while Sidestage runs; that is how the original
  is hidden. If the app changes its own volume, its original sound is audible for about 0.2 s.
* Windows: shutting down or logging off while Sidestage runs can leave the music app turned down
  until Sidestage starts again (or run `sidestage restore`).
