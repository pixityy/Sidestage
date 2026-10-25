# Sidestage on macOS

Sidestage needs **macOS 14.2 or newer**: it captures each app you add with a Core Audio
process tap, which macOS 14.2 introduced.

## Build the app

```sh
packaging/macos/make-app.sh              # this Mac's architecture
packaging/macos/make-app.sh --universal  # Apple silicon + Intel (needs both rustup targets)
packaging/macos/make-app.sh --out dist   # somewhere other than target/macos-app
```

(If the script is not executable in your checkout, run it as `bash packaging/macos/make-app.sh`.)

The result is `target/macos-app/Sidestage.app` and a zip next to it. CI builds the universal
bundle and uploads the zip as the `sidestage-<version>-macos-universal` artifact. Set
`SIDESTAGE_SIGN_IDENTITY="Developer ID Application: ..."` to sign for distribution (hardened
runtime, `Sidestage.entitlements`), then notarize the zip with `xcrun notarytool` and staple
the app.

## Deployment target

All macOS builds use `MACOSX_DEPLOYMENT_TARGET=14.2`. `crates/sidestage/build.rs` sets it for
the `sidestage` binary (raising a lower value) and `make-app.sh` exports it. The process-tap
functions are linked strongly, so the binary must not claim to run on older systems.

## Permission

The first start shows macOS's audio-capture prompt ("System Audio Recording Only"). Its text
comes from `NSAudioCaptureUsageDescription` in `Info.plist`, which `build.rs` also embeds into
the executable, so `cargo run` builds can ask too. Without that key macOS never asks and
Sidestage receives silence.

* Answer the prompt: an ignored prompt times out after about a minute and Core Audio then fails
  until Sidestage restarts.
* When you run Sidestage from a terminal, macOS attributes the request to the terminal app.
  Allow the terminal, or run the bundled app (`open target/macos-app/Sidestage.app`).
* Ad-hoc signed builds may ask again after every rebuild.
* To ask again from scratch: `tccutil reset All com.pixityy.sidestage`.

While a start waits for the prompt, Sidestage's status says that macOS may be asking. If
permission is denied, Sidestage says so and stops (the apps are un-muted at once); allow it in
System Settings > Privacy & Security > Screen & System Audio Recording and start again, or quit
and reopen Sidestage if it still fails.
