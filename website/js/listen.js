// Act III: the sound check. A radar you can play with: drag the music and the voice around a
// headphone listener while game footsteps cross the GAME wedge, untouched. The sound comes from
// audio.js; this file draws the stage and wires up the desk.

import { DemoAudio, supported } from './audio.js';
import {
  C, OUTER, HEAD, NSS, GLYPH, rd, clamp, wrap, radius, pt, polar, arcD, speakers,
  headSVG, ringsSVG, wedgeSVG, ticksSVG, ringLabelsSVG, layoutRingLabels,
  directionName, formatDistance, setAttrs,
} from './stage.js';

// Music presets from crates/sidestage-dsp/src/presets.rs. The rooms are approximations: the page
// has a small room and a hall.
const PRESETS = {
  behind: { az: 180, el: 0, dist: 1.5, width: 60, lowpass: 18000, highpass: 30, bass: 0, room: 'small', reverbDb: -12 },
  next: { az: 150, el: 0, dist: 5, width: 0, lowpass: 1400, highpass: 30, bass: 3, room: 'small', reverbDb: -6 },
  far: { az: 180, el: 5, dist: 15, width: 10, lowpass: 6000, highpass: 30, bass: 0, room: 'hall', reverbDb: -10 },
  above: { az: 180, el: 60, dist: 2, width: 30, lowpass: 18000, highpass: 30, bass: 0, room: 'small', reverbDb: -12 },
  desk: { az: 35, el: -20, dist: 0.7, width: 0, lowpass: 8000, highpass: 180, bass: 0, room: 'small', reverbDb: -16 },
  lounge: { az: 180, el: 10, dist: 2.5, width: 90, lowpass: 16000, highpass: 30, bass: 0, room: 'small', reverbDb: -10 },
};
const VOICE_HOME = { az: 90, el: 0, dist: 1 };
const SNAP = 15;

export function initSoundCheck({ anim }) {
  const root = document.getElementById('soundcheck');
  if (!root) return;
  const host = root.querySelector('#sc-radar');
  const $ = (sel) => root.querySelector(sel);
  const audio = new DemoAudio();

  const src = {
    // `base` is the preset whose sound (room, muffle) is in use; `preset` is set while the music
    // also sits exactly on that preset's mark.
    music: { id: 'music', name: 'Music', glyph: 'note', ...PRESETS.behind, base: 'behind', preset: 'behind' },
    voice: { id: 'voice', name: 'Voice chat', glyph: 'voice', ...VOICE_HOME, width: 0 },
  };
  let selected = 'music';

  host.innerHTML = radarSVG();
  const svg = host.firstElementChild;
  const stepsLayer = svg.querySelector('.steps');
  for (const s of Object.values(src)) {
    s.node = svg.querySelector(`#sc-${s.id}`);
    s.link = svg.querySelector(`#sc-ln-${s.id}`);
    s.chip = svg.querySelector(`#sc-ch-${s.id}`);
    s.meta = s.chip.querySelector('.chip-meta');
  }
  const spread = svg.querySelector('#sc-sp');
  const lr = svg.querySelector('#sc-lr');
  const badge = svg.querySelector('#sc-badge');

  const mq = window.matchMedia('(max-width: 720px)');
  let K = 1;

  // ---- drawing --------------------------------------------------------------------------------

  function draw(s) {
    const r = radius(s.dist);
    const [x, y] = pt(s.az, r);
    s.x = x;
    s.y = y;
    s.node.setAttribute('transform', `translate(${x} ${y})`);
    s.node.querySelector('.p-scale').setAttribute('transform', `scale(${K})`);
    const h = pt(s.az, HEAD);
    setAttrs(s.link, { x1: h[0], y1: h[1], x2: x, y2: y });
    const label = `${directionName(s.az)} · ${formatDistance(s.dist)}`;
    s.meta.textContent = label.toUpperCase();
    s.node.setAttribute('aria-valuenow', String(Math.round(wrap(s.az))));
    s.node.setAttribute('aria-valuetext', `${label}, height ${heightText(s.el)}`);
    if (s.id === 'music') {
      if (s.width > 0.5) {
        const [aL, aR] = speakers(s.az, s.width);
        const [lo, hi] = aL < aR ? [aL, aR] : [aR, aL];
        spread.setAttribute('d', hi - lo > 0.5 ? arcD(lo, hi, r) : '');
        const show = hi - lo > 0.5;
        lr.style.display = show ? '' : 'none';
        const pl = pt(aL, r);
        const pr = pt(aR, r);
        const tl = pt(aL, r + 20 * K);
        const tr = pt(aR, r + 20 * K);
        setAttrs(lr.querySelector('.lr-l'), { cx: pl[0], cy: pl[1], r: 5.5 * K });
        setAttrs(lr.querySelector('.lr-r'), { cx: pr[0], cy: pr[1], r: 5.5 * K });
        setAttrs(lr.querySelector('.lr-lt'), { x: tl[0], y: tl[1] });
        setAttrs(lr.querySelector('.lr-rt'), { x: tr[0], y: tr[1] });
      } else {
        spread.setAttribute('d', '');
        lr.style.display = 'none';
      }
      drawBadge(s);
    }
    placeChip(s);
  }

  function drawBadge(s) {
    const e = Math.round(s.el);
    badge.style.display = e ? '' : 'none';
    if (!e) return;
    const t = badge.querySelector('text');
    t.textContent = heightText(e);
    const fs = parseFloat(getComputedStyle(t).fontSize) || 13;
    const w = t.getComputedTextLength() + 26 * K;
    const h = fs * 1.7;
    let bx = s.x + 24 * K;
    if (bx + w > C + OUTER + 30) bx = s.x - 24 * K - w;
    const by = s.y - 24 * K - h / 2;
    setAttrs(badge.querySelector('rect'), { x: bx, y: by, width: w, height: h, rx: h / 2 });
    const tx = bx + 10 * K;
    const ty = by + h / 2;
    const sg = e > 0 ? -1 : 1;
    badge.querySelector('path').setAttribute('d', `M${rd(tx - 4 * K)} ${rd(ty - sg * 3 * K)}L${rd(tx + 4 * K)} ${rd(ty - sg * 3 * K)}L${rd(tx)} ${rd(ty + sg * 4 * K)}Z`);
    setAttrs(t, { x: tx + 8 * K, y: ty });
  }

  function placeChip(s) {
    const bg = s.chip.querySelector('.chip-bg');
    const dot = s.chip.querySelector('.chip-dot');
    const nm = s.chip.querySelector('.chip-name');
    const mt = s.meta;
    const ns = parseFloat(getComputedStyle(nm).fontSize) || 16;
    const ms = parseFloat(getComputedStyle(mt).fontSize) || 12;
    let nw;
    let mw;
    try {
      nw = nm.getComputedTextLength();
      mw = mt.getComputedTextLength();
    } catch {
      return;
    }
    const padX = 11 * K;
    const padY = 9 * K;
    const dotW = 13 * K;
    const w = Math.max(nw + dotW, mw) + padX * 2;
    const b1 = padY + ns * 0.74;
    const b2 = b1 + ms * 1.6;
    const h = b2 + ms * 0.3 + padY * 0.9;
    // Below the puck, or above it when the puck is low on the radar; kept inside the box.
    const below = s.y < C + OUTER * 0.55;
    let x0 = clamp(s.x - w / 2, -6, 806 - w);
    let y0 = below ? s.y + 30 * K : s.y - 30 * K - h;
    x0 = rd(x0);
    y0 = rd(clamp(y0, -6, 806 - h));
    setAttrs(bg, { x: x0, y: y0, width: w, height: h, rx: 7 * K });
    setAttrs(dot, { cx: x0 + padX + 4 * K, cy: y0 + b1 - ns * 0.33, r: 3.6 * K });
    setAttrs(nm, { x: x0 + padX + dotW, y: y0 + b1 });
    setAttrs(mt, { x: x0 + padX, y: y0 + b2 });
  }

  function layout() {
    K = mq.matches ? 1.5 : 1;
    layoutRingLabels(svg, K);
    for (const s of Object.values(src)) draw(s);
  }

  // ---- state ---------------------------------------------------------------------------------

  const readPos = $('#sc-pos');
  const readSub = $('#sc-sub');
  const readWho = $('#sc-who');
  function readout() {
    const s = src[selected];
    readWho.textContent = s.id === 'music' ? 'Music' : 'Voice chat';
    readPos.textContent = `${directionName(s.az)} · ${formatDistance(s.dist)}`;
    const width = s.id === 'music' && s.width > 0.5 ? `${Math.round(s.width)}° wide` : 'point source';
    readSub.textContent = `${width} · height ${heightText(s.el)}`;
    for (const b of root.querySelectorAll('[data-preset]')) {
      b.setAttribute('aria-pressed', String(src.music.preset === b.dataset.preset));
    }
  }

  function push(s) {
    if (s.id === 'music') {
      const { az, el, dist, width, lowpass, highpass, bass, room, reverbDb } = s;
      audio.setMusic({ az, el, dist, width, lowpass, highpass, bass, room, reverbDb });
    } else {
      audio.setVoice({ az: s.az, el: s.el, dist: s.dist });
    }
  }

  function move(s, az, dist, { el } = {}) {
    s.az = wrap(az);
    s.dist = clamp(dist, 0.5, 20);
    if (el !== undefined) s.el = clamp(el, -40, 80);
    if (s.id === 'music') s.preset = onMark(s) ? s.base : null;
    draw(s);
    push(s);
    readout();
  }

  function select(id) {
    selected = id;
    for (const s of Object.values(src)) s.node.classList.toggle('is-selected', s.id === id);
    readout();
  }

  function applyPreset(name) {
    const p = PRESETS[name];
    Object.assign(src.music, p, { base: name, preset: name });
    select('music');
    draw(src.music);
    push(src.music);
    readout();
  }

  // ---- pointer -------------------------------------------------------------------------------

  let drag = null;
  const toSvg = (e) => {
    const p = svg.createSVGPoint();
    p.x = e.clientX;
    p.y = e.clientY;
    return p.matrixTransform(svg.getScreenCTM().inverse());
  };
  function pointAt(e) {
    const p = toSvg(e);
    let [az, dist] = polar(p.x, p.y);
    if (e.ctrlKey || e.metaKey) az = wrap(Math.round(az / SNAP) * SNAP);
    return [az, dist];
  }
  svg.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    const p = toSvg(e);
    const hit = Object.values(src).find((s) => Math.hypot(p.x - s.x, p.y - s.y) < 34 * K);
    // A touch on the empty stage scrolls the page; a click there places the selected source.
    if (!hit && e.pointerType === 'touch') return;
    const s = hit || src[selected];
    select(s.id);
    s.node.focus({ preventScroll: true });
    drag = { id: s.id, pointer: e.pointerId };
    svg.setPointerCapture(e.pointerId);
    svg.classList.add('is-dragging');
    move(s, ...pointAt(e));
    e.preventDefault();
  });
  svg.addEventListener('pointermove', (e) => {
    if (!drag || e.pointerId !== drag.pointer) return;
    move(src[drag.id], ...pointAt(e));
  });
  const end = (e) => {
    if (!drag || e.pointerId !== drag.pointer) return;
    drag = null;
    svg.classList.remove('is-dragging');
  };
  svg.addEventListener('pointerup', end);
  svg.addEventListener('pointercancel', end);
  svg.addEventListener('dblclick', () => {
    if (selected === 'music') applyPreset(src.music.base);
    else move(src.voice, VOICE_HOME.az, VOICE_HOME.dist, { el: 0 });
  });

  // ---- keyboard: the app's radar keys ----------------------------------------------------------

  for (const s of Object.values(src)) {
    s.node.addEventListener('focus', () => select(s.id));
    s.node.addEventListener('keydown', (e) => {
      const step = (e.shiftKey ? 0.01 : 0.04) * OUTER;
      let dx = 0;
      let dy = 0;
      switch (e.key) {
        case 'ArrowLeft': dx = -step; break;
        case 'ArrowRight': dx = step; break;
        case 'ArrowUp': dy = -step; break;
        case 'ArrowDown': dy = step; break;
        case 'PageUp': move(s, s.az, s.dist, { el: s.el + 5 }); e.preventDefault(); return;
        case 'PageDown': move(s, s.az, s.dist, { el: s.el - 5 }); e.preventDefault(); return;
        case 'Home':
          if (s.id === 'music') applyPreset(s.base);
          else move(s, VOICE_HOME.az, VOICE_HOME.dist, { el: 0 });
          e.preventDefault();
          return;
        default: return;
      }
      e.preventDefault();
      const [az, dist] = polar(s.x + dx, s.y + dy);
      move(s, az, dist);
    });
  }

  // ---- the desk ------------------------------------------------------------------------------

  const play = $('#sc-play');
  const status = $('#sc-status');
  const pills = { run: $('#sc-run'), duck: $('#sc-duck'), bypass: $('#sc-bypass-pill') };

  if (!supported) {
    play.disabled = true;
    $('#sc-unsupported').hidden = false;
  }

  async function setRunning(on) {
    if (on) {
      push(src.music);
      push(src.voice);
      await audio.start();
    } else {
      await audio.stop();
      audio.events.length = 0;
    }
    root.classList.toggle('is-running', on);
    play.setAttribute('aria-pressed', String(on));
    play.querySelector('.sc-play-label').textContent = on ? 'Stop the sound check' : 'Start the sound check';
    pills.run.textContent = on ? 'Running' : 'Stopped';
    pills.run.classList.toggle('on', on);
    if (on) tick();
  }
  play.addEventListener('click', () => setRunning(!audio.running));

  for (const b of root.querySelectorAll('[data-preset]')) {
    b.addEventListener('click', () => applyPreset(b.dataset.preset));
  }
  for (const input of root.querySelectorAll('[data-layer]')) {
    input.addEventListener('change', () => {
      audio.setLayer(input.dataset.layer, input.checked);
      root.classList.toggle(`no-${input.dataset.layer}`, !input.checked);
    });
  }
  $('#sc-footstep').addEventListener('change', (e) => audio.setFootstepSafe(e.target.checked));

  // Bypass: hold the button, or B, to hear the sound without the 3D placement (like the app).
  const bypassBtn = $('#sc-bypass');
  const setBypass = (on) => {
    audio.setBypass(on);
    bypassBtn.setAttribute('aria-pressed', String(on));
    pills.bypass.classList.toggle('on', on);
    root.classList.toggle('is-bypassed', on);
  };
  bypassBtn.addEventListener('pointerdown', (e) => {
    bypassBtn.setPointerCapture(e.pointerId);
    setBypass(true);
  });
  for (const ev of ['pointerup', 'pointercancel', 'blur']) bypassBtn.addEventListener(ev, () => setBypass(false));
  bypassBtn.addEventListener('keydown', (e) => {
    if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) {
      e.preventDefault();
      setBypass(true);
    }
  });
  bypassBtn.addEventListener('keyup', (e) => {
    if (e.key === ' ' || e.key === 'Enter') setBypass(false);
  });
  window.addEventListener('keydown', (e) => {
    if (e.key.toLowerCase() !== 'b' || e.repeat || e.ctrlKey || e.metaKey || e.altKey) return;
    if (!audio.running || /input|textarea|select/i.test(document.activeElement?.tagName || '')) return;
    if (!root.contains(document.activeElement) && !isOnScreen(root)) return;
    setBypass(true);
  });
  window.addEventListener('keyup', (e) => {
    if (e.key.toLowerCase() === 'b' && bypassBtn.getAttribute('aria-pressed') === 'true') setBypass(false);
  });

  // Stop when the tab is hidden; nobody wants a loop playing in a background tab.
  document.addEventListener('visibilitychange', () => {
    if (document.hidden && audio.running) setRunning(false);
  });

  // ---- live visuals while running --------------------------------------------------------------

  let wasDucking = false;
  function tick() {
    if (!audio.running) {
      for (const s of Object.values(src)) s.node.style.setProperty('--level', '0');
      svg.classList.remove('ducking', 'talking');
      pills.duck.classList.remove('on');
      return;
    }
    const lv = audio.levels();
    src.music.node.style.setProperty('--level', lv.music.toFixed(3));
    src.voice.node.style.setProperty('--level', lv.voice.toFixed(3));
    const ducking = (audio.ducking && audio.layers.game) || (audio.talking && audio.layers.voice);
    svg.classList.toggle('ducking', ducking && audio.layers.music);
    svg.classList.toggle('talking', audio.talking && audio.layers.voice);
    if (ducking !== wasDucking) {
      wasDucking = ducking;
      pills.duck.classList.toggle('on', ducking);
      pills.duck.textContent = !ducking ? 'Ducking' : audio.ducking ? 'Ducking · game is loud' : 'Ducking · voice talking';
    }
    const now = audio.now;
    while (audio.events.length && audio.events[0].t <= now) {
      const ev = audio.events.shift();
      if (audio.layers.game && now - ev.t < 0.25) spawnStep(ev);
    }
    requestAnimationFrame(tick);
  }

  let stepSide = 1;
  function spawnStep(ev) {
    // The game's own stereo position, shown inside the wedge; never moved by Sidestage.
    stepSide = -stepSide;
    const az = ev.pan * 72 + (ev.kind === 'step' ? stepSide * 2.5 : 0);
    const [x, y] = pt(az, radius(ev.kind === 'shot' ? 9 : 5.5));
    const g = document.createElementNS('http://www.w3.org/2000/svg', 'g');
    g.setAttribute('class', `step ${ev.kind}`);
    g.setAttribute('transform', `translate(${x} ${y})`);
    g.innerHTML = ev.kind === 'shot'
      ? `<circle r="16"${NSS}/><circle class="core" r="4"/>`
      : `<circle class="core" r="${rd(3.2 * K)}"/>`;
    stepsLayer.appendChild(g);
    setTimeout(() => g.remove(), 1400);
  }

  layout();
  select('music');
  document.fonts?.ready.then(layout);
  mq.addEventListener?.('change', layout);
  if (!anim) root.classList.add('still');
}

function onMark(s) {
  const p = PRESETS[s.base];
  return Math.abs(wrap(p.az - s.az)) < 0.5 && Math.abs(p.dist - s.dist) < 0.01 && Math.abs(p.el - s.el) < 0.5;
}

function heightText(deg) {
  const r = Math.round(deg);
  return r === 0 ? '0°' : `${r > 0 ? '+' : '−'}${Math.abs(r)}°`;
}

function isOnScreen(el) {
  const r = el.getBoundingClientRect();
  return r.bottom > 0 && r.top < window.innerHeight;
}

function radarSVG() {
  let s = '<svg class="radar sc-svg" viewBox="-10 -10 820 820" aria-labelledby="sc-t">' +
    '<title id="sc-t">Sound check radar: drag the music and the voice around your head</title>';
  s += `<circle class="floor" cx="${C}" cy="${C}" r="${OUTER + 8}"/>`;
  s += wedgeSVG(true);
  s += ringsSVG();
  s += ticksSVG(['GAME · IN FRONT', 'BEHIND YOU', 'LEFT', 'RIGHT']);
  s += ringLabelsSVG();
  s += '<g class="steps" aria-hidden="true"></g>';
  s += `<line class="link" id="sc-ln-voice"${NSS}/><line class="link" id="sc-ln-music"${NSS}/>`;
  s += `<path class="spread" id="sc-sp" d=""/><g class="lr" id="sc-lr"><circle class="lr-l" r="5.5"${NSS}/><circle class="lr-r" r="5.5"${NSS}/><text class="lr-lt">L</text><text class="lr-rt">R</text></g>`;
  s += headSVG();
  for (const [id, glyph, label] of [['voice', 'voice', 'Voice chat position'], ['music', 'note', 'Music position']]) {
    s += `<g class="puck sc-puck" id="sc-${id}" tabindex="0" role="slider" aria-label="${label}" aria-valuemin="-179" aria-valuemax="180">` +
      `<g class="p-scale"><circle class="p-glow" r="40"/><circle class="hit-area" r="34"/>` +
      `<g class="duck"><circle class="duck-ring" r="21"${NSS}/><circle class="duck-ring" r="21"${NSS}/><circle class="duck-ring" r="21"${NSS}/></g>` +
      `<circle class="talk-ring" r="21"${NSS}/><circle class="talk-ring" r="21"${NSS}/>` +
      `<g class="p-core"><circle class="sel-ring" r="24"${NSS}/><circle class="p-body" r="17"${NSS}/>${GLYPH[glyph]}</g></g></g>`;
  }
  s += `<g class="hbadge" id="sc-badge" style="display:none"><rect${NSS}/><path/><text></text></g>`;
  for (const [id, name] of [['voice', 'Voice chat'], ['music', 'Music']]) {
    s += `<g class="chip on" id="sc-ch-${id}" aria-hidden="true"><rect class="chip-bg"${NSS}/><circle class="chip-dot"/><text class="chip-name">${name}</text><text class="chip-meta"></text></g>`;
  }
  return `${s}</svg>`;
}
