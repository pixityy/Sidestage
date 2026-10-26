# Sound check recordings

The sound check plays three layers. The music is a Spotify preview (see `js/tracks.js`). The
voice and the game come from the files listed in `sounds.json`; while a list is empty, a
synthesized stand-in plays instead. Not published with the site (this file only).

```json
{
  "voice": ["voice-01.mp3", "voice-02.mp3"],
  "steps": ["step-01.mp3", "step-02.mp3"],
  "shots": ["shot-01.mp3"]
}
```

Paths are relative to this folder. The page levels every clip itself (voice lines to -6 dBFS
peak, footsteps to -10, shots to -12), so the files only need to be clean and trimmed.

## What each layer needs

| Layer | Clips | Each | What it should sound like |
|---|---|---|---|
| Voice chat | 8 to 12 lines | 1 to 4 s | One teammate on a headset mic giving short callouts: "one on B, he's low", "rotating A", "nice shot", "watch left", "push with me", a laugh. Close and dry: no music, no room echo, no background noise. |
| Footsteps | 6 to 10 one-shots | 0.2 to 0.5 s | Single steps on a hard floor (concrete or tile), sneakers or boots, slightly different from each other, like an enemy walking in a shooter. |
| Shots | 3 to 5 one-shots | 0.5 to 1.5 s | A rifle or pistol shot from across the map, with a short tail. Not right at the ears, not harsh. |

Mono WAV or FLAC at 44.1 or 48 kHz is best; they get converted to small MP3s for the site.
Each clip in its own file, trimmed to about 0.1 s of silence at each end.

Only use sounds you may publish: your own recordings (and friends who agree), or CC0 / public
domain libraries.

The game layer plays the clips in the game's own stereo position (an enemy crossing in front of
you) and never through the 3D stage. The voice is placed in 3D like a Discord source in
Sidestage, and the music turns down a little while it talks.
