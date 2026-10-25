// Entry point: the hero, the preset plans, the sound check and the small page behaviours.

import { initHero, initPresetPlans } from './hero.js';
import { initSoundCheck } from './listen.js';

const anim = document.documentElement.classList.contains('anim');

initHero({ anim });
initPresetPlans();
initSoundCheck({ anim });

// The slim bar slides in once the hero's own navigation has scrolled away.
const bar = document.getElementById('bar');
const heroNav = document.querySelector('.hero .nav');
if (bar && heroNav) {
  new IntersectionObserver(([e]) => {
    const show = !e.isIntersecting && e.boundingClientRect.top < 0;
    bar.classList.toggle('on', show);
    bar.inert = !show;
  }).observe(heroNav);

  // Mark the act being read.
  const links = new Map([...bar.querySelectorAll('.bar-links a')].map((a) => [a.hash.slice(1), a]));
  const seen = new IntersectionObserver((entries) => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      for (const [id, a] of links) {
        if (id === e.target.id) a.setAttribute('aria-current', 'true');
        else a.removeAttribute('aria-current');
      }
    }
  }, { rootMargin: '-45% 0px -50% 0px' });
  for (const id of links.keys()) {
    const el = document.getElementById(id);
    if (el) seen.observe(el);
  }
}

// Point out the visitor's own system on the box-office tickets (desktop systems only).
const ua = navigator.userAgent;
const os = /Android|iPhone|iPad|iPod/i.test(ua) ? null
  : /Windows/i.test(ua) ? 'windows'
  : /Macintosh|Mac OS X/i.test(ua) ? 'mac'
  : /Linux|X11|CrOS/i.test(ua) ? 'linux'
  : null;
if (os) document.querySelector(`.tk[data-os="${os}"]`)?.classList.add('is-yours');
