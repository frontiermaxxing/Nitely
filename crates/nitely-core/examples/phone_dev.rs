//! Try pairing a phone end to end without the app (docs/phone.md): the real phone server on
//! loopback, serving the built phone page from `dist/` (run `npm run build` first), with a
//! throwaway approval key and the device list in memory. A second page on another port stands in
//! for the app's Phone screen: make a pairing code, confirm or decline requests, remove phones.
//!
//!     cargo run -p nitely-core --example phone_dev
//!
//! then open http://127.0.0.1:7423/ (the "app") and the pairing link it shows in another tab or
//! browser (the "phone"). It uses a scratch database with a sample project, two changes waiting for
//! approval and a question, so the phone has decisions to make. Development only: nothing here is
//! part of the app.

use nitely_core::approval::{new_secret, public_of};
use nitely_core::http::{self, read_body, read_head, write_response, Slots};
use nitely_core::phone::{self, sign_list, verified, Assets, DeviceList, Devices, OpenDb, Phone};
use nitely_core::issues::{Actor, NewIssue};
use nitely_core::{playbook, review, submit, Db};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let app_key = new_secret()?;
    let public = public_of(&app_key);
    let store: Arc<Mutex<DeviceList>> = Arc::new(Mutex::new(sign_list(&app_key, vec![])));
    let read = store.clone();
    let devices: Devices = Arc::new(move || Ok(verified(&public, &read.lock().unwrap()).unwrap_or_default()));
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("dist");
    let assets: Assets = Arc::new(move |p: &str| std::fs::read(dist.join(p)).ok().map(|b| (b, phone::content_type(p))));
    let db_path = nitely_core::paths::unique_temp("nitely-phone-dev", ".db");
    seed(&db_path)?;
    println!("scratch database: {}", db_path.display());
    let open: OpenDb = Arc::new(move || Db::open(&db_path));
    let phone = Phone::new(devices, assets, open, Arc::new(|_: &Db| vec![]));
    let running = phone.start(&["127.0.0.1".parse()?], phone::PORT, None)?;
    println!("phone server: {}", running.pages()[0]);

    let admin = TcpListener::bind("127.0.0.1:7423")?;
    println!("the app's Phone screen, standing in: http://127.0.0.1:7423/");
    let pages = running.pages();
    let offer: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let handler: http::Handler = Arc::new(move |stream: &mut TcpStream, _: SocketAddr| {
        let deadline = Instant::now() + Duration::from_secs(5);
        let Ok(head) = read_head(stream, deadline) else { return };
        let body = read_body(stream, &head, deadline).unwrap_or_default();
        let form = String::from_utf8_lossy(&body).to_string();
        let field = |k: &str| form.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(str::to_string);
        if head.method == "POST" {
            match head.path.as_str() {
                "/offer" => *offer.lock().unwrap() = phone.offer(&pages).ok().and_then(|o| o.urls.into_iter().next()),
                "/decide" => {
                    if let (Some(id), Some(accept)) = (field("id"), field("accept")) {
                        if let Ok(Some(device)) = phone.decide(&id, accept == "yes") {
                            let mut list = store.lock().unwrap();
                            let mut devices = verified(&public, &list).unwrap_or_default();
                            devices.push(device);
                            *list = sign_list(&app_key, devices);
                            *offer.lock().unwrap() = None;
                        }
                    }
                }
                "/remove" => {
                    if let Some(id) = field("id") {
                        let mut list = store.lock().unwrap();
                        let devices = verified(&public, &list).unwrap_or_default().into_iter().filter(|d| d.id != id).collect();
                        *list = sign_list(&app_key, devices);
                    }
                }
                _ => {}
            }
            return write_response(stream, 303, "text/plain", &[("Location", "/")], b"");
        }
        let mut html = String::from("<!doctype html><meta charset=utf-8><meta http-equiv=refresh content=3><title>Phone (dev)</title><body style=\"font:16px system-ui;max-width:640px;margin:24px auto\"><h1>Phone (dev stand-in for the app)</h1>");
        match &*offer.lock().unwrap() {
            Some(url) => html.push_str(&format!("<p>Pairing link, once, ten minutes:</p><p><a id=pairlink href=\"{url}\">{url}</a></p>")),
            None => html.push_str("<form method=post action=/offer><button>Pair a phone</button></form>"),
        }
        for r in phone.waiting() {
            html.push_str(&format!("<form method=post action=/decide><p>Pair <b>{}</b>? The phone shows <code id=code>{}</code></p><input type=hidden name=id value={}><button name=accept value=yes>Pair</button> <button name=accept value=no>Decline</button></form>", esc(&r.name), r.code, r.id));
        }
        html.push_str("<h2>Paired phones</h2>");
        for d in verified(&public, &store.lock().unwrap()).unwrap_or_default() {
            html.push_str(&format!("<form method=post action=/remove><p>{} <input type=hidden name=id value={}><button>Remove</button></p></form>", esc(&d.name), d.id));
        }
        write_response(stream, 200, "text/html; charset=utf-8", &[], html.as_bytes());
    });
    http::accept(admin, Arc::new(|ip: std::net::IpAddr| ip.is_loopback()), Arc::new(Slots::default()), handler);
    Ok(())
}

/// The phone names itself; its name goes on this page escaped.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A sample project with two changes waiting for approval and a question from an agent.
fn seed(path: &std::path::Path) -> anyhow::Result<()> {
    let mut db = Db::open(path)?;
    db.create_project("DEMO", "Demo")?;
    let agent = Actor::Agent("claude:demo".into());
    let waiting = |db: &mut Db, title: &str, priority: i64, reasons: Vec<String>, findings: Vec<review::Finding>| -> anyhow::Result<String> {
        let new = NewIssue { project: "DEMO".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "claude".into(), priority, allow_similar: true, ..Default::default() };
        let key = db.create_issue(&new, &agent)?.key;
        db.move_issue(&key, "In Progress", None, None, &agent)?;
        let review = review::Review { reviewer: "codex".into(), verdict: "pass".into(), findings, flagged: vec![], summary: String::new(), seconds: 80, previous: vec![], note: String::new(), batch: None, tokens: None, reused: None };
        let gate = playbook::GateResult { passed: true, commit: "abc1234".into(), results: vec![], warnings: vec![], flaked: vec![], lane_run: None };
        let s = submit::Submitted {
            issue: key.clone(), branch: format!("claude/{}", key.to_lowercase()), base: "main".into(), patch_id: format!("patch-{}", key.to_lowercase()),
            commit: "abc1234".into(), files: 3, gate, review: Some(review), approver: "human".into(), reasons, approval_stands: false,
            pushed: true, pr_url: None, pr_error: None, land: None, shadow: None, state: "Needs You".into(), next: String::new(),
        };
        db.record_review(&s)?;
        db.move_issue(&key, "Needs You", None, None, &agent)?;
        Ok(key)
    };
    waiting(&mut db, "Round invoice totals to the cent", 1, vec!["billing and money: src/billing/invoice.rs".into()], vec![review::Finding { priority: 3, text: "Rename `amt` to `amount`".into(), carried: false, raised_as: None, now: None }])?;
    waiting(&mut db, "Retry webhook deliveries with backoff", 2, vec!["the change touches the release process".into()], vec![])?;
    let asked = waiting(&mut db, "Show prices in the visitor's currency", 3, vec![], vec![])?;
    db.create_ask("Which currencies should the price page offer at launch?", &["EUR and USD".into(), "EUR, USD and GBP".into(), "Every currency Stripe supports".into()], Some(&asked), None, &agent)?;
    Ok(())
}
