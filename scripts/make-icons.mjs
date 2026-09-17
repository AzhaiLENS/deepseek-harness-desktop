#!/usr/bin/env node
/**
 * Generate the application icons from code — no image dependency, no binary
 * blobs in the repository.
 *
 * Produces (under `src-tauri/icons/`):
 *   - PNGs at every size Tauri's bundler asks for
 *   - `icon.ico` for the Windows bundle
 *   - `icon.icns` for the macOS bundle (written only when `iconutil` exists,
 *     i.e. on macOS; the CI macOS leg generates it, the other legs don't care)
 *
 * Usage: node scripts/make-icons.mjs
 */

import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const OUT = join(ROOT, "src-tauri", "icons");
mkdirSync(OUT, { recursive: true });

// ------------------------------------------------------------------ drawing --
const BRAND = [77, 107, 254]; // #4d6bfe — the DeepSeek blue

/**
 * Draw the icon at `size`×`size` and return RGBA pixels.
 *
 * A rounded square in the brand colour with a white "harness" mark: a rounded
 * ring with three spokes, readable even at 32 px.
 */
function draw(size) {
  const px = Buffer.alloc(size * size * 4);
  const radius = size * 0.22;
  const inset = size * 0.06;

  const put = (x, y, [r, g, b], a = 255) => {
    if (x < 0 || y < 0 || x >= size || y >= size) return;
    const i = (y * size + x) * 4;
    const src = a / 255;
    const dst = px[i + 3] / 255;
    const out = src + dst * (1 - src);
    if (out === 0) return;
    px[i] = Math.round((r * src + px[i] * dst * (1 - src)) / out);
    px[i + 1] = Math.round((g * src + px[i + 1] * dst * (1 - src)) / out);
    px[i + 2] = Math.round((b * src + px[i + 2] * dst * (1 - src)) / out);
    px[i + 3] = Math.round(out * 255);
  };

  // Supersample 3× for smooth edges without a drawing library.
  const SS = 3;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let accA = 0;
      let accR = 0;
      let accG = 0;
      let accB = 0;

      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const fx = x + (sx + 0.5) / SS;
          const fy = y + (sy + 0.5) / SS;
          const n = size;

          // rounded-square mask
          const dx = Math.max(inset + radius - fx, fx - (n - inset - radius), 0);
          const dy = Math.max(inset + radius - fy, fy - (n - inset - radius), 0);
          const inside = Math.hypot(dx, dy) <= radius;

          if (!inside) continue;

          // white harness mark: ring + three spokes
          const cx = n / 2;
          const cy = n / 2;
          const dist = Math.hypot(fx - cx, fy - cy);
          const ringR = n * 0.235;
          const ringW = n * 0.05;
          const onRing = Math.abs(dist - ringR) <= ringW / 2;

          const angle = Math.atan2(fy - cy, fx - cx);
          const spokeW = n * 0.028;
          const onSpoke =
            dist > ringR - ringW && dist <= n * 0.4 && [0, 2, 4].some((k) => {
              const a = (k * Math.PI) / 3 - Math.PI / 2;
              const da = Math.abs(((angle - a + Math.PI * 3) % (Math.PI * 2)) - Math.PI);
              return da * dist <= spokeW;
            });

          const mark = onRing || onSpoke;
          accA += 255;
          if (mark) {
            accR += 255;
            accG += 255;
            accB += 255;
          } else {
            accR += BRAND[0];
            accG += BRAND[1];
            accB += BRAND[2];
          }
        }
      }

      const samples = SS * SS;
      if (accA === 0) continue;
      put(x, y, [accR / samples, accG / samples, accB / samples], (accA / samples) * (size >= 128 ? 1 : 1));
    }
  }
  return px;
}

// -------------------------------------------------------------- PNG writer ---
function crc32(buf) {
  let c;
  const table = [];
  for (let n = 0; n < 256; n++) {
    c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let crc = 0xffffffff;
  for (const byte of buf) crc = table[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

function png(size, pixels) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: none
    pixels.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// --------------------------------------------------------------- ICO writer --
function ico(images) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(images.length, 4);

  let offset = 6 + images.length * 16;
  const entries = [];
  for (const { size, data } of images) {
    const entry = Buffer.alloc(16);
    entry[0] = size >= 256 ? 0 : size;
    entry[1] = size >= 256 ? 0 : size;
    entry[2] = 0;
    entry[3] = 0;
    entry.writeUInt16LE(1, 4);
    entry.writeUInt16LE(32, 6);
    entry.writeUInt32LE(data.length, 8);
    entry.writeUInt32LE(offset, 12);
    entries.push(entry);
    offset += data.length;
  }
  return Buffer.concat([header, ...entries, ...images.map((i) => i.data)]);
}

// ------------------------------------------------------------------- run -----
const SIZES = [16, 24, 32, 48, 64, 128, 256, 512, 1024];
const rendered = new Map();
for (const size of SIZES) rendered.set(size, draw(size));

const named = {
  "32x32.png": 32,
  "128x128.png": 128,
  "128x128@2x.png": 256,
  "icon.png": 512,
  "Square30x30Logo.png": 32,
  "Square44x44Logo.png": 44,
  "Square71x71Logo.png": 71,
  "Square89x89Logo.png": 89,
  "Square107x107Logo.png": 107,
  "Square142x142Logo.png": 142,
  "Square150x150Logo.png": 150,
  "Square284x284Logo.png": 284,
  "Square310x310Logo.png": 310,
  "StoreLogo.png": 50,
};

for (const [file, size] of Object.entries(named)) {
  const px = rendered.get(size) ?? draw(size);
  rendered.set(size, px);
  writeFileSync(join(OUT, file), png(size, px));
}

writeFileSync(
  join(OUT, "icon.ico"),
  ico([16, 24, 32, 48, 64, 128, 256].map((size) => ({
    size,
    data: png(size, rendered.get(size) ?? draw(size)),
  }))),
);

// icon.icns is produced with Apple's own tool so the bundle stays valid.
const iconset = join(OUT, "icon.iconset");
if (process.platform === "darwin") {
  try {
    if (existsSync(iconset)) rmSync(iconset, { recursive: true, force: true });
    mkdirSync(iconset, { recursive: true });
    const pairs = [
      [16, "icon_16x16.png"],
      [32, "icon_16x16@2x.png"],
      [32, "icon_32x32.png"],
      [64, "icon_32x32@2x.png"],
      [128, "icon_128x128.png"],
      [256, "icon_128x128@2x.png"],
      [256, "icon_256x256.png"],
      [512, "icon_256x256@2x.png"],
      [512, "icon_512x512.png"],
      [1024, "icon_512x512@2x.png"],
    ];
    for (const [size, file] of pairs) {
      writeFileSync(join(iconset, file), png(size, rendered.get(size) ?? draw(size)));
    }
    execFileSync("iconutil", ["-c", "icns", iconset, "-o", join(OUT, "icon.icns")]);
    rmSync(iconset, { recursive: true, force: true });
    console.log("icons: wrote icon.icns via iconutil");
  } catch (error) {
    console.warn(`icons: iconutil failed (${error.message}); macOS bundle will fall back to PNG icons`);
  }
} else {
  console.log("icons: skipping icon.icns (only generated on macOS)");
}

console.log(`icons: wrote ${Object.keys(named).length} PNGs + icon.ico into ${OUT}`);
