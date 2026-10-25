// The Sidestage website: the radar hero, the sound check and the page's small behaviours. The
// radar follows the app's geometry (crates/sidestage/src/ui/radar.rs): log distance rings from
// 0.5 to 20 m, the GAME wedge of +-60 degrees in front, angle 0 straight ahead, +90 to the right.

import {DemoAudio, supported} from './audio.js?v=dev';

const doc = document, root = doc.documentElement;
const $ = (s, r = doc) => r.querySelector(s);
const $$ = (s, r = doc) => Array.from(r.querySelectorAll(s));
const RM = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
const NS = 'http://www.w3.org/2000/svg';

/* ---------------------------------------------------------------- radar geometry (radar.rs) */
const RINGS = [0.5, 1, 2, 5, 10, 20];
const LN40 = Math.log(20 / 0.5);
const OUTER = 250, INNER = OUTER * 0.16, HEAD = INNER * 0.62;   // viewBox units (-300..300)
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const c01 = v => clamp(v, 0, 1);
const eOut = (p, k) => 1 - Math.pow(1 - p, k || 3);
const rFor = d => INNER + Math.log(d / 0.5) / LN40 * (OUTER - INNER);          // unclamped: the field keeps going
const rClamp = d => clamp(rFor(clamp(d, 0.5, 20)), INNER, OUTER);
const dFor = r => 0.5 * Math.exp(c01((r - INNER) / (OUTER - INNER)) * LN40);
const dir = az => { const a = az * Math.PI / 180; return [Math.sin(a), -Math.cos(a)]; };  // 0 = up, +90 = right
const pos = (az, d) => { const u = dir(az), r = rClamp(d); return [u[0] * r, u[1] * r]; };
const wrapDeg = a => 180 - ((((180 - a) % 360) + 360) % 360);
const fmtD = d => d < 10 ? d.toFixed(1) + ' m' : Math.round(d) + ' m';
function dirName(az) {
  const a = wrapDeg(az), x = Math.abs(a), right = a > 0;
  if (x < 15) return 'In front';
  if (x < 67.5) return right ? 'Front right' : 'Front left';
  if (x < 112.5) return right ? 'Right' : 'Left';
  if (x < 165) return right ? 'Behind right' : 'Behind left';
  return 'Behind you';
}
function el(tag, attrs, parent) {
  const e = doc.createElementNS(NS, tag);
  if (attrs) for (const k in attrs) e.setAttribute(k, attrs[k]);
  if (parent) parent.appendChild(e);
  return e;
}
const stop = (g, off, v, a) => el('stop', {offset: off, style: `stop-color:var(${v});stop-opacity:${a}`}, g);
const KIND = {
  music: ['--accent-hi', '--accent', '--accent-deep'],
  voice: ['--c-voice-hi', '--c-voice', '--c-voice-deep'],
  pod: ['--c-pod-hi', '--c-pod', '--c-pod-deep'],
};

/* ---------------------------------------------------------------- builders */
function buildRadar(svg, p, opt) {
  svg.setAttribute('viewBox', '-300 -300 600 600');
  const R = {svg, p, k: 1, fs: 10.5, pucks: []};
  const defs = el('defs', null, svg);
  for (const k in KIND) {
    const [hi, base, deep] = KIND[k];
    const b = el('radialGradient', {id: `${p}body-${k}`, cx: '.4', cy: '.34', r: '.75', fx: '.34', fy: '.27'}, defs);
    stop(b, 0, hi, 1); stop(b, .52, base, 1); stop(b, 1, deep, 1);
    const g = el('radialGradient', {id: `${p}glow-${k}`}, defs);
    stop(g, 0, base, .62); stop(g, .3, base, .22); stop(g, 1, base, 0);
  }
  const halo = el('radialGradient', {id: `${p}halo`}, defs);
  stop(halo, 0, '--accent', 0); stop(halo, .72, '--accent', 0); stop(halo, .79, '--accent', .14); stop(halo, 1, '--accent', 0);
  const disc = el('radialGradient', {id: `${p}disc`}, defs);
  stop(disc, 0, '--bg', .92); stop(disc, .82, '--stage', .88); stop(disc, 1, '--stage', .55);

  el('circle', {r: OUTER * 1.32, fill: `url(#${p}halo)`}, svg);
  el('circle', {r: OUTER, fill: `url(#${p}disc)`}, svg);
  R.waves = el('g', {class: 'waves'}, svg);
  const ww = el('g', {class: 'wedge-wrap'}, svg);
  R.wedge = el('path', {class: 'wedge'}, ww);
  R.edges = [el('line', {class: 'wedge-edge'}, ww), el('line', {class: 'wedge-edge'}, ww)];
  R.rings = RINGS.map((d, i) => el('circle', {class: 'ring' + (i === RINGS.length - 1 ? ' ring-outer' : ''), r: rFor(d).toFixed(2), style: `--i:${i}`}, svg));
  R.game = el('text', {class: 'game-label', x: 0, 'text-anchor': 'middle', 'dominant-baseline': 'central'}, svg);
  R.game.textContent = 'GAME';
  R.labels = makeLabels(el('g', {class: 'rl'}, svg), RINGS.slice().reverse().map(d => ({r: rFor(d), text: (d < 1 ? d : d.toFixed(0)) + ' m'})));
  R.fx = el('g', {class: 'fx'}, svg);
  R.tethers = el('g', null, svg);
  R.arcs = el('g', null, svg);
  R.head = buildHead(el('g', {class: 'head'}, svg), opt.headphones);
  R.trails = el('g', {class: 'trails'}, svg);
  R.pl = el('g', {class: 'pucks'}, svg);
  setWedge(R, opt.wedge);
  return R;
}
function setWedge(R, h) {
  if (h < 0.2) { R.wedge.setAttribute('d', ''); R.edges.forEach(e => e.setAttribute('opacity', 0)); return; }
  const a = dir(-h), b = dir(h);
  R.wedge.setAttribute('d', `M0 0L${(a[0] * OUTER).toFixed(2)} ${(a[1] * OUTER).toFixed(2)}A${OUTER} ${OUTER} 0 0 1 ${(b[0] * OUTER).toFixed(2)} ${(b[1] * OUTER).toFixed(2)}Z`);
  [a, b].forEach((u, i) => {
    const e = R.edges[i];
    e.setAttribute('opacity', 1);
    e.setAttribute('x1', u[0] * INNER); e.setAttribute('y1', u[1] * INNER);
    e.setAttribute('x2', u[0] * OUTER); e.setAttribute('y2', u[1] * OUTER);
  });
}
function buildHead(g, headphones) {
  const hr = HEAD;
  if (!headphones) for (const s of [-1, 1]) {
    el('rect', {class: 'head-part', x: s * hr * 0.98 - hr * 0.16, y: hr * 0.05 - hr * 0.31, width: hr * 0.32, height: hr * 0.62, rx: hr * 0.16}, g);
  }
  el('polygon', {class: 'head-part', points: `${-hr * .26},${-hr * .9} 0,${-hr * 1.28} ${hr * .26},${-hr * .9}`}, g);
  el('circle', {class: 'head-part', r: hr}, g);
  if (headphones) {
    el('path', {class: 'hp-band', 'stroke-width': hr * 0.17, d: `M${-hr * 1.02} ${hr * 0.02}Q0 ${-hr * 0.2} ${hr * 1.02} ${hr * 0.02}`}, g);
    for (const s of [-1, 1]) el('rect', {class: 'hp-cup', x: s * hr * 1.08 - hr * 0.23, y: hr * 0.04 - hr * 0.42, width: hr * 0.46, height: hr * 0.84, rx: hr * 0.2}, g);
  }
  return g;
}
function makeLabels(g, list) {
  const u = dir(-45);
  return list.map(o => {
    const lg = el('g', null, g);
    if (o.op != null) lg.setAttribute('opacity', o.op.toFixed(3));
    const rect = el('rect', {rx: 3}, lg);
    const x = u[0] * o.r, y = u[1] * o.r;
    const t = el('text', {x: x.toFixed(2), y: y.toFixed(2), 'text-anchor': 'middle', 'dominant-baseline': 'central'}, lg);
    t.textContent = o.text;
    return {lg, rect, t, x, y, text: o.text};
  });
}
function layoutLabels(items, fs, avoidHead) {
  const placed = [], hb = HEAD * 2.7 / 2, pad = fs * 0.34;
  for (const it of items) {
    it.lg.style.display = '';
    it.t.setAttribute('font-size', fs.toFixed(2));
    let w = 0;
    try { w = it.t.getBBox().width; } catch (e) { w = 0; }
    if (!w) w = it.text.length * fs * 0.66;
    const bw = w + pad * 2, bh = fs * 1.3;
    const box = [it.x - bw / 2, it.y - bh / 2, it.x + bw / 2, it.y + bh / 2];
    const hit = placed.some(q => box[2] + 1 > q[0] && box[0] - 1 < q[2] && box[3] + 1 > q[1] && box[1] - 1 < q[3]) ||
      (avoidHead && box[2] > -hb && box[0] < hb && box[3] > -hb && box[1] < hb);
    if (hit) { it.lg.style.display = 'none'; continue; }
    placed.push(box);
    it.rect.setAttribute('x', box[0].toFixed(2)); it.rect.setAttribute('y', box[1].toFixed(2));
    it.rect.setAttribute('width', bw.toFixed(2)); it.rect.setAttribute('height', bh.toFixed(2));
    it.rect.setAttribute('rx', (fs * 0.28).toFixed(2));
  }
}
const FIELD_D = [50, 100, 200, 500, 1e3, 2e3, 5e3, 1e4, 2e4, 5e4, 1e5, 2e5, 5e5, 1e6];
const fmtFar = d => d >= 1000 ? (d / 1000).toLocaleString('en') + ' km' : d + ' m';
function buildField(svg, withLabels) {
  if (!svg) return null;
  svg.setAttribute('viewBox', '-900 -900 1800 1800');
  for (let a = 0; a < 360; a += 30) {
    const u = dir(a);
    el('line', {class: 'spoke', x1: u[0] * (OUTER + 16), y1: u[1] * (OUTER + 16), x2: u[0] * 880, y2: u[1] * 880}, svg);
  }
  const items = [];
  FIELD_D.forEach((d, i) => {
    const r = rFor(d);
    if (r > 880) return;
    const f = 1 - (r - OUTER) / (880 - OUTER);
    el('circle', {class: 'fring', r: r.toFixed(1), 'stroke-opacity': (0.025 + 0.1 * f * f).toFixed(3), style: `--i:${i + 6}`}, svg);
    if (withLabels && i < 6) items.push({r, text: fmtFar(d), op: 0.2 + 0.6 * f * f});
  });
  const labels = items.length ? makeLabels(el('g', {class: 'rl'}, svg), items) : [];
  return {svg, labels};
}
function glyph(g, kind, s) {
  if (kind === 'music') {
    const hx = -s * .22, hy = s * .32, rx = s * .3, ry = s * .22, sx = hx + rx * .92, top = -s * .55;
    el('ellipse', {class: 'gl', cx: hx, cy: hy, rx, ry, transform: `rotate(-20 ${hx} ${hy})`}, g);
    el('path', {class: 'gl-s', 'stroke-width': s * .12, d: `M${sx} ${hy - ry * .3}V${top}L${sx + s * .3} ${top + s * .25}L${sx + s * .28} ${top + s * .52}`}, g);
  } else if (kind === 'pod') {
    el('rect', {class: 'gl', x: -s * .19, y: -s * .62, width: s * .38, height: s * .72, rx: s * .19}, g);
    el('path', {class: 'gl-s', 'stroke-width': s * .11, d: `M${-s * .38} ${-s * .14}A${s * .38} ${s * .38} 0 0 0 ${s * .38} ${-s * .14}M0 ${s * .24}V${s * .5}M${-s * .2} ${s * .52}H${s * .2}`}, g);
  } else {
    const u = s;
    el('path', {class: 'gl', d: `M${-.33 * u} ${-.45 * u}H${.33 * u}Q${.55 * u} ${-.45 * u} ${.55 * u} ${-.23 * u}V${.08 * u}Q${.55 * u} ${.3 * u} ${.33 * u} ${.3 * u}H${-.02 * u}L${-.34 * u} ${.56 * u}L${-.24 * u} ${.3 * u}H${-.33 * u}Q${-.55 * u} ${.3 * u} ${-.55 * u} ${.08 * u}V${-.23 * u}Q${-.55 * u} ${-.45 * u} ${-.33 * u} ${-.45 * u}Z`}, g);
    for (const x of [-.24, 0, .24]) el('circle', {class: 'gl-cut', cx: x * u, cy: -.075 * u, r: .075 * u}, g);
  }
}
function makePuck(R, kind) {
  const g = el('g', {class: `puck puck-${kind}`}, R.pl);
  const pop = el('g', {class: 'pop'}, g);
  const k = el('g', null, pop);
  const P = {kind, g, pop, k};
  P.glow = el('circle', {class: 'p-glow', r: 52, fill: `url(#${R.p}glow-${kind})`}, k);
  P.duck = el('circle', {class: 'p-duck', r: 19}, k);
  P.speak = el('circle', {class: 'p-speak', r: 19.5}, k);
  P.focus = el('circle', {class: 'p-focus', r: 24}, k);
  P.core = el('g', {class: 'p-core'}, k);
  el('circle', {class: 'p-halo', r: 20.5}, P.core);
  el('circle', {class: 'p-body', r: 15, fill: `url(#${R.p}body-${kind})`}, P.core);
  glyph(el('g', null, P.core), kind, 13.5);
  R.pucks.push(P);
  return P;
}
function place(P, x, y) {
  P.x = x; P.y = y;
  P.g.setAttribute('transform', `translate(${x.toFixed(2)} ${y.toFixed(2)})`);
}
function tether(line, x, y) {
  const len = Math.hypot(x, y) || 1, ux = x / len, uy = y / len;
  line.setAttribute('x1', (ux * HEAD).toFixed(2)); line.setAttribute('y1', (uy * HEAD).toFixed(2));
  line.setAttribute('x2', x.toFixed(2)); line.setAttribute('y2', y.toFixed(2));
}
/* Scale-dependent bits: keep text ~10 px and pucks >= ~12 px whatever the radar's size. */
function layoutRadar(R, wrapEl, fields) {
  const w = wrapEl.getBoundingClientRect().width || 600, s = w / 600;
  R.fs = Math.max(10.5, 10 / s);
  R.k = Math.max(15, 12.5 / s) / 15;
  layoutLabels(R.labels, R.fs, true);
  R.game.setAttribute('font-size', R.fs.toFixed(2));
  R.game.setAttribute('y', (-(OUTER - 16 * R.fs / 10.5)).toFixed(2));
  for (const P of R.pucks) P.k.setAttribute('transform', `scale(${R.k.toFixed(3)})`);
  for (const F of fields || []) if (F && F.labels.length) layoutLabels(F.labels, R.fs, false);
  return w;
}
/* An expanding wavefront: the sound leaving its source and crossing the page. */
function emitWave(layer, x, y, cvar, o) {
  if (RM || !layer || !layer.animate) return;
  const c = el('circle', {class: 'wave', r: 1}, layer);
  c.style.stroke = `var(${cvar})`;
  const a = c.animate([
    {transform: `translate(${x}px, ${y}px) scale(${o.r0})`, opacity: o.op},
    {transform: `translate(${x}px, ${y}px) scale(${o.r1})`, opacity: 0},
  ], {duration: o.dur, easing: o.ease || 'cubic-bezier(.2,.6,.35,1)', fill: 'forwards'});
  a.onfinish = () => c.remove();
}
function duckRing(P) {
  if (RM || !P.duck.animate) return;
  P.duck.animate([{transform: 'scale(1)', opacity: .95}, {transform: `scale(${35 / 19})`, opacity: 0}], {duration: 455, easing: 'linear'});
}

/* =================================================================== HERO */
const heroSec = $('#top'), heroWrap = $('#hero-wrap');
const H = buildRadar($('#hero-radar'), 'h', {headphones: false, wedge: RM ? 60 : 0});
const HF = buildField($('#hero-field'), true);
const SRC = [
  {kind: 'music', az: 180, d: 1.5, width: 60, start: 760, dur: 1700, from: -90},
  {kind: 'pod', az: -90, d: 1.2, start: 1000, dur: 1600, from: 160},
  {kind: 'voice', az: 90, d: 1.2, start: 1240, dur: 1550, from: -150},
];
for (const s of SRC) {
  s.line = el('line', {class: `tether t-${s.kind}`}, H.tethers);
  s.dots = Array.from({length: 13}, () => el('circle', {class: `tr-${s.kind}`, r: 0}, H.trails));
  s.P = makePuck(H, s.kind);
  s.chip = $(`.chip[data-src="${s.kind}"]`);
  s.hist = [];
}
const music = SRC[0], pod = SRC[1], voice = SRC[2];
const arc = {
  g: el('g', {style: 'display:none'}, H.arcs),
};
arc.path = el('path', {class: 'lr-arc'}, arc.g);
arc.dots = [el('circle', {class: 'lr-dot'}, arc.g), el('circle', {class: 'lr-dot'}, arc.g)];
arc.txt = ['L', 'R'].map(t => { const e = el('text', {class: 'lr-t', 'text-anchor': 'middle', 'dominant-baseline': 'central'}, arc.g); e.textContent = t; return e; });
arc.half = 0;
function drawArc() {
  const s = music, half = arc.half;
  if (half < 0.5) { arc.g.style.display = 'none'; return; }
  arc.g.style.display = '';
  const r = rClamp(s.d), a0 = s.az - half, a1 = s.az + half;
  const p0 = dir(a0).map(v => v * r), p1 = dir(a1).map(v => v * r);
  arc.path.setAttribute('d', `M${p0[0].toFixed(2)} ${p0[1].toFixed(2)}A${r.toFixed(2)} ${r.toFixed(2)} 0 0 1 ${p1[0].toFixed(2)} ${p1[1].toFixed(2)}`);
  // The music's left channel stays on the listener's left.
  const ends = p0[0] <= p1[0] ? [[a0, p0], [a1, p1]] : [[a1, p1], [a0, p0]];
  ends.forEach(([a, p], i) => {
    arc.dots[i].setAttribute('cx', p[0].toFixed(2)); arc.dots[i].setAttribute('cy', p[1].toFixed(2));
    arc.dots[i].setAttribute('r', (4.5 * H.k).toFixed(2));
    const u = dir(a), o = r + 14 * H.k;
    arc.txt[i].setAttribute('x', (u[0] * o).toFixed(2)); arc.txt[i].setAttribute('y', (u[1] * o).toFixed(2));
    arc.txt[i].setAttribute('font-size', H.fs.toFixed(2));
  });
}
function placeChips(w) {
  const compact = w < 460, k = H.k;
  for (const s of SRC) {
    const p = pos(s.az, s.d), R = 15 * k + 9;
    let ax = p[0], ay = p[1], tx = '-50%', ty = '0%';
    if (s.kind === 'music') ay = p[1] + R + 4 * k;
    else if (compact) ay = p[1] + R - 2;
    else if (s.az < 0) { ax = p[0] - R; tx = '-100%'; ty = '-50%'; }
    else { ax = p[0] + R; tx = '0%'; ty = '-50%'; }
    s.chip.style.left = ((ax + 300) / 6).toFixed(3) + '%';
    s.chip.style.top = ((ay + 300) / 6).toFixed(3) + '%';
    s.chip.style.setProperty('--tx', tx);
    s.chip.style.setProperty('--ty', ty);
    s.chip.classList.toggle('compact', compact);
  }
}
function layoutHero() {
  const w = layoutRadar(H, heroWrap, [HF]);
  // Far-field scale labels that would be cut by the screen edge are left out.
  const fr = HF.svg.getBoundingClientRect(), vw = doc.documentElement.clientWidth;
  for (const it of HF.labels) {
    const sx = fr.left + (it.x + 900) / 1800 * fr.width, half = it.text.length * 4 + 10;
    if (sx - half < 6 || sx + half > vw - 6) it.lg.style.display = 'none';
  }
  drawArc();
  placeChips(w);
}
function drawTrail(s, fade) {
  const h = s.hist, n = s.dots.length;
  for (let i = 0; i < n; i++) {
    const c = s.dots[i], idx = h.length - 2 - i * 2;
    if (idx < 0 || fade <= 0) { c.setAttribute('r', 0); continue; }
    const f = 1 - i / n;
    c.setAttribute('cx', h[idx][0].toFixed(1)); c.setAttribute('cy', h[idx][1].toFixed(1));
    c.setAttribute('r', (15 * H.k * 0.48 * f).toFixed(2));
    c.setAttribute('opacity', (0.5 * f * fade).toFixed(3));
  }
}
function land(s) {
  s.landed = true;
  s.P.pop.style.transform = '';
  s.P.pop.classList.add('landed');
  s.line.classList.add('on');
  s.chip.classList.add('on');
  emitWave(H.waves, s.P.x, s.P.y, KIND[s.kind][1], {r0: 16, r1: 150, dur: 1400, op: .7});
}
function flight(s, t) {
  const p = c01((t - s.start) / s.dur);
  if (p <= 0) return true;
  const rf = rClamp(s.d), r0 = OUTER * 2.2;
  const r = r0 + (rf - r0) * eOut(p, 3.4);           // falls inward quickly...
  const az = s.from + (s.az - s.from) * eOut(p, 2.3); // ...then glides along its orbit into place
  const u = dir(az);
  place(s.P, u[0] * r, u[1] * r);
  tether(s.line, s.P.x, s.P.y);
  s.P.g.style.opacity = c01(p / 0.16).toFixed(3);
  if (!s.landed) s.P.pop.style.transform = `scale(${(0.5 + 0.5 * eOut(c01(p / 0.55), 2)).toFixed(3)})`;
  s.hist.push([s.P.x, s.P.y]);
  if (s.hist.length > 40) s.hist.shift();
  const after = t - s.start - s.dur;
  drawTrail(s, p < 1 ? 1 : c01(1 - after / 380));
  if (p >= 1 && !s.landed) land(s);
  let busy = p < 1 || after < 420;
  if (s === music && s.landed) {
    const q = c01(after / 700);
    arc.half = (s.width / 2) * eOut(q, 3);
    drawArc();
    if (q < 1) busy = true;
  }
  return busy;
}
function finalHero() {
  setWedge(H, 60);
  H.svg.classList.add('in'); HF.svg.classList.add('in');
  for (const s of SRC) {
    const p = pos(s.az, s.d);
    place(s.P, p[0], p[1]);
    tether(s.line, p[0], p[1]);
    s.line.classList.add('on');
    s.chip.classList.add('on');
    s.landed = true;
  }
  arc.half = music.width / 2;
  drawArc();
}
let heroOn = true;
const live = () => heroOn && !doc.hidden;
function loop(fn, every, jitter) {
  const tick = () => { if (live()) fn(); setTimeout(tick, every + Math.random() * (jitter || 0)); };
  tick();
}
function heroDuck() {
  // The game gets loud (a burst in the GAME wedge), so the music ducks: amber rings pulse out of it.
  H.svg.classList.add('game-loud');
  [[-14, 7], [21, 11], [3, 4.5]].forEach(([az, d], i) => setTimeout(() => {
    const p = pos(az, d);
    emitWave(H.fx, p[0], p[1], '--info', {r0: 2, r1: 44, dur: 900, op: .85, ease: 'cubic-bezier(.1,.7,.3,1)'});
  }, i * 140));
  const sb = $('#sb-duck');
  setTimeout(() => {
    music.P.g.classList.add('ducked'); sb.classList.add('on');
    for (let i = 0; i < 3; i++) setTimeout(() => duckRing(music.P), i * 455);
  }, 150);
  setTimeout(() => H.svg.classList.remove('game-loud'), 650);
  setTimeout(() => { music.P.g.classList.remove('ducked'); sb.classList.remove('on'); }, 1650);
}
function voiceLoop() {
  if (live()) {
    const n = 2 + Math.floor(Math.random() * 2);
    voice.P.g.classList.add('speaking');
    for (let i = 0; i < n; i++) setTimeout(() => emitWave(H.waves, voice.P.x, voice.P.y, '--c-voice', {r0: 19, r1: 640, dur: 6400, op: .38}), i * 430);
    setTimeout(() => voice.P.g.classList.remove('speaking'), n * 430 + 420);
  }
  setTimeout(voiceLoop, 3800 + Math.random() * 2600);
}
function startIdle() {
  if (RM) return;
  H.svg.classList.add('idle');
  loop(() => emitWave(H.waves, music.P.x, music.P.y, '--accent', {r0: 18, r1: 780, dur: 7200, op: .42}), 2857);
  setTimeout(() => loop(() => emitWave(H.waves, pod.P.x, pod.P.y, '--c-pod', {r0: 18, r1: 720, dur: 7800, op: .34}), 4400), 1400);
  setTimeout(voiceLoop, 2300);
  setTimeout(() => loop(heroDuck, 9600), 1500);
}
function heroIntro() {
  layoutHero();
  if (RM) { finalHero(); return; }
  for (const s of SRC) s.P.g.style.opacity = 0;
  H.svg.classList.add('in'); HF.svg.classList.add('in');
  const t0 = performance.now();
  const frame = now => {
    const t = now - t0;
    setWedge(H, 60 * eOut(c01((t - 360) / 950), 3));
    let busy = t < 1400;
    for (const s of SRC) busy = flight(s, t) || busy;
    if (busy) requestAnimationFrame(frame);
    else startIdle();
  };
  requestAnimationFrame(frame);
}
heroIntro();
if (doc.fonts && doc.fonts.ready) doc.fonts.ready.then(layoutHero);

/* =================================================================== SOUND CHECK (demo) */
// Drag the dMusic and a voice around a headphone listener while the game's footsteps cross the
// GAME wedge without going through the 3D stage. The sound itself comes from audio.js.

// Music presets from crates/sidestage-dsp/src/presets.rs (the page has a small room and a hall).
const PRESETS = {
  behind: {az: 180, el: 0, d: 1.5, width: 60, lowpass: 18000, highpass: 30, bass: 0, room: 'small', reverbDb: -12},
  next: {az: 150, el: 0, d: 5, width: 0, lowpass: 1400, highpass: 30, bass: 3, room: 'small', reverbDb: -6},
  far: {az: 180, el: 5, d: 15, width: 10, lowpass: 6000, highpass: 30, bass: 0, room: 'hall', reverbDb: -10},
  above: {az: 180, el: 60, d: 2, width: 30, lowpass: 18000, highpass: 30, bass: 0, room: 'small', reverbDb: -12},
  desk: {az: 35, el: -20, d: 0.7, width: 0, lowpass: 8000, highpass: 180, bass: 0, room: 'small', reverbDb: -16},
  lounge: {az: 180, el: 10, d: 2.5, width: 90, lowpass: 16000, highpass: 30, bass: 0, room: 'small', reverbDb: -10},
};
const VOICE_HOME = {az: 90, el: 0, d: 1};
const heightText = deg => { const r = Math.round(deg); return r === 0 ? '0°' : `${r > 0 ? '+' : '−'}${Math.abs(r)}°`; };

const audio = new DemoAudio();
const demoWrap = $('#demo-wrap');
const D = buildRadar($('#demo-radar'), 'd', {headphones: true, wedge: 60});
const DF = buildField($('#demo-field'), false);
const dsvg = D.svg;
let demoVisible = false;

// `base` is the preset whose sound (room, muffle) is in use; `preset` is set while the music also
// sits exactly on that preset's mark.
const dMusic = {kind: 'music', name: 'Music', ...PRESETS.behind, base: 'behind', preset: 'behind'};
const dVoice = {kind: 'voice', name: 'Voice chat', ...VOICE_HOME, width: 0};
const DSRC = [dVoice, dMusic];
let selected = dMusic;

const spread = {g: el('g', {style: 'display:none'}, D.arcs)};
spread.path = el('path', {class: 'lr-arc'}, spread.g);
spread.dots = [el('circle', {class: 'lr-dot'}, spread.g), el('circle', {class: 'lr-dot'}, spread.g)];
spread.txt = ['L', 'R'].map(t => { const e = el('text', {class: 'lr-t', 'text-anchor': 'middle', 'dominant-baseline': 'central'}, spread.g); e.textContent = t; return e; });
for (const s of DSRC) {
  s.line = el('line', {class: `tether t-${s.kind} on`}, D.tethers);
  s.P = makePuck(D, s.kind);
  el('circle', {class: 'p-hit', r: 30}, s.P.k);
  s.chip = $(`.chip[data-demo="${s.kind}"]`);
  s.chipText = $('em', s.chip);
  const g = s.P.g;
  g.setAttribute('tabindex', '0');
  g.setAttribute('role', 'slider');
  g.setAttribute('aria-label', `${s.name} position`);
  g.setAttribute('aria-valuemin', '-179');
  g.setAttribute('aria-valuemax', '180');
}
const badge = el('g', {class: 'hbadge', style: 'display:none'}, D.pl);
badge.rect = el('rect', null, badge);
badge.tri = el('path', null, badge);
badge.text = el('text', {'dominant-baseline': 'central'}, badge);

// The music's two virtual speakers, as in Sidestage's renderer: they narrow to a point at the
// sides, so the music's left channel always stays on the listener's left.
function drawSpread(s) {
  const half = (s.width / 2) * clamp(2 * Math.cos(s.az * Math.PI / 180), -1, 1);
  if (Math.abs(half) < 0.5) { spread.g.style.display = 'none'; return; }
  spread.g.style.display = '';
  const r = rClamp(s.d), aL = s.az - half, aR = s.az + half;
  const [lo, hi] = aL < aR ? [aL, aR] : [aR, aL];
  const p0 = dir(lo).map(v => v * r), p1 = dir(hi).map(v => v * r);
  spread.path.setAttribute('d', `M${p0[0].toFixed(2)} ${p0[1].toFixed(2)}A${r.toFixed(2)} ${r.toFixed(2)} 0 0 1 ${p1[0].toFixed(2)} ${p1[1].toFixed(2)}`);
  [aL, aR].forEach((a, i) => {
    const u = dir(a), o = r + 14 * D.k;
    spread.dots[i].setAttribute('cx', (u[0] * r).toFixed(2)); spread.dots[i].setAttribute('cy', (u[1] * r).toFixed(2));
    spread.dots[i].setAttribute('r', (4.5 * D.k).toFixed(2));
    spread.txt[i].setAttribute('x', (u[0] * o).toFixed(2)); spread.txt[i].setAttribute('y', (u[1] * o).toFixed(2));
    spread.txt[i].setAttribute('font-size', D.fs.toFixed(2));
  });
}
// Height badge beside the music puck, like the app's.
function drawBadge(s) {
  const e = Math.round(s.el);
  badge.style.display = e ? '' : 'none';
  if (!e) return;
  const fs = D.fs, h = fs * 1.8, w = fs * (heightText(e).length * 0.66 + 2.4);
  let x = s.P.x + 22 * D.k;
  if (x + w > OUTER + 40) x = s.P.x - 22 * D.k - w;
  // Up-right of the puck, like the app; level with it when the name chip is above the puck.
  const y = chipBelow(s) ? s.P.y - 22 * D.k - h / 2 : s.P.y - h / 2, cx = x + fs * 0.85, cy = y + h / 2, sg = e > 0 ? -1 : 1;
  for (const [k, v] of [['x', x], ['y', y], ['width', w], ['height', h], ['rx', h / 2]]) badge.rect.setAttribute(k, v.toFixed(2));
  badge.tri.setAttribute('d', `M${(cx - fs * .32).toFixed(2)} ${(cy - sg * fs * .24).toFixed(2)}L${(cx + fs * .32).toFixed(2)} ${(cy - sg * fs * .24).toFixed(2)}L${cx.toFixed(2)} ${(cy + sg * fs * .32).toFixed(2)}Z`);
  badge.text.setAttribute('x', (cx + fs * .62).toFixed(2));
  badge.text.setAttribute('y', cy.toFixed(2));
  badge.text.setAttribute('font-size', fs.toFixed(2));
  badge.text.textContent = heightText(e);
}
// Name chips sit below their puck, or above it near the bottom edge of the radar.
const chipBelow = s => s.P.y < OUTER * 0.72;
function placeDemoChip(s) {
  const R = 15 * D.k + 9, below = chipBelow(s);
  s.chip.style.left = ((s.P.x + 300) / 6).toFixed(3) + '%';
  s.chip.style.top = ((s.P.y + (below ? R : -R) + 300) / 6).toFixed(3) + '%';
  s.chip.style.setProperty('--tx', '-50%');
  s.chip.style.setProperty('--ty', below ? '0%' : '-100%');
}
const onMark = s => { const p = PRESETS[s.base]; return Math.abs(wrapDeg(p.az - s.az)) < 0.5 && Math.abs(p.d - s.d) < 0.01 && Math.abs(p.el - s.el) < 0.5; };

const roWho = $('#ro-who'), roPos = $('#ro-pos'), roSub = $('#ro-sub'), pchips = $$('.pchip');
function readout() {
  const s = selected;
  roWho.textContent = s === dMusic ? 'Music position' : 'Voice chat position';
  roPos.textContent = `${dirName(s.az)} · ${fmtD(s.d)}`;
  const width = s === dMusic && s.width > 0.5 ? `${Math.round(s.width)}° wide` : 'point source';
  roSub.textContent = `${width} · height ${heightText(s.el)}`;
  for (const b of pchips) b.setAttribute('aria-pressed', String(dMusic.preset === b.dataset.preset));
}
function pushAudio(s) {
  if (s === dMusic) {
    const {az, el: elev, d, width, lowpass, highpass, bass, room, reverbDb} = s;
    audio.setMusic({az, el: elev, dist: d, width, lowpass, highpass, bass, room, reverbDb});
  } else {
    audio.setVoice({az: s.az, el: s.el, dist: s.d});
  }
}
function setSource(s, az, d, elev = s.el) {
  s.az = wrapDeg(az); s.d = clamp(d, 0.5, 20); s.el = clamp(elev, -40, 80);
  const p = pos(s.az, s.d);
  place(s.P, p[0], p[1]);
  tether(s.line, p[0], p[1]);
  if (s === dMusic) {
    s.preset = onMark(s) ? s.base : null;
    drawSpread(s);
    drawBadge(s);
  }
  const text = `${dirName(s.az)} · ${fmtD(s.d)}`;
  s.chipText.textContent = text;
  placeDemoChip(s);
  s.P.g.setAttribute('aria-valuenow', String(Math.round(s.az)));
  s.P.g.setAttribute('aria-valuetext', `${text}, height ${heightText(s.el)}`);
  pushAudio(s);
  if (s === selected) readout();
}
function select(s) {
  selected = s;
  for (const o of DSRC) o.P.g.classList.toggle('selected', o === s);
  readout();
}
let glideRAF = 0;
function glideTo(s, az, d, elev = s.el) {
  cancelAnimationFrame(glideRAF);
  if (RM) { setSource(s, az, d, elev); return; }
  const a0 = s.az, da = wrapDeg(az - a0), l0 = Math.log(s.d), l1 = Math.log(clamp(d, 0.5, 20)), e0 = s.el, t0 = performance.now();
  const stepF = now => {
    const e = eOut(c01((now - t0) / 650), 3);
    setSource(s, a0 + da * e, Math.exp(l0 + (l1 - l0) * e), e0 + (elev - e0) * e);
    if (e < 1) glideRAF = requestAnimationFrame(stepF);
  };
  glideRAF = requestAnimationFrame(stepF);
}
function applyPreset(name) {
  const p = PRESETS[name];
  Object.assign(dMusic, {width: p.width, lowpass: p.lowpass, highpass: p.highpass, bass: p.bass, room: p.room, reverbDb: p.reverbDb, base: name});
  select(dMusic);
  glideTo(dMusic, p.az, p.d, p.el);
}
const goHome = s => (s === dMusic ? applyPreset(dMusic.base) : glideTo(dVoice, VOICE_HOME.az, VOICE_HOME.d, 0));
function layoutDemo() {
  layoutRadar(D, demoWrap, []);
  const compact = (demoWrap.getBoundingClientRect().width || 600) < 420;
  for (const s of DSRC) { s.chip.classList.toggle('compact', compact); setSource(s, s.az, s.d, s.el); }
}
layoutDemo();
select(dMusic);
if (doc.fonts && doc.fonts.ready) doc.fonts.ready.then(layoutDemo);

// Pointer: drag a puck (mouse, pen, touch); a mouse can also grab anywhere to move the selected
// source, and a tap glides it there. Ctrl (Cmd on a Mac) snaps to 15 degrees, like the app.
let dragId = null, dragSrc = null, tap = null;
const local = e => { const b = dsvg.getBoundingClientRect(); return [(e.clientX - b.left) / b.width * 600 - 300, (e.clientY - b.top) / b.height * 600 - 300]; };
function fromPoint(s, x, y, e) {
  const len = Math.hypot(x, y);
  let az = len < 1e-3 ? 180 : Math.atan2(x, -y) * 180 / Math.PI;
  if (e && (e.ctrlKey || e.metaKey)) az = Math.round(az / 15) * 15;
  setSource(s, az, dFor(len));
}
function startDrag(e, s) {
  cancelAnimationFrame(glideRAF);
  dragId = e.pointerId; dragSrc = s;
  try { dsvg.setPointerCapture(e.pointerId); } catch (err) { /* ignore */ }
  dsvg.classList.add('dragging');
  select(s);
}
for (const s of DSRC) {
  s.P.g.addEventListener('pointerdown', e => {
    if (e.button > 0) return;
    e.preventDefault(); e.stopPropagation();
    startDrag(e, s);
    s.P.g.focus({preventScroll: true});
  });
  s.P.g.addEventListener('focus', () => select(s));
  // Keyboard, like the app: arrows move the puck on screen (Shift = fine), Page Up/Down = height,
  // Home = back to the preset.
  s.P.g.addEventListener('keydown', e => {
    const st = (e.shiftKey ? 0.01 : 0.04) * OUTER;
    let dx = 0, dy = 0;
    switch (e.key) {
      case 'ArrowLeft': dx = -st; break;
      case 'ArrowRight': dx = st; break;
      case 'ArrowUp': dy = -st; break;
      case 'ArrowDown': dy = st; break;
      case 'PageUp': e.preventDefault(); setSource(s, s.az, s.d, s.el + 5); return;
      case 'PageDown': e.preventDefault(); setSource(s, s.az, s.d, s.el - 5); return;
      case 'Home': e.preventDefault(); goHome(s); return;
      default: return;
    }
    e.preventDefault();
    cancelAnimationFrame(glideRAF);
    fromPoint(s, s.P.x + dx, s.P.y + dy);
  });
}
dsvg.addEventListener('pointerdown', e => {
  if (dragId !== null || e.button > 0) return;
  if (e.pointerType === 'mouse') { e.preventDefault(); startDrag(e, selected); fromPoint(selected, ...local(e), e); }
  else tap = {id: e.pointerId, x: e.clientX, y: e.clientY, t: performance.now()};
});
dsvg.addEventListener('pointermove', e => { if (e.pointerId === dragId) fromPoint(dragSrc, ...local(e), e); });
function endPointer(e) {
  if (e.pointerId === dragId) { dragId = null; dsvg.classList.remove('dragging'); return; }
  if (tap && e.pointerId === tap.id) {
    if (e.type === 'pointerup' && Math.hypot(e.clientX - tap.x, e.clientY - tap.y) < 10 && performance.now() - tap.t < 600) {
      const [x, y] = local(e), len = Math.hypot(x, y);
      if (len < OUTER + 30) glideTo(selected, len < 1e-3 ? 180 : Math.atan2(x, -y) * 180 / Math.PI, dFor(len));
    }
    tap = null;
  }
}
dsvg.addEventListener('pointerup', endPointer);
dsvg.addEventListener('pointercancel', endPointer);
dsvg.addEventListener('dblclick', () => goHome(selected));
pchips.forEach(b => b.addEventListener('click', () => applyPreset(b.dataset.preset)));

// The desk.
const playBtn = $('#play'), playLabel = $('#play-label'), playSub = $('#play-sub');
const dRun = $('#d-run'), dDuck = $('#d-duck'), dBypass = $('#d-bypass');
let demoRAF = 0;
function syncUI() {
  const on = audio.running;
  playBtn.setAttribute('aria-pressed', String(on));
  playLabel.textContent = on ? 'Pause' : 'Play the sound check';
  playSub.textContent = on ? 'Now playing · drag the pucks' : 'Synthesized live in this page';
  dRun.classList.toggle('on', on);
  dRun.lastChild.textContent = on ? 'Running' : 'Stopped';
  if (on && !demoRAF) demoRAF = requestAnimationFrame(demoFrame);
}
if (!supported) {
  playBtn.disabled = true;
  $('#unsupported').hidden = false;
}
playBtn.addEventListener('click', async () => {
  if (audio.running) { await audio.stop(); audio.events.length = 0; }
  else { for (const s of DSRC) pushAudio(s); await audio.start(); }
  syncUI();
});
for (const btn of $$('.tg[data-layer]')) {
  btn.addEventListener('click', () => {
    const v = btn.getAttribute('aria-checked') !== 'true', layer = btn.dataset.layer;
    btn.setAttribute('aria-checked', String(v));
    audio.setLayer(layer, v);
    if (layer === 'game') dsvg.classList.toggle('game-off', !v);
    else {
      const s = layer === 'music' ? dMusic : dVoice;
      for (const n of [s.P.g, s.line, s.chip].concat(s === dMusic ? [spread.g, badge] : [])) n.classList.toggle('layer-off', !v);
    }
  });
}
const tgSafe = $('#tg-safe');
tgSafe.addEventListener('click', () => {
  const v = tgSafe.getAttribute('aria-checked') !== 'true';
  tgSafe.setAttribute('aria-checked', String(v));
  audio.setFootstepSafe(v);
});
// Bypass: hold the button, or B, to hear the sound without the 3D placement (as in the app).
const bypassBtn = $('#bypass');
function setBypass(on) {
  audio.setBypass(on);
  bypassBtn.setAttribute('aria-pressed', String(on));
  dBypass.classList.toggle('on', on);
  dsvg.classList.toggle('bypassed', on);
}
bypassBtn.addEventListener('pointerdown', e => { try { bypassBtn.setPointerCapture(e.pointerId); } catch (err) { /* ignore */ } setBypass(true); });
for (const ev of ['pointerup', 'pointercancel', 'blur']) bypassBtn.addEventListener(ev, () => setBypass(false));
bypassBtn.addEventListener('keydown', e => { if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) { e.preventDefault(); setBypass(true); } });
bypassBtn.addEventListener('keyup', e => { if (e.key === ' ' || e.key === 'Enter') setBypass(false); });
window.addEventListener('keydown', e => {
  if (e.key.toLowerCase() !== 'b' || e.repeat || e.ctrlKey || e.metaKey || e.altKey || !audio.running) return;
  if (/input|textarea|select/i.test((doc.activeElement && doc.activeElement.tagName) || '') || !demoVisible) return;
  setBypass(true);
});
window.addEventListener('keyup', e => { if (e.key.toLowerCase() === 'b' && bypassBtn.getAttribute('aria-pressed') === 'true') setBypass(false); });
doc.addEventListener('visibilitychange', async () => {
  if (doc.hidden && audio.running) { await audio.stop(); audio.events.length = 0; syncUI(); }
});

// Live visuals while it plays: glow with the level, amber rings while the music ducks, green
// rings while the voice talks, and the game's sounds landing in the wedge.
const lv = {music: 0, voice: 0};
let lastDuck = 0, lastBar = 0, lastTalk = 0;
function demoFrame(now) {
  demoRAF = 0;
  if (!audio.running) {
    for (const s of DSRC) { s.P.glow.removeAttribute('transform'); s.P.g.classList.remove('ducked', 'speaking'); }
    dDuck.classList.remove('on');
    return;
  }
  const L = audio.levels();
  for (const s of DSRC) {
    lv[s.kind] += (L[s.kind] - lv[s.kind]) * 0.25;
    if (!RM) s.P.glow.setAttribute('transform', `scale(${(0.78 + lv[s.kind] * 1.15).toFixed(3)})`);
  }
  const lay = audio.layers;
  const ducking = lay.music && ((audio.ducking && lay.game) || (audio.talking && lay.voice));
  const talking = audio.talking && lay.voice;
  dDuck.classList.toggle('on', ducking);
  dMusic.P.g.classList.toggle('ducked', ducking);
  dVoice.P.g.classList.toggle('speaking', talking);
  if (ducking && now - lastDuck > 455) { duckRing(dMusic.P); lastDuck = now; }
  if (demoVisible) {
    if (lay.music && now - lastBar > 2857) { emitWave(D.waves, dMusic.P.x, dMusic.P.y, '--accent', {r0: 18, r1: 680, dur: 5600, op: .42}); lastBar = now; }
    if (talking && now - lastTalk > 430) { emitWave(D.waves, dVoice.P.x, dVoice.P.y, '--c-voice', {r0: 19, r1: 520, dur: 5200, op: .34}); lastTalk = now; }
  }
  const t = audio.now;
  while (audio.events.length && audio.events[0].t <= t) {
    const ev = audio.events.shift();
    if (!lay.game || t - ev.t > 0.25) continue;
    // Where the game's own stereo mix puts the sound: inside the wedge, never moved.
    const shot = ev.kind === 'shot', p = pos(ev.pan * 70, shot ? 9 : 5);
    emitWave(D.fx, p[0], p[1], '--info', shot ? {r0: 3, r1: 64, dur: 900, op: .95, ease: 'cubic-bezier(.1,.7,.3,1)'} : {r0: 3, r1: 34, dur: 800, op: .9, ease: 'cubic-bezier(.1,.7,.3,1)'});
    if (shot) {
      dsvg.classList.add('game-loud');
      setTimeout(() => dsvg.classList.remove('game-loud'), 260);
    }
  }
  demoRAF = requestAnimationFrame(demoFrame);
}

/* =================================================================== SECTION FIELDS */
// Faint log rings that keep going behind the downloads, like a stage floor.
function bgRings(svg, half, cx, cy, scale, maxR) {
  if (!svg) return;
  svg.setAttribute('viewBox', `${-half} ${-half} ${half * 2} ${half * 2}`);
  const all = RINGS.concat(FIELD_D);
  all.forEach((d, i) => {
    const r = rFor(d) * scale;
    if (r > maxR) return;
    const f = 1 - r / maxR;
    el('circle', {cx, cy, r: r.toFixed(1), fill: 'none', style: 'stroke:var(--text)', 'stroke-width': 1, 'vector-effect': 'non-scaling-stroke', 'stroke-opacity': (0.015 + 0.07 * f).toFixed(3)}, svg);
    if (i === 5) el('circle', {cx, cy, r: r.toFixed(1), fill: 'none', style: 'stroke:var(--accent)', 'stroke-width': 1, 'vector-effect': 'non-scaling-stroke', 'stroke-opacity': '.16'}, svg);
  });
}
bgRings($('#dl-field'), 1000, 0, 0, 2.1, 990);

/* =================================================================== REVEALS, PARALLAX, NAV */
const nav = $('#nav');
const io = 'IntersectionObserver' in window ? IntersectionObserver : null;
if (io) {
  const rv = new io(es => es.forEach(e => { if (e.isIntersecting) { e.target.classList.add('in'); rv.unobserve(e.target); } }), {rootMargin: '0px 0px -8% 0px', threshold: 0.12});
  $$('.reveal, .kin').forEach(n => rv.observe(n));
  new io(es => { heroOn = es[0].isIntersecting; root.classList.toggle('hero-off', !heroOn); }).observe(heroSec);
  new io(es => {
    demoVisible = es[0].isIntersecting;
    if (demoVisible) { D.svg.classList.add('in'); if (DF) DF.svg.classList.add('in'); }
  }, {threshold: 0.2}).observe(demoWrap);
} else {
  D.svg.classList.add('in'); if (DF) DF.svg.classList.add('in'); demoVisible = true;
}
if (RM) { D.svg.classList.add('in'); if (DF) DF.svg.classList.add('in'); }

const speedEls = $$('[data-speed]'), depthEls = $$('[data-depth]');
let ticking = false;
function onScroll() {
  ticking = false;
  const y = window.scrollY || 0, vh = window.innerHeight;
  nav.classList.toggle('scrolled', y > 8);
  if (RM) return;
  if (y < vh * 1.8) for (const e of speedEls) e.style.transform = `translate3d(0, ${(y * +e.dataset.speed).toFixed(1)}px, 0)`;
  for (const e of depthEls) {
    const r = e.parentElement.getBoundingClientRect();
    if (r.bottom < -300 || r.top > vh + 300) continue;
    const c = r.top + r.height / 2 - vh / 2;
    e.style.transform = `translate3d(0, ${(c * -(+e.dataset.depth)).toFixed(1)}px, 0)`;
  }
}
window.addEventListener('scroll', () => { if (!ticking) { ticking = true; requestAnimationFrame(onScroll); } }, {passive: true});
onScroll();

let rT = 0;
const relayout = () => { clearTimeout(rT); rT = setTimeout(() => { layoutHero(); layoutDemo(); }, 60); };
if ('ResizeObserver' in window) { const ro = new ResizeObserver(relayout); ro.observe(heroWrap); ro.observe(demoWrap); }
else window.addEventListener('resize', relayout);

// Point out the visitor's own system in the downloads (desktop systems only; just the user agent,
// nothing is sent anywhere).
(() => {
  const ua = navigator.userAgent || '';
  const os = /Android|iPhone|iPad|iPod/i.test(ua) ? null
    : /Windows/i.test(ua) ? 'win'
    : /Macintosh|Mac OS X/i.test(ua) ? 'mac'
    : /Linux|X11|CrOS/i.test(ua) ? 'linux'
    : null;
  if (!os) return;
  $$('.plat').forEach(p => {
    const mine = p.dataset.os === os, b = $('.btn', p);
    p.classList.toggle('mine', mine);
    b.classList.toggle('btn-primary', mine);
    b.classList.toggle('btn-ghost', !mine);
  });
})();
