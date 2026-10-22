//! A person's page (NIT-632): `/u/<id>`, the id being the install's public key. The body is
//! site/u/index.html, which draws the numbers in the browser; the head is filled in here, so a
//! link preview gets the name, a summary and the card (`/u/<id>/card.png`) without running script.
//! Everything from a report is escaped where it lands.

use serde_json::Value;

use crate::card::Card;

const TEMPLATE: &str = include_str!("../../../site/u/index.html");

/// The periods a page shows, and how a sentence names each.
const PERIODS: [(&str, &str); 3] = [("week", "this week"), ("30d", "in the last 30 days"), ("all", "all time")];

/// An install's public key as the board shows it: 32 bytes, base64url without padding.
pub fn valid_id(id: &str) -> bool {
    id.len() == 43 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The page for `person` (what `Store::person` returns), or the page saying nobody is there.
pub fn render(person: Option<&Value>, id: &str, site: &str) -> String {
    let url = format!("{site}/u/{id}");
    let head = match person.and_then(|p| headline(p).map(|h| (p, h))) {
        Some((p, h)) => {
            let name = p["display_name"].as_str().unwrap_or("Someone");
            let title = format!("{name} · Nitely Leaderboard");
            let description = format!(
                "{} landed {}, #{} of {} on the Nitely Leaderboard. {} first-pass review, {} agent hours.",
                count(h.entry["landed"].as_u64().unwrap_or(0), "change"),
                h.label,
                h.rank,
                h.of,
                percent(h.entry["first_pass_rate"].as_f64()),
                hours(h.entry["agent_hours"].as_f64().unwrap_or(0.0)),
            );
            let image = format!("{url}/card.png?v={}", p["updated_at"].as_i64().unwrap_or(0));
            [
                format!("<title>{}</title>", escape(&title)),
                meta("name", "description", &description),
                meta("property", "og:type", "profile"),
                meta("property", "og:site_name", "Nitely"),
                meta("property", "og:title", &title),
                meta("property", "og:description", &description),
                meta("property", "og:url", &url),
                meta("property", "og:image", &image),
                meta("property", "og:image:width", &crate::card::WIDTH.to_string()),
                meta("property", "og:image:height", &crate::card::HEIGHT.to_string()),
                meta("property", "og:image:alt", &format!("{name}'s numbers on the Nitely Leaderboard")),
                meta("name", "twitter:card", "summary_large_image"),
                meta("name", "twitter:title", &title),
                meta("name", "twitter:description", &description),
                meta("name", "twitter:image", &image),
                format!("<link rel=\"canonical\" href=\"{}\">", escape(&url)),
            ]
            .join("\n  ")
        }
        None => ["<title>Not on the board · Nitely Leaderboard</title>".to_string(), meta("name", "robots", "noindex")].join("\n  "),
    };
    // Inside <script type="application/json">: only `<` could end it early, so none is left raw.
    let data = person.map_or("null".to_string(), |p| p.to_string()).replace('<', "\\u003c");
    TEMPLATE.replacen("{{head}}", &head, 1).replacen("{{person}}", &data, 1)
}

/// The card for `person`, worded from its headline period.
pub fn card(person: &Value, site: &str) -> Option<Card> {
    let h = headline(person)?;
    let e = h.entry;
    let name = person["display_name"].as_str().unwrap_or("").to_string();
    let lead = e["lead_time"]["median"].as_i64().map_or("–".to_string(), duration);
    Some(Card {
        initials: initials(&name),
        name,
        rank: format!("#{} of {} {}", h.rank, h.of, h.label),
        figure: group(e["landed"].as_u64().unwrap_or(0)),
        figure_label: format!("landed {}", h.label),
        stats: vec![
            (percent(e["first_pass_rate"].as_f64()), "first-pass review".into()),
            (lead, "lead time".into()),
            (hours(e["agent_hours"].as_f64().unwrap_or(0.0)), "agent hours".into()),
        ],
        weekly: person["weekly"].as_array().map(|w| w.iter().filter_map(Value::as_u64).collect()).unwrap_or_default(),
        footer: format!("{}/leaderboard", site.trim_start_matches("https://").trim_start_matches("http://")),
    })
}

struct Headline<'a> {
    label: &'static str,
    rank: u64,
    of: u64,
    entry: &'a Value,
}

/// The shortest period the person is on the board for: this week, else 30 days, else all time.
fn headline(person: &Value) -> Option<Headline<'_>> {
    PERIODS.iter().find_map(|(period, label)| {
        let p = person["periods"].get(*period)?;
        Some(Headline { label, rank: p["rank"].as_u64()?, of: p["of"].as_u64()?, entry: &p["entry"] })
    })
}

fn meta(attr: &str, name: &str, content: &str) -> String {
    format!("<meta {attr}=\"{name}\" content=\"{}\">", escape(content))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Up to two initials, as the app's picture placeholder makes them.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let picked = match words.as_slice() {
        [] => vec![],
        [one] => vec![*one],
        [first, .., last] => vec![*first, *last],
    };
    picked.iter().filter_map(|w| w.chars().next()).flat_map(char::to_uppercase).collect()
}

fn count(n: u64, what: &str) -> String {
    format!("{} {what}{}", group(n), if n == 1 { "" } else { "s" })
}

/// 12,345.
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn percent(r: Option<f64>) -> String {
    r.map_or("–".to_string(), |r| format!("{}%", (r * 100.0).round()))
}

fn hours(h: f64) -> String {
    if h < 100.0 {
        let t = (h * 10.0).round() / 10.0;
        if t.fract() == 0.0 {
            format!("{t:.0}")
        } else {
            format!("{t:.1}")
        }
    } else {
        group(h.round() as u64)
    }
}

/// 45s, 12m, 3h 20m, 2d 4h: as the board shows a duration.
fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", (s as f64 / 60.0).round() as i64),
        3600..=86_399 => match (s % 3600 + 30) / 60 {
            0 | 60 => format!("{}h", (s + 1800) / 3600),
            m => format!("{}h {m:02}m", s / 3600),
        },
        _ => match (s % 86_400 + 1800) / 3600 {
            0 | 24 => format!("{}d", (s + 43_200) / 86_400),
            h => format!("{}d {h}h", s / 86_400),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_as_the_board_shows_them() {
        assert_eq!(group(1_234_567), "1,234,567");
        assert_eq!(hours(31.54), "31.5");
        assert_eq!(hours(4.0), "4");
        assert_eq!(duration(5400), "1h 30m");
        assert_eq!(duration(90_000), "1d 1h");
        assert_eq!(initials("night owl"), "NO");
        assert_eq!(initials("  "), "");
    }
}
