// Boo's own sound set — synthesised from scratch, no samples, no dependencies.
//   node scripts/gen-sounds.mjs   (or: npm run sounds)
// Writes 28 mono 16-bit 44.1 kHz WAVs to windows/sounds/. Output is deterministic
// (seeded noise), so re-running produces byte-identical files.
//
// Sonic identity: a friendly little ghost. Soft sine/triangle voices, gentle pitch
// glides, a breathy "oooo" vibrato on character sounds, short clean blips for UI.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SR = 44100;
const OUT = join(dirname(fileURLToPath(import.meta.url)), "..", "sounds");
const TAU = Math.PI * 2;

// ── Tiny toolkit ──────────────────────────────────────────────────────────────

function rng(seed) { // mulberry32
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const clamp = (x, lo = 0, hi = 1) => Math.min(hi, Math.max(lo, x));
const semis = (n) => 2 ** (n / 12);

// Partial recipes: [ratio, amplitude]
const SINE = [[1, 1]];
const GHOST = [[1, 1], [2, 0.16], [3, 0.05]]; // warm voice-ish
const TRI = [[1, 1], [3, 0.11], [5, 0.04]];
const BELL = [[1, 1], [2.76, 0.16], [5.4, 0.05]];

/** gain at time t of a note lasting dur: smooth attack, exp decay to sustain level, smooth release. */
function env(t, dur, { a = 0.01, d = 0.1, s = 0.7, r = 0.05 }) {
  if (t < 0 || t > dur) return 0;
  const att = t < a ? 0.5 - 0.5 * Math.cos((Math.PI * t) / a) : 1;
  const body = s + (1 - s) * Math.exp(-Math.max(0, t - a) / d);
  const rel = t > dur - r ? 0.5 + 0.5 * Math.cos((Math.PI * (t - (dur - r))) / r) : 1;
  return att * body * rel;
}

/** Add a pitched note. freq: Hz | [from, to] (exponential glide) | (t, dur) => Hz. */
function tone(buf, t0, dur, freq, o = {}) {
  const { partials = SINE, amp = 1, vib, trem, a = 0.01, d = 0.1, s = 0.7, r = 0.05 } = o;
  const n = Math.floor(dur * SR);
  const i0 = Math.floor(t0 * SR);
  const ph = partials.map(() => 0);
  for (let i = 0; i < n && i0 + i < buf.length; i++) {
    const t = i / SR;
    let f;
    if (typeof freq === "function") f = freq(t, dur);
    else if (Array.isArray(freq)) f = freq[0] * (freq[1] / freq[0]) ** clamp(t / dur);
    else f = freq;
    if (vib) {
      const ramp = clamp((t - (vib.delay ?? 0)) / 0.15);
      f *= semis(vib.depth * ramp * Math.sin(TAU * vib.rate * t));
    }
    let y = 0;
    for (let k = 0; k < partials.length; k++) {
      ph[k] += (TAU * f * partials[k][0]) / SR;
      y += partials[k][1] * Math.sin(ph[k]);
    }
    let g = env(t, dur, { a, d, s, r }) * amp;
    if (trem) g *= 1 - trem.depth * (0.5 - 0.5 * Math.cos(TAU * trem.rate * t));
    buf[i0 + i] += y * g;
  }
}

/** Add band-passed noise (airy breath / click). freq: Hz | [from, to]. Seeded. */
function breath(buf, t0, dur, freq, o = {}) {
  const { q = 1.5, amp = 1, seed = 1, a = 0.02, d = 0.2, s = 0.6, r = 0.05 } = o;
  const rand = rng(seed);
  const n = Math.floor(dur * SR);
  const i0 = Math.floor(t0 * SR);
  let low = 0, band = 0;
  for (let i = 0; i < n && i0 + i < buf.length; i++) {
    const t = i / SR;
    const f = Array.isArray(freq) ? freq[0] * (freq[1] / freq[0]) ** clamp(t / dur) : freq;
    const k = 2 * Math.sin((Math.PI * f) / SR); // state-variable filter
    const x = rand() * 2 - 1;
    const high = x - low - band / q;
    band += k * high;
    low += k * band;
    buf[i0 + i] += band * env(t, dur, { a, d, s, r }) * amp;
  }
}

/** Recursive echo — turns chimes into little ghostly tails. */
function echo(buf, delay, fb) {
  const d = Math.floor(delay * SR);
  for (let i = d; i < buf.length; i++) buf[i] += fb * buf[i - d];
}

// ── The 28 sounds ─────────────────────────────────────────────────────────────
// Each: { len (s), level (peak, linear), build(buf) }. Notes named by MIDI-ish Hz.
const N = { // equal-tempered Hz
  G3: 196, C4: 261.63, D4: 293.66, E4: 329.63, G4: 392, A4: 440, B4: 493.88,
  C5: 523.25, D5: 587.33, E5: 659.25, F5: 698.46, G5: 783.99, A5: 880, B5: 987.77,
  C6: 1046.5, E6: 1318.5, G6: 1568,
};
const OOO = { rate: 5.5, depth: 0.25, delay: 0.08 }; // the airy "oooo" wobble

const SOUNDS = {
  // ── UI blips ──
  hover: { len: 0.08, level: 0.3, build: (b) => tone(b, 0, 0.08, [880, 1040], { a: 0.01, d: 0.03, s: 0.2, r: 0.04 }) },
  tick: { len: 0.06, level: 0.3, build: (b) => tone(b, 0, 0.06, 1500, { partials: TRI, a: 0.006, d: 0.012, s: 0, r: 0.03 }) },
  blip: { len: 0.1, level: 0.4, build: (b) => tone(b, 0, 0.1, [660, 990], { a: 0.008, d: 0.04, s: 0.2, r: 0.05 }) },
  pop: {
    len: 0.14, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.12, [320, 1100], { a: 0.006, d: 0.03, s: 0, r: 0.06 });
      breath(b, 0, 0.03, 2500, { amp: 0.25, seed: 3, a: 0.006, d: 0.01, s: 0, r: 0.015 });
    },
  },
  open: { // soft rising "whup-up"
    len: 0.3, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.15, [N.C5, N.E5], { partials: GHOST, a: 0.012, d: 0.1, s: 0.3, r: 0.07 });
      tone(b, 0.1, 0.19, [N.E5, N.A5], { partials: GHOST, a: 0.012, d: 0.12, s: 0.3, r: 0.1 });
    },
  },
  close: { // the same, folding back down
    len: 0.3, level: 0.4,
    build: (b) => {
      tone(b, 0, 0.15, [N.A5, N.E5], { partials: GHOST, a: 0.012, d: 0.1, s: 0.3, r: 0.07 });
      tone(b, 0.1, 0.19, [N.E5, N.C5], { partials: GHOST, a: 0.012, d: 0.12, s: 0.3, r: 0.1 });
    },
  },
  peek: { // "oo?" — Boo pokes its head out
    len: 0.36, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.34, [N.G4, N.C5], { partials: GHOST, vib: { rate: 6, depth: 0.3, delay: 0.05 }, a: 0.05, d: 0.2, s: 0.5, r: 0.12 });
      breath(b, 0, 0.3, 900, { amp: 0.04, seed: 5, a: 0.06, d: 0.2, s: 0.4, r: 0.1 });
    },
  },

  // ── Clicks on the ghost ──
  slap: { // "oof!" — tap, then a startled falling "ooh"
    len: 0.3, level: 0.5,
    build: (b) => {
      breath(b, 0, 0.05, 1400, { q: 0.8, amp: 0.9, seed: 7, a: 0.006, d: 0.012, s: 0, r: 0.025 });
      tone(b, 0.01, 0.28, [820, 300], { partials: GHOST, vib: { rate: 14, depth: 0.5 }, a: 0.008, d: 0.12, s: 0.15, r: 0.1 });
    },
  },
  annoyed: { // "hmph, hmph" — two tired low grumbles
    len: 0.5, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.2, [N.E4, N.D4], { partials: TRI, a: 0.02, d: 0.1, s: 0.4, r: 0.08 });
      tone(b, 0.24, 0.24, [N.D4, 247], { partials: TRI, a: 0.02, d: 0.12, s: 0.4, r: 0.1 });
      breath(b, 0, 0.45, 700, { amp: 0.05, seed: 11, a: 0.03, d: 0.3, s: 0.3, r: 0.1 });
    },
  },
  dizzy: { // wobbly spiral sinking down, wobble speeding then slowing
    len: 1.0, level: 0.5,
    build: (b) => {
      let phase = 0; // wobble phase accumulated so the rate can change smoothly
      const wob = (t, dur) => {
        const u = t / dur;
        const rate = 3 + 9 * Math.sin(Math.PI * u); // 3 → 12 → 3 Hz
        phase += (TAU * rate) / SR;
        const centre = 620 * (330 / 620) ** u; // slow downward spiral
        return centre * semis(3.2 * Math.sin(phase));
      };
      tone(b, 0, 1.0, wob, { partials: GHOST, trem: { rate: 6, depth: 0.25 }, a: 0.05, d: 0.5, s: 0.7, r: 0.25 });
    },
  },
  love: { // warm "ooh~" with two little sparkles
    len: 0.8, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.55, [N.E5, N.A5], { partials: GHOST, vib: { rate: 6, depth: 0.35, delay: 0.1 }, a: 0.05, d: 0.3, s: 0.6, r: 0.2 });
      tone(b, 0.32, 0.18, N.E6, { partials: BELL, amp: 0.3, a: 0.008, d: 0.05, s: 0, r: 0.08 });
      tone(b, 0.44, 0.22, N.G6, { partials: BELL, amp: 0.25, a: 0.008, d: 0.06, s: 0, r: 0.1 });
      echo(b, 0.11, 0.3);
    },
  },
  greet: { // "hel-looo" on launch: a small hop then a held, wobbling note
    len: 0.85, level: 0.5,
    build: (b) => {
      tone(b, 0, 0.17, [N.D5, N.G5], { partials: GHOST, a: 0.015, d: 0.1, s: 0.5, r: 0.06 });
      tone(b, 0.15, 0.55, [N.G5, N.E5], { partials: GHOST, vib: OOO, a: 0.03, d: 0.3, s: 0.6, r: 0.22 });
      tone(b, 0.5, 0.2, N.C6, { partials: BELL, amp: 0.2, a: 0.008, d: 0.06, s: 0, r: 0.1 });
      echo(b, 0.12, 0.28);
    },
  },
  wink: { // quick sparkly "ting"
    len: 0.18, level: 0.4,
    build: (b) => {
      tone(b, 0, 0.1, [1200, 1800], { a: 0.006, d: 0.03, s: 0, r: 0.05 });
      tone(b, 0.04, 0.13, 2400, { partials: BELL, amp: 0.35, a: 0.006, d: 0.04, s: 0, r: 0.07 });
    },
  },
  proud: { // "ta-daa": a step up, then a bright held note with shimmer
    len: 0.9, level: 0.5,
    build: (b) => {
      tone(b, 0, 0.16, N.C5, { partials: TRI, a: 0.012, d: 0.08, s: 0.4, r: 0.06 });
      tone(b, 0.14, 0.62, N.G5, { partials: GHOST, vib: { rate: 5.5, depth: 0.2, delay: 0.15 }, a: 0.02, d: 0.35, s: 0.55, r: 0.25 });
      tone(b, 0.18, 0.5, N.G6, { partials: BELL, amp: 0.18, a: 0.01, d: 0.2, s: 0, r: 0.2 });
      echo(b, 0.13, 0.3);
    },
  },
  yawn: { // rise, then a long breathy fall
    len: 0.8, level: 0.45,
    build: (b) => {
      const f = (t, dur) => { const u = t / dur; return u < 0.3 ? 330 * (560 / 330) ** (u / 0.3) : 560 * (190 / 560) ** ((u - 0.3) / 0.7); };
      tone(b, 0, 0.75, f, { partials: GHOST, vib: { rate: 4, depth: 0.3, delay: 0.1 }, a: 0.12, d: 0.3, s: 0.6, r: 0.25 });
      breath(b, 0, 0.75, [700, 400], { q: 1, amp: 0.12, seed: 13, a: 0.15, d: 0.3, s: 0.6, r: 0.25 });
    },
  },

  // ── Agent states ──
  work: { // two small steady taps: "bip-bup"
    len: 0.3, level: 0.4,
    build: (b) => {
      tone(b, 0, 0.1, N.A4, { partials: TRI, a: 0.008, d: 0.04, s: 0.2, r: 0.05 });
      tone(b, 0.13, 0.14, N.C5, { partials: TRI, a: 0.008, d: 0.05, s: 0.2, r: 0.07 });
    },
  },
  think: { // "hmmm…" a slow wobbling hum
    len: 0.5, level: 0.4,
    build: (b) => {
      tone(b, 0, 0.48, [N.D4 * 1.1, N.D4 * 1.25], { partials: GHOST, vib: { rate: 4.5, depth: 0.4, delay: 0.05 }, a: 0.08, d: 0.3, s: 0.6, r: 0.18 });
    },
  },
  search: { // sonar sweep: ping up, ping back
    len: 0.45, level: 0.4,
    build: (b) => {
      tone(b, 0, 0.18, [600, 1000], { partials: GHOST, a: 0.01, d: 0.08, s: 0.3, r: 0.08 });
      tone(b, 0.2, 0.22, [1000, 720], { partials: GHOST, amp: 0.7, a: 0.01, d: 0.1, s: 0.3, r: 0.1 });
    },
  },
  finish: { // happy rising arpeggio, last note blooms
    len: 1.2, level: 0.5,
    build: (b) => {
      [N.C5, N.E5, N.G5, N.C6].forEach((f, i) =>
        tone(b, i * 0.1, 0.3, f, { partials: BELL, amp: 0.75, a: 0.008, d: 0.12, s: 0.05, r: 0.15 }));
      tone(b, 0.4, 0.6, N.E6, { partials: GHOST, vib: { rate: 5.5, depth: 0.2, delay: 0.15 }, a: 0.01, d: 0.3, s: 0.45, r: 0.3 });
      echo(b, 0.14, 0.32);
    },
  },
  error: { // low soft "bonk-bonk", falling
    len: 0.55, level: 0.5,
    build: (b) => {
      tone(b, 0, 0.22, [N.G3, 128], { partials: TRI, a: 0.008, d: 0.07, s: 0.1, r: 0.12 });
      tone(b, 0.2, 0.33, [165, 90], { partials: TRI, a: 0.008, d: 0.1, s: 0.1, r: 0.18 });
      breath(b, 0, 0.03, 500, { amp: 0.25, seed: 17, a: 0.006, d: 0.01, s: 0, r: 0.015 });
    },
  },
  approval: { // gentle two-note bell chime: "something needs you"
    len: 0.95, level: 0.5,
    build: (b) => {
      tone(b, 0, 0.5, N.E5, { partials: BELL, a: 0.01, d: 0.22, s: 0, r: 0.2 });
      tone(b, 0.17, 0.7, N.A5, { partials: BELL, a: 0.01, d: 0.3, s: 0, r: 0.3 });
      echo(b, 0.16, 0.3);
    },
  },
  question: { // a voiced "oo-OO?" with a lift at the end
    len: 0.75, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.26, N.C5, { partials: GHOST, a: 0.02, d: 0.12, s: 0.5, r: 0.09 });
      tone(b, 0.24, 0.42, [N.F5, N.A5], { partials: GHOST, vib: { rate: 6, depth: 0.2, delay: 0.1 }, a: 0.02, d: 0.25, s: 0.5, r: 0.2 });
      echo(b, 0.14, 0.25);
    },
  },
  approve: { // bright little confirm "ding-ding"
    len: 0.32, level: 0.45,
    build: (b) => {
      tone(b, 0, 0.16, N.E5, { partials: BELL, a: 0.006, d: 0.06, s: 0, r: 0.08 });
      tone(b, 0.1, 0.22, N.B5, { partials: BELL, a: 0.006, d: 0.08, s: 0, r: 0.12 });
    },
  },
  rate: { // rate limit: three tired, slowing pulses
    len: 0.7, level: 0.4,
    build: (b) => {
      [[0, N.D4], [0.2, 277], [0.42, 247]].forEach(([t, f], i) =>
        tone(b, t, 0.22, [f, f * 0.93], { partials: TRI, amp: 1 - i * 0.12, a: 0.02, d: 0.1, s: 0.3, r: 0.1, trem: { rate: 9, depth: 0.35 } }));
    },
  },
  sleep: { // two-note lullaby sinking into breath
    len: 0.95, level: 0.4,
    build: (b) => {
      tone(b, 0, 0.5, [N.E4, N.D4], { partials: GHOST, vib: { rate: 3.5, depth: 0.25, delay: 0.1 }, a: 0.1, d: 0.3, s: 0.6, r: 0.25 });
      tone(b, 0.38, 0.55, [N.D4, 220], { partials: GHOST, amp: 0.8, vib: { rate: 3.5, depth: 0.25, delay: 0.1 }, a: 0.08, d: 0.3, s: 0.5, r: 0.35 });
      breath(b, 0, 0.9, [500, 300], { q: 1, amp: 0.1, seed: 19, a: 0.2, d: 0.4, s: 0.5, r: 0.35 });
    },
  },

  // ── Actions ──
  gulp: { // two liquid "glug"s and a satisfied little up-note
    len: 0.6, level: 0.5,
    build: (b) => {
      const glug = { vib: { rate: 32, depth: 1.2 }, a: 0.01, d: 0.05, s: 0.2, r: 0.04 };
      tone(b, 0, 0.14, [430, 170], { partials: GHOST, ...glug });
      tone(b, 0.17, 0.14, [380, 150], { partials: GHOST, ...glug });
      tone(b, 0.36, 0.16, [200, 340], { partials: GHOST, amp: 0.6, a: 0.015, d: 0.06, s: 0.2, r: 0.07 });
    },
  },
  attach: { // paperclip: tiny click, then a clipped-on rising ping
    len: 0.5, level: 0.45,
    build: (b) => {
      breath(b, 0, 0.025, 3200, { q: 2, amp: 0.6, seed: 23, a: 0.006, d: 0.008, s: 0, r: 0.012 });
      tone(b, 0.06, 0.12, [700, 1050], { partials: TRI, a: 0.008, d: 0.04, s: 0.2, r: 0.06 });
      tone(b, 0.17, 0.3, N.C6, { partials: BELL, amp: 0.7, a: 0.008, d: 0.12, s: 0, r: 0.15 });
      echo(b, 0.09, 0.25);
    },
  },
  send: { // soft whoosh up and away, with a landing ping
    len: 0.45, level: 0.45,
    build: (b) => {
      breath(b, 0, 0.26, [500, 3200], { q: 1.2, amp: 0.5, seed: 29, a: 0.08, d: 0.15, s: 0.4, r: 0.1 });
      tone(b, 0, 0.26, [500, 1250], { amp: 0.4, a: 0.06, d: 0.12, s: 0.4, r: 0.1 });
      tone(b, 0.22, 0.22, N.G6, { partials: BELL, amp: 0.45, a: 0.008, d: 0.07, s: 0, r: 0.1 });
    },
  },
};

// ── WAV writing ───────────────────────────────────────────────────────────────

const FADE = Math.round(0.008 * SR); // 8 ms in and out: no clicks, ends exactly at 0

function finalize(buf, level) {
  let peak = 0;
  for (const v of buf) peak = Math.max(peak, Math.abs(v));
  const g = peak > 0 ? level / peak : 0;
  for (let i = 0; i < buf.length; i++) buf[i] *= g;
  for (let i = 0; i < FADE; i++) { // cosine fades
    const w = 0.5 - 0.5 * Math.cos((Math.PI * i) / FADE);
    buf[i] *= w;
    buf[buf.length - 1 - i] *= w;
  }
  buf[0] = 0;
  buf[buf.length - 1] = 0;
}

function wav(buf) {
  const data = Buffer.alloc(buf.length * 2);
  for (let i = 0; i < buf.length; i++) data.writeInt16LE(Math.round(clamp(buf[i], -1, 1) * 32767), i * 2);
  const h = Buffer.alloc(44);
  h.write("RIFF", 0); h.writeUInt32LE(36 + data.length, 4); h.write("WAVE", 8);
  h.write("fmt ", 12); h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(1, 22);
  h.writeUInt32LE(SR, 24); h.writeUInt32LE(SR * 2, 28); h.writeUInt16LE(2, 32); h.writeUInt16LE(16, 34);
  h.write("data", 36); h.writeUInt32LE(data.length, 40);
  return Buffer.concat([h, data]);
}

mkdirSync(OUT, { recursive: true });
for (const [name, { len, level, build }] of Object.entries(SOUNDS)) {
  const buf = new Float32Array(Math.round(len * SR));
  build(buf);
  finalize(buf, level);
  writeFileSync(join(OUT, `${name}.wav`), wav(buf));
}
console.log(`wrote ${Object.keys(SOUNDS).length} sounds to ${OUT}`);
