// Act I: the hero stage. The house lights dim, a spotlight finds the GAME wedge, the rings ripple
// out from the head, and the cast enters from the wings onto their marks: Spotify behind, a
// podcast on the left, Discord on the right. Then, every few seconds, the game gets loud and the
// music ducks, so the idle motion explains a feature instead of only glowing.

import {
  C, OUTER, HEAD, NSS, GLYPH, WEDGE_D, rd, clamp, radius, pt, arcD, speakers,
  headSVG, ringsSVG, wedgeSVG, ticksSVG, ringLabelsSVG, layoutRingLabels, setAttrs,
} from './stage.js';

const SRC = [
  { id: 'spotify', name: 'Spotify', meta: 'BEHIND · 1.5 M', duckMeta: 'DUCKING', az: 180, d: 1.5, width: 60, glyph: 'note', side: 'below', az0: 194, r0: 720, start: 2000, dur: 1500, bd: '0s' },
  { id: 'podcast', name: 'Podcast', meta: 'LEFT · 1 M', az: -90, d: 1, width: 0, glyph: 'mic', side: 'left', az0: -118, r0: 720, start: 2700, dur: 1500, bd: '-1.3s' },
  { id: 'discord', name: 'Discord', meta: 'RIGHT · 1 M', az: 90, d: 1, width: 0, glyph: 'voice', side: 'right', az0: 118, r0: 720, start: 3400, dur: 1500, bd: '-2.5s' },
];

// The log scale keeps going past the radar's 20 m, faintly, out into the house.
const FAR_RINGS = [50, 120, 300];

const easeOutQuart = (t) => 1 - Math.pow(1 - t, 4);
const easeOutCubic = (t) => 1 - Math.pow(1 - t, 3);
const smooth = (u) => u * u * (3 - 2 * u);

export function initHero({ anim }) {
  const host = document.getElementById('hero-radar');
  if (!host) return;
  for (const s of SRC) {
    s.rr = radius(s.d);
    [s.x, s.y] = pt(s.az, s.rr);
    s.uStart = Math.max(0, (s.r0 - (OUTER + 70)) / (s.r0 - s.rr));
    s.curW = s.width;
  }
  host.innerHTML = heroSVG();
  const svg = host.firstElementChild;
  for (const s of SRC) {
    const q = (p) => svg.querySelector(`#${p}-${s.id}`);
    s.el = q('p');
    s.scaleEl = s.el.querySelector('.p-scale');
    s.spike = q('sk');
    s.trail = q('tr');
    s.link = q('ln');
    s.chip = q('ch');
    s.metaEl = s.chip.querySelector('.chip-meta');
    if (s.width) {
      s.spread = q('sp');
      s.lr = q('lr');
    }
  }

  const mq = window.matchMedia('(max-width: 720px)');
  let K = 1;

  function setSpread(s, w) {
    s.curW = w;
    if (w < 0.5) {
      s.spread.setAttribute('d', '');
      s.lr.style.visibility = 'hidden';
      return;
    }
    s.lr.style.visibility = '';
    const [aL, aR] = speakers(s.az, w);
    const [lo, hi] = aL < aR ? [aL, aR] : [aR, aL];
    s.spread.setAttribute('d', arcD(lo, hi, s.rr));
    const pl = pt(aL, s.rr);
    const pr = pt(aR, s.rr);
    const tl = pt(aL, s.rr + 20 * K);
    const tr = pt(aR, s.rr + 20 * K);
    setAttrs(s.lr.querySelector('.lr-l'), { cx: pl[0], cy: pl[1], r: 5.5 * K });
    setAttrs(s.lr.querySelector('.lr-r'), { cx: pr[0], cy: pr[1], r: 5.5 * K });
    setAttrs(s.lr.querySelector('.lr-lt'), { x: tl[0], y: tl[1] });
    setAttrs(s.lr.querySelector('.lr-rt'), { x: tr[0], y: tr[1] });
  }

  function layout() {
    K = mq.matches ? 1.5 : 1;
    for (const s of SRC) {
      s.scaleEl.setAttribute('transform', `scale(${K})`);
      s.spike.setAttribute('transform', `translate(${s.x} ${s.y}) scale(${K})`);
      if (s.width) setSpread(s, s.curW);
    }
    layoutRingLabels(svg, K);
    // Name chips beside each puck.
    for (const s of SRC) {
      const g = s.chip;
      const bg = g.querySelector('.chip-bg');
      const dot = g.querySelector('.chip-dot');
      const nm = g.querySelector('.chip-name');
      const mt = s.metaEl;
      const ns = parseFloat(getComputedStyle(nm).fontSize) || 16;
      const ms = parseFloat(getComputedStyle(mt).fontSize) || 12;
      const cur = mt.textContent;
      let mw = 0;
      for (const v of [s.meta, s.duckMeta || s.meta]) {
        mt.textContent = v;
        mw = Math.max(mw, mt.getComputedTextLength());
      }
      mt.textContent = cur;
      const nw = nm.getComputedTextLength();
      const padX = 11 * K;
      const padY = 9 * K;
      const dotW = 13 * K;
      const w = Math.max(nw + dotW, mw) + padX * 2;
      const b1 = padY + ns * 0.74;
      const b2 = b1 + ms * 1.6;
      const h = b2 + ms * 0.3 + padY * 0.9;
      const off = 31 * K;
      let x0;
      let y0;
      if (s.side === 'below') {
        x0 = s.x - w / 2;
        y0 = s.y + off + 6 * K;
      } else if (s.side === 'left') {
        x0 = s.x - off - w;
        y0 = s.y - h / 2;
      } else {
        x0 = s.x + off;
        y0 = s.y - h / 2;
      }
      setAttrs(bg, { x: x0, y: y0, width: w, height: h, rx: 7 * K });
      setAttrs(dot, { cx: x0 + padX + 4 * K, cy: y0 + b1 - ns * 0.33, r: 3.6 * K });
      setAttrs(nm, { x: x0 + padX + dotW, y: y0 + b1 });
      setAttrs(mt, { x: x0 + padX, y: y0 + b2 });
    }
  }

  // The stage manager's cue line.
  const cueBox = document.querySelector('.cue');
  const cueText = document.getElementById('cue-text');
  const cue = (txt) => {
    if (!cueBox) return;
    cueBox.classList.add('swap');
    setTimeout(() => {
      cueText.textContent = txt;
      cueBox.classList.remove('swap');
    }, 190);
  };

  // The game gets loud, the music steps back.
  let paused = false;
  const LIVE = 'The game is live · untouched';
  function duck() {
    if (paused) return;
    const sp = SRC[0];
    svg.classList.add('ducking');
    sp.metaEl.textContent = sp.duckMeta;
    sp.metaEl.classList.add('is-duck');
    cue('Game gets loud · music ducks');
    setTimeout(() => {
      svg.classList.remove('ducking');
      sp.metaEl.textContent = sp.meta;
      sp.metaEl.classList.remove('is-duck');
      cue(LIVE);
    }, 2900);
  }

  // Entrance from the wings: a polar glide, distance linear, direction eased.
  const pathAt = (s, u) => pt(s.az0 + (s.az - s.az0) * smooth(u), s.r0 + (s.rr - s.r0) * u);
  function trailD(s, u) {
    if (u <= s.uStart) return '';
    const n = Math.max(2, Math.ceil((u - s.uStart) / 0.012));
    let d = '';
    for (let i = 0; i <= n; i++) {
      const p = pathAt(s, s.uStart + ((u - s.uStart) * i) / n);
      d += `${i ? 'L' : 'M'}${p[0]} ${p[1]}`;
    }
    return d;
  }
  for (const s of SRC) s.trail.setAttribute('d', trailD(s, 1));

  function place(s, u) {
    const p = pathAt(s, u);
    s.el.setAttribute('transform', `translate(${p[0]} ${p[1]})`);
    s.el.style.opacity = clamp((u - 0.22) / 0.28, 0, 1).toFixed(3);
    s.trail.setAttribute('d', trailD(s, u));
  }
  function land(s, now) {
    s.landed = true;
    s.landedAt = now;
    s.el.style.opacity = '';
    s.spike.classList.add('hit-on');
    s.link.classList.add('on');
    s.trail.classList.add('rest');
    if (s.width) {
      s.spread.classList.add('on');
      s.lr.classList.add('on');
    }
    setTimeout(() => s.chip.classList.add('on'), 260);
  }
  function sequence() {
    for (const s of SRC) {
      s.el.classList.add('live');
      s.trail.classList.remove('rest');
      place(s, 0);
      if (s.width) setSpread(s, 0);
    }
    cueText.textContent = 'Standby · house lights out';
    for (const [t, text] of [[900, 'Cue 1 · Spot on centre stage'], [2000, 'Cue 2 · Music, behind you'], [2700, 'Cue 3 · Podcast, your left'], [3400, 'Cue 4 · Discord, your right']]) {
      setTimeout(() => cue(text), t);
    }
    const t0 = performance.now();
    function frame(now) {
      const el = now - t0;
      let busy = false;
      for (const s of SRC) {
        const t = (el - s.start) / s.dur;
        if (t < 0) {
          busy = true;
          continue;
        }
        if (!s.landed) {
          place(s, easeOutQuart(Math.min(1, t)));
          if (t >= 1) land(s, now);
          else busy = true;
        }
        if (s.width && s.landedAt) {
          const a = clamp((now - s.landedAt - 120) / 850, 0, 1);
          if (a < 1) busy = true;
          setSpread(s, s.width * easeOutCubic(a));
        }
      }
      if (busy) requestAnimationFrame(frame);
      else {
        setTimeout(() => cue(LIVE), 500);
        setTimeout(() => {
          duck();
          setInterval(duck, 9000);
        }, 2600);
      }
    }
    requestAnimationFrame(frame);
  }

  // Dust in the beam.
  let seed = 11;
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
  const motes = document.querySelector('.motes');
  if (anim && motes) {
    for (let i = 0; i < 18; i++) {
      const m = document.createElement('i');
      m.className = 'mote';
      m.style.cssText = `--x:${(24 + rnd() * 52).toFixed(1)}%;--y:${(60 + rnd() * 34).toFixed(1)}%;--s:${(1.4 + rnd() * 2.4).toFixed(1)}px;--dur:${(7 + rnd() * 8).toFixed(1)}s;--del:${(-rnd() * 12).toFixed(1)}s;--dx:${((rnd() - 0.5) * 44).toFixed(0)}px;--o:${(0.25 + rnd() * 0.5).toFixed(2)}`;
      motes.appendChild(m);
    }
  }

  layout();
  document.fonts?.ready.then(layout);
  mq.addEventListener?.('change', layout);

  if (anim) {
    sequence();
    const hero = document.querySelector('.hero');
    new IntersectionObserver((entries) => {
      paused = !entries[0].isIntersecting;
      hero.classList.toggle('paused', paused);
    }).observe(hero);
  }
}

function heroSVG() {
  let s = '<svg class="radar hero-radar" viewBox="-10 -10 820 820" role="img" aria-labelledby="radar-t radar-d">' +
    '<title id="radar-t">The Sidestage radar, seen from above</title>' +
    '<desc id="radar-d">Your head is in the centre, facing up. The game plays in the blue wedge in front of you and is left untouched. Spotify sits behind you at 1.5 metres, spread 60 degrees between left and right. A podcast is on your left at 1 metre and Discord is on your right at 1 metre. Several sources at once comes with the next update.</desc>' +
    `<defs><clipPath id="wedgeClip"><path d="${WEDGE_D}"/></clipPath></defs>`;
  for (const [i, d] of FAR_RINGS.entries()) {
    // Same log spacing as the radar's own rings, continued outwards.
    const r = OUTER + (Math.log(d / 20) / Math.log(40)) * (OUTER - OUTER * 0.16);
    s += `<circle class="far-ring" style="--i:${i}" cx="${C}" cy="${C}" r="${rd(r)}"${NSS}/>`;
  }
  s += `<circle class="floor" cx="${C}" cy="${C}" r="${OUTER + 8}"/>`;
  s += `<ellipse class="pool" cx="${C}" cy="268" rx="340" ry="300"/>`;
  s += wedgeSVG(true);
  s += `<g clip-path="url(#wedgeClip)"><ellipse class="wedge-light" cx="${C}" cy="236" rx="290" ry="245"/></g>`;
  s += ringsSVG();
  s += ticksSVG();
  s += ringLabelsSVG();
  // A loud moment in the game (drives the ducking cue).
  const b = pt(-14, radius(6));
  s += `<g class="burst" transform="translate(${b[0]} ${b[1]})"><circle r="18"${NSS}/><circle r="18"${NSS}/><circle class="core" r="3.5"/></g>`;
  // Blocking: entrance trails and spike marks.
  for (const src of SRC) s += `<path class="trail rest" id="tr-${src.id}" d=""${NSS}/>`;
  SRC.forEach((src, i) => {
    s += `<g class="spike" id="sk-${src.id}" style="--sd:${(1.8 + i * 0.16).toFixed(2)}s" transform="translate(${src.x} ${src.y})"><circle class="hit" r="22"${NSS}/><path d="M-28-18V-28H-18M18-28H28V-18M28 18V28H18M-18 28H-28V18"/></g>`;
  });
  for (const src of SRC) {
    const p = pt(src.az, HEAD);
    s += `<line class="link js-reveal" id="ln-${src.id}" x1="${p[0]}" y1="${p[1]}" x2="${src.x}" y2="${src.y}"${NSS}/>`;
  }
  for (const src of SRC) {
    if (!src.width) continue;
    s += `<path class="spread js-reveal" id="sp-${src.id}" d=""/>` +
      `<g class="lr js-reveal" id="lr-${src.id}"><circle class="lr-l" r="5.5"${NSS}/><circle class="lr-r" r="5.5"${NSS}/><text class="lr-lt">L</text><text class="lr-rt">R</text></g>`;
  }
  s += headSVG();
  for (const src of SRC) {
    const duck = src.id === 'spotify' ? `<g class="duck"><circle class="duck-ring" r="21"${NSS}/><circle class="duck-ring" r="21"${NSS}/><circle class="duck-ring" r="21"${NSS}/></g>` : '';
    s += `<g class="puck" id="p-${src.id}" transform="translate(${src.x} ${src.y})"><g class="p-scale"><circle class="p-glow" r="40" style="--bd:${src.bd}"/>${duck}` +
      `<g class="p-core"><circle class="p-body" r="17"${NSS}/>${GLYPH[src.glyph]}</g></g></g>`;
  }
  for (const src of SRC) {
    s += `<g class="chip js-reveal" id="ch-${src.id}"><rect class="chip-bg"${NSS}/><circle class="chip-dot"/><text class="chip-name">${src.name}</text><text class="chip-meta">${src.meta}</text></g>`;
  }
  return `${s}</svg>`;
}

// Scene ii: the six presets as small stage plans, from crates/sidestage-dsp/src/presets.rs.
const PRESET_PLANS = {
  behind: { az: 180, d: 1.5, w: 60, e: 0 },
  next: { az: 150, d: 5, w: 0, e: 0, muffled: true },
  far: { az: 180, d: 15, w: 10, e: 5 },
  above: { az: 180, d: 2, w: 30, e: 60 },
  desk: { az: 35, d: 0.7, w: 0, e: -20 },
  lounge: { az: 180, d: 2.5, w: 90, e: 10 },
};

function miniSVG(p) {
  const r = radius(p.d);
  const [x, y] = pt(p.az, r);
  let s = `<svg class="radar mini${p.muffled ? ' muffled' : ''}" viewBox="44 44 712 712" aria-hidden="true" focusable="false">`;
  s += `<circle class="floor" cx="${C}" cy="${C}" r="${OUTER + 6}"/>`;
  s += wedgeSVG(false) + ringsSVG();
  const h = pt(p.az, HEAD);
  s += `<line class="link" x1="${h[0]}" y1="${h[1]}" x2="${x}" y2="${y}"${NSS}/>`;
  if (p.w > 0) {
    const [a0, a1] = speakers(p.az, p.w).sort((m, n) => m - n);
    const e0 = pt(a0, r);
    const e1 = pt(a1, r);
    s += `<path class="spread" d="${arcD(a0, a1, r)}"/><g class="lr"><circle cx="${e0[0]}" cy="${e0[1]}" r="13"${NSS}/><circle cx="${e1[0]}" cy="${e1[1]}" r="13"${NSS}/></g>`;
  }
  s += headSVG();
  s += `<g class="mpuck" transform="translate(${x} ${y})"><circle class="p-glow" r="86"/><g transform="scale(2.3)"><circle class="p-body" r="17"${NSS}/>${GLYPH.note}</g></g>`;
  if (p.e) {
    // Height badge centred above the puck (the app puts it beside; here it must fit a small card).
    const txt = `${p.e > 0 ? '+' : '−'}${Math.abs(p.e)}°`;
    const fs = 54;
    const bw = txt.length * fs * 0.6 + 78;
    const bh = 76;
    const bx = x - bw / 2;
    const by = y - 66 - bh;
    const tx = bx + 30;
    const ty = by + bh / 2;
    const sg = p.e > 0 ? -1 : 1;
    s += `<g class="mbadge"><rect class="mb-bg" x="${rd(bx)}" y="${rd(by)}" width="${rd(bw)}" height="${bh}" rx="38"${NSS}/>` +
      `<path class="mb-tri" d="M${rd(tx - 12)} ${rd(ty - sg * 8)}L${rd(tx + 12)} ${rd(ty - sg * 8)}L${rd(tx)} ${rd(ty + sg * 12)}Z"/>` +
      `<text class="mb-t" x="${rd(bx + 54)}" y="${rd(ty)}">${txt}</text></g>`;
  }
  return `${s}</svg>`;
}

export function initPresetPlans() {
  for (const el of document.querySelectorAll('.rep-svg')) {
    const p = PRESET_PLANS[el.dataset.preset];
    if (p) el.innerHTML = miniSVG(p);
  }
  // The ducking sketch's game-level bars.
  const bars = document.getElementById('duck-bars');
  if (bars) {
    let seed = 7;
    const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
    let html = '';
    for (let bx = 22; bx <= 378; bx += 5) {
      const loud = (bx > 118 && bx < 180) || (bx > 252 && bx < 300);
      const hh = loud ? 22 + rnd() * 20 : 3 + rnd() * 6;
      html += `<rect x="${bx}" y="${(118 - hh).toFixed(1)}" width="2.6" height="${hh.toFixed(1)}" rx="1"/>`;
    }
    bars.innerHTML = html;
  }
}
