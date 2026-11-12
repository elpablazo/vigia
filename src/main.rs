mod extract;
mod fetch;
mod mcp;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
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
    relay: Option<fetch::Relay>,
    token: String,
    started: Instant,
    /// Watches being checked right now, so the scheduler and "check now" never overlap.
    busy: Mutex<HashSet<String>>,
    slots: tokio::sync::Semaphore,
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
";

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
    /// direct | agent
    via: Option<String>,
    price: Option<f64>,
    currency: Option<String>,
    in_stock: Option<bool>,
    ms: Option<i64>,
}
const COLS: &str = "id,url,title,image,tag,mode,selector,interval,paused,created_at,checked_at,changed_at,status,error,via,price,currency,in_stock,ms";

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
    })
}

impl App {
    fn watch(&self, id: &str) -> Result<Watch, ApiError> {
        self.db
            .lock()
            .unwrap()
            .query_row(&format!("SELECT {COLS} FROM watches WHERE id=?1"), [id], row_watch)
            .optional()?
            .ok_or(ApiError(StatusCode::NOT_FOUND, format!("No existe la vigilancia {id}")))
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

// ---------------------------------------------------------------- core operations (shared by REST and MCP)

pub fn list_watches(app: &Ctx) -> ApiResult {
    let db = app.db.lock().unwrap();
    let mut sparks: HashMap<String, Vec<Value>> = HashMap::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut st = db.prepare("SELECT watch_id, at, price FROM snapshots ORDER BY id")?;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?)))? {
        let (id, at, price) = row?;
        *counts.entry(id.clone()).or_default() += 1;
        if let Some(p) = price {
            sparks.entry(id).or_default().push(json!([at, p]));
        }
    }
    let mut st = db.prepare(&format!("SELECT {COLS} FROM watches ORDER BY created_at DESC, id"))?;
    let mut out = Vec::new();
    for w in st.query_map([], row_watch)? {
        let w = w?;
        let mut v = serde_json::to_value(&w).unwrap();
        v["spark"] = json!(sparks.remove(&w.id).unwrap_or_default());
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
    let mut v = serde_json::to_value(&w).unwrap();
    v["history"] = json!(history);
    Ok(v)
}

#[derive(Deserialize, Default)]
pub struct NewWatch {
    pub url: String,
    pub tag: Option<String>,
    pub interval: Option<i64>,
    pub selector: Option<String>,
    pub mode: Option<String>,
}

fn clean_mode(m: Option<&str>) -> Result<&str, ApiError> {
    match m.unwrap_or("auto") {
        m @ ("auto" | "price" | "text") => Ok(m),
        other => Err(bad(format!("mode debe ser auto, price o text (llegó «{other}»)"))),
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
            "INSERT INTO watches(id,url,tag,mode,selector,interval,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id, url, n.tag.unwrap_or_default().trim(), mode, selector, interval, now()],
        )?;
        id
    };
    let _ = check(app, &id).await;
    get_watch(app, &id)
}

pub fn delete_watch(app: &Ctx, id: &str) -> ApiResult {
    let n = app.db.lock().unwrap().execute("DELETE FROM watches WHERE id=?1", [id])?;
    if n == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("No existe la vigilancia {id}")));
    }
    Ok(json!({ "deleted": id }))
}

/// One-off: fetch a page and say what is there, without saving anything.
pub async fn extract_once(app: &Ctx, url: &str, selector: Option<&str>) -> ApiResult {
    let url = clean_url(url)?;
    let (page, via) = fetch_routed(app, &url, None).await;
    let page = page.map_err(|(_, msg)| ApiError(StatusCode::BAD_GATEWAY, msg))?;
    let ex = extract::extract(&page.body, selector).map_err(bad)?;
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
    let order = route_order(preferred, app.relay.is_some(), !app.agents.online().is_empty());
    let leader = order[0];
    let (mut direct_rejected, mut tried, mut worst): (bool, Vec<&str>, Option<fetch::Page>) = (false, Vec::new(), None);
    let mut direct_error = None;
    for route in order {
        if route != "direct" && route != leader && !direct_rejected {
            continue;
        }
        let res = match (route, &app.relay) {
            ("relay", Some(relay)) => relay.fetch(&app.http, url, &ua).await,
            ("agent", _) => app.agents.fetch(url, &ua).await,
            _ => fetch::direct(&app.http, url, &ua).await,
        };
        match res {
            Ok(p) if usable(&p) => return (Ok(p), route),
            Ok(p) => {
                tried.push(match route {
                    "relay" => "el relevo",
                    "agent" => "el agente",
                    _ => "este servidor",
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
    let from = tried.join(" y a ");
    let err = if p.challenge && tried.len() == 1 {
        (
            "blocked",
            format!("El sitio le pone un desafío anti-bots a {from}. Configura un relevo para revisarlo desde otra red (README → Relevo)."),
        )
    } else if p.challenge {
        ("blocked", format!("El sitio le pone un desafío anti-bots a {from}."))
    } else if p.status == 429 {
        (
            "blocked",
            format!("El sitio dice que recibió demasiadas visitas (HTTP 429) al contestarle a {from}. Suele pasar con IPs de servidores; revisar con menos frecuencia ayuda."),
        )
    } else if p.status == 403 {
        ("blocked", format!("El sitio rechazó la visita (HTTP 403) al contestarle a {from}."))
    } else {
        ("error", format!("El sitio respondió HTTP {}.", p.status))
    };
    (Err(err), "direct")
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

    let outcome = page
        .and_then(|p| extract::extract(&p.body, w.selector.as_deref()).map_err(|e| ("error", e)))
        .and_then(|ex| {
            let as_price = w.mode != "text" && ex.price.is_some();
            if !as_price && (w.mode == "price" || (w.mode == "auto" && w.price.is_some())) {
                Err(("error", "No encontré un precio en la página.".to_string()))
            } else if !as_price && ex.text.is_empty() {
                Err(("error", "La página llegó sin texto visible (puede que arme su contenido con JavaScript).".to_string()))
            } else {
                Ok((ex, as_price))
            }
        });

    let mut hook = None;
    let changed = {
        let db = app.db.lock().unwrap();
        match outcome {
            Err((status, msg)) => {
                db.execute(
                    "UPDATE watches SET status=?1, error=?2, checked_at=?3, ms=?4 WHERE id=?5",
                    params![status, msg, at, ms, w.id],
                )?;
                false
            }
            Ok((ex, as_price)) => {
                let last: Option<(Option<f64>, Option<bool>, Option<String>)> = db
                    .query_row("SELECT price, in_stock, text FROM snapshots WHERE watch_id=?1 ORDER BY id DESC LIMIT 1", [&w.id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
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
                        db.execute("INSERT INTO snapshots(watch_id,at,text) VALUES(?1,?2,?3)", params![w.id, at, ex.text])?;
                        // Text snapshots are whole pages: keep the last 50 per watch.
                        db.execute(
                            "DELETE FROM snapshots WHERE watch_id=?1 AND id NOT IN (SELECT id FROM snapshots WHERE watch_id=?1 ORDER BY id DESC LIMIT 50)",
                            [&w.id],
                        )?;
                    }
                }
                let (price, currency, stock) = if as_price { (ex.price, ex.currency.clone(), ex.in_stock) } else { (None, None, None) };
                db.execute(
                    "UPDATE watches SET status='ok', error=NULL, checked_at=?1, ms=?2, via=?3, title=COALESCE(?4,title), image=COALESCE(?5,image),
                     price=?6, currency=?7, in_stock=?8, changed_at=CASE WHEN ?9 THEN ?1 ELSE changed_at END WHERE id=?10",
                    params![at, ms, via, ex.title, ex.image, price, currency, stock, changed, w.id],
                )?;
                if changed {
                    let (before_price, before_stock, before_text) = last.unwrap();
                    let diff = (!as_price).then(|| {
                        let old = before_text.unwrap_or_default();
                        let d = similar::TextDiff::from_lines(old.as_str(), ex.text.as_str()).unified_diff().context_radius(1).to_string();
                        d.chars().take(4000).collect::<String>()
                    });
                    hook = Some(json!({ "before": { "price": before_price, "in_stock": before_stock }, "after": { "price": price, "in_stock": stock }, "diff": diff }));
                }
                changed
            }
        }
    };

    let watch = app.watch(&w.id)?;
    if let (Some(mut payload), Some(url)) = (hook, app.setting("webhook_url")) {
        payload["event"] = json!("change");
        payload["watch"] = serde_json::to_value(&watch).unwrap();
        tokio::spawn(send_webhook(app.http.clone(), url, payload));
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

async fn scheduler(app: Ctx) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let due: Vec<String> = {
            let db = app.db.lock().unwrap();
            let mut st = db
                .prepare("SELECT id FROM watches WHERE paused=0 AND (checked_at IS NULL OR checked_at + interval <= ?1) ORDER BY checked_at LIMIT 20")
                .unwrap();
            st.query_map([now()], |r| r.get(0)).unwrap().filter_map(Result::ok).collect()
        };
        for id in due {
            if app.busy.lock().unwrap().contains(&id) {
                continue;
            }
            let app = app.clone();
            tokio::spawn(async move {
                let _ = check(&app, &id).await;
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
}
async fn h_patch(State(app): State<Ctx>, Path(id): Path<String>, Json(p): Json<Patch>) -> Api {
    let w = app.watch(&id)?;
    let mode = clean_mode(p.mode.as_deref().or(Some(&w.mode)))?.to_string();
    let selector = match p.selector {
        Some(s) => clean_selector(Some(s))?,
        None => w.selector.clone(),
    };
    {
        let db = app.db.lock().unwrap();
        db.execute(
            "UPDATE watches SET tag=?1, interval=?2, selector=?3, mode=?4, paused=?5 WHERE id=?6",
            params![
                p.tag.as_deref().map(str::trim).unwrap_or(&w.tag),
                p.interval.unwrap_or(w.interval).max(60),
                selector,
                mode,
                p.paused.unwrap_or(w.paused),
                id
            ],
        )?;
        // What is compared changed, so the old history no longer applies: start over.
        if selector != w.selector || mode != w.mode {
            db.execute("DELETE FROM snapshots WHERE watch_id=?1", [&id])?;
            db.execute(
                "UPDATE watches SET checked_at=NULL, changed_at=NULL, price=NULL, currency=NULL, in_stock=NULL, status='new', error=NULL WHERE id=?1",
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
            .query_row("SELECT text FROM snapshots WHERE watch_id=?1 AND id=?2", params![id, snap], |r| r.get(0))
            .optional()?
            .ok_or(ApiError(StatusCode::NOT_FOUND, "No existe esa versión".into()))?;
        let old: Option<String> = db
            .query_row(
                "SELECT text FROM snapshots WHERE watch_id=?1 AND id<?2 AND text IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![id, snap],
                |r| r.get(0),
            )
            .optional()?;
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

const SETTINGS: [&str; 3] = ["webhook_url", "default_interval", "user_agent"];

fn settings_json(app: &Ctx) -> Value {
    let mut v = json!({ "default_user_agent": fetch::DEFAULT_UA });
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
            if k == "webhook_url" && !s.is_empty() && !s.starts_with("http://") && !s.starts_with("https://") {
                return Err(bad("El webhook debe empezar con http:// o https://"));
            }
            db.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
                params![k, s.trim()],
            )?;
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

async fn h_status(State(app): State<Ctx>) -> Api {
    let watches: i64 = app.db.lock().unwrap().query_row("SELECT COUNT(*) FROM watches", [], |r| r.get(0))?;
    let rss_kb = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split(' ').nth(1)?.parse::<i64>().ok())
        .map(|pages| pages * 4);
    let agents: Vec<Value> = app.agents.online().into_iter().map(|name| json!({ "name": name })).collect();
    Ok(Json(
        json!({ "version": env!("CARGO_PKG_VERSION"), "uptime_secs": app.started.elapsed().as_secs(), "watches": watches, "rss_kb": rss_kb, "agents": agents, "relay": app.relay.is_some() }),
    ))
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
    let path = req.uri().path();
    let protected = (path.starts_with("/api/") || path == "/mcp") && path != "/api/login";
    if !protected {
        return next.run(req).await;
    }
    let h = req.headers();
    let bearer = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let cookie = h
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("vigia=")));
    let ok = bearer.is_some_and(|t| ct_eq(t.trim(), &app.token)) || cookie.is_some_and(|c| ct_eq(c, &hex(&app.token)));
    if ok {
        next.run(req).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "Falta el token (Authorization: Bearer …)".into()).into_response()
    }
}

#[derive(Deserialize)]
struct Login {
    token: String,
}
async fn h_login(State(app): State<Ctx>, headers: HeaderMap, Json(l): Json<Login>) -> Response {
    if !ct_eq(l.token.trim(), &app.token) {
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

async fn h_ui() -> Response {
    let mut r = Html(include_str!("ui.html")).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self' 'unsafe-inline'; img-src https: data:; frame-ancestors 'none'"),
    );
    r
}

// ---------------------------------------------------------------- main

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
            let mut b = [0u8; 16];
            std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").expect("urandom"), &mut b).expect("urandom");
            let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
            eprintln!("VIGIA_TOKEN no está definido. Token para esta ejecución: {t}");
            t
        });
    let path = std::env::var("VIGIA_DB").unwrap_or_else(|_| "vigia.db".into());
    let db = Connection::open(&path).unwrap_or_else(|e| panic!("no se pudo abrir {path}: {e}"));
    db.execute_batch(SCHEMA).expect("schema");

    let app: Ctx = Arc::new(App {
        db: Mutex::new(db),
        http: fetch::client(),
        agents: Default::default(),
        relay: fetch::Relay::from_env(),
        token,
        started: Instant::now(),
        busy: Default::default(),
        slots: tokio::sync::Semaphore::new(4),
    });
    tokio::spawn(scheduler(app.clone()));

    let router = Router::new()
        .route("/", get(h_ui))
        .route("/healthz", get(|| async { "ok" }))
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
    use super::route_order;

    #[test]
    fn routes() {
        assert_eq!(route_order(None, false, false), ["direct"]);
        assert_eq!(route_order(None, true, true), ["direct", "relay", "agent"]);
        assert_eq!(route_order(Some("relay"), true, true), ["relay", "direct", "agent"]);
        assert_eq!(route_order(Some("agent"), true, false), ["direct", "relay"]); // agent went away
        assert_eq!(route_order(Some("direct"), true, false), ["direct", "relay"]);
    }
}
