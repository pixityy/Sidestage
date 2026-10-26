// The sound check's sound. The music is a 30-second Spotify preview, streamed from Spotify when the
// visitor presses play and rotating through the tracks in tracks.js. The voice (a friend on voice
// chat) and the game (footsteps and the odd burst of shots) are recordings listed in
// audio/sounds.json; until those exist, a synthesized murmur and noise footsteps stand in.
//
// The music and the voice go through the browser's HRTF panner, the music as two virtual speakers
// like Sidestage does. The game goes straight to the output, untouched, like Sidestage never
// touches a game's audio. This is the browser's generic HRTF, not Sidestage's renderer.

const db = (d) => Math.pow(10, d / 20);
const rad = (deg) => (deg * Math.PI) / 180;
const pick = (list) => list[Math.floor(Math.random() * list.length)];

const LOOKAHEAD = 0.15;
// Previews are mastered loud; each is brought to this RMS level before the 3D stage.
const MUSIC_RMS_DB = -17;
// Recorded voice lines and game one-shots are brought to these peak levels.
const VOICE_PEAK_DB = -6;
const STEP_PEAK_DB = -10;
const SHOT_PEAK_DB = -12;
const CROSSFADE = 1.2;

// Formants (Hz) of a few vowels, for the stand-in voice.
const VOWELS = [
  [730, 1090, 2440],
  [530, 1840, 2480],
  [300, 2200, 2950],
  [570, 840, 2410],
  [440, 1020, 2240],
  [660, 1700, 2400],
];

export const supported = typeof window !== 'undefined' && !!(window.AudioContext || window.webkitAudioContext);

export class DemoAudio {
  /** `tracks`: [{ preview, title, ... }]; `soundsUrl`: the list of recorded voice and game clips. */
  constructor({ tracks = [], soundsUrl = 'audio/sounds.json' } = {}) {
    this.ctx = null;
    this.running = false;
    this.layers = { music: true, voice: true, game: true };
    this.music = { az: 180, dist: 1.5, el: 0, width: 60, lowpass: 18000, room: 'small', reverbDb: -12 };
    this.voice = { az: 90, dist: 1, el: 0 };
    this.footstepSafe = false;
    this.bypass = false;
    // Raised while the voice talks and while the game is loud, for the visuals.
    this.talking = false;
    this.ducking = false;
    // Game sounds as they are scheduled, { t (audio clock), kind: 'step' | 'shot', pan }, for
    // the radar to show; the page takes them off the front as they play.
    this.events = [];
    this.tracks = tracks.map((t) => ({ ...t }));
    this.trackIndex = 0;
    this.current = null;
    /** Called with (index, state) when the music changes: 'loading', 'playing' or 'error'. */
    this.onTrack = null;
    this.soundsUrl = soundsUrl;
    this.clips = { voice: [], steps: [], shots: [] };
  }

  get now() {
    return this.ctx ? this.ctx.currentTime : 0;
  }

  async start() {
    if (!this.ctx) this.#build();
    await this.ctx.resume();
    this.running = true;
    if (!this.current && this.tracks.length) this.#playTrack(this.trackIndex, false);
    if (!this.timer) {
      this.nextPhrase = this.ctx.currentTime + 1.6;
      this.nextWalk = this.ctx.currentTime + 2.6;
      this.walks = 0;
      this.timer = setInterval(() => this.#schedule(), 25);
    }
  }

  async stop() {
    this.running = false;
    clearInterval(this.timer);
    this.timer = null;
    if (this.ctx) await this.ctx.suspend();
  }

  /** Crossfade to the next preview (or to `index`). */
  skip(index = (this.trackIndex + 1) % this.tracks.length) {
    if (!this.ctx || !this.tracks.length) {
      this.trackIndex = index;
      this.onTrack?.(index, 'idle');
      return;
    }
    this.#playTrack(index, true);
  }

  /** Where the current preview is: { index, position, duration } in seconds. */
  trackTime() {
    const c = this.current;
    if (!c || !this.ctx) return { index: this.trackIndex, position: 0, duration: 30 };
    return { index: c.index, position: Math.max(0, Math.min(c.duration, this.ctx.currentTime - c.start)), duration: c.duration };
  }

  setMusic(m) {
    Object.assign(this.music, m);
    if (this.ctx) this.#applyMusic();
  }

  setVoice(v) {
    Object.assign(this.voice, v);
    if (this.ctx) this.#applyVoice();
  }

  setLayer(name, on) {
    this.layers[name] = on;
    if (this.ctx) this.#ramp(this.out[name].gain, on ? 1 : 0, 0.05);
  }

  setFootstepSafe(on) {
    this.footstepSafe = on;
    if (this.ctx) this.#applyMusic();
  }

  setBypass(on) {
    this.bypass = on;
    if (!this.ctx) return;
    for (const p of [this.mWet, this.vWet]) this.#ramp(p.gain, on ? 0 : 1, 0.012);
    for (const p of [this.mDry, this.vDry]) this.#ramp(p.gain, on ? 1 : 0, 0.012);
  }

  /** Smoothed levels 0..1 for the radar's glow. */
  levels() {
    if (!this.ctx || !this.running) return { music: 0, voice: 0, game: 0 };
    const rms = (a) => {
      a.getFloatTimeDomainData(this.scratch);
      let s = 0;
      for (const x of this.scratch) s += x * x;
      return Math.min(1, Math.sqrt(s / this.scratch.length) * 4);
    };
    return { music: rms(this.meters.music), voice: rms(this.meters.voice), game: rms(this.meters.game) };
  }

  // ---- graph -------------------------------------------------------------------------------

  #build() {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    const ctx = (this.ctx = new Ctx({ latencyHint: 'interactive' }));
    const L = ctx.listener;
    if (L.forwardX) {
      L.forwardX.value = 0; L.forwardY.value = 0; L.forwardZ.value = -1;
      L.upX.value = 0; L.upY.value = 1; L.upZ.value = 0;
    } else {
      L.setOrientation(0, 0, -1, 0, 1, 0);
    }

    this.noise = ctx.createBuffer(1, ctx.sampleRate * 2, ctx.sampleRate);
    const nd = this.noise.getChannelData(0);
    for (let i = 0; i < nd.length; i++) nd[i] = Math.random() * 2 - 1;

    const limiter = ctx.createDynamicsCompressor();
    limiter.threshold.value = -8;
    limiter.knee.value = 6;
    limiter.ratio.value = 8;
    limiter.attack.value = 0.003;
    limiter.release.value = 0.2;
    this.master = this.#gain(0.9);
    this.master.connect(limiter).connect(ctx.destination);

    this.out = { music: this.#gain(1), voice: this.#gain(1), game: this.#gain(1) };
    this.meters = {};
    this.scratch = new Float32Array(512);
    for (const k of Object.keys(this.out)) {
      const a = ctx.createAnalyser();
      a.fftSize = 512;
      this.out[k].connect(this.master);
      this.out[k].connect(a);
      this.meters[k] = a;
    }

    // Music: bus -> tone -> two HRTF speakers (L, R) -> distance -> out; dry path for bypass.
    // Centred parts reach both ears from both speakers, so the bus starts 6 dB down.
    this.musicBus = this.#gain(0.5);
    this.musicBus.channelCount = 2;
    this.musicBus.channelCountMode = 'explicit';
    this.mHigh = this.#filter('highpass', 30);
    this.mLow = this.#filter('lowpass', 18000, 0.5);
    this.mBass = this.#filter('lowshelf', 150);
    this.mDip = this.#filter('peaking', 3200, 1.1);
    this.mAir = this.#filter('highshelf', 8000);
    this.mDist = this.#filter('highshelf', 10000);
    this.musicBus.connect(this.mHigh).connect(this.mLow).connect(this.mBass).connect(this.mDip).connect(this.mAir);
    const split = ctx.createChannelSplitter(2);
    this.mAir.connect(split);
    this.mPan = [this.#panner(), this.#panner()];
    this.mDistGain = this.#gain(1);
    split.connect(this.mPan[0], 0);
    split.connect(this.mPan[1], 1);
    this.mPan[0].connect(this.mDist);
    this.mPan[1].connect(this.mDist);
    this.mDist.connect(this.mDistGain);
    this.mWet = this.#gain(1);
    this.mDistGain.connect(this.mWet);
    // Rooms, fed the unprocessed music like Sidestage's: a small room and a hall.
    this.mSend = { small: this.#gain(0), hall: this.#gain(0) };
    for (const [k, t60] of [['small', 0.5], ['hall', 2.2]]) {
      const [input, output] = this.#room(t60);
      this.mAir.connect(this.mSend[k]).connect(input);
      output.connect(this.mWet);
    }
    this.mDry = this.#gain(0);
    this.musicBus.connect(this.mDry);
    this.duck = this.#gain(1);
    this.mWet.connect(this.duck);
    this.mDry.connect(this.duck);
    this.duck.connect(this.out.music);

    // Voice: recorded lines (or the stand-in murmur) through a voice-chat tone, then placed.
    this.vBus = this.#gain(1);
    const vHp = this.#filter('highpass', 140, 0.7);
    const vLp = this.#filter('lowpass', 7000, 0.7);
    const vClarity = this.#filter('peaking', 2600, 0.9);
    vClarity.gain.value = 3;
    this.vBus.connect(vHp).connect(vLp).connect(vClarity);
    this.vPan = this.#panner();
    this.vDistGain = this.#gain(1);
    this.vWet = this.#gain(1);
    vClarity.connect(this.vPan).connect(this.vDistGain).connect(this.vWet).connect(this.out.voice);
    const [vRoomIn, vRoomOut] = this.#room(0.4);
    vClarity.connect(this.#gain(db(-18))).connect(vRoomIn);
    vRoomOut.connect(this.vWet);
    this.vDry = this.#gain(0);
    vClarity.connect(this.vDry).connect(this.out.voice);

    // Game: plain stereo, straight to the output.
    this.gPan = ctx.createStereoPanner();
    this.gBus = this.#gain(1);
    this.gBus.connect(this.gPan).connect(this.out.game);

    for (const [k, on] of Object.entries(this.layers)) this.out[k].gain.value = on ? 1 : 0;
    this.#applyMusic();
    this.#applyVoice();
    this.setBypass(this.bypass);
    this.#loadClips();
  }

  #gain(v) {
    const g = this.ctx.createGain();
    g.gain.value = v;
    return g;
  }

  #filter(type, freq, q) {
    const f = this.ctx.createBiquadFilter();
    f.type = type;
    f.frequency.value = freq;
    if (q !== undefined) f.Q.value = q;
    return f;
  }

  #panner() {
    const p = this.ctx.createPanner();
    p.panningModel = 'HRTF';
    p.distanceModel = 'linear';
    p.rolloffFactor = 0; // distance is handled by our own gain, like Sidestage's 1/sqrt(r)
    return p;
  }

  /** A room as decaying stereo noise: `t60` seconds to fade by 60 dB. Returns [input, output]. */
  #room(t60) {
    const ctx = this.ctx;
    const len = Math.floor(ctx.sampleRate * t60 * 1.2);
    const buf = ctx.createBuffer(2, len, ctx.sampleRate);
    for (let c = 0; c < 2; c++) {
      const d = buf.getChannelData(c);
      for (let i = 0; i < len; i++) {
        const t = i / ctx.sampleRate;
        d[i] = (Math.random() * 2 - 1) * Math.exp((-6.9 * t) / t60) * Math.min(1, t * 300);
      }
    }
    const conv = ctx.createConvolver();
    conv.buffer = buf;
    const damp = this.#filter('lowpass', t60 > 1 ? 4500 : 7000, 0.5);
    conv.connect(damp);
    return [conv, damp];
  }

  #ramp(param, value, tc = 0.03) {
    param.setTargetAtTime(value, this.ctx.currentTime, tc);
  }

  #place(panner, az, el, dist) {
    const a = rad(az);
    const e = rad(el);
    const x = Math.sin(a) * Math.cos(e) * dist;
    const y = Math.sin(e) * dist;
    const z = -Math.cos(a) * Math.cos(e) * dist;
    if (panner.positionX) {
      this.#ramp(panner.positionX, x, 0.02);
      this.#ramp(panner.positionY, y, 0.02);
      this.#ramp(panner.positionZ, z, 0.02);
    } else {
      panner.setPosition(x, y, z);
    }
  }

  #applyMusic() {
    const m = this.music;
    // The two speakers sit at az -/+ width/2, narrowing to a point at the sides so the music's
    // left channel always stays on the listener's left (as in Sidestage).
    const spread = (m.width / 2) * Math.max(-1, Math.min(1, 2 * Math.cos(rad(m.az))));
    this.#place(this.mPan[0], m.az - spread, m.el, m.dist);
    this.#place(this.mPan[1], m.az + spread, m.el, m.dist);
    const d = Math.max(0.5, Math.min(20, m.dist));
    this.#ramp(this.mDistGain.gain, Math.sqrt(1.5 / d));
    this.#ramp(this.mDist.gain, -1.2 * Math.log2(Math.max(1, d)));
    const fs = this.footstepSafe;
    this.#ramp(this.mLow.frequency, fs ? Math.min(m.lowpass, 8000) : m.lowpass);
    this.#ramp(this.mDip.gain, fs ? -6 : -3);
    this.#ramp(this.mAir.gain, fs ? -6 : 0);
    this.#ramp(this.mBass.gain, fs ? -3 : m.bass || 0);
    this.#ramp(this.mHigh.frequency, m.highpass || 30);
    for (const k of ['small', 'hall']) this.#ramp(this.mSend[k].gain, m.room === k ? db(m.reverbDb) : 0, 0.05);
  }

  #applyVoice() {
    const v = this.voice;
    this.#place(this.vPan, v.az, v.el, v.dist);
    const d = Math.max(0.5, Math.min(20, v.dist));
    this.#ramp(this.vDistGain.gain, Math.sqrt(1 / d));
  }

  // ---- the music: Spotify previews -----------------------------------------------------------

  /** Fetch and decode a preview once; remembers its loudness-matching gain. */
  #loadTrack(i) {
    const t = this.tracks[i];
    if (!t.buffer) {
      t.buffer = fetch(t.preview, { mode: 'cors', credentials: 'omit', referrerPolicy: 'no-referrer' })
        .then((r) => {
          if (!r.ok) throw new Error(`preview ${r.status}`);
          return r.arrayBuffer();
        })
        .then((b) => this.ctx.decodeAudioData(b))
        .then((buf) => {
          t.gain = Math.min(1.5, db(MUSIC_RMS_DB) / Math.max(1e-4, rmsOf(buf)));
          return buf;
        });
      t.buffer.catch(() => (t.buffer = null));
    }
    return t.buffer;
  }

  async #playTrack(i, fade) {
    const token = (this.trackToken = (this.trackToken || 0) + 1);
    this.trackIndex = i;
    this.onTrack?.(i, 'loading');
    let buf;
    try {
      buf = await this.#loadTrack(i);
    } catch {
      if (token !== this.trackToken) return;
      this.onTrack?.(i, 'error');
      // Try the next one; give up quietly after a full round of failures.
      this.failures = (this.failures || 0) + 1;
      if (this.failures < this.tracks.length) setTimeout(() => token === this.trackToken && this.#playTrack((i + 1) % this.tracks.length, false), 800);
      return;
    }
    if (token !== this.trackToken || !this.ctx) return;
    this.failures = 0;
    const ctx = this.ctx;
    const now = ctx.currentTime;
    const g = this.#gain(0);
    const src = ctx.createBufferSource();
    src.buffer = buf;
    src.connect(g).connect(this.musicBus);
    const level = this.tracks[i].gain;
    g.gain.setValueAtTime(0, now);
    g.gain.linearRampToValueAtTime(level, now + (fade ? CROSSFADE : 0.04));
    g.gain.setValueAtTime(level, now + buf.duration - CROSSFADE);
    g.gain.linearRampToValueAtTime(0, now + buf.duration);
    src.start(now);
    src.stop(now + buf.duration + 0.05);
    const old = this.current;
    if (old) {
      old.g.gain.cancelScheduledValues(now);
      old.g.gain.setValueAtTime(old.g.gain.value, now);
      old.g.gain.linearRampToValueAtTime(0, now + (fade ? CROSSFADE : 0.05));
      try { old.src.stop(now + CROSSFADE + 0.05); } catch { /* already stopped */ }
    }
    this.current = { src, g, start: now, duration: buf.duration, index: i };
    this.onTrack?.(i, 'playing');
    this.#loadTrack((i + 1) % this.tracks.length).catch(() => {});
  }

  // ---- recorded voice and game clips ---------------------------------------------------------

  async #loadClips() {
    let list;
    try {
      const r = await fetch(this.soundsUrl);
      if (!r.ok) return;
      list = await r.json();
    } catch {
      return; // no recordings yet: the stand-ins play
    }
    const base = new URL(this.soundsUrl, document.baseURI);
    const load = async (file, peakDb) => {
      const r = await fetch(new URL(file, base));
      const buf = await this.ctx.decodeAudioData(await r.arrayBuffer());
      return { buf, gain: db(peakDb) / Math.max(1e-4, peakOf(buf)) };
    };
    for (const [key, peakDb] of [['voice', VOICE_PEAK_DB], ['steps', STEP_PEAK_DB], ['shots', SHOT_PEAK_DB]]) {
      const files = Array.isArray(list[key]) ? list[key] : [];
      const loaded = await Promise.allSettled(files.map((f) => load(f, peakDb)));
      this.clips[key] = loaded.filter((x) => x.status === 'fulfilled').map((x) => x.value);
    }
  }

  /** Play a clip into `dest` at `t`; returns its length in seconds. */
  #clip(c, t, dest, { gain = 1, rate = 1 } = {}) {
    const src = this.ctx.createBufferSource();
    src.buffer = c.buf;
    src.playbackRate.value = rate;
    const g = this.#gain(c.gain * gain);
    src.connect(g).connect(dest);
    src.start(t);
    return c.buf.duration / rate;
  }

  // ---- sequencing ----------------------------------------------------------------------------

  #schedule() {
    const ctx = this.ctx;
    const horizon = ctx.currentTime + LOOKAHEAD;
    // After a stall (a background tab), start from now instead of firing a burst of late events.
    this.nextPhrase = Math.max(this.nextPhrase, ctx.currentTime);
    this.nextWalk = Math.max(this.nextWalk, ctx.currentTime);
    if (this.nextPhrase < horizon) this.nextPhrase = this.#phrase(this.nextPhrase);
    if (this.nextWalk < horizon) this.nextWalk = this.#walk(this.nextWalk);
    // Crossfade into the next preview as this one ends.
    const c = this.current;
    if (c && !c.handedOff && ctx.currentTime > c.start + c.duration - CROSSFADE - 0.05) {
      c.handedOff = true;
      this.#playTrack((c.index + 1) % this.tracks.length, true);
    }
  }

  #env(g, t, peak, attack, decay) {
    g.gain.setValueAtTime(0, t);
    g.gain.linearRampToValueAtTime(peak, t + attack);
    g.gain.setTargetAtTime(0, t + attack, decay / 4);
  }

  #noiseHit(t, dest, type, freq, q, peak, decay, rate = 1) {
    const src = this.ctx.createBufferSource();
    src.buffer = this.noise;
    src.playbackRate.value = rate;
    const g = this.#gain(0);
    src.connect(this.#filter(type, freq, q)).connect(g).connect(dest);
    this.#env(g, t, peak, 0.002, decay);
    src.start(t, Math.random() * 1.5);
    src.stop(t + decay * 2 + 0.05);
  }

  /** One spoken line from `t`; returns when the next one starts. */
  #phrase(t) {
    let end;
    if (this.clips.voice.length) {
      end = t + this.#clip(pick(this.clips.voice), t, this.vBus);
    } else {
      end = this.#murmur(t);
    }
    // Voice chat is never ducked; the music turns down a little while someone talks.
    this.duck.gain.setTargetAtTime(db(-5), t, 0.08);
    this.duck.gain.setTargetAtTime(1, end + 0.25, 0.25);
    this.#flag('talking', t, end + 0.25);
    return end + 1.4 + Math.random() * 2.2;
  }

  /** The stand-in voice: a buzzy source through three formant filters. Returns its end time. */
  #murmur(t) {
    if (!this.vAmp) {
      const ctx = this.ctx;
      const glottis = ctx.createOscillator();
      glottis.type = 'sawtooth';
      glottis.frequency.value = 130;
      const vib = ctx.createOscillator();
      vib.frequency.value = 5.2;
      vib.connect(this.#gain(1.8)).connect(glottis.frequency);
      this.f0 = glottis.frequency;
      this.vAmp = this.#gain(0);
      glottis.connect(this.vAmp);
      this.formants = [[730, 7, 1.0], [1090, 11, 0.55], [2440, 16, 0.3]].map(([f, q, g]) => {
        const bp = this.#filter('bandpass', f, q);
        this.vAmp.connect(bp);
        bp.connect(this.#gain(g * 1.3)).connect(this.vBus);
        return bp;
      });
      glottis.start();
      vib.start();
    }
    const n = 5 + Math.floor(Math.random() * 7);
    const base = 118 + Math.random() * 22;
    let at = t;
    for (let i = 0; i < n; i++) {
      const dur = 0.11 + Math.random() * 0.12 + (i === n - 1 ? 0.12 : 0);
      const v = pick(VOWELS);
      this.formants.forEach((f, k) => f.frequency.setTargetAtTime(v[k] * (0.95 + Math.random() * 0.1), at, 0.022));
      this.f0.setTargetAtTime(base * (1 + 0.16 * (1 - i / n)) * (0.96 + Math.random() * 0.08), at, 0.04);
      const peak = 0.75 + Math.random() * 0.25;
      this.vAmp.gain.setTargetAtTime(peak, at + 0.015, 0.02);
      this.vAmp.gain.setTargetAtTime(Math.random() < 0.3 ? 0 : peak * 0.25, at + dur - 0.035, 0.018);
      at += dur;
    }
    this.vAmp.gain.setTargetAtTime(0, at, 0.03);
    return at;
  }

  /** A few footsteps crossing in front, sometimes followed by shots; returns the next start. */
  #walk(t) {
    this.walks++;
    const dir = this.walks % 2 ? 1 : -1;
    const steps = 6;
    const gap = 0.36;
    // The game's own stereo mix moves the enemy across in front of you; Sidestage leaves it be.
    this.gPan.pan.setValueAtTime(-0.65 * dir, t);
    this.gPan.pan.linearRampToValueAtTime(0.65 * dir, t + steps * gap);
    for (let i = 0; i < steps; i++) {
      const at = t + i * gap + Math.random() * 0.03;
      const v = 0.8 + Math.random() * 0.3;
      this.events.push({ t: at, kind: 'step', pan: -0.65 * dir + (1.3 * dir * i) / steps });
      if (this.clips.steps.length) {
        this.#clip(pick(this.clips.steps), at, this.gBus, { gain: v, rate: 0.94 + Math.random() * 0.12 });
      } else {
        this.#noiseHit(at, this.gBus, 'lowpass', 900, 0.8, 1.25 * v, 0.07, 0.7);
        this.#noiseHit(at + 0.05, this.gBus, 'bandpass', 3200, 1.4, 0.6 * v, 0.03);
      }
    }
    let end = t + steps * gap;
    if (this.walks % 3 === 0) {
      const at0 = end + 0.35;
      for (let i = 0; i < 3; i++) {
        const at = at0 + i * 0.13;
        this.events.push({ t: at, kind: 'shot', pan: 0.65 * dir });
        if (this.clips.shots.length) {
          this.#clip(pick(this.clips.shots), at, this.gBus, { gain: 0.9 + Math.random() * 0.1 });
        } else {
          this.#noiseHit(at, this.gBus, 'lowpass', 2600, 0.6, 0.5, 0.16);
        }
      }
      end = at0 + 0.5;
      // A loud moment: the music ducks for the game (Standard ducking).
      this.duck.gain.setTargetAtTime(db(-8), at0, 0.02);
      this.duck.gain.setTargetAtTime(1, end + 0.15, 0.2);
      this.#flag('ducking', at0, end + 0.5);
    }
    return end + 1.8 + Math.random() * 1.5;
  }

  /** Raise `this[name]` between two audio-clock times, for the visuals. */
  #flag(name, from, to) {
    const ms = (s) => Math.max(0, (s - this.ctx.currentTime) * 1000);
    setTimeout(() => (this[name] = true), ms(from));
    setTimeout(() => (this[name] = false), ms(to));
  }
}

function rmsOf(buf) {
  let sum = 0;
  let n = 0;
  for (let c = 0; c < buf.numberOfChannels; c++) {
    const d = buf.getChannelData(c);
    for (let i = 0; i < d.length; i += 4) sum += d[i] * d[i];
    n += Math.ceil(d.length / 4);
  }
  return Math.sqrt(sum / Math.max(1, n));
}

function peakOf(buf) {
  let p = 0;
  for (let c = 0; c < buf.numberOfChannels; c++) {
    for (const x of buf.getChannelData(c)) p = Math.max(p, Math.abs(x));
  }
  return p;
}
