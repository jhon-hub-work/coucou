// Boo's ghost silhouette — round dome, straight-ish sides, wavy scalloped hem.
// Shared by the engine, the launch greeting and the drop canvas so they all draw
// the same character. Points are found by casting a ray from the body centre, so
// callers can sample it at the same angles as the mailbox morph (rrPoint) and
// lerp ghost → box point for point.

const clamp01 = (v: number) => Math.max(0, Math.min(1, v));

/** Hem wave phase speed (rad/s): lazy at rest, quicker while an agent works. */
export const HEM_SLOW = 2.2;
export const HEM_FAST = 5.5;

interface Ghost {
  rx: number; ry: number;
  yc: number; dh: number;
  hem: number; amp: number;
  n: number; ph: number;
}

function makeGhost(rx: number, ry: number, scallops: number, ph: number): Ghost {
  const dh = rx;                    // dome is a half-ellipse rx wide, dh tall
  const amp = ry * 0.2;             // scallop depth at rest
  return {
    rx, ry, dh, amp, n: scallops, ph,
    yc: -ry + dh,                   // dome centre
    hem: ry - amp * 1.3,            // hem baseline, leaves room for the wiggle
  };
}

/** Hem y at body-local x: a row of scallops whose heights breathe out of phase. */
function hemY(g: Ghost, x: number): number {
  const W = g.rx * 1.06;
  let u = clamp01((x + W) / (2 * W));
  u += 0.05 * Math.sin(2 * Math.PI * u) * Math.sin(g.ph * 0.7 + 1); // sway, ends stay put
  const k = Math.max(0, Math.min(g.n - 0.0001, u * g.n));
  const j = Math.floor(k);
  const f = k - j;
  const bump = Math.sqrt(Math.max(0, 1 - (2 * f - 1) ** 2)); // round scallop
  return g.hem + g.amp * (1 + 0.28 * Math.sin(g.ph + j * 2.1)) * bump;
}

function inside(g: Ghost, x: number, y: number): boolean {
  if (y < g.yc) {
    const a = x / g.rx;
    const b = (y - g.yc) / g.dh;
    return a * a + b * b <= 1;
  }
  const t = clamp01((y - g.yc) / (g.hem - g.yc));
  if (Math.abs(x) > g.rx * (1 + 0.06 * t)) return false; // skirt flares a touch
  return y <= hemY(g, x);
}

/**
 * Outline point along the ray (ca, sa) from the body centre.
 * `rx`/`ry` are the half width / half height, `ph` the hem wave phase.
 */
export function ghostPoint(
  ca: number, sa: number, rx: number, ry: number, scallops: number, ph: number,
): { x: number; y: number } {
  const g = makeGhost(rx, ry, scallops, ph);
  let lo = 0;
  let hi = ry * 1.6;
  for (let i = 0; i < 18; i++) {
    const mid = (lo + hi) / 2;
    if (inside(g, ca * mid, sa * mid)) lo = mid;
    else hi = mid;
  }
  return { x: ca * lo, y: sa * lo };
}

/** Closed ghost outline as a path (no morph). */
export function ghostPath(rx: number, ry: number, scallops: number, ph: number, steps = 120): Path2D {
  const p = new Path2D();
  for (let i = 0; i <= steps; i++) {
    const a = (i / steps) * Math.PI * 2;
    const pt = ghostPoint(Math.cos(a), Math.sin(a), rx, ry, scallops, ph);
    if (i === 0) p.moveTo(pt.x, pt.y);
    else p.lineTo(pt.x, pt.y);
  }
  p.closePath();
  return p;
}
