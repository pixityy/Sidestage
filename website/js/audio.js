// The "Hear it" demo's sound. Everything is synthesized here, so the page ships no audio files:
// a lo-fi loop (the music), a murmuring voice (a friend on voice chat) and footsteps with the odd
// burst of shots (the game).
//
// The music and the voice go through the browser's HRTF panner, the music as two virtual
// speakers like Sidestage does. The game goes straight to the output, untouched, like Sidestage
// never touches a game's audio. This is the browser's generic HRTF, not Sidestage's renderer.

const db = (d) => Math.pow(10, d / 20);
const midi = (n) => 440 * Math.pow(2, (n - 69) / 12);
const rad = (deg) => (deg * Math.PI) / 180;

const BPM = 84;
const STEP = 60 / BPM / 4; // a 16th note
const SWING = STEP * 0.22;
const LOOKAHEAD = 0.15;

// Four bars: Fmaj7, Em7, Dm7, Cmaj7.
const CHORDS = [
  { bass: 41, keys: [57, 60, 64, 65] },
  { bass: 40, keys: [55, 59, 62, 64] },
  { bass: 38, keys: [53, 57, 60, 62] },
  { bass: 36, keys: [55, 59, 60, 64] },
];
// [bar, step, note] of the little top line.
const MELODY = [
  [0, 3, 72], [0, 6, 69], [0, 10, 67],
  [1, 3, 71], [1, 6, 67], [1, 11, 64],
  [2, 3, 69], [2, 6, 65], [2, 10, 69], [2, 14, 72],
  [3, 3, 67], [3, 7, 64], [3, 10, 62],
];

// Formants (Hz) of a few vowels, for the voice.
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
  constructor() {
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
  }

  get now() {
    return this.ctx ? this.ctx.currentTime : 0;
  }

  async start() {
    if (!this.ctx) this.#build();
    await this.ctx.resume();
    this.running = true;
    if (!this.timer) {
      this.next = { step: 0, time: this.ctx.currentTime + 0.1 };
      this.nextPhrase = this.ctx.currentTime + 1.2;
      this.nextWalk = this.ctx.currentTime + 2.4;
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

    // Instrument busses inside the music, each placed in the stereo mix.
    const place = (pan, g) => {
      const n = ctx.createStereoPanner();
      n.pan.value = pan;
      const gain = this.#gain(g);
      gain.connect(n).connect(this.musicBus);
      return gain;
    };
    this.inst = {
      keys: place(-0.45, 0.85),
      bass: place(0, 0.9),
      kick: place(0, 0.9),
      snare: place(0.1, 0.55),
      hat: place(0.55, 0.32),
      pluck: place(0.4, 0.5),
      crackle: place(-0.2, 0.09),
    };

    // Voice: a buzzy source through three formant filters, plus breath for consonants.
    this.vBus = this.#gain(1);
    const glottis = ctx.createOscillator();
    glottis.type = 'sawtooth';
    glottis.frequency.value = 130;
    const vib = ctx.createOscillator();
    vib.frequency.value = 5.2;
    const vibDepth = this.#gain(1.8);
    vib.connect(vibDepth).connect(glottis.frequency);
    this.f0 = glottis.frequency;
    this.vAmp = this.#gain(0);
    glottis.connect(this.vAmp);
    this.formants = [
      [this.#filter('bandpass', 730, 7), 1.0],
      [this.#filter('bandpass', 1090, 11), 0.55],
      [this.#filter('bandpass', 2440, 16), 0.3],
    ].map(([f, g]) => {
      this.vAmp.connect(f);
      f.connect(this.#gain(g * 1.3)).connect(this.vBus);
      return f;
    });
    const breath = ctx.createBufferSource();
    breath.buffer = this.noise;
    breath.loop = true;
    this.vBreath = this.#gain(0);
    breath.connect(this.#filter('bandpass', 3800, 1.2)).connect(this.vBreath).connect(this.vBus);
    glottis.start();
    vib.start();
    breath.start();
    // Voice-chat tone: band-limited, with a clarity lift.
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

  // ---- sequencing --------------------------------------------------------------------------

  #schedule() {
    const ctx = this.ctx;
    const horizon = ctx.currentTime + LOOKAHEAD;
    // After a stall (a background tab), skip ahead instead of firing a burst of late notes.
    if (this.next.time < ctx.currentTime - 0.05) {
      const behind = Math.ceil((ctx.currentTime - this.next.time) / STEP);
      this.next.step = (this.next.step + behind) % 64;
      this.next.time += behind * STEP;
    }
    this.nextPhrase = Math.max(this.nextPhrase, ctx.currentTime);
    this.nextWalk = Math.max(this.nextWalk, ctx.currentTime);
    while (this.next.time < horizon) {
      this.#musicStep(this.next.step, this.next.time);
      this.next.step = (this.next.step + 1) % 64;
      this.next.time += STEP;
    }
    if (this.nextPhrase < horizon) this.nextPhrase = this.#phrase(this.nextPhrase);
    if (this.nextWalk < horizon) this.nextWalk = this.#walk(this.nextWalk);
  }

  #musicStep(step, t) {
    const bar = Math.floor(step / 16);
    const s = step % 16;
    const swung = s % 2 === 1 ? t + SWING : t;
    const chord = CHORDS[bar];
    if (s === 0) chord.keys.forEach((n, i) => this.#key(midi(n), t + i * 0.012, 0.09, 2.6));
    if (s === 7 || s === 11) chord.keys.slice(1).forEach((n) => this.#key(midi(n), swung, 0.045, 0.9));
    if (s === 0 || s === 10) this.#bass(midi(chord.bass), t, s === 0 ? 1.3 : 0.5);
    if (s === 14 && bar % 2 === 1) this.#bass(midi(chord.bass + 7), swung, 0.3);
    if (s === 0 || s === 8 || (s === 10 && bar % 2 === 0)) this.#kick(t, s === 10 ? 0.6 : 1);
    if (s === 4 || s === 12) this.#snare(t);
    if (s % 2 === 0 || s === 15) this.#hat(swung, s % 4 === 2 ? 1 : 0.55);
    for (const [b, st, n] of MELODY) if (b === bar && st === s) this.#pluck(midi(n), swung);
    if (Math.random() < 0.35) this.#crackle(t + Math.random() * STEP);
  }

  #env(g, t, peak, attack, decay) {
    g.gain.setValueAtTime(0, t);
    g.gain.linearRampToValueAtTime(peak, t + attack);
    g.gain.setTargetAtTime(0, t + attack, decay / 4);
  }

  #key(f, t, vel, len) {
    const ctx = this.ctx;
    const g = this.#gain(0);
    const lp = this.#filter('lowpass', 2400, 0.4);
    for (const [mult, amp, type] of [[1, 1, 'sine'], [2.001, 0.22, 'sine'], [3.998, 0.05, 'triangle']]) {
      const o = ctx.createOscillator();
      o.type = type;
      o.frequency.value = f * mult;
      const a = this.#gain(amp);
      o.connect(a).connect(lp);
      o.start(t);
      o.stop(t + len + 0.2);
    }
    lp.connect(g).connect(this.inst.keys);
    this.#env(g, t, vel, 0.008, len);
  }

  #bass(f, t, len) {
    const ctx = this.ctx;
    const o = ctx.createOscillator();
    o.type = 'triangle';
    o.frequency.value = f;
    const g = this.#gain(0);
    o.connect(this.#filter('lowpass', 420, 0.6)).connect(g).connect(this.inst.bass);
    this.#env(g, t, 0.42, 0.01, len);
    o.start(t);
    o.stop(t + len + 0.3);
  }

  #kick(t, vel) {
    const ctx = this.ctx;
    const o = ctx.createOscillator();
    o.frequency.setValueAtTime(130, t);
    o.frequency.exponentialRampToValueAtTime(44, t + 0.12);
    const g = this.#gain(0);
    o.connect(g).connect(this.inst.kick);
    this.#env(g, t, 0.6 * vel, 0.004, 0.32);
    o.start(t);
    o.stop(t + 0.5);
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

  #snare(t) {
    this.#noiseHit(t, this.inst.snare, 'bandpass', 1900, 0.7, 0.5, 0.2);
    const o = this.ctx.createOscillator();
    o.frequency.value = 185;
    const g = this.#gain(0);
    o.connect(g).connect(this.inst.snare);
    this.#env(g, t, 0.25, 0.002, 0.08);
    o.start(t);
    o.stop(t + 0.2);
  }

  #hat(t, vel) {
    this.#noiseHit(t, this.inst.hat, 'highpass', 7500, 0.7, 0.35 * vel, 0.045);
  }

  #pluck(f, t) {
    const o = this.ctx.createOscillator();
    o.type = 'triangle';
    o.frequency.value = f;
    const g = this.#gain(0);
    o.connect(this.#filter('lowpass', 3200, 0.5)).connect(g).connect(this.inst.pluck);
    this.#env(g, t, 0.16, 0.004, 0.55);
    o.start(t);
    o.stop(t + 0.8);
  }

  #crackle(t) {
    this.#noiseHit(t, this.inst.crackle, 'highpass', 2500, 0.5, 0.5 + Math.random(), 0.004);
  }

  /** One spoken phrase from `t`; returns when the next one starts. */
  #phrase(t) {
    const n = 5 + Math.floor(Math.random() * 7);
    const base = 118 + Math.random() * 22;
    let at = t;
    for (let i = 0; i < n; i++) {
      const dur = 0.11 + Math.random() * 0.12 + (i === n - 1 ? 0.12 : 0);
      const v = VOWELS[Math.floor(Math.random() * VOWELS.length)];
      this.formants.forEach((f, k) => f.frequency.setTargetAtTime(v[k] * (0.95 + Math.random() * 0.1), at, 0.022));
      const lift = 1 + 0.16 * (1 - i / n) + (i === 1 ? 0.08 : 0);
      this.f0.setTargetAtTime(base * lift * (0.96 + Math.random() * 0.08), at, 0.04);
      const peak = 0.75 + Math.random() * 0.25;
      this.vAmp.gain.setTargetAtTime(peak, at + 0.015, 0.02);
      this.vAmp.gain.setTargetAtTime(Math.random() < 0.3 ? 0 : peak * 0.25, at + dur - 0.035, 0.018);
      if (Math.random() < 0.45) {
        this.vBreath.gain.setTargetAtTime(0.22, at - 0.03, 0.006);
        this.vBreath.gain.setTargetAtTime(0, at + 0.01, 0.012);
      }
      at += dur;
    }
    this.vAmp.gain.setTargetAtTime(0, at, 0.03);
    // The music turns down a little while someone talks (the next update's voice ducking).
    this.duck.gain.setTargetAtTime(db(-5), t, 0.08);
    this.duck.gain.setTargetAtTime(1, at + 0.25, 0.25);
    this.#flag('talking', t, at + 0.25);
    return at + 1.1 + Math.random() * 1.8;
  }

  /** A few footsteps crossing in front, sometimes followed by shots; returns the next start. */
  #walk(t) {
    this.walks++;
    const dir = this.walks % 2 ? 1 : -1;
    const steps = 6;
    const gap = 0.36;
    this.gPan.pan.setValueAtTime(-0.65 * dir, t);
    this.gPan.pan.linearRampToValueAtTime(0.65 * dir, t + steps * gap);
    for (let i = 0; i < steps; i++) {
      const at = t + i * gap + Math.random() * 0.03;
      const v = 0.8 + Math.random() * 0.3;
      this.events.push({ t: at, kind: 'step', pan: -0.65 * dir + (1.3 * dir * i) / steps });
      this.#noiseHit(at, this.gBus, 'lowpass', 900, 0.8, 1.25 * v, 0.07, 0.7);
      this.#noiseHit(at + 0.05, this.gBus, 'bandpass', 3200, 1.4, 0.6 * v, 0.03);
    }
    let end = t + steps * gap;
    if (this.walks % 3 === 0) {
      const at0 = end + 0.35;
      for (let i = 0; i < 3; i++) {
        const at = at0 + i * 0.13;
        this.events.push({ t: at, kind: 'shot', pan: 0.65 * dir });
        this.#noiseHit(at, this.gBus, 'lowpass', 2600, 0.6, 0.5, 0.16);
        const o = this.ctx.createOscillator();
        o.frequency.setValueAtTime(110, at);
        o.frequency.exponentialRampToValueAtTime(50, at + 0.1);
        const g = this.#gain(0);
        o.connect(g).connect(this.gBus);
        this.#env(g, at, 0.4, 0.002, 0.14);
        o.start(at);
        o.stop(at + 0.3);
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
