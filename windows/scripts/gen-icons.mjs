// Draws Boo into the PNG/ICO set Tauri needs. No dependencies: the icons are
// rasterised here and encoded with node:zlib, so the app icon stays "drawn in
// code" like the character itself.
//
//   node scripts/gen-icons.mjs

import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const OUT = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "icons");

// ── Boo ─────────────────────────────────────────────────────────────────────
// A small white ghost (dome head, straight sides, scalloped hem) on a rounded
// deep-purple tile. The tile keeps it legible at 16 px on both light and dark
// tray backgrounds.

const TILE_TOP = [91, 53, 147]; // #5B3593
const TILE_BOTTOM = [59, 31, 102]; // #3B1F66
const GHOST_TOP = [250, 250, 255];
const GHOST_BOTTOM = [225, 228, 245];
const INK = [42, 20, 80]; // #2A1450

const SS = 5; // supersampling factor

/** Ghost silhouette test in body-local coords (same shape as src/boo/ghost.ts). */
function insideGhost(x, y, rx, ry, scallops) {
  const dh = rx;
  const yc = -ry + dh;
  const amp = ry * 0.2;
  const hem = ry - amp;
  if (y < yc) {
    const a = x / rx;
    const b = (y - yc) / dh;
    return a * a + b * b <= 1;
  }
  const t = Math.min(1, (y - yc) / (hem - yc));
  if (Math.abs(x) > rx * (1 + 0.06 * t)) return false;
  const W = rx * 1.06;
  const k = Math.max(0, Math.min(scallops - 1e-4, ((x + W) / (2 * W)) * scallops));
  const f = k - Math.floor(k);
  return y <= hem + amp * Math.sqrt(Math.max(0, 1 - (2 * f - 1) ** 2));
}

function insideEllipse(x, y, cx, cy, ax, ay) {
  return ((x - cx) / ax) ** 2 + ((y - cy) / ay) ** 2 <= 1;
}

function insideRoundRect(x, y, size, r) {
  const cx = Math.max(r, Math.min(size - r, x));
  const cy = Math.max(r, Math.min(size - r, y));
  return (x - cx) ** 2 + (y - cy) ** 2 <= r * r;
}

const mix = (a, b, t) => a.map((v, i) => v + (b[i] - v) * t);

function renderBoo(size) {
  const px = new Uint8Array(size * size * 4);
  const rx = size * 0.29;
  const ry = size * 0.34;
  const cx = size / 2;
  const cy = size / 2 + size * 0.01;
  const scallops = size <= 32 ? 3 : 4;
  const tileR = size * 0.22;
  const eyeDx = rx * 0.4;
  const eyeY = ry * 0.08;
  const eyeAx = rx * 0.17;
  const eyeAy = ry * 0.22;
  const showMouth = size >= 48;

  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const fx = x + (sx + 0.5) / SS;
          const fy = y + (sy + 0.5) / SS;
          if (!insideRoundRect(fx, fy, size, tileR)) continue;
          let c = mix(TILE_TOP, TILE_BOTTOM, fy / size);
          const lx = fx - cx;
          const ly = fy - cy;
          if (insideGhost(lx, ly, rx, ry, scallops)) {
            c = mix(GHOST_TOP, GHOST_BOTTOM, (ly + ry) / (2 * ry));
            if (
              insideEllipse(lx, ly, -eyeDx, eyeY, eyeAx, eyeAy) ||
              insideEllipse(lx, ly, eyeDx, eyeY, eyeAx, eyeAy) ||
              (showMouth && insideEllipse(lx, ly, 0, eyeY + eyeAy * 1.9, eyeAx * 0.7, eyeAy * 0.45))
            ) {
              c = INK;
            }
          }
          r += c[0]; g += c[1]; b += c[2]; a += 1;
        }
      }
      if (a === 0) continue;
      const o = (y * size + x) * 4;
      px[o] = Math.round(r / a);
      px[o + 1] = Math.round(g / a);
      px[o + 2] = Math.round(b / a);
      px[o + 3] = Math.round((a / (SS * SS)) * 255);
    }
  }
  return px;
}

// ── PNG ───────────────────────────────────────────────────────────────────────

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function encodePNG(size, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: none
    Buffer.from(rgba.buffer, y * size * 4, size * 4).copy(raw, y * (size * 4 + 1) + 1);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// ── ICO (PNG-in-ICO, Vista and later) ─────────────────────────────────────────

function encodeICO(entries) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(entries.length, 4);
  const dir = Buffer.alloc(16 * entries.length);
  let offset = header.length + dir.length;
  entries.forEach((e, i) => {
    const o = i * 16;
    dir[o] = e.size >= 256 ? 0 : e.size;
    dir[o + 1] = e.size >= 256 ? 0 : e.size;
    dir[o + 2] = 0;
    dir[o + 3] = 0;
    dir.writeUInt16LE(1, o + 4);
    dir.writeUInt16LE(32, o + 6);
    dir.writeUInt32LE(e.png.length, o + 8);
    dir.writeUInt32LE(offset, o + 12);
    offset += e.png.length;
  });
  return Buffer.concat([header, dir, ...entries.map((e) => e.png)]);
}

// ── Go ────────────────────────────────────────────────────────────────────────

mkdirSync(OUT, { recursive: true });

const png = (size) => encodePNG(size, renderBoo(size));

const files = {
  "32x32.png": png(32),
  "128x128.png": png(128),
  "128x128@2x.png": png(256),
  "icon.png": png(512),
};
for (const [name, data] of Object.entries(files)) {
  writeFileSync(join(OUT, name), data);
  console.log(`${name} — ${data.length} bytes`);
}

const ico = encodeICO([16, 24, 32, 48, 64, 128, 256].map((size) => ({ size, png: png(size) })));
writeFileSync(join(OUT, "icon.ico"), ico);
console.log(`icon.ico — ${ico.length} bytes`);
