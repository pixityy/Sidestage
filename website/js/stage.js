// The radar ("the stage plan"), drawn as SVG markup with the app's geometry
// (crates/sidestage/src/ui/radar.rs): the listener's head in the middle facing up, log-spaced
// distance rings from 0.5 to 20 m, the GAME wedge of +-60 degrees in front, and pucks for the
// sources. Angle 0 is straight ahead, +90 is right, 180 is behind. Every radar on the page uses a
// 800 x 800 user space centred on (400, 400).

export const RINGS = [0.5, 1, 2, 5, 10, 20];
export const MIN = 0.5;
export const MAX = 20;
export const WEDGE = 60;
const LABEL_AZ = -45;
export const C = 400;
export const OUTER = 340;
const INNER = OUTER * 0.16;
export const HEAD = INNER * 0.62;
export const NSS = ' vector-effect="non-scaling-stroke"';

export const rd = (n) => Math.round(n * 100) / 100;
export const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
export const wrap = (a) => 180 - ((((180 - a) % 360) + 360) % 360);
const RAD = Math.PI / 180;

/** Ring radius for a distance in metres (log scale, clamped). */
export function radius(d) {
  const c = clamp(d, MIN, MAX);
  return INNER + (Math.log(c / MIN) / Math.log(MAX / MIN)) * (OUTER - INNER);
}

/** Distance in metres for a radius (clamped to the radar's range). */
export function distance(r) {
  const t = clamp((r - INNER) / (OUTER - INNER), 0, 1);
  return MIN * Math.exp(t * Math.log(MAX / MIN));
}

/** Point for a direction and a radius. */
export function pt(az, r) {
  return [rd(C + Math.sin(az * RAD) * r), rd(C - Math.cos(az * RAD) * r)];
}

/** [azimuth in (-180, 180], distance in metres] for a point. */
export function polar(x, y) {
  const dx = x - C;
  const dy = y - C;
  const len = Math.hypot(dx, dy);
  const az = len < 1e-3 ? 180 : wrap(Math.atan2(dx, -dy) / RAD);
  return [az, distance(len)];
}

export function arcD(a0, a1, r) {
  const s = pt(a0, r);
  const e = pt(a1, r);
  return `M${s[0]} ${s[1]}A${rd(r)} ${rd(r)} 0 ${a1 - a0 > 180 ? 1 : 0} 1 ${e[0]} ${e[1]}`;
}

/** Where the two virtual speakers of a stereo source sit, as in Sidestage's renderer: the pair
 * narrows to a point at the sides so the left channel always stays on the listener's left. */
export function speakers(az, width) {
  const spread = (width / 2) * clamp(2 * Math.cos(az * RAD), -1, 1);
  return [az - spread, az + spread];
}

export const WEDGE_D = `M${C} ${C}L${pt(-WEDGE, OUTER).join(' ')}A${OUTER} ${OUTER} 0 0 1 ${pt(WEDGE, OUTER).join(' ')}Z`;

export const GLYPH = {
  note: '<g class="glyph"><ellipse cx="-3.3" cy="4.6" rx="4.5" ry="3.3" transform="rotate(-20 -3.3 4.6)"/><path class="gs" d="M0.9 4V-8.3L5.4-4.6L5.1-0.5"/></g>',
  mic: '<g class="glyph"><rect x="-3.6" y="-9.6" width="7.2" height="11.6" rx="3.6"/><path class="gs" d="M-6.8-1.6a6.8 6.8 0 0 0 13.6 0M0 5.2V9M-3.8 9H3.8"/></g>',
  voice: '<g class="glyph" transform="translate(0 -1.2)"><path d="M-8.6-8h17.2a2.8 2.8 0 0 1 2.8 2.8v8.4a2.8 2.8 0 0 1-2.8 2.8H-1.2l-5.2 4.4V6h-2.2a2.8 2.8 0 0 1-2.8-2.8v-8.4a2.8 2.8 0 0 1 2.8-2.8z"/><path class="gc" d="M-5-2.6v3.2M-1.7-4.6v7.2M1.7-3.4v4.8M5-1.8v1.6"/></g>',
};

export function headSVG() {
  const hr = HEAD;
  let s = '';
  for (const side of [-1, 1]) {
    const ex = C + side * hr * 0.98;
    const ey = C + hr * 0.05;
    const w = hr * 0.32;
    const h = hr * 0.62;
    s += `<rect class="ear" x="${rd(ex - w / 2)}" y="${rd(ey - h / 2)}" width="${rd(w)}" height="${rd(h)}" rx="${rd(hr * 0.16)}"${NSS}/>`;
  }
  s += `<path class="nose" d="M${rd(C - hr * 0.26)} ${rd(C - hr * 0.9)}L${C} ${rd(C - hr * 1.28)}L${rd(C + hr * 0.26)} ${rd(C - hr * 0.9)}Z"${NSS}/>`;
  s += `<circle class="face" cx="${C}" cy="${C}" r="${rd(hr)}"${NSS}/>`;
  return `<g class="head">${s}</g>`;
}

export function ringsSVG() {
  return `<g class="rings">${RINGS.map(
    (d, i) => `<circle class="ring${i === RINGS.length - 1 ? ' outer' : ''}" style="--i:${i}" cx="${C}" cy="${C}" r="${rd(radius(d))}"${NSS}/>`,
  ).join('')}</g>`;
}

export function wedgeSVG(withLabel) {
  let s = `<path class="wedge-fill" d="${WEDGE_D}"/>`;
  for (const a of [-WEDGE, WEDGE]) {
    const p1 = pt(a, INNER);
    const p2 = pt(a, OUTER);
    s += `<line class="wedge-edge" x1="${p1[0]}" y1="${p1[1]}" x2="${p2[0]}" y2="${p2[1]}"${NSS}/>`;
  }
  if (withLabel) s += `<text class="wedge-label" x="${C}" y="${rd(C - OUTER + 28)}">GAME</text>`;
  return s;
}

/** Stage-plan ticks every 15 degrees around the outer ring, with the four sides named. */
export function ticksSVG(names = ['CENTRE STAGE', 'BEHIND YOU', 'LEFT WING', 'RIGHT WING']) {
  let t = '';
  for (let a = -165; a <= 180; a += 15) {
    const major = a % 90 === 0;
    const mid = a % 45 === 0;
    const p1 = pt(a, OUTER + 8);
    const p2 = pt(a, OUTER + (major ? 22 : mid ? 16 : 12));
    t += `<line class="tick${major ? ' major' : ''}" x1="${p1[0]}" y1="${p1[1]}" x2="${p2[0]}" y2="${p2[1]}"${NSS}/>`;
  }
  const L = OUTER + 38;
  t += `<text class="olabel centre" x="${C}" y="${C - L}">${names[0]}</text>`;
  t += `<text class="olabel" x="${C}" y="${C + L}">${names[1]}</text>`;
  t += `<text class="olabel" x="${C - L}" y="${C}" transform="rotate(-90 ${C - L} ${C})">${names[2]}</text>`;
  t += `<text class="olabel" x="${C + L}" y="${C}" transform="rotate(90 ${C + L} ${C})">${names[3]}</text>`;
  return `<g class="ticks">${t}</g>`;
}

/** Ring labels on the front-left diagonal, outermost first; laid out by `layoutRingLabels`. */
export function ringLabelsSVG() {
  return `<g class="rlabels">${[...RINGS].reverse().map((d, i) => {
    const p = pt(LABEL_AZ, radius(d));
    return `<g class="rl" style="--i:${i}" data-x="${p[0]}" data-y="${p[1]}"><rect rx="3" x="${rd(p[0] - 18)}" y="${rd(p[1] - 9)}" width="36" height="18"/><text x="${p[0]}" y="${p[1]}">${d} m</text></g>`;
  }).join('')}</g>`;
}

/** Size the ring-label chips to their text and hide any that would collide (as the app does). */
export function layoutRingLabels(svg, k = 1) {
  const hb = HEAD * 1.35;
  const placed = [];
  for (const g of svg.querySelectorAll('.rl')) {
    g.style.display = '';
    const t = g.querySelector('text');
    const r = g.querySelector('rect');
    const fs = parseFloat(getComputedStyle(t).fontSize) || 13;
    let tw;
    try {
      tw = t.getComputedTextLength();
    } catch {
      tw = t.textContent.length * fs * 0.6;
    }
    const w = tw + 9 * k;
    const h = fs * 1.35;
    const x = +g.dataset.x;
    const y = +g.dataset.y;
    const box = [x - w / 2, y - h / 2, x + w / 2, y + h / 2];
    const hitsHead = !(box[2] < C - hb || box[0] > C + hb || box[3] < C - hb || box[1] > C + hb);
    const hitsOther = placed.some((q) => !(box[2] + 1 < q[0] || box[0] - 1 > q[2] || box[3] + 1 < q[1] || box[1] - 1 > q[3]));
    if (hitsHead || hitsOther) {
      g.style.display = 'none';
      continue;
    }
    placed.push(box);
    for (const [a, v] of [['x', box[0]], ['y', box[1]], ['width', w], ['height', h], ['rx', 3 * k]]) r.setAttribute(a, rd(v));
  }
}

/** Plain-language direction, as the app's readout says it. */
export function directionName(az) {
  const a = wrap(az);
  const right = a > 0;
  const x = Math.abs(a);
  if (x < 15) return 'In front';
  if (x < 67.5) return right ? 'Front right' : 'Front left';
  if (x < 112.5) return right ? 'Right' : 'Left';
  if (x < 165) return right ? 'Behind right' : 'Behind left';
  return 'Behind you';
}

export const formatDistance = (m) => (m < 10 ? `${m.toFixed(1)} m` : `${Math.round(m)} m`);

export function setAttrs(el, o) {
  for (const k in o) el.setAttribute(k, typeof o[k] === 'number' ? rd(o[k]) : o[k]);
}
