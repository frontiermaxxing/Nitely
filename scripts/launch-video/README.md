# The launch video

A 64 second video (1080p, 60 fps) introducing Nitely, with a soundtrack synthesized to match it.
Everything is source: no video editor, no samples, no stock music.

| File | What it is |
| --- | --- |
| `cues.js` | The timeline in seconds, at 120 BPM (a bar is 2 s). The picture and the sound both read it, so a hit moved here moves in both. |
| `index.html` | The picture. `seek(t)` draws the frame at `t` as a pure function of time (seeded randomness, no clocks), so any frame renders the same every time. Fonts come from `site/assets`. |
| `synth.cjs` | The score and sound effects in plain Node: oscillators, filters, a reverb, sidechain ducking and a soft clipper. Writes `out/score.wav` in about 5 seconds. |
| `render.mjs` | Opens `index.html` in headless Chrome over the DevTools protocol, captures each frame and pipes it into ffmpeg, then joins picture and score. |

## Render it

You need Node 22 (for its built-in WebSocket), ffmpeg on `PATH`, and Chrome or Edge (set `CHROME` if
it is somewhere unusual). No `npm install`.

```bash
node scripts/launch-video/render.mjs all
```

That writes `out/nitely-launch.mp4` (about 140 MB) and `out/nitely-launch-720p.mp4` (about 25 MB).
The picture takes 6 to 8 minutes: headless Chrome draws without the GPU, about 120 ms a frame,
3,840 frames. Nothing opens on screen.

While you work on a scene:

- `node scripts/launch-video/render.mjs stills 16.3 24.4`: single frames as PNGs, in seconds.
- Open `index.html?play` in a browser to watch it live (no sound), or `index.html?t=33.5` for one
  frame.
- `node scripts/launch-video/render.mjs score` then `mux` remakes the sound without redrawing the
  picture.

## The cut

| Time | Scene |
| --- | --- |
| 0 to 8 s | One teal dot at 23:40, "One of you.", then it splits on the beat into 64 agents: "Dozens of them." |
| 8 to 14.5 s | Terminals pile up with conflicts and failing CI while words slam in. |
| 14.5 to 16 s | A hard cut to silence: "What if they worked like a team?" |
| 16 to 22 s | The drop. The mark, the wordmark and the tagline, then the lifecycle track with one bell per station. |
| 22 to 48 s | Six features, two bars each: the backlog, the playbook, cross-vendor review, one merge at a time, the live view, the Inbox. |
| 48 to 56 s | The night runs from 23:40 to 07:00: "You wake to decisions, not noise." |
| 56 to 64 s | The end card. |

All data in it is made up (projects Acme, Orbit and Harbor, keys like `APP-207`). Keep it that way:
the video is public.
