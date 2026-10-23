// The soundtrack, synthesized from the same cue sheet as the picture (cues.js), so every hit lands
// on its cut. 120 BPM in A minor. Writes out/score.wav (48 kHz, 16-bit stereo).
const fs = require("fs");
const path = require("path");
// cues.js is a plain browser script (the page loads it too), so it is evaluated rather than required.
const Q = new Function(`${fs.readFileSync(path.join(__dirname, "cues.js"), "utf8")}; return CUES;`)();

const SR = 48000, DUR = Q.duration + 0.5, N = Math.ceil(SR * DUR);
const BEAT = 60 / Q.bpm, BAR = BEAT * 4;
const TAU = Math.PI * 2;
const mtof = (m) => 440 * Math.pow(2, (m - 69) / 12);
const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));

let seed = 12345;
const rand = () => { seed = (seed * 1664525 + 1013904223) >>> 0; return seed / 4294967296; };
const noise = () => rand() * 2 - 1;

function bus() { return { L: new Float32Array(N), R: new Float32Array(N) }; }
const B = { drums: bus(), bass: bus(), music: bus(), sfx: bus(), rev: bus(), dly: bus() };
function put(b, i, l, r = l) { if (i >= 0 && i < N) { b.L[i] += l; b.R[i] += r; } }
const pan = (p) => [Math.cos((p + 1) * Math.PI / 4), Math.sin((p + 1) * Math.PI / 4)];

// RBJ biquad, coefficients refreshed as the cutoff moves.
class Biquad {
  constructor(type) { this.type = type; this.x1 = this.x2 = this.y1 = this.y2 = 0; }
  set(f, q = 0.707) {
    f = clamp(f, 10, SR * 0.45);
    const w = TAU * f / SR, c = Math.cos(w), s = Math.sin(w), a = s / (2 * q);
    let b0, b1, b2;
    if (this.type === "lp") { b0 = (1 - c) / 2; b1 = 1 - c; b2 = b0; }
    else if (this.type === "hp") { b0 = (1 + c) / 2; b1 = -(1 + c); b2 = b0; }
    else { b0 = a; b1 = 0; b2 = -a; }
    const a0 = 1 + a;
    this.b0 = b0 / a0; this.b1 = b1 / a0; this.b2 = b2 / a0; this.a1 = (-2 * c) / a0; this.a2 = (1 - a) / a0;
    return this;
  }
  run(x) {
    const y = this.b0 * x + this.b1 * this.x1 + this.b2 * this.x2 - this.a1 * this.y1 - this.a2 * this.y2;
    this.x2 = this.x1; this.x1 = x; this.y2 = this.y1; this.y1 = y; return y;
  }
}
// Band-limited saw (polyBLEP).
function blep(t, dt) {
  if (t < dt) { t /= dt; return t + t - t * t - 1; }
  if (t > 1 - dt) { t = (t - 1) / dt; return t * t + t + t + 1; }
  return 0;
}
class Saw { constructor(ph = rand()) { this.ph = ph; } next(f) { const dt = f / SR; const v = 2 * this.ph - 1 - blep(this.ph, dt); this.ph += dt; if (this.ph >= 1) this.ph -= 1; return v; } }

/* ---------------- drums ---------------- */
function kick(t, g = 1, b = B.drums) {
  const i0 = Math.round(t * SR), n = Math.round(0.55 * SR); let ph = 0;
  for (let i = 0; i < n; i++) {
    const x = i / SR, f = 44 + 120 * Math.exp(-x * 32) + 30 * Math.exp(-x * 8);
    ph += TAU * f / SR;
    let s = Math.sin(ph) * Math.exp(-x * 6.5) + noise() * 0.25 * Math.exp(-x * 350);
    s = Math.tanh(s * 1.6) * g * 0.9;
    put(b, i0 + i, s);
  }
}
function clap(t, g = 1) {
  const i0 = Math.round(t * SR), n = Math.round(0.35 * SR), bp = new Biquad("bp").set(1400, 1.2), hp = new Biquad("hp").set(600);
  for (let i = 0; i < n; i++) {
    const x = i / SR;
    let env = Math.exp(-x * 14);
    for (const o of [0, 0.011, 0.022]) if (x >= o && x < o + 0.01) env = Math.max(env, Math.exp(-(x - o) * 200));
    const s = hp.run(bp.run(noise())) * env * g * 1.4;
    put(B.drums, i0 + i, s * 0.9, s); put(B.rev, i0 + i, s * 0.25);
  }
}
function hat(t, g = 1, open = false, p = 0.2) {
  const i0 = Math.round(t * SR), dec = open ? 14 : 45, n = Math.round((open ? 0.35 : 0.09) * SR);
  const hp = new Biquad("hp").set(7500, 0.9), [l, r] = pan(p);
  for (let i = 0; i < n; i++) { const s = hp.run(noise()) * Math.exp(-(i / SR) * dec) * g * 0.5; put(B.drums, i0 + i, s * l, s * r); }
}
function snare(t, g = 1) {
  const i0 = Math.round(t * SR), n = Math.round(0.3 * SR), bp = new Biquad("bp").set(2600, 0.8); let ph = 0;
  for (let i = 0; i < n; i++) {
    const x = i / SR; ph += TAU * (185 + 60 * Math.exp(-x * 40)) / SR;
    const s = (Math.sin(ph) * Math.exp(-x * 25) * 0.6 + bp.run(noise()) * Math.exp(-x * 16) * 1.2) * g;
    put(B.drums, i0 + i, s); put(B.rev, i0 + i, s * 0.2);
  }
}
function crash(t, g = 1, len = 2.2) {
  const i0 = Math.round(t * SR), n = Math.round(len * SR), hp = new Biquad("hp").set(4200, 0.6), hp2 = new Biquad("hp").set(4200, 0.6);
  for (let i = 0; i < n; i++) {
    const x = i / SR, env = Math.exp(-x * (3 / len)) * (1 - Math.exp(-x * 400));
    put(B.drums, i0 + i, hp.run(noise()) * env * g * 0.35, hp2.run(noise()) * env * g * 0.35);
  }
}

/* ---------------- tonal ---------------- */
function supersaw(t, dur, notes, g, { att = 0.01, rel = 0.3, cutoff = () => 3000, q = 0.8, voices = 7, detune = 0.18, b = B.music, send = 0.25, width = 0.8 } = {}) {
  const i0 = Math.round(t * SR), n = Math.round((dur + rel) * SR);
  const oscs = [];
  for (const m of notes) for (let v = 0; v < voices; v++) {
    const d = voices === 1 ? 0 : (v / (voices - 1) - 0.5) * 2 * detune;
    oscs.push({ o: new Saw(), f: mtof(m + d), p: voices === 1 ? 0 : (v / (voices - 1) - 0.5) * 2 * width });
  }
  const fl = new Biquad("lp"), fr = new Biquad("lp"), norm = g / Math.sqrt(oscs.length);
  for (let i = 0; i < n; i++) {
    const x = i / SR;
    if (i % 32 === 0) { const c = cutoff(x, t + x); fl.set(c, q); fr.set(c, q); }
    const env = Math.min(1, x / att) * (x > dur ? Math.exp(-(x - dur) / (rel / 4)) : 1);
    let l = 0, r = 0;
    for (const s of oscs) { const v = s.o.next(s.f); const [pl, pr] = pan(s.p); l += v * pl; r += v * pr; }
    l = fl.run(l * norm * env); r = fr.run(r * norm * env);
    put(b, i0 + i, l, r); put(B.rev, i0 + i, (l + r) * 0.5 * send);
  }
}
function bassNote(t, m, dur, g = 1, open = 900) {
  const i0 = Math.round(t * SR), n = Math.round((dur + 0.05) * SR), saw = new Saw(0), lp = new Biquad("lp"); let ph = 0;
  for (let i = 0; i < n; i++) {
    const x = i / SR, f = mtof(m);
    if (i % 32 === 0) lp.set(180 + open * Math.exp(-x * 9), 1.1);
    ph += TAU * f / SR;
    const env = Math.min(1, x / 0.004) * (x > dur ? Math.exp(-(x - dur) * 120) : 1);
    const s = (Math.tanh(lp.run(saw.next(f)) * 1.8) * 0.5 + Math.sin(ph) * 0.75) * env * g;
    put(B.bass, i0 + i, s);
  }
}
function pluck(t, m, g = 1, { dec = 5, bright = 2600, p = 0, send = 0.3, delay = 0.35, b = B.music } = {}) {
  const i0 = Math.round(t * SR), n = Math.round(1.2 * SR), o1 = new Saw(), o2 = new Saw(), lp = new Biquad("lp"), [pl, pr] = pan(p);
  for (let i = 0; i < n; i++) {
    const x = i / SR, f = mtof(m);
    if (i % 32 === 0) lp.set(300 + bright * Math.exp(-x * 14), 1.3);
    const s = lp.run(o1.next(f) + o2.next(f * 1.004)) * Math.exp(-x * dec) * Math.min(1, x / 0.002) * g * 0.35;
    put(b, i0 + i, s * pl, s * pr); put(B.rev, i0 + i, s * send); put(B.dly, i0 + i, s * delay);
  }
}
function bell(t, m, g = 1, { dec = 2.2, ratio = 3.5, index = 2.2, p = 0, send = 0.45, b = B.sfx } = {}) {
  const i0 = Math.round(t * SR), n = Math.round(Math.min(3, 6 / dec) * SR), f = mtof(m), [pl, pr] = pan(p);
  for (let i = 0; i < n; i++) {
    const x = i / SR, mod = Math.sin(TAU * f * ratio * x) * index * Math.exp(-x * 5);
    const s = Math.sin(TAU * f * x + mod) * Math.exp(-x * dec) * Math.min(1, x / 0.003) * g * 0.3;
    put(b, i0 + i, s * pl, s * pr); put(B.rev, i0 + i, s * send);
  }
}
function blip(t, m, g = 1, dec = 30, p = 0) {
  const i0 = Math.round(t * SR), n = Math.round(0.25 * SR), f = mtof(m), [pl, pr] = pan(p); let ph = 0;
  for (let i = 0; i < n; i++) {
    const x = i / SR; ph += TAU * f * (1 + 0.08 * Math.exp(-x * 60)) / SR;
    const s = (Math.sin(ph) + 0.3 * Math.sin(ph * 2)) * Math.exp(-x * dec) * Math.min(1, x / 0.001) * g * 0.3;
    put(B.sfx, i0 + i, s * pl, s * pr); put(B.rev, i0 + i, s * 0.2);
  }
}
function click(t, g = 1, f = 3000, p = 0) {
  const i0 = Math.round(t * SR), n = Math.round(0.03 * SR), bp = new Biquad("bp").set(f, 1.5), [pl, pr] = pan(p);
  for (let i = 0; i < n; i++) { const s = bp.run(noise()) * Math.exp(-(i / SR) * 260) * g; put(B.sfx, i0 + i, s * pl, s * pr); }
}
function buzz(t, g = 1, m = 45, dur = 0.32) {
  const i0 = Math.round(t * SR), n = Math.round((dur + 0.05) * SR), lp = new Biquad("lp").set(1100, 2), a = new Saw(), b2 = new Saw();
  for (let i = 0; i < n; i++) {
    const x = i / SR, env = Math.min(1, x / 0.004) * (x > dur ? Math.exp(-(x - dur) * 80) : 1) * (0.75 + 0.25 * Math.sin(TAU * 18 * x));
    const s = Math.tanh(lp.run(a.next(mtof(m)) + b2.next(mtof(m + 1))) * 2) * env * g * 0.3;
    put(B.sfx, i0 + i, s); put(B.rev, i0 + i, s * 0.15);
  }
}
function thud(t, g = 1, f0 = 90) {
  const i0 = Math.round(t * SR), n = Math.round(0.35 * SR); let ph = 0;
  for (let i = 0; i < n; i++) {
    const x = i / SR; ph += TAU * (f0 * (1 + Math.exp(-x * 40))) / SR;
    put(B.sfx, i0 + i, Math.sin(ph) * Math.exp(-x * 14) * g * 0.6);
  }
  click(t, g * 0.8, 5200); click(t + 0.045, g * 0.5, 3800);
}

/* ---------------- fx ---------------- */
function impact(t, g = 1) {
  const i0 = Math.round(t * SR), n = Math.round(3.2 * SR), lp = new Biquad("lp"), lp2 = new Biquad("lp"); let ph = 0;
  for (let i = 0; i < n; i++) {
    const x = i / SR; ph += TAU * (28 + 50 * Math.exp(-x * 4)) / SR;
    if (i % 32 === 0) { const c = 200 + 9000 * Math.exp(-x * 5); lp.set(c, 0.7); lp2.set(c, 0.7); }
    const sub = Math.sin(ph) * Math.exp(-x * 1.4) * 0.9;
    const nl = lp.run(noise()) * Math.exp(-x * 2.6) * 0.55, nr = lp2.run(noise()) * Math.exp(-x * 2.6) * 0.55;
    put(B.sfx, i0 + i, Math.tanh((sub + nl) * 1.3) * g, Math.tanh((sub + nr) * 1.3) * g);
    put(B.rev, i0 + i, (nl + nr) * 0.6 * g);
  }
}
function riser(t0, t1, g = 1, { from = 300, to = 7000 } = {}) {
  const i0 = Math.round(t0 * SR), n = Math.round((t1 - t0) * SR), bl = new Biquad("bp"), br = new Biquad("bp"), s1 = new Saw(), s2 = new Saw();
  for (let i = 0; i < n; i++) {
    const k = i / n;
    if (i % 32 === 0) { const f = from * Math.pow(to / from, k * k); bl.set(f, 2.5); br.set(f * 1.03, 2.5); }
    const env = Math.pow(k, 2.2) * g, pitch = mtof(57) * Math.pow(2, k * 2);
    const tone = (s1.next(pitch) + s2.next(pitch * 1.01)) * 0.08 * k;
    put(B.sfx, i0 + i, (bl.run(noise()) * 0.8 + tone) * env, (br.run(noise()) * 0.8 + tone) * env);
  }
}
function reverseSwell(t0, t1, g = 1) {
  const i0 = Math.round(t0 * SR), n = Math.round((t1 - t0) * SR), hl = new Biquad("hp").set(1800, 0.7), hr = new Biquad("hp").set(1800, 0.7);
  for (let i = 0; i < n; i++) { const k = i / n, env = Math.pow(k, 4) * g * 0.6; put(B.sfx, i0 + i, hl.run(noise()) * env, hr.run(noise()) * env); }
}
function whoosh(tPeak, len = 0.6, g = 1, up = true) {
  const t0 = tPeak - len * 0.7, i0 = Math.round(t0 * SR), n = Math.round(len * SR), bl = new Biquad("bp"), br = new Biquad("bp");
  for (let i = 0; i < n; i++) {
    const k = i / n;
    if (i % 32 === 0) { const f = up ? 400 * Math.pow(12, k) : 5000 / Math.pow(12, k); bl.set(f, 1.6); br.set(f * 1.1, 1.6); }
    const env = Math.sin(Math.PI * Math.pow(k, 0.8)) ** 2 * g * 0.9;
    put(B.sfx, i0 + i, bl.run(noise()) * env, br.run(noise()) * env);
  }
}
function drone(t0, t1, m, g = 1) {
  const i0 = Math.round(t0 * SR), n = Math.round((t1 - t0) * SR); let ph = 0, ph2 = 0;
  for (let i = 0; i < n; i++) {
    const k = i / n, env = Math.min(1, k * 4) * Math.min(1, (1 - k) * 30) * g;
    ph += TAU * mtof(m) / SR; ph2 += TAU * mtof(m + 12.05) / SR;
    put(B.music, i0 + i, (Math.sin(ph) * 0.7 + Math.sin(ph2) * 0.2) * env);
  }
}

/* ================= arrangement ================= */
const CH = { Am: [57, 60, 64, 69], F: [57, 60, 65, 69], C: [55, 60, 64, 67], G: [55, 59, 62, 67] };
const ROOT = { Am: 33, F: 29, C: 36, G: 31 };
const PROG = ["Am", "F", "C", "G"];
const chordAt = (t) => PROG[Math.floor(t / BAR) % 4];
const kicks = [];

// 0-8: the cold open. A pad breathing, a heartbeat, a blip for every doubling.
supersaw(0.2, 3.8, CH.Am, 0.22, { att: 1.8, rel: 1.5, cutoff: (x) => 500 + 500 * Math.min(1, x / 3), voices: 5, send: 0.5 });
supersaw(4, 3.9, CH.F, 0.22, { att: 0.6, rel: 0.4, cutoff: (x) => 900 + 1400 * (x / 4), voices: 5, send: 0.5 });
for (const b of [0.5, 2.5, 4.5, 6.5]) { kick(b, 0.45); kick(b + 0.2, 0.28); }
for (let b = 0; b < 4; b += BEAT) click(Q.clockType + b, 0.12, 6000, 0.3);
Q.doublings.forEach((d, i) => { pluck(d, [69, 72, 76, 79, 81, 84][i], 1.1, { p: (i % 2 ? 0.4 : -0.4), delay: 0.45 }); blip(d, [81, 84, 88, 91, 93, 96][i], 0.35, 22); });
riser(6.0, 8.0, 0.55);
for (let b = 6; b < 8; b += BEAT / 2) hat(b, 0.25 * ((b - 6) / 2), false);

// 8-14.5: chaos. Driving dissonant bass, slams, pings, and a roll into a hard stop.
for (let b = 8; b < Q.cut - 0.01; b += BEAT) { kick(b, 0.8); kicks.push(b); }
for (let b = 8; b < Q.cut - 0.01; b += BEAT / 4) {
  const step = Math.round((b - 8) / (BEAT / 4));
  bassNote(b, step % 8 === 6 ? 34 : 33, BEAT / 4 * 0.85, 0.42, 500 + 1500 * ((b - 8) / 6.5));
  hat(b, step % 2 ? 0.2 : 0.3, false, step % 2 ? 0.3 : -0.3);
}
Q.slams.forEach((s, i) => {
  impact(s, 0.5); snare(s, 0.6); crash(s, 0.35, 1.0);
  supersaw(s, 0.32, [45, 57, 58, 64].map((m) => m + (i >= 4 ? 1 : 0)), 0.5, { att: 0.003, rel: 0.25, cutoff: (x) => 400 + 4000 * Math.exp(-x * 8), send: 0.35 });
});
Q.toasts.forEach((t, i) => { blip(t, 88, 0.5, 18, 0.6); blip(t + 0.08, 84, 0.45, 18, 0.6); });
Q.terminals.forEach((t, i) => click(t, 0.25, 2200 + (i % 5) * 400, (i % 2 ? -0.5 : 0.5)));
for (let b = 13.5; b < Q.cut - 0.01; ) { const k = (b - 13.5) / 1.0; snare(b, 0.25 + 0.5 * k); b += BEAT / (4 + 8 * k); }
riser(12.5, Q.cut, 0.7, { from: 500, to: 9000 });

// 14.5-16: silence, a low drone under the question, then the implosion swells into the drop.
drone(Q.question, Q.drop, 33, 0.18);
reverseSwell(Q.implode - 0.9, Q.drop, 1.0);
supersaw(Q.question, 1.2, CH.Am, 0.12, { att: 0.6, rel: 0.2, cutoff: () => 700, voices: 5, send: 0.7 });

// 16-47.5: the drop and the feature run. Four on the floor, offbeat bass, chords and arps.
const GROOVE_END = 47.5;
impact(Q.drop, 1.4); crash(Q.drop, 0.8, 3); supersaw(Q.drop, 0.9, [45, ...CH.Am, 76], 0.7, { att: 0.002, rel: 1.4, cutoff: (x) => 1200 + 7000 * Math.exp(-x * 2.5), send: 0.5 });
for (let b = Q.drop; b < GROOVE_END - 0.01; b += BEAT) {
  const n = Math.round((b - Q.drop) / BEAT);
  kick(b, 1); kicks.push(b);
  if (n % 2 === 1) clap(b, 0.75);
  hat(b + BEAT / 2, 0.3, true, 0.25);
  hat(b + BEAT / 4, 0.18, false, -0.35); hat(b + BEAT * 0.75, 0.2, false, -0.35);
  const ch = chordAt(b);
  bassNote(b + BEAT / 2, ROOT[ch] + (n % 4 === 3 ? 12 : 0), BEAT / 2 * 0.9, 0.8, 1300);
}
for (let bar = Q.drop; bar < GROOVE_END - 0.01; bar += BAR) {
  const ch = chordAt(bar), lift = bar >= 22 ? 1 : 0.6;
  supersaw(bar, BAR - 0.05, CH[ch], 0.34 * lift, { att: 0.02, rel: 0.25, cutoff: (x, T) => 1500 + 1800 * Math.sin(T * 0.4) ** 2, send: 0.35 });
  if (bar >= 22) for (let s = 0; s < 16; s++) {
    const notes = CH[ch].concat(CH[ch].map((m) => m + 12)), m = notes[[0, 2, 4, 1, 3, 5, 7, 4][s % 8]];
    pluck(bar + s * BEAT / 4, m + 12, s % 4 === 0 ? 0.55 : 0.38, { dec: 9, bright: 3200, p: s % 2 ? 0.35 : -0.35, send: 0.15, delay: 0.25 });
  }
}
// The track: one bell per station, Needs you in a warmer voice.
Q.stations.forEach((s, i) => bell(s, [69, 72, 76, 79, 81, 84, 88, 93][i], i === 5 ? 0.9 : 0.7, { ratio: i === 5 ? 2 : 3.5, dec: i === 5 ? 1.4 : 2.6, p: -0.6 + i * 0.17 }));
// Feature boundaries.
Q.features.forEach((f) => { whoosh(f, 0.7, 0.6); crash(f, 0.25, 1.4); });
// 01 claims, and the refusal.
Q.claims.forEach((c, i) => { blip(c, 81 + [0, 3, 7, 0, 5][i], 0.55, 20, 0.3); click(c, 0.5, 4000, 0.3); });
buzz(Q.bounce, 0.55, 45, 0.12); buzz(Q.bounce + 0.15, 0.5, 44, 0.12);
// 02 typing and the strip.
for (let t = Q.typing[0], i = 0; t < Q.typing[1]; i++) { click(t, 0.18 + 0.1 * rand(), 2500 + rand() * 2500, -0.2 + 0.4 * rand()); t += 0.045 + 0.035 * rand(); }
Q.strip.forEach((s, i) => bell(s, [76, 79, 81, 84, 88][i], i === 3 ? 0.65 : 0.45, { ratio: i === 3 ? 2 : 3.5, dec: 3, p: -0.4 + i * 0.2 }));
// 03 review.
const R = Q.review;
whoosh(R.arrive1, 0.6, 0.45); whoosh(R.arrive2, 0.5, 0.45); whoosh(R.home, 0.5, 0.4, false);
riser(R.arrive1, R.find, 0.18, { from: 800, to: 5000 }); riser(R.arrive2, R.pass, 0.18, { from: 800, to: 5000 });
buzz(R.find, 0.7, 40, 0.3); pluck(R.fix, 76, 0.7, { p: -0.4 });
[0, 0.06, 0.12, 0.18].forEach((d, i) => bell(R.pass + d, [81, 85, 88, 93][i], 0.6, { dec: 2, p: 0.3 }));
// 04 merges: a lock closing, each a step higher.
Q.merges.forEach((m, i) => { thud(m, 0.8, 70 + i * 8); bell(m + 0.02, 69 + [0, 3, 7, 12][i], 0.45, { dec: 4 }); });
// 05 live rows updating.
for (let i = 0; i < 8; i++) for (let st = 1; st < 4; st++) { const t = Q.live + 0.4 + i * 0.11 + st * 0.85; if (t < 41.7) blip(t, 93 + (i % 3) * 2, 0.12, 40, -0.6 + i * 0.15); }
// 06 inbox.
Q.inboxCards.forEach((c, i) => { whoosh(c + 0.15, 0.4, 0.3); bell(c + 0.05, [76, 79, 83][i], 0.5, { ratio: 2, dec: 2, p: 0.4 }); });
click(Q.approve, 0.8, 1800); bell(Q.approve + 0.03, 81, 0.6, { dec: 2.5 }); bell(Q.approve + 0.1, 88, 0.45, { dec: 2.5 });
for (let t = Q.answerType[0]; t < Q.answerType[1]; t += 0.06) click(t, 0.2 + 0.1 * rand(), 3000 + rand() * 2000, 0.3);
click(Q.enter, 0.9, 1500); bell(Q.enter + 0.03, 84, 0.6, { dec: 2.5 }); bell(Q.enter + 0.1, 91, 0.45, { dec: 2.5 });
whoosh(GROOVE_END + 0.3, 1.2, 0.5, false);

// 48-56: the night. Drums drop out; a pad and an arpeggio rise toward morning.
const NIGHT_CH = ["Am", "F", "C", "G"];
NIGHT_CH.forEach((ch, i) => {
  const t = Q.night + i * BAR;
  supersaw(t, BAR, CH[ch], 0.3, { att: i === 0 ? 0.5 : 0.05, rel: 0.4, cutoff: (x, T) => 500 + 3200 * clamp((T - Q.night) / 7) ** 1.5, send: 0.6, voices: 7 });
  bassNote(t, ROOT[ch], BAR * 0.95, 0.35, 200);
  for (let s = 0; s < 8; s++) {
    const notes = CH[ch].concat(CH[ch].map((m) => m + 12));
    pluck(t + s * BEAT / 2, notes[[0, 2, 4, 6, 3, 5, 7, 4][s]] + 12, 0.4 + 0.3 * (i / 3), { dec: 5, bright: 1500 + 1500 * (i / 3), p: s % 2 ? 0.45 : -0.45, send: 0.4, delay: 0.5 });
  }
});
Q.nightSteps.forEach((s, i) => bell(s, [76, 79, 81, 84][i], 0.4, { dec: 1.8, p: 0.5 }));
for (let t = Q.clock[0]; t < Q.clock[1]; t += BEAT / 4) hat(t, 0.06 + 0.12 * ((t - Q.clock[0]) / 5), false, 0.4);
for (let b = 52; b < Q.end - 0.01; b += BEAT) { kick(b, 0.5 + 0.4 * ((b - 52) / 4)); kicks.push(b); }
[0, 0.07, 0.14, 0.21].forEach((d, i) => bell(Q.clock[1] + d, [69, 73, 76, 81][i], 0.55, { dec: 1.6, ratio: 2 }));
impact(Q.notNoise, 0.35);
for (let b = 54.5; b < Q.end - 0.01; ) { const k = (b - 54.5) / 1.5; snare(b, 0.15 + 0.5 * k); b += BEAT / (2 + 10 * k); }
riser(54, Q.end, 0.65);

// 56-64: the end card. One last hit, a chord left to ring, and the heartbeat to close.
impact(Q.end, 0.9); crash(Q.end, 0.7, 3.5); kick(Q.end, 1); kicks.push(Q.end);
supersaw(Q.end, 5.5, [45, 57, 60, 64, 71, 76], 0.5, { att: 0.003, rel: 2.5, cutoff: (x) => 600 + 6000 * Math.exp(-x * 1.2), send: 0.7 });
bassNote(Q.end, 33, 3, 0.6, 400);
Q.endLines.forEach((t, i) => bell(t, [76, 81, 84, 88, 93][i], 0.3, { dec: 2.5, p: -0.4 + i * 0.2 }));
for (const b of [Q.fade + 0.1, Q.fade + 2.5]) { kick(b, 0.4); kick(b + 0.2, 0.25); }

/* ================= mix ================= */
// Sidechain: the kick ducks the bass and the music.
kicks.sort((a, b) => a - b);
const duck = new Float32Array(N);
{ let k = 0; for (let i = 0; i < N; i++) { const t = i / SR; while (k + 1 < kicks.length && kicks[k + 1] <= t) k++; const d = t - kicks[k]; duck[i] = d >= 0 && d < 0.5 ? 1 - 0.65 * Math.exp(-d / 0.09) : 1; } }

// Ping-pong delay (dotted eighth) into the music.
{
  const D = Math.round(BEAT * 0.75 * SR), bl = new Float32Array(D), br = new Float32Array(D); let w = 0;
  const lp = new Biquad("lp").set(3500), lp2 = new Biquad("lp").set(3500);
  for (let i = 0; i < N; i++) {
    const ol = bl[w], or = br[w];
    bl[w] = lp.run(B.dly.R[i] + or * 0.42); br[w] = lp2.run(B.dly.L[i] + ol * 0.42);
    B.music.L[i] += ol * 0.6; B.music.R[i] += or * 0.6; w = (w + 1) % D;
  }
}
// Freeverb-style reverb on the send.
function reverb(inL, inR) {
  const combs = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617].map((n) => Math.round(n * SR / 44100));
  const aps = [556, 441, 341, 225].map((n) => Math.round(n * SR / 44100));
  const mk = (spread) => ({ c: combs.map((n) => ({ b: new Float32Array(n + spread), i: 0, f: 0 })), a: aps.map((n) => ({ b: new Float32Array(n + spread), i: 0 })) });
  const ch = [mk(0), mk(23)], out = [new Float32Array(N), new Float32Array(N)], fb = 0.86, damp = 0.3;
  [inL, inR].forEach((inp, c) => {
    const { c: cs, a: as } = ch[c], o = out[c];
    for (let i = 0; i < N; i++) {
      const x = inp[i] * 0.015; let s = 0;
      for (const cb of cs) { const y = cb.b[cb.i]; cb.f = y * (1 - damp) + cb.f * damp; cb.b[cb.i] = x + cb.f * fb; cb.i = (cb.i + 1) % cb.b.length; s += y; }
      for (const ap of as) { const y = ap.b[ap.i]; ap.b[ap.i] = s + y * 0.5; ap.i = (ap.i + 1) % ap.b.length; s = y - s; }
      o[i] = s;
    }
  });
  return out;
}
const [rvL, rvR] = reverb(B.rev.L, B.rev.R);

const outL = new Float32Array(N), outR = new Float32Array(N);
const hpL = new Biquad("hp").set(28), hpR = new Biquad("hp").set(28);
// The hard cut before the drop: everything but the question's drone and swell stops dead.
const cutA = Math.round(Q.cut * SR), cutB = Math.round(Q.question * SR), dropI = Math.round(Q.drop * SR);
for (let i = 0; i < N; i++) {
  const gate = i >= cutA && i < cutB ? 0 : 1, quiet = i >= cutA && i < dropI;
  let l, r;
  if (quiet) { l = B.music.L[i] * 0.8 + B.sfx.L[i] * 0.9; r = B.music.R[i] * 0.8 + B.sfx.R[i] * 0.9; }
  else {
    l = B.drums.L[i] * 0.9 + (B.bass.L[i] * 0.85 + B.music.L[i] * 0.8) * duck[i] + B.sfx.L[i] * 0.9 + rvL[i] * 1.1;
    r = B.drums.R[i] * 0.9 + (B.bass.R[i] * 0.85 + B.music.R[i] * 0.8) * duck[i] + B.sfx.R[i] * 0.9 + rvR[i] * 1.1;
  }
  const t = i / SR, fade = Math.min(1, t / 0.05) * clamp((Q.duration - t) / 0.8);
  outL[i] = hpL.run(l * gate) * fade; outR[i] = hpR.run(r * gate) * fade;
}
// Gain so the loudest 0.1% sit at the knee; only what is above it is softly clipped.
const mags = new Float32Array(N); for (let i = 0; i < N; i++) mags[i] = Math.max(Math.abs(outL[i]), Math.abs(outR[i]));
const sorted = Float32Array.from(mags).sort(), p999 = sorted[Math.floor(N * 0.999)];
const gain = 0.8 / p999, knee = 0.8;
const soft = (x) => { const a = Math.abs(x); return a <= knee ? x : Math.sign(x) * (knee + (1 - knee) * Math.tanh((a - knee) / (1 - knee))); };
let peak = 0;
for (let i = 0; i < N; i++) { outL[i] = soft(outL[i] * gain); outR[i] = soft(outR[i] * gain); peak = Math.max(peak, Math.abs(outL[i]), Math.abs(outR[i])); }
for (const [a, b] of [[0, 8], [8, 14.5], [16, 22], [22, 47.5], [48, 56], [56, 64]]) {
  let e = 0; for (let i = Math.round(a * SR); i < Math.round(b * SR); i++) e += outL[i] * outL[i] + outR[i] * outR[i];
  console.log(`${a}-${b}s rms ${(10 * Math.log10(e / ((b - a) * SR * 2))).toFixed(1)} dB`);
}
const norm = 0.93 / peak;
const pcm = Buffer.alloc(44 + N * 4);
pcm.write("RIFF", 0); pcm.writeUInt32LE(36 + N * 4, 4); pcm.write("WAVE", 8); pcm.write("fmt ", 12);
pcm.writeUInt32LE(16, 16); pcm.writeUInt16LE(1, 20); pcm.writeUInt16LE(2, 22); pcm.writeUInt32LE(SR, 24);
pcm.writeUInt32LE(SR * 4, 28); pcm.writeUInt16LE(4, 32); pcm.writeUInt16LE(16, 34); pcm.write("data", 36); pcm.writeUInt32LE(N * 4, 40);
for (let i = 0; i < N; i++) {
  pcm.writeInt16LE(Math.round(clamp(outL[i] * norm, -1, 1) * 32767), 44 + i * 4);
  pcm.writeInt16LE(Math.round(clamp(outR[i] * norm, -1, 1) * 32767), 46 + i * 4);
}
fs.mkdirSync(path.join(__dirname, "out"), { recursive: true });
fs.writeFileSync(path.join(__dirname, "out", "score.wav"), pcm);
console.log(`score.wav: ${DUR.toFixed(1)} s, gain ${gain.toFixed(2)}, peak ${peak.toFixed(2)}`);
