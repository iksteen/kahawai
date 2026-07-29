#version 100
// HUB-15a: HDR10 (PQ/BT.2020) → SDR (BT.709) tone map, run by glshader
// in the video encode chain. Chosen after the alternatives failed on
// real pixels: vapostproc's hdr-tone-mapping silently no-ops below
// TGL (Intel media-driver feature matrix: "HDR10 TM" is TGL+ only),
// and videoconvert's gamma-mode=remap linearizes PQ into absolute
// nits with no tone operator — output crushed to near-black.
#ifdef GL_ES
precision highp float;
#endif
varying vec2 v_texcoord;
uniform sampler2D tex;
// Scene peak (HUB-15a dynamic adaptation): the worker's luma probe
// tracks a smoothed p99.9 scene peak and feeds these three via the
// glshader uniforms property (uMaxE = PQ(peak), uMaxTgt = PQ(203)/
// uMaxE, uKS = 1.5*uMaxTgt - 0.5). Unset uniforms read 0.0 — the
// <= 0.0 guard falls back to the static 1000-nit constants so the
// shader stands alone (dry-run, still harnesses).
uniform float uMaxE;
uniform float uMaxTgt;
uniform float uKS;

// SMPTE ST 2084 (PQ) EOTF constants.
const float m1 = 0.1593017578125;
const float m2 = 78.84375;
const float c1 = 0.8359375;
const float c2 = 18.8515625;
const float c3 = 18.6875;

float pq_eotf(float ep) {
  float p = pow(max(ep, 0.0), 1.0 / m2);
  return pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0 / m1); // 1.0 = 10000 nits
}

float pq_encode(float y) { // inverse: linear (1.0 = 10000 nits) -> PQ code
  float p = pow(max(y, 0.0), m1);
  return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

// BT.2390 EETF in the PQ domain, 1000-nit source → 203-nit SDR white:
// IDENTITY below the knee (the whole midrange keeps its intended
// brightness — the extended-Reinhard curve this replaces compressed
// everything, landing reference white at 52% and reading flat next to
// libplacebo), Hermite rolloff above it. Constants precomputed:
// PQ(1000)=0.751827096, PQ(203)=0.580688881, maxTgt=tgtE/maxE,
// KS=1.5*maxTgt-0.5.
const float maxE = 0.751827096;
const float maxTgt = 0.772370248;
const float KS = 0.658555373;

float eetf(float e, float ks, float mt) { // e = pixel PQ code / maxE
  // Peak at or below the target: nothing to compress — and KS→1
  // makes the Hermite divisor vanish (NaN → clamp → BLACK blobs on
  // speculars, owner-observed on near-SDR scenes).
  if (mt >= 0.999) return min(e, 1.0);
  if (e <= ks) return e;
  // t clamped: pixels above the tracked p99.9 peak (the top 0.1% of
  // every frame) land at the rolloff END — white, like libplacebo —
  // instead of extrapolating the cubic off a cliff.
  float t = clamp((e - ks) / (1.0 - ks), 0.0, 1.0);
  float t2 = t * t;
  float t3 = t2 * t;
  return (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks)
       + (-2.0 * t3 + 3.0 * t2) * mt;
}

// Display mapping refit 2026-07-29 (late) against what mpv actually
// DISPLAYS: vo=gpu/libplacebo window captures via IPC
// screenshot-to-file. Every earlier fit (W=100-112) chased mpv's
// vo=image screenshots, which run zimg's SOFTWARE tone mapper — a
// different, brighter renderer (owner caught it live: "way brighter
// than mpv" while the stream matched the zimg refs). Against the
// real libplacebo: W=200 (the textbook BT.2408-ish 203-nit white),
// knee 0.80, gamma 2.19 — joint-fit loss 0.0025, per-title
// percentile RMS <= 0.006 on 9/10 matrix titles. The scene-adaptive
// peak (uniforms above) is what makes the textbook mapping land:
// without it, bright-scene highlights stall and the whole image
// needs fake exposure to compensate.
const float W_REL = 10000.0 / 200.0;
const float Z_MAX = 203.0 / 200.0;
const float KNEE = 0.80;
const float GAMMA = 2.19;

float shoulder(float z) {
  if (z <= KNEE) return z;
  float t = clamp((z - KNEE) / (Z_MAX - KNEE), 0.0, 1.0);
  float t2 = t * t;
  float t3 = t2 * t;
  return (2.0 * t3 - 3.0 * t2 + 1.0) * KNEE + (t3 - 2.0 * t2 + t) * (Z_MAX - KNEE)
       + (-2.0 * t3 + 3.0 * t2) * 1.0;
}

void main() {
  // Scene-adaptive peak when the probe feeds uniforms; static
  // 1000-nit fallback otherwise.
  float mE = (uMaxE > 0.0) ? uMaxE : maxE;
  float mT = (uMaxE > 0.0) ? uMaxTgt : maxTgt;
  float ks = (uMaxE > 0.0) ? uKS : KS;
  vec3 e = texture2D(tex, v_texcoord).rgb;
  vec3 lin = vec3(pq_eotf(e.r), pq_eotf(e.g), pq_eotf(e.b));
  // Hue-preserving: tone-map the maxRGB component through the EETF,
  // run the display shoulder on the same scalar, and scale the pixel
  // by the single combined ratio (per-channel application shifts hue
  // and saturation on brights — owner-visible vs libplacebo).
  float mx = max(lin.r, max(lin.g, lin.b));
  float mp = pq_eotf(clamp(eetf(pq_encode(mx) / mE, ks, mT), 0.0, 1.0) * mE);
  float z = mp * W_REL;
  float scale = shoulder(z) / max(z, 1e-9);
  vec3 y = lin * (mp / max(mx, 1e-9)) * W_REL * scale;
  // BT.2020 → BT.709 primaries, linear light.
  mat3 m = mat3(
     1.6605, -0.1246, -0.0182,
    -0.5876,  1.1329, -0.1006,
    -0.0728, -0.0083,  1.1187);
  vec3 r = m * y;
  // Gamut: desaturate toward luma just enough to fit — a hard clip
  // pins wide-gamut colors to the 709 boundary at MAXIMUM saturation
  // (neon reds on real footage, owner-observed); this keeps hue and
  // luminance and gives up only chroma.
  float Y = clamp(dot(r, vec3(0.2126, 0.7152, 0.0722)), 0.0, 1.0);
  // Only out-of-range channels constrain: r.c > 1 implies r.c - Y >=
  // 1 - Y >= 0, so each divisor is safe where its branch is taken.
  float f = 1.0;
  if (r.r > 1.0) f = min(f, (1.0 - Y) / (r.r - Y));
  if (r.g > 1.0) f = min(f, (1.0 - Y) / (r.g - Y));
  if (r.b > 1.0) f = min(f, (1.0 - Y) / (r.b - Y));
  if (r.r < 0.0) f = min(f, Y / (Y - r.r));
  if (r.g < 0.0) f = min(f, Y / (Y - r.g));
  if (r.b < 0.0) f = min(f, Y / (Y - r.b));
  vec3 fit = clamp(vec3(Y) + (r - vec3(Y)) * f, 0.0, 1.0);
  // Display-referred output (NOT the BT.709 camera OETF — encoding
  // with the OETF while displays decode at gamma nets ~1.2: darker,
  // harder, oversaturated); exponent from the libplacebo fit.
  gl_FragColor =
      vec4(pow(fit.r, 1.0 / GAMMA), pow(fit.g, 1.0 / GAMMA), pow(fit.b, 1.0 / GAMMA), 1.0);
}
