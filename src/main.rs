mod extract;
mod fetch;
mod history;
mod mcp;
mod webpush;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use p256::SecretKey;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

pub struct App {
    db: Mutex<Connection>,
    http: reqwest::Client,
    agents: fetch::Agents,
    /// Signs Web Push requests (VAPID).
    vapid: SecretKey,
    /// Signs requests to the relay.
    relay_key: SecretKey,
    token: String,
    started: Instant,
    /// Watches being checked right now, so the scheduler and "check now" never overlap.
    busy: Mutex<HashSet<String>>,
    /// Hosts with a scheduled check in flight: one request per site at a time.
    hosts: Mutex<HashSet<String>>,
    slots: tokio::sync::Semaphore,
    /// Failed logins in the current minute: (minute, count).
    login_fails: Mutex<(i64, u32)>,
}
pub type Ctx = Arc<App>;

pub struct ApiError(pub StatusCode, pub String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("Base de datos: {e}"))
    }
}
fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn not_found(id: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("No existe la vigilancia {id}"))
}
pub type ApiResult = Result<Value, ApiError>;

const SCHEMA: &str = "
PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;
CREATE TABLE IF NOT EXISTS watches(
  id TEXT PRIMARY KEY, url TEXT NOT NULL, title TEXT, image TEXT, tag TEXT NOT NULL DEFAULT '',
  mode TEXT NOT NULL DEFAULT 'auto', selector TEXT, interval INTEGER NOT NULL, paused INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, checked_at INTEGER, changed_at INTEGER,
  status TEXT NOT NULL DEFAULT 'new', error TEXT, via TEXT,
  price REAL, currency TEXT, in_stock INTEGER, ms INTEGER);
CREATE TABLE IF NOT EXISTS snapshots(
  id INTEGER PRIMARY KEY, watch_id TEXT NOT NULL REFERENCES watches(id) ON DELETE CASCADE,
  at INTEGER NOT NULL, price REAL, currency TEXT, in_stock INTEGER, text TEXT);
CREATE INDEX IF NOT EXISTS snapshots_watch ON snapshots(watch_id, id);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS push_subs(endpoint TEXT PRIMARY KEY, p256dh TEXT NOT NULL, auth TEXT NOT NULL, origin TEXT NOT NULL, created_at INTEGER NOT NULL);
";

/// Columns added after the first release. SQLite has no ADD COLUMN IF NOT EXISTS.
const ADDED_COLUMNS: [(&str, &str); 5] = [
    ("notify", "TEXT NOT NULL DEFAULT 'all'"),
    ("target", "REAL"),
    ("ignore", "TEXT"),
    ("fails", "INTEGER NOT NULL DEFAULT 0"),
    ("next_at", "INTEGER"),
];

fn migrate(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(SCHEMA)?;
    let have: HashSet<String> = db
        .prepare("SELECT name FROM pragma_table_info('watches')")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for (name, def) in ADDED_COLUMNS {
        if !have.contains(name) {
            db.execute_batch(&format!("ALTER TABLE watches ADD COLUMN {name} {def}"))?;
        }
    }
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct Watch {
    id: String,
    url: String,
    title: Option<String>,
    image: Option<String>,
    tag: String,
    /// auto | price | text
    mode: String,
    selector: Option<String>,
    /// Seconds between checks.
    interval: i64,
    paused: bool,
    created_at: i64,
    checked_at: Option<i64>,
    changed_at: Option<i64>,
    /// new | ok | blocked | error
    status: String,
    error: Option<String>,
    /// direct | relay | agent
    via: Option<String>,
    price: Option<f64>,
    currency: Option<String>,
    in_stock: Option<bool>,
    ms: Option<i64>,
    /// When to send a push: all | drop | below | restock | none
    notify: String,
    /// Price for `notify = below`.
    target: Option<f64>,
    /// Text mode: lines containing any of these (one per line) are left out of the comparison.
    ignore: Option<String>,
    /// Failed checks in a row; each one doubles the wait before the next (up to a day).
    fails: i64,
    next_at: Option<i64>,
}
const COLS: &str =
    "id,url,title,image,tag,mode,selector,interval,paused,created_at,checked_at,changed_at,status,error,via,price,currency,in_stock,ms,notify,target,ignore,fails,next_at";

fn row_watch(r: &rusqlite::Row) -> rusqlite::Result<Watch> {
    Ok(Watch {
        id: r.get(0)?,
        url: r.get(1)?,
        title: r.get(2)?,
        image: r.get(3)?,
        tag: r.get(4)?,
        mode: r.get(5)?,
        selector: r.get(6)?,
        interval: r.get(7)?,
        paused: r.get(8)?,
        created_at: r.get(9)?,
        checked_at: r.get(10)?,
        changed_at: r.get(11)?,
        status: r.get(12)?,
        error: r.get(13)?,
        via: r.get(14)?,
        price: r.get(15)?,
        currency: r.get(16)?,
        in_stock: r.get(17)?,
        ms: r.get(18)?,
        notify: r.get(19)?,
        target: r.get(20)?,
        ignore: r.get(21)?,
        fails: r.get(22)?,
        next_at: r.get(23)?,
    })
}

impl App {
    fn watch(&self, id: &str) -> Result<Watch, ApiError> {
        self.db
            .lock()
            .unwrap()
            .query_row(&format!("SELECT {COLS} FROM watches WHERE id=?1"), [id], row_watch)
            .optional()?
            .ok_or_else(|| not_found(id))
    }

    fn setting(&self, key: &str) -> Option<String> {
        let v: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0))
            .optional()
            .ok()?;
        v.filter(|s| !s.trim().is_empty())
    }

    fn user_agent(&self) -> String {
        self.setting("user_agent").unwrap_or_else(|| fetch::DEFAULT_UA.into())
    }
}

/// A key kept in the settings table, created on first start.
fn stored_key(db: &Connection, name: &str) -> SecretKey {
    let saved: Option<String> = db.query_row("SELECT value FROM settings WHERE key=?1", [name], |r| r.get(0)).optional().expect("settings");
    if let Some(k) = saved.and_then(|s| webpush::unb64(&s).ok()).and_then(|b| SecretKey::from_slice(&b).ok()) {
        return k;
    }
    let k = webpush::new_key();
    db.execute("INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)", params![name, webpush::b64(&k.to_bytes())])
        .expect("settings");
    k
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()
}

// ---------------------------------------------------------------- core operations (shared by REST and MCP)

pub fn list_watches(app: &Ctx) -> ApiResult {
    let db = app.db.lock().unwrap();
    let mut points: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut st = db.prepare("SELECT watch_id, at, price FROM snapshots ORDER BY id")?;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?)))? {
        let (id, at, price) = row?;
        *counts.entry(id.clone()).or_default() += 1;
        if let Some(p) = price {
            points.entry(id).or_default().push((at, p));
        }
    }
    // The list only draws the last 90 days (plus the price in effect when they began).
    let cut = now() - 90 * 86400;
    let mut st = db.prepare(&format!("SELECT {COLS} FROM watches ORDER BY created_at DESC, id"))?;
    let mut out = Vec::new();
    for w in st.query_map([], row_watch)? {
        let w = w?;
        let pts = points.remove(&w.id).unwrap_or_default();
        let keep_from = pts.iter().rposition(|p| p.0 <= cut).unwrap_or(0);
        let mut v = serde_json::to_value(&w).unwrap();
        v["spark"] = json!(pts[keep_from..].iter().map(|p| json!([p.0, p.1])).collect::<Vec<_>>());
        v["changes"] = json!((counts.get(&w.id).copied().unwrap_or(0) - 1).max(0));
        out.push(v);
    }
    Ok(json!(out))
}

pub fn get_watch(app: &Ctx, id: &str) -> ApiResult {
    let w = app.watch(id)?;
    let db = app.db.lock().unwrap();
    let mut st = db.prepare("SELECT id, at, price, currency, in_stock, text IS NOT NULL FROM snapshots WHERE watch_id=?1 ORDER BY id DESC LIMIT 300")?;
    let history: Vec<Value> = st
        .query_map([id], |r| {
            Ok(json!({ "id": r.get::<_, i64>(0)?, "at": r.get::<_, i64>(1)?, "price": r.get::<_, Option<f64>>(2)?,
                "currency": r.get::<_, Option<String>>(3)?, "in_stock": r.get::<_, Option<bool>>(4)?, "text": r.get::<_, bool>(5)? }))
        })?
        .collect::<Result<_, _>>()?;
    let mut st = db.prepare("SELECT at, price, in_stock FROM snapshots WHERE watch_id=?1 AND price IS NOT NULL ORDER BY id")?;
    let prices: Vec<(i64, f64, Option<bool>)> = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    let series: Vec<(i64, f64)> = prices.iter().map(|p| (p.0, p.1)).collect();
    let t = now();
    let mut stats = json!({});
    for (key, days) in [("d7", Some(7)), ("d30", Some(30)), ("d90", Some(90)), ("d365", Some(365)), ("all", None)] {
        stats[key] = json!(history::stats(&series, days.map(|d| t - d * 86400), t));
    }
    let mut v = serde_json::to_value(&w).unwrap();
    v["history"] = json!(history);
    v["prices"] = json!(prices.iter().map(|p| json!([p.0, p.1, p.2])).collect::<Vec<_>>());
    v["stats"] = stats;
    Ok(v)
}

#[derive(Deserialize, Default)]
pub struct NewWatch {
    pub url: String,
    pub tag: Option<String>,
    pub interval: Option<i64>,
    pub selector: Option<String>,
    pub mode: Option<String>,
    pub notify: Option<String>,
    pub target: Option<f64>,
}

fn clean_mode(m: Option<&str>) -> Result<&str, ApiError> {
    match m.unwrap_or("auto") {
        m @ ("auto" | "price" | "text") => Ok(m),
        other => Err(bad(format!("mode debe ser auto, price o text (llegó «{other}»)"))),
    }
}

fn clean_notify(n: Option<&str>) -> Result<&str, ApiError> {
    match n.unwrap_or("all") {
        n @ ("all" | "drop" | "below" | "restock" | "none") => Ok(n),
        other => Err(bad(format!("notify debe ser all, drop, below, restock o none (llegó «{other}»)"))),
    }
}

fn clean_selector(s: Option<String>) -> Result<Option<String>, ApiError> {
    let s = s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(s) = &s {
        scraper::Selector::parse(s).map_err(|_| bad(format!("Selector CSS inválido: {s}")))?;
    }
    Ok(s)
}

fn clean_url(raw: &str) -> Result<String, ApiError> {
    let raw = raw.trim();
    let with_scheme = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
    let url = reqwest::Url::parse(&with_scheme).map_err(|_| bad("Esa URL no es válida"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(|h| !h.contains('.') && h != "localhost") {
        return Err(bad("Sólo se pueden vigilar direcciones http:// o https://"));
    }
    Ok(url.to_string())
}

/// Creates the watch and runs its first check before answering, so the caller already gets the price.
pub async fn add_watch(app: &Ctx, n: NewWatch) -> ApiResult {
    let url = clean_url(&n.url)?;
    let mode = clean_mode(n.mode.as_deref())?.to_string();
    let notify = clean_notify(n.notify.as_deref())?.to_string();
    if notify == "below" && n.target.is_none() {
        return Err(bad("Para avisar debajo de un precio falta target"));
    }
    let selector = clean_selector(n.selector)?;
    let interval = n.interval.or_else(|| app.setting("default_interval")?.parse().ok()).unwrap_or(3600).max(60);
    let existing: Option<String> = app
        .db
        .lock()
        .unwrap()
        .query_row("SELECT id FROM watches WHERE url=?1 AND selector IS ?2", params![url, selector], |r| r.get(0))
        .optional()?;
    if let Some(id) = existing {
        let mut v = get_watch(app, &id)?;
        v["existing"] = json!(true);
        return Ok(v);
    }
    let id: String = {
        let db = app.db.lock().unwrap();
        let id: String = db.query_row("SELECT lower(hex(randomblob(5)))", [], |r| r.get(0))?;
        db.execute(
            "INSERT INTO watches(id,url,tag,mode,selector,interval,created_at,notify,target) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id, url, n.tag.unwrap_or_default().trim(), mode, selector, interval, now(), notify, n.target],
        )?;
        id
    };
    let _ = check(app, &id).await;
    get_watch(app, &id)
}

pub fn delete_watch(app: &Ctx, id: &str) -> ApiResult {
    let n = app.db.lock().unwrap().execute("DELETE FROM watches WHERE id=?1", [id])?;
    if n == 0 {
        return Err(not_found(id));
    }
    Ok(json!({ "deleted": id }))
}

/// One-off: fetch a page and say what is there, without saving anything.
pub async fn extract_once(app: &Ctx, url: &str, selector: Option<&str>) -> ApiResult {
    let url = clean_url(url)?;
    let (page, via) = fetch_routed(app, &url, None).await;
    let page = page.map_err(|(_, msg)| ApiError(StatusCode::BAD_GATEWAY, msg))?;
    let ex = extract::extract_page(&page.body, &page.content_type, selector).map_err(bad)?;
    let mut v = serde_json::to_value(&ex).unwrap();
    v["url"] = json!(url);
    v["via"] = json!(via);
    v["text"] = json!(ex.text.chars().take(20_000).collect::<String>());
    Ok(v)
}

fn usable(p: &fetch::Page) -> bool {
    !p.challenge && (200..300).contains(&p.status)
}

/// Where to fetch from, in order. Direct always comes first unless the watch last worked through
/// another route: then that one leads, to stop poking a site that rejects this server.
fn route_order(preferred: Option<&str>, has_relay: bool, agent_up: bool) -> Vec<&'static str> {
    let mut order = vec!["direct"];
    if has_relay {
        order.push("relay");
    }
    if agent_up {
        order.push("agent");
    }
    if let Some(i) = order.iter().position(|r| Some(*r) == preferred) {
        let p = order.remove(i);
        order.insert(0, p);
    }
    order
}

/// Tries each route until one returns the page. The relay and agents are only asked when the site
/// rejected this server (challenge, 403, 429) or when they are the watch's known-good route.
async fn fetch_routed(app: &Ctx, url: &str, preferred: Option<&str>) -> (Result<fetch::Page, (&'static str, String)>, &'static str) {
    let ua = app.user_agent();
    let relay_url = app.setting("relay_url");
    let order = route_order(preferred, relay_url.is_some(), !app.agents.online().is_empty());
    let leader = order[0];
    let (mut direct_rejected, mut tried, mut worst): (bool, Vec<&str>, Option<fetch::Page>) = (false, Vec::new(), None);
    let mut direct_error = None;
    for route in order {
        if route != "direct" && route != leader && !direct_rejected {
            continue;
        }
        let res = match (route, &relay_url) {
            ("relay", Some(relay)) => fetch::via_relay(&app.http, relay, &app.relay_key, url, &ua).await,
            ("agent", _) => app.agents.fetch(url, &ua).await,
            _ => fetch::direct(&app.http, url, &ua).await,
        };
        match res {
            Ok(p) if usable(&p) => return (Ok(p), route),
            Ok(p) => {
                tried.push(match route {
                    "relay" => "al relevo",
                    "agent" => "al agente",
                    _ => "a este servidor",
                });
                if route == "direct" {
                    direct_rejected = p.challenge || matches!(p.status, 403 | 429);
                }
                // Report the direct answer when there is one; otherwise whatever came back last.
                if route == "direct" || worst.is_none() {
                    worst = Some(p);
                }
            }
            Err(e) if route == "direct" => direct_error = Some(e),
            Err(e) => eprintln!("{route}: {e}"),
        }
    }
    let Some(p) = worst else {
        return (Err(("error", direct_error.unwrap_or_else(|| "No se pudo obtener la página.".into()))), "direct");
    };
    let from = tried.join(" y ");
    let err = if p.challenge && tried.len() == 1 {
        (
            "blocked",
            format!("El sitio le pone un desafío anti-bots {from}. Configura un relevo para revisarlo desde otra red (Ajustes → Relevo)."),
        )
    } else if p.challenge {
        ("blocked", format!("El sitio le pone un desafío anti-bots {from}."))
    } else if p.status == 429 {
        (
            "blocked",
            format!("El sitio dice que recibió demasiadas visitas (HTTP 429) al contestarle {from}. Se vuelve a intentar con más espera."),
        )
    } else if p.status == 403 {
        ("blocked", format!("El sitio rechazó la visita (HTTP 403) al contestarle {from}."))
    } else {
        ("error", format!("El sitio respondió HTTP {}.", p.status))
    };
    (Err(err), "direct")
}

/// Seconds until the next check: the interval, doubled per failure in a row (capped at a day or the
/// interval, whichever is longer), ±10 % so watches on the same site drift apart.
fn next_delay(interval: i64, fails: i64, rand: u16) -> i64 {
    let base = (interval << fails.clamp(0, 5)).min(interval.max(86400));
    let spread = base / 10;
    base - spread + (rand as i64 % (2 * spread + 1))
}

/// Whether a change deserves a push under the watch's rule. Webhooks always fire.
fn should_notify(rule: &str, target: Option<f64>, before: (Option<f64>, Option<bool>), after: (Option<f64>, Option<bool>)) -> bool {
    let (bp, bs) = before;
    let (ap, as_) = after;
    match rule {
        "none" => false,
        "drop" => matches!((bp, ap), (Some(b), Some(a)) if a < b) || (bs == Some(false) && as_ == Some(true)),
        "below" => match (ap, target) {
            (Some(a), Some(t)) => a <= t && bp.is_none_or(|b| b > t || a < b),
            _ => false,
        },
        "restock" => bs == Some(false) && as_ == Some(true),
        _ => true,
    }
}

pub fn money(p: f64) -> String {
    let cents = (p * 100.0).round() as i64;
    let (int, frac) = (cents.abs() / 100, cents.abs() % 100);
    let digits = int.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if cents < 0 { "-" } else { "" };
    if frac == 0 {
        format!("{sign}${grouped}")
    } else {
        format!("{sign}${grouped}.{frac:02}")
    }
}

fn change_message(before: (Option<f64>, Option<bool>), after: (Option<f64>, Option<bool>), currency: Option<&str>) -> String {
    let cur = currency.map(|c| format!(" {c}")).unwrap_or_default();
    match (before, after) {
        ((_, Some(false)), (_, Some(true))) => format!("Volvió a haber existencias{}", after.0.map(|p| format!(" a {}{cur}", money(p))).unwrap_or_default()),
        ((_, Some(true)), (_, Some(false))) => "Se agotó".into(),
        ((Some(b), _), (Some(a), _)) if b != a => {
            let pct = (a - b) / b * 100.0;
            let verb = if a < b { "Bajó" } else { "Subió" };
            format!("{verb} a {}{cur} (antes {}, {:+.1} %)", money(a), money(b), pct)
        }
        _ => "Cambió el contenido de la página".into(),
    }
}

pub async fn check(app: &Ctx, id: &str) -> ApiResult {
    let w = app.watch(id)?;
    if !app.busy.lock().unwrap().insert(id.to_string()) {
        return Err(ApiError(StatusCode::CONFLICT, "Esa vigilancia ya se está revisando".into()));
    }
    let out = check_inner(app, &w).await;
    app.busy.lock().unwrap().remove(id);
    out
}

async fn check_inner(app: &Ctx, w: &Watch) -> ApiResult {
    let _slot = app.slots.acquire().await.unwrap();
    let t0 = Instant::now();
    let (page, via) = fetch_routed(app, &w.url, w.via.as_deref()).await;
    let ms = t0.elapsed().as_millis() as i64;
    let at = now();
    let rand = u16::from_le_bytes(webpush::random());

    let ignore: Vec<String> = w.ignore.iter().flat_map(|s| s.lines()).map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect();
    let outcome = page
        .and_then(|p| extract::extract_page(&p.body, &p.content_type, w.selector.as_deref()).map_err(|e| ("error", e)))
        .and_then(|mut ex| {
            let as_price = w.mode != "text" && ex.price.is_some();
            if !ignore.is_empty() {
                ex.text = ex
                    .text
                    .lines()
                    .filter(|l| !ignore.iter().any(|i| l.to_lowercase().contains(i)))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            if !as_price && (w.mode == "price" || (w.mode == "auto" && w.price.is_some())) {
                Err(("error", "No encontré un precio en la página.".to_string()))
            } else if !as_price && ex.text.is_empty() {
                Err(("error", "La página llegó sin texto visible (puede que arme su contenido con JavaScript).".to_string()))
            } else {
                Ok((ex, as_price))
            }
        });

    let mut event = None;
    let changed = {
        let db = app.db.lock().unwrap();
        match outcome {
            Err((status, msg)) => {
                let fails = w.fails + 1;
                db.execute(
                    "UPDATE watches SET status=?1, error=?2, checked_at=?3, ms=?4, fails=?5, next_at=?6 WHERE id=?7",
                    params![status, msg, at, ms, fails, at + next_delay(w.interval, fails, rand), w.id],
                )?;
                // Say it once, on the third failure in a row, so a site that broke does not go unnoticed.
                if fails == 3 && w.notify != "none" {
                    event = Some((false, format!("Dejó de poder revisarse: {msg}"), None));
                }
                false
            }
            Ok((ex, as_price)) => {
                let last: Option<(Option<f64>, Option<bool>, Option<String>)> = db
                    .query_row("SELECT price, in_stock, text FROM snapshots WHERE watch_id=?1 ORDER BY id DESC LIMIT 1", [&w.id], |r| {
                        Ok((r.get(0)?, r.get(1)?, history::unpack(r.get_ref(2)?)))
                    })
                    .optional()?;
                let differs = match &last {
                    None => true,
                    Some((p, s, _)) if as_price => p.zip(ex.price).is_none_or(|(a, b)| (a - b).abs() > 0.004) || matches!((s, ex.in_stock), (Some(a), Some(b)) if *a != b),
                    Some((_, _, t)) => t.as_deref() != Some(ex.text.as_str()),
                };
                let changed = differs && last.is_some();
                if differs {
                    if as_price {
                        db.execute(
                            "INSERT INTO snapshots(watch_id,at,price,currency,in_stock) VALUES(?1,?2,?3,?4,?5)",
                            params![w.id, at, ex.price, ex.currency, ex.in_stock],
                        )?;
                    } else {
                        db.execute("INSERT INTO snapshots(watch_id,at,text) VALUES(?1,?2,?3)", params![w.id, at, history::pack(&ex.text)])?;
                        // Text snapshots are whole pages: keep the last 50 per watch. Prices are kept forever.
                        db.execute(
                            "DELETE FROM snapshots WHERE watch_id=?1 AND id NOT IN (SELECT id FROM snapshots WHERE watch_id=?1 ORDER BY id DESC LIMIT 50)",
                            [&w.id],
                        )?;
                    }
                }
                let (price, currency, stock) = if as_price { (ex.price, ex.currency.clone(), ex.in_stock) } else { (None, None, None) };
                db.execute(
                    "UPDATE watches SET status='ok', error=NULL, checked_at=?1, ms=?2, via=?3, title=COALESCE(?4,title), image=COALESCE(?5,image),
                     price=?6, currency=?7, in_stock=?8, changed_at=CASE WHEN ?9 THEN ?1 ELSE changed_at END, fails=0, next_at=?10 WHERE id=?11",
                    params![at, ms, via, ex.title, ex.image, price, currency, stock, changed, at + next_delay(w.interval, 0, rand), w.id],
                )?;
                if changed {
                    let (bp, bs, bt) = last.unwrap();
                    let before = (bp, bs);
                    let after = (price, stock);
                    let diff = (!as_price).then(|| {
                        let old = bt.unwrap_or_default();
                        let d = similar::TextDiff::from_lines(old.as_str(), ex.text.as_str()).unified_diff().context_radius(1).to_string();
                        d.chars().take(4000).collect::<String>()
                    });
                    let push = should_notify(&w.notify, w.target, before, after);
                    let detail = json!({ "before": { "price": bp, "in_stock": bs }, "after": { "price": price, "in_stock": stock }, "diff": diff, "notified": push });
                    event = Some((push, change_message(before, after, currency.as_deref()), Some(detail)));
                }
                changed
            }
        }
    };

    let watch = app.watch(&w.id)?;
    if let Some((push, message, detail)) = event {
        if push || detail.is_none() {
            let title: String = watch.title.clone().unwrap_or_else(|| host_of(&watch.url)).chars().take(80).collect();
            let payload = json!({ "title": title, "body": message, "tag": watch.id, "url": format!("/#w={}", watch.id), "image": watch.image });
            tokio::spawn(push_all(app.clone(), payload.to_string()));
        }
        if let (Some(mut detail), Some(url)) = (detail, app.setting("webhook_url")) {
            detail["event"] = json!("change");
            detail["message"] = json!(message);
            detail["watch"] = serde_json::to_value(&watch).unwrap();
            tokio::spawn(send_webhook(app.http.clone(), url, detail));
        }
    }
    Ok(json!({ "changed": changed, "watch": watch }))
}

async fn send_webhook(http: reqwest::Client, url: String, payload: Value) -> Result<u16, String> {
    let r = http.post(&url).timeout(Duration::from_secs(10)).json(&payload).send().await;
    match r {
        Ok(r) if r.status().is_success() => Ok(r.status().as_u16()),
        Ok(r) => {
            eprintln!("webhook {url}: HTTP {}", r.status());
            Err(format!("El webhook respondió HTTP {}", r.status()))
        }
        Err(e) => {
            eprintln!("webhook {url}: {e}");
            Err(format!("No se pudo llamar al webhook: {e}"))
        }
    }
}

/// Sends a notification to every subscribed device; drops subscriptions the push service says are gone.
/// Returns (delivered, failed).
async fn push_all(app: Ctx, payload: String) -> (usize, usize) {
    let subs: Vec<webpush::Sub> = {
        let db = app.db.lock().unwrap();
        let Ok(mut st) = db.prepare("SELECT endpoint, p256dh, auth, origin FROM push_subs") else {
            return (0, 0);
        };
        st.query_map([], |r| {
            Ok(webpush::Sub {
                endpoint: r.get(0)?,
                p256dh: r.get(1)?,
                auth: r.get(2)?,
                origin: r.get(3)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    };
    let (mut ok, mut failed) = (0, 0);
    for sub in subs {
        match webpush::send(&app.http, &sub, &payload, &app.vapid).await {
            Ok(s) if (200..300).contains(&s) => ok += 1,
            Ok(404 | 410) => {
                let _ = app.db.lock().unwrap().execute("DELETE FROM push_subs WHERE endpoint=?1", [&sub.endpoint]);
                failed += 1;
            }
            other => {
                eprintln!("push {}: {other:?}", host_of(&sub.endpoint));
                failed += 1;
            }
        }
    }
    (ok, failed)
}

async fn scheduler(app: Ctx) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let due: Vec<(String, String)> = {
            let db = app.db.lock().unwrap();
            let mut st = db
                .prepare("SELECT id, url FROM watches WHERE paused=0 AND (next_at IS NULL OR next_at <= ?1) ORDER BY next_at LIMIT 50")
                .unwrap();
            st.query_map([now()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().filter_map(Result::ok).collect()
        };
        for (id, url) in due {
            let host = host_of(&url);
            if app.busy.lock().unwrap().contains(&id) || !app.hosts.lock().unwrap().insert(host.clone()) {
                continue; // same site already being checked: it gets its turn on a later tick
            }
            let app = app.clone();
            tokio::spawn(async move {
                let _ = check(&app, &id).await;
                app.hosts.lock().unwrap().remove(&host);
            });
        }
    }
}

// ---------------------------------------------------------------- HTTP handlers

type Api = Result<Json<Value>, ApiError>;

async fn h_list(State(app): State<Ctx>) -> Api {
    list_watches(&app).map(Json)
}
async fn h_create(State(app): State<Ctx>, Json(n): Json<NewWatch>) -> Api {
    add_watch(&app, n).await.map(Json)
}
async fn h_get(State(app): State<Ctx>, Path(id): Path<String>) -> Api {
    get_watch(&app, &id).map(Json)
}
async fn h_delete(State(app): State<Ctx>, Path(id): Path<String>) -> Api {
    delete_watch(&app, &id).map(Json)
}
async fn h_check(State(app): State<Ctx>, Path(id): Path<String>) -> Api {
    check(&app, &id).await.map(Json)
}

#[derive(Deserialize)]
struct Patch {
    tag: Option<String>,
    interval: Option<i64>,
    selector: Option<String>,
    mode: Option<String>,
    paused: Option<bool>,
    notify: Option<String>,
    /// A number sets it, null clears it, absent leaves it.
    target: Option<Value>,
    ignore: Option<String>,
}
async fn h_patch(State(app): State<Ctx>, Path(id): Path<String>, Json(p): Json<Patch>) -> Api {
    let w = app.watch(&id)?;
    let mode = clean_mode(p.mode.as_deref().or(Some(&w.mode)))?.to_string();
    let notify = clean_notify(p.notify.as_deref().or(Some(&w.notify)))?.to_string();
    let target = match p.target {
        None => w.target,
        Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => Some(extract::parse_price_str(&s).ok_or(bad("Precio objetivo inválido"))?),
        Some(_) => return Err(bad("target debe ser un número")),
    };
    if notify == "below" && target.is_none() {
        return Err(bad("Para avisar debajo de un precio, escribe el precio objetivo"));
    }
    let selector = match p.selector {
        Some(s) => clean_selector(Some(s))?,
        None => w.selector.clone(),
    };
    let ignore = match p.ignore {
        Some(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        None => w.ignore.clone(),
    };
    let interval = p.interval.unwrap_or(w.interval).max(60);
    {
        let db = app.db.lock().unwrap();
        db.execute(
            "UPDATE watches SET tag=?1, interval=?2, selector=?3, mode=?4, paused=?5, notify=?6, target=?7, ignore=?8 WHERE id=?9",
            params![
                p.tag.as_deref().map(str::trim).unwrap_or(&w.tag),
                interval,
                selector,
                mode,
                p.paused.unwrap_or(w.paused),
                notify,
                target,
                ignore,
                id
            ],
        )?;
        if interval != w.interval {
            db.execute("UPDATE watches SET next_at=COALESCE(checked_at, 0) + ?1 WHERE id=?2", params![interval, id])?;
        }
        // What is compared changed, so the old history no longer applies: start over.
        if selector != w.selector || mode != w.mode || (ignore != w.ignore && w.price.is_none()) {
            db.execute("DELETE FROM snapshots WHERE watch_id=?1", [&id])?;
            db.execute(
                "UPDATE watches SET checked_at=NULL, changed_at=NULL, next_at=NULL, price=NULL, currency=NULL, in_stock=NULL, status='new', error=NULL, fails=0 WHERE id=?1",
                [&id],
            )?;
        }
    }
    get_watch(&app, &id).map(Json)
}

/// Line diff between a text snapshot and the one before it.
async fn h_diff(State(app): State<Ctx>, Path((id, snap)): Path<(String, i64)>) -> Api {
    let (new, old) = {
        let db = app.db.lock().unwrap();
        let new: Option<String> = db
            .query_row("SELECT text FROM snapshots WHERE watch_id=?1 AND id=?2", params![id, snap], |r| {
                Ok(history::unpack(r.get_ref(0)?))
            })
            .optional()?
            .ok_or(ApiError(StatusCode::NOT_FOUND, "No existe esa versión".into()))?;
        let old: Option<String> = db
            .query_row(
                "SELECT text FROM snapshots WHERE watch_id=?1 AND id<?2 AND text IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![id, snap],
                |r| Ok(history::unpack(r.get_ref(0)?)),
            )
            .optional()?
            .flatten();
        (new.unwrap_or_default(), old)
    };
    let Some(old) = old else {
        return Ok(Json(json!({ "first": true, "text": new.chars().take(20_000).collect::<String>() })));
    };
    let diff = similar::TextDiff::from_lines(old.as_str(), new.as_str());
    let mut lines = Vec::new();
    for (i, group) in diff.grouped_ops(2).iter().enumerate() {
        if i > 0 {
            lines.push(json!({ "t": "@", "s": "" }));
        }
        for op in group {
            for ch in diff.iter_changes(op) {
                let t = match ch.tag() {
                    similar::ChangeTag::Delete => "-",
                    similar::ChangeTag::Insert => "+",
                    similar::ChangeTag::Equal => " ",
                };
                lines.push(json!({ "t": t, "s": ch.value().trim_end() }));
            }
        }
    }
    Ok(Json(json!({ "first": false, "lines": lines })))
}

#[derive(Deserialize)]
struct ExtractReq {
    url: String,
    selector: Option<String>,
}
async fn h_extract(State(app): State<Ctx>, Json(r): Json<ExtractReq>) -> Api {
    extract_once(&app, &r.url, r.selector.as_deref()).await.map(Json)
}

const SETTINGS: [&str; 4] = ["webhook_url", "default_interval", "user_agent", "relay_url"];

fn settings_json(app: &Ctx) -> Value {
    let mut v = json!({ "default_user_agent": fetch::DEFAULT_UA, "relay_public_key": webpush::b64(&webpush::public_bytes(&app.relay_key)) });
    for k in SETTINGS {
        v[k] = json!(app.setting(k).unwrap_or_default());
    }
    v
}
async fn h_settings(State(app): State<Ctx>) -> Api {
    Ok(Json(settings_json(&app)))
}
async fn h_settings_put(State(app): State<Ctx>, Json(body): Json<HashMap<String, Value>>) -> Api {
    {
        let db = app.db.lock().unwrap();
        for (k, v) in &body {
            if !SETTINGS.contains(&k.as_str()) {
                return Err(bad(format!("Ajuste desconocido: {k}")));
            }
            let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
            let s = s.trim();
            if k == "webhook_url" && !s.is_empty() && !s.starts_with("http://") && !s.starts_with("https://") {
                return Err(bad("El webhook debe empezar con http:// o https://"));
            }
            if k == "relay_url" && !s.is_empty() && !s.starts_with("https://") {
                return Err(bad("El relevo debe ser una dirección https://"));
            }
            db.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", params![k, s])?;
        }
    }
    Ok(Json(settings_json(&app)))
}
async fn h_webhook_test(State(app): State<Ctx>) -> Api {
    let url = app.setting("webhook_url").ok_or(bad("Primero guarda la URL del webhook"))?;
    let status = send_webhook(app.http.clone(), url, json!({ "event": "test", "at": now() }))
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(json!({ "ok": true, "status": status })))
}

/// The relay's source with this server's public key filled in, ready to paste into a Worker.
async fn h_relay_code(State(app): State<Ctx>) -> Response {
    let key = webpush::b64(&webpush::public_bytes(&app.relay_key));
    let code = include_str!("../relay/worker.js").replace("PASTE_PUBLIC_KEY_HERE", &key);
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], code).into_response()
}

async fn h_status(State(app): State<Ctx>) -> Api {
    let (watches, devices, db_bytes): (i64, i64, i64) = app.db.lock().unwrap().query_row(
        "SELECT (SELECT COUNT(*) FROM watches), (SELECT COUNT(*) FROM push_subs), (SELECT page_count * page_size FROM pragma_page_count, pragma_page_size)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let rss_kb = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split(' ').nth(1)?.parse::<i64>().ok())
        .map(|pages| pages * 4);
    let agents: Vec<Value> = app.agents.online().into_iter().map(|name| json!({ "name": name })).collect();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"), "uptime_secs": app.started.elapsed().as_secs(), "watches": watches,
        "rss_kb": rss_kb, "db_kb": db_bytes / 1024, "agents": agents, "relay": app.setting("relay_url").is_some(), "devices": devices,
    })))
}

// ---------------------------------------------------------------- push subscriptions

async fn h_push_key(State(app): State<Ctx>) -> Api {
    Ok(Json(json!({ "key": webpush::b64(&webpush::public_bytes(&app.vapid)) })))
}

#[derive(Deserialize)]
struct SubKeys {
    p256dh: String,
    auth: String,
}
#[derive(Deserialize)]
struct Subscribe {
    endpoint: String,
    keys: SubKeys,
}
async fn h_push_subscribe(State(app): State<Ctx>, headers: HeaderMap, Json(s): Json<Subscribe>) -> Api {
    let url = reqwest::Url::parse(&s.endpoint).map_err(|_| bad("endpoint inválido"))?;
    if url.scheme() != "https" {
        return Err(bad("El endpoint de avisos debe ser https"));
    }
    let check = url.clone();
    tokio::task::spawn_blocking(move || fetch::public_host(&check))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(bad)?;
    if webpush::unb64(&s.keys.p256dh).map_err(bad)?.len() != 65 || webpush::unb64(&s.keys.auth).map_err(bad)?.len() != 16 {
        return Err(bad("Claves de suscripción inválidas"));
    }
    // The VAPID contact is this app's own address, as the browser saw it.
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| o.starts_with("https://") || o.starts_with("http://"))
        .map(String::from)
        .ok_or(bad("Falta el encabezado Origin"))?;
    app.db.lock().unwrap().execute(
        "INSERT INTO push_subs(endpoint,p256dh,auth,origin,created_at) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(endpoint) DO UPDATE SET p256dh=?2, auth=?3, origin=?4",
        params![s.endpoint, s.keys.p256dh, s.keys.auth, origin, now()],
    )?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Unsubscribe {
    endpoint: String,
}
async fn h_push_unsubscribe(State(app): State<Ctx>, Json(u): Json<Unsubscribe>) -> Api {
    app.db.lock().unwrap().execute("DELETE FROM push_subs WHERE endpoint=?1", [&u.endpoint])?;
    Ok(Json(json!({ "ok": true })))
}

async fn h_push_test(State(app): State<Ctx>) -> Api {
    let payload = json!({ "title": "Vigía", "body": "Así se ven los avisos de cambios.", "tag": "test", "url": "/" });
    let (ok, failed) = push_all(app.clone(), payload.to_string()).await;
    if ok == 0 {
        return Err(ApiError(StatusCode::BAD_GATEWAY, "Ningún dispositivo recibió el aviso".into()));
    }
    Ok(Json(json!({ "delivered": ok, "failed": failed })))
}

// ---------------------------------------------------------------- export

/// Every price ever recorded, one row per change: ready for a spreadsheet or pandas.
async fn h_export_csv(State(app): State<Ctx>) -> Result<Response, ApiError> {
    let db = app.db.lock().unwrap();
    let mut st = db.prepare(
        "SELECT w.id, w.url, COALESCE(w.title,''), s.at, s.price, COALESCE(s.currency,''), s.in_stock
         FROM snapshots s JOIN watches w ON w.id = s.watch_id WHERE s.price IS NOT NULL ORDER BY w.id, s.id",
    )?;
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("watch_id,url,title,time_utc,unix_time,price,currency,in_stock\n");
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<bool>>(6)?,
        ))
    })?;
    for row in rows {
        let (id, url, title, at, price, cur, stock) = row?;
        let stock = stock.map_or(String::new(), |b| b.to_string());
        out.push_str(&format!("{id},{},{},{},{at},{price},{cur},{stock}\n", quote(&url), quote(&title), iso(at)));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"vigia-precios.csv\""),
        ],
        out,
    )
        .into_response())
}

/// UTC date-time without a date library (civil-from-days, Howard Hinnant).
fn iso(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// A consistent copy of the whole database (VACUUM INTO), as a file download.
async fn h_backup(State(app): State<Ctx>) -> Result<Response, ApiError> {
    let path = format!("{}.backup", std::env::var("VIGIA_DB").unwrap_or_else(|_| "vigia.db".into()));
    let _ = std::fs::remove_file(&path);
    app.db.lock().unwrap().execute("VACUUM INTO ?1", [&path])?;
    let bytes = std::fs::read(&path).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = std::fs::remove_file(&path);
    let name = format!("attachment; filename=\"vigia-{}.db\"", &iso(now())[..10]);
    Ok(([(header::CONTENT_TYPE, "application/vnd.sqlite3".to_string()), (header::CONTENT_DISPOSITION, name)], bytes).into_response())
}

#[derive(Deserialize)]
struct AgentQuery {
    name: Option<String>,
}
async fn h_agent_poll(State(app): State<Ctx>, Query(q): Query<AgentQuery>) -> Response {
    let name: String = q.name.unwrap_or_else(|| "agente".into()).chars().take(40).collect();
    match app.agents.next_job(&name).await {
        Some(job) => Json(job).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
async fn h_agent_result(State(app): State<Ctx>, Json(r): Json<fetch::JobResult>) -> StatusCode {
    app.agents.finish(r);
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------- auth

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

async fn auth(State(app): State<Ctx>, req: Request, next: Next) -> Response {
    let https = req.headers().get("x-forwarded-proto").is_some_and(|v| v == "https");
    let path = req.uri().path();
    let protected = (path.starts_with("/api/") || path == "/mcp") && path != "/api/login";
    let mut res = if !protected {
        next.run(req).await
    } else {
        let h = req.headers();
        let bearer = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        let cookie = h
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("vigia=")));
        if bearer.is_some_and(|t| ct_eq(t.trim(), &app.token)) || cookie.is_some_and(|c| ct_eq(c, &hex(&app.token))) {
            next.run(req).await
        } else {
            ApiError(StatusCode::UNAUTHORIZED, "Falta el token (Authorization: Bearer …)".into()).into_response()
        }
    };
    let h = res.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if https {
        h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
    res
}

#[derive(Deserialize)]
struct Login {
    token: String,
}
async fn h_login(State(app): State<Ctx>, headers: HeaderMap, Json(l): Json<Login>) -> Response {
    let minute = now() / 60;
    // ponytail: one global budget of 10 failed logins per minute; per-IP limits if this ever has many users.
    {
        let f = app.login_fails.lock().unwrap();
        if f.0 == minute && f.1 >= 10 {
            return ApiError(StatusCode::TOO_MANY_REQUESTS, "Demasiados intentos. Espera un minuto.".into()).into_response();
        }
    }
    if !ct_eq(l.token.trim(), &app.token) {
        {
            let mut f = app.login_fails.lock().unwrap();
            *f = if f.0 == minute { (minute, f.1 + 1) } else { (minute, 1) };
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        return ApiError(StatusCode::UNAUTHORIZED, "Token incorrecto".into()).into_response();
    }
    let secure = if headers.get("x-forwarded-proto").is_some_and(|v| v == "https") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!("vigia={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000{secure}", hex(&app.token));
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}
async fn h_logout() -> Response {
    ([(header::SET_COOKIE, "vigia=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")], Json(json!({ "ok": true }))).into_response()
}

// ---------------------------------------------------------------- the app itself (UI + installable PWA)

async fn h_ui() -> Response {
    let mut r = Html(include_str!("ui.html")).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self' 'unsafe-inline'; img-src 'self' https: data:; frame-ancestors 'none'"),
    );
    r
}

fn asset(content_type: &'static str, cache: &'static str, body: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, cache)], body).into_response()
}

fn port() -> u16 {
    std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("agent") => fetch::run_agent().await,
        // Used by the container HEALTHCHECK: the image has no shell or curl.
        Some("health") => {
            let ok = reqwest::get(format!("http://127.0.0.1:{}/healthz", port())).await.is_ok_and(|r| r.status().is_success());
            std::process::exit(if ok { 0 } else { 1 });
        }
        _ => {}
    }

    let token = std::env::var("VIGIA_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            let t: String = webpush::random::<16>().iter().map(|x| format!("{x:02x}")).collect();
            eprintln!("VIGIA_TOKEN no está definido. Token para esta ejecución: {t}");
            t
        });
    let path = std::env::var("VIGIA_DB").unwrap_or_else(|_| "vigia.db".into());
    let db = Connection::open(&path).unwrap_or_else(|e| panic!("no se pudo abrir {path}: {e}"));
    migrate(&db).expect("schema");
    let (vapid, relay_key) = (stored_key(&db, "vapid_key"), stored_key(&db, "relay_key"));

    let app: Ctx = Arc::new(App {
        db: Mutex::new(db),
        http: fetch::client(),
        agents: Default::default(),
        vapid,
        relay_key,
        token,
        started: Instant::now(),
        busy: Default::default(),
        hosts: Default::default(),
        slots: tokio::sync::Semaphore::new(4),
        login_fails: Mutex::new((0, 0)),
    });
    tokio::spawn(scheduler(app.clone()));

    const DAY: &str = "public, max-age=86400";
    let router = Router::new()
        .route("/", get(h_ui))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/manifest.webmanifest",
            get(|| async { asset("application/manifest+json", DAY, include_bytes!("pwa/manifest.webmanifest")) }),
        )
        .route("/sw.js", get(|| async { asset("text/javascript; charset=utf-8", "no-cache", include_bytes!("pwa/sw.js")) }))
        .route("/icon-192.png", get(|| async { asset("image/png", DAY, include_bytes!("pwa/icon-192.png")) }))
        .route("/icon-512.png", get(|| async { asset("image/png", DAY, include_bytes!("pwa/icon-512.png")) }))
        .route("/icon-maskable.png", get(|| async { asset("image/png", DAY, include_bytes!("pwa/icon-maskable.png")) }))
        .route(
            "/apple-touch-icon.png",
            get(|| async { asset("image/png", DAY, include_bytes!("pwa/apple-touch-icon.png")) }),
        )
        .route("/api/login", post(h_login))
        .route("/api/logout", post(h_logout))
        .route("/api/status", get(h_status))
        .route("/api/watches", get(h_list).post(h_create))
        .route("/api/watches/{id}", get(h_get).patch(h_patch).delete(h_delete))
        .route("/api/watches/{id}/check", post(h_check))
        .route("/api/watches/{id}/snapshots/{snap}/diff", get(h_diff))
        .route("/api/extract", post(h_extract))
        .route("/api/settings", get(h_settings).put(h_settings_put))
        .route("/api/settings/webhook-test", post(h_webhook_test))
        .route("/api/relay/worker.js", get(h_relay_code))
        .route("/api/push/key", get(h_push_key))
        .route("/api/push/subscribe", post(h_push_subscribe))
        .route("/api/push/unsubscribe", post(h_push_unsubscribe))
        .route("/api/push/test", post(h_push_test))
        .route("/api/export/prices.csv", get(h_export_csv))
        .route("/api/backup", get(h_backup))
        .route("/api/agent/poll", get(h_agent_poll))
        .route("/api/agent/result", post(h_agent_result).layer(DefaultBodyLimit::max(12 * 1024 * 1024)))
        .route("/mcp", post(mcp::handle))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port())).await.expect("bind");
    eprintln!("vigia {} en http://0.0.0.0:{} · datos en {path}", env!("CARGO_PKG_VERSION"), port());
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
    tokio::select! {
        r = axum::serve(listener, router) => r.expect("server"),
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route_order(None, false, false), ["direct"]);
        assert_eq!(route_order(None, true, true), ["direct", "relay", "agent"]);
        assert_eq!(route_order(Some("relay"), true, true), ["relay", "direct", "agent"]);
        assert_eq!(route_order(Some("agent"), true, false), ["direct", "relay"]); // agent went away
        assert_eq!(route_order(Some("direct"), true, false), ["direct", "relay"]);
    }

    #[test]
    fn notify_rules() {
        let (p, s) = (Some(100.0), Some(true));
        assert!(should_notify("all", None, (p, s), (Some(120.0), s)));
        assert!(!should_notify("none", None, (p, s), (Some(80.0), s)));
        assert!(should_notify("drop", None, (p, s), (Some(80.0), s)));
        assert!(!should_notify("drop", None, (p, s), (Some(120.0), s)));
        assert!(should_notify("drop", None, (p, Some(false)), (p, s)), "back in stock counts as good news");
        assert!(should_notify("below", Some(90.0), (p, s), (Some(89.0), s)), "crossed the target");
        assert!(should_notify("below", Some(90.0), (Some(89.0), s), (Some(85.0), s)), "kept dropping under it");
        assert!(!should_notify("below", Some(90.0), (Some(85.0), s), (Some(88.0), s)), "rose, still under");
        assert!(!should_notify("below", Some(90.0), (p, s), (Some(95.0), s)));
        assert!(should_notify("restock", None, (p, Some(false)), (p, s)));
        assert!(!should_notify("restock", None, (p, s), (Some(80.0), s)));
    }

    #[test]
    fn delays_back_off_and_spread() {
        for r in [0u16, 7, 65535] {
            let d = next_delay(3600, 0, r);
            assert!((3240..=3960).contains(&d), "{d}");
        }
        assert_eq!(next_delay(3600, 1, 720), 7200); // rand lands at the middle of the spread
        assert_eq!(next_delay(3600, 9, 8640), 86400); // capped at a day
        assert_eq!(next_delay(172800, 3, 17280), 172800); // intervals over a day never grow
    }

    #[test]
    fn messages() {
        assert_eq!(money(27639.0), "$27,639");
        assert_eq!(money(1234567.5), "$1,234,567.50");
        assert_eq!(money(99.0), "$99");
        assert_eq!(
            change_message((Some(27639.0), Some(true)), (Some(25799.0), Some(true)), Some("MXN")),
            "Bajó a $25,799 MXN (antes $27,639, -6.7 %)"
        );
        assert_eq!(
            change_message((Some(10.0), Some(false)), (Some(10.0), Some(true)), None),
            "Volvió a haber existencias a $10"
        );
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1794446887), "2026-11-12T01:28:07Z");
    }
}
