//! The share card (NIT-632): the 1200x630 PNG a link preview shows for a person's page. It is drawn
//! here, from the stored report, with Geist outlines (assets/, made from site/assets/geist.woff2)
//! and plain shapes: no browser, no image from anywhere else. Colors are the site's dark tokens.

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use anyhow::Result;

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

const REGULAR: &[u8] = include_bytes!("../assets/geist-400.ttf");
const STRONG: &[u8] = include_bytes!("../assets/geist-600.ttf");

type Rgb = [u8; 3];
const BG: Rgb = [0x12, 0x12, 0x11];
const SURFACE: Rgb = [0x1b, 0x1b, 0x1a];
const LINE: Rgb = [0x29, 0x29, 0x27];
const LINE_STRONG: Rgb = [0x3a, 0x3a, 0x37];
const FG: Rgb = [0xec, 0xeb, 0xe6];
const FG_2: Rgb = [0xa7, 0xa6, 0x9f];
const FG_3: Rgb = [0x7c, 0x7b, 0x75];
/// `--live`, oklch(0.76 0.12 170), in sRGB.
const LIVE: Rgb = [0x3e, 0xc9, 0xa7];

/// What the card shows, already worded.
pub struct Card {
    pub name: String,
    /// Up to two letters, for the picture's place: the card never fetches the picture itself.
    pub initials: String,
    /// "#3 of 120 this week".
    pub rank: String,
    pub figure: String,
    pub figure_label: String,
    /// (value, label), up to three.
    pub stats: Vec<(String, String)>,
    pub weekly: Vec<u64>,
    pub footer: String,
}

struct Canvas {
    px: Vec<u8>,
    regular: FontRef<'static>,
    strong: FontRef<'static>,
}

impl Canvas {
    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let i = (y as usize * WIDTH as usize + x as usize) * 3;
        for (dst, src) in self.px[i..i + 3].iter_mut().zip(c) {
            *dst = (*dst as f32 * (1.0 - a) + src as f32 * a).round() as u8;
        }
    }

    /// A rectangle with corners of radius `r`, its edges anti-aliased.
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgb) {
        let r = r.min(w / 2.0).min(h / 2.0);
        let (mx, my, hw, hh) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        for py in y.floor() as i32..(y + h).ceil() as i32 {
            for px in x.floor() as i32..(x + w).ceil() as i32 {
                // The pixel's signed distance to the shape: coverage fades over one pixel at the edge.
                let qx = (px as f32 + 0.5 - mx).abs() - hw;
                let qy = (py as f32 + 0.5 - my).abs() - hh;
                let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                self.blend(px, py, c, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgb) {
        self.rect(cx - r, cy - r, 2.0 * r, 2.0 * r, r, c);
    }

    fn font(&self, strong: bool) -> &FontRef<'static> {
        if strong {
            &self.strong
        } else {
            &self.regular
        }
    }

    /// The width `text` takes at `size`.
    fn measure(&self, text: &str, size: f32, strong: bool) -> f32 {
        let font = self.font(strong).as_scaled(PxScale::from(size));
        let mut x = 0.0;
        let mut prev = None;
        for c in text.chars() {
            let id = glyph(self.font(strong), c);
            if let Some(p) = prev {
                x += font.kern(p, id);
            }
            x += font.h_advance(id);
            prev = Some(id);
        }
        x
    }

    /// `text` cut to `max` wide, with an ellipsis when it had to be.
    fn fit(&self, text: &str, size: f32, strong: bool, max: f32) -> String {
        if self.measure(text, size, strong) <= max {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let t: String = chars.iter().collect::<String>().trim_end().to_string() + "\u{2026}";
            if self.measure(&t, size, strong) <= max {
                return t;
            }
        }
        String::new()
    }

    /// Draws `text` with its baseline at `y`, starting at `x`; returns where it ends.
    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, strong: bool, c: Rgb) -> f32 {
        let font = self.font(strong).clone();
        let scaled = font.as_scaled(PxScale::from(size));
        let mut pen = x;
        let mut prev = None;
        for ch in text.chars() {
            let id = glyph(&font, ch);
            if let Some(p) = prev {
                pen += scaled.kern(p, id);
            }
            if let Some(outline) = font.outline_glyph(id.with_scale_and_position(PxScale::from(size), point(pen, y))) {
                let b = outline.px_bounds();
                outline.draw(|gx, gy, cov| self.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, c, cov));
            }
            pen += scaled.h_advance(id);
            prev = Some(id);
        }
        pen
    }
}

/// The glyph for `c`, or `?` when the font has none (Geist here covers Latin only).
fn glyph(font: &FontRef<'static>, c: char) -> ab_glyph::GlyphId {
    let id = font.glyph_id(c);
    if id.0 == 0 {
        font.glyph_id('?')
    } else {
        id
    }
}

/// The card as a PNG.
pub fn render(card: &Card) -> Result<Vec<u8>> {
    let mut cv = Canvas {
        px: BG.iter().copied().cycle().take((WIDTH * HEIGHT * 3) as usize).collect(),
        regular: FontRef::try_from_slice(REGULAR)?,
        strong: FontRef::try_from_slice(STRONG)?,
    };
    let (left, right) = (72.0, WIDTH as f32 - 72.0);

    // The mark and the board's name.
    cv.rect(left + 4.0, 82.0, 34.0, 2.5, 1.25, FG_3);
    cv.circle(left + 5.0, 83.25, 5.0, FG_3);
    cv.circle(left + 21.0, 83.25, 7.0, FG);
    cv.circle(left + 37.0, 83.25, 5.5, FG_3);
    cv.circle(left + 37.0, 83.25, 3.0, BG);
    cv.text("Nitely Leaderboard", left + 58.0, 92.0, 26.0, false, FG_2);

    // Who: the picture's place with their initials, the name and the rank.
    let (ax, ay, ar) = (left + 56.0, 214.0, 56.0);
    cv.circle(ax, ay, ar, LINE_STRONG);
    cv.circle(ax, ay, ar - 1.5, SURFACE);
    let iw = cv.measure(&card.initials, 40.0, true);
    cv.text(&card.initials, ax - iw / 2.0, ay + 14.0, 40.0, true, FG_2);
    let name_x = left + 2.0 * ar + 32.0;
    let name = cv.fit(&card.name, 60.0, true, right - name_x);
    cv.text(&name, name_x, 214.0, 60.0, true, FG);
    cv.text(&card.rank, name_x, 258.0, 28.0, false, FG_2);

    // The headline number, then up to three others beside it.
    let end = cv.text(&card.figure, left - 4.0, 420.0, 128.0, true, FG);
    cv.text(&card.figure_label, end + 16.0, 420.0, 30.0, false, FG_2);
    let mut x = right;
    for (value, label) in card.stats.iter().rev().take(3) {
        let w = cv.measure(value, 44.0, true).max(cv.measure(label, 22.0, false));
        x -= w;
        cv.text(value, x, 378.0, 44.0, true, FG);
        cv.text(label, x, 414.0, 22.0, false, FG_3);
        x -= 48.0;
    }

    // The weekly series, the latest week in the live color.
    let (top, bottom) = (466.0, 552.0);
    cv.rect(left, bottom + 1.0, right - left, 1.0, 0.0, LINE);
    let n = card.weekly.len().max(1);
    let max = card.weekly.iter().copied().max().unwrap_or(0).max(1) as f32;
    let step = (right - left) / n as f32;
    for (i, v) in card.weekly.iter().enumerate() {
        let h = if *v == 0 { 2.0 } else { ((*v as f32 / max) * (bottom - top)).max(3.0) };
        let c = if *v == 0 { LINE_STRONG } else if i + 1 == n { LIVE } else { FG_3 };
        cv.rect(left + i as f32 * step, bottom - h, step - 6.0, h, 2.0, c);
    }
    cv.text("Landed per week", left, 596.0, 20.0, false, FG_3);
    let fw = cv.measure(&card.footer, 20.0, false);
    cv.text(&card.footer, right - fw, 596.0, 20.0, false, FG_3);

    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, WIDTH, HEIGHT);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Best);
        let mut w = enc.write_header()?;
        w.write_image_data(&cv.px)?;
    }
    Ok(out)
}
