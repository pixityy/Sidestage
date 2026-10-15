# Changelog

Each `## v<version>` section is also the text of that version's GitHub release.

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
