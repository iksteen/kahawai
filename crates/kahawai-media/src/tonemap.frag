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

float oetf709(float l) {
  return (l < 0.018) ? 4.5 * l : 1.0993 * pow(l, 0.45) - 0.0993;
}

void main() {
  vec3 e = texture2D(tex, v_texcoord).rgb;
  vec3 lin = vec3(pq_eotf(e.r), pq_eotf(e.g), pq_eotf(e.b)) * 10000.0;
  // Exposure: 1.0 at SDR reference white (BT.2408: 203 nits).
  vec3 x = lin / 203.0;
  // ponytail: fixed 1000-nit mastering peak; read the file's
  // mastering-display SEI and pass a uniform if 4000-nit masters
  // ever look flat. Per-channel Reinhard shifts hue on extreme
  // brights; switch to maxRGB scaling if that ever shows.
  float w = 1000.0 / 203.0;
  vec3 y = x * (1.0 + x / (w * w)) / (1.0 + x);
  // BT.2020 → BT.709 primaries, linear light, hard clip.
  mat3 m = mat3(
     1.6605, -0.1246, -0.0182,
    -0.5876,  1.1329, -0.1006,
    -0.0728, -0.0083,  1.1187);
  vec3 r709 = clamp(m * y, 0.0, 1.0);
  gl_FragColor = vec4(oetf709(r709.r), oetf709(r709.g), oetf709(r709.b), 1.0);
}
