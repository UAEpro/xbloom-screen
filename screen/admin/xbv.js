// In-browser screensaver converter.
//
// Turns any video the browser can play (MP4, WebM, MOV…) into the device's XBV1 format: a
// header + frame index + JPEG frames (Motion-JPEG). Converting here, on the phone or laptop,
// means the device never needs ffmpeg — and the same tiny JPEG decoder plays it on a
// Raspberry Pi and on an ESP32-S3. Photos are resized to the screen the same way.
//
// XBV1 layout (little-endian): "XBV1" | u16 width | u16 height | u16 fps×100 | u16 flags |
// u32 frames | u32 reserved | frames × {u32 offset, u32 length} | JPEG frames.

const once = (el, ev) => new Promise((resolve, reject) => {
  const ok = () => { cleanup(); resolve(); };
  const bad = () => { cleanup(); reject(new Error("this video can't be decoded by the browser")); };
  const cleanup = () => { el.removeEventListener(ev, ok); el.removeEventListener("error", bad); };
  el.addEventListener(ev, ok, { once: true });
  el.addEventListener("error", bad, { once: true });
});

/** Draw `src` into the canvas with "cover" scaling (fill, crop the overflow, centred). */
export function drawCover(ctx, src, w, h) {
  const sw = src.videoWidth || src.width;
  const sh = src.videoHeight || src.height;
  const scale = Math.max(w / sw, h / sh);
  const dw = sw * scale;
  const dh = sh * scale;
  ctx.drawImage(src, (w - dw) / 2, (h - dh) / 2, dw, dh);
}

const toJpeg = (canvas, quality) =>
  new Promise((resolve, reject) => canvas.toBlob((b) => (b ? resolve(b) : reject(new Error("JPEG encoding failed"))), "image/jpeg", quality));

/** Build an XBV1 Blob from JPEG frame Blobs. */
export function buildXbv(width, height, fps, frames) {
  const n = frames.length;
  const head = new ArrayBuffer(20 + n * 8);
  const dv = new DataView(head);
  [0x58, 0x42, 0x56, 0x31].forEach((b, i) => dv.setUint8(i, b));
  dv.setUint16(4, width, true);
  dv.setUint16(6, height, true);
  dv.setUint16(8, Math.round(fps * 100), true);
  dv.setUint16(10, 1, true); // loop
  dv.setUint32(12, n, true);
  dv.setUint32(16, 0, true);
  let offset = head.byteLength;
  frames.forEach((f, i) => {
    dv.setUint32(20 + i * 8, offset, true);
    dv.setUint32(24 + i * 8, f.size, true);
    offset += f.size;
  });
  return new Blob([head, ...frames], { type: "application/octet-stream" });
}

/** Load a video file and report its duration / size (for the trim controls). */
export async function probeVideo(file) {
  const video = document.createElement("video");
  video.muted = true;
  video.playsInline = true;
  video.preload = "auto";
  video.src = URL.createObjectURL(file);
  await once(video, "loadedmetadata");
  return { video, duration: video.duration, width: video.videoWidth, height: video.videoHeight };
}

/**
 * Convert `video` (from probeVideo) into an XBV1 Blob.
 *
 * profile: { width, height, fps, quality } from the device. Frames are sampled by seeking, so
 * the result is exact regardless of the device's speed. With `smoothLoop`, the last half second
 * cross-fades into the first frame so the loop has no visible jump.
 */
export async function convertVideo(video, profile, { start = 0, seconds = 10, smoothLoop = true, onProgress, signal } = {}) {
  const { width: w, height: h, fps, quality } = profile;
  const length = Math.max(0.5, Math.min(seconds, video.duration - start));
  const count = Math.max(1, Math.round(length * fps));
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d");
  ctx.imageSmoothingQuality = "high";
  const blend = smoothLoop ? Math.min(Math.round(fps / 2), Math.floor(count / 4)) : 0;
  let first = null;
  const frames = [];
  for (let i = 0; i < count; i++) {
    if (signal?.aborted) throw new DOMException("cancelled", "AbortError");
    const t = start + i / fps;
    if (Math.abs(video.currentTime - t) > 1e-3) {
      video.currentTime = Math.min(t, video.duration - 0.001);
      await once(video, "seeked");
    }
    drawCover(ctx, video, w, h);
    if (i === 0 && blend > 0) first = await createImageBitmap(canvas);
    const k = i - (count - blend);
    if (first && k >= 0) {
      ctx.globalAlpha = (k + 1) / (blend + 1);
      ctx.drawImage(first, 0, 0);
      ctx.globalAlpha = 1;
    }
    frames.push(await toJpeg(canvas, quality));
    onProgress?.(i + 1, count);
  }
  return buildXbv(w, h, fps, frames);
}

/** Resize a photo to the screen ("cover") and encode it as JPEG. */
export async function convertPhoto(file, profile) {
  const bitmap = await createImageBitmap(file);
  const canvas = document.createElement("canvas");
  canvas.width = profile.width;
  canvas.height = profile.height;
  const ctx = canvas.getContext("2d");
  ctx.imageSmoothingQuality = "high";
  drawCover(ctx, bitmap, profile.width, profile.height);
  return toJpeg(canvas, Math.min(0.9, profile.quality + 0.08));
}
