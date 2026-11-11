//! drop: a tiny clipboard and file drop for a private network.
//!
//! Everything lives in RAM: items expire on their own, can burn after the first
//! read, and vanish on restart. Hard caps on item size, total size and count keep
//! memory bounded.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Largest single item (bytes).
const MAX_ITEM: usize = 25 * 1024 * 1024;
/// Total bytes held across all items.
const MAX_TOTAL: usize = 200 * 1024 * 1024;
/// Maximum number of live items.
const MAX_ITEMS: usize = 200;
/// Allowed lifetimes in seconds: 10 min, 1 h, 1 day, 7 days.
const TTLS: [u64; 4] = [600, 3600, 86_400, 604_800];
/// Unambiguous id alphabet (no 0/o, 1/l/i).
const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
/// Image types safe to render inline. SVG is deliberately excluded.
const INLINE_IMAGES: [&str; 5] = ["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif"];

#[derive(Clone)]
struct Item {
    kind: &'static str,
    name: String,
    mime: String,
    data: Bytes,
    created: u64,
    expires: u64,
    burn: bool,
    private: bool,
}

#[derive(Serialize)]
struct Meta {
    id: String,
    kind: &'static str,
    name: String,
    mime: String,
    size: usize,
    created: u64,
    expires: u64,
    burn: bool,
    private: bool,
    /// Start of the text, only for text items that do not burn.
    preview: Option<String>,
}

#[derive(Default)]
struct Store {
    items: HashMap<String, Item>,
}

impl Store {
    fn used(&self) -> usize {
        self.items.values().map(|i| i.data.len()).sum()
    }
    /// Returns the item if it exists and has not expired; drops it if expired.
    fn live(&mut self, id: &str) -> Option<&Item> {
        if self.items.get(id).is_some_and(|i| i.expires <= now()) {
            self.items.remove(id);
        }
        self.items.get(id)
    }
}

type AppState = Arc<Mutex<Store>>;
type Fail = (StatusCode, String);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn new_id(store: &Store) -> String {
    let mut rng = rand::rng();
    loop {
        let id: String = (0..5).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect();
        if !store.items.contains_key(&id) {
            return id;
        }
    }
}

fn meta(id: &str, it: &Item) -> Meta {
    let preview = (it.kind == "text" && !it.burn).then(|| String::from_utf8_lossy(&it.data).chars().take(160).collect());
    Meta {
        id: id.to_string(),
        kind: it.kind,
        name: it.name.clone(),
        mime: it.mime.clone(),
        size: it.data.len(),
        created: it.created,
        expires: it.expires,
        burn: it.burn,
        private: it.private,
        preview,
    }
}

/// Keeps file names printable and short.
fn clean_name(s: &str) -> String {
    let n: String = s.chars().filter(|c| !c.is_control() && !"/\\\"".contains(*c)).take(120).collect();
    if n.trim().is_empty() { "file".into() } else { n.trim().to_string() }
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// POST /api/drops: multipart with `text` or `file`, plus `ttl`, `burn`, `private`.
async fn create(State(st): State<AppState>, mut mp: Multipart) -> Result<Json<serde_json::Value>, Fail> {
    let bad = |e: axum::extract::multipart::MultipartError| {
        let msg = e.body_text();
        if msg.contains("length limit") {
            (StatusCode::PAYLOAD_TOO_LARGE, "Too large: 25 MB max per drop".to_string())
        } else {
            (StatusCode::BAD_REQUEST, msg)
        }
    };
    let (mut text, mut file) = (None::<String>, None::<(String, String, Bytes)>);
    let (mut ttl, mut burn, mut private) = (3600u64, false, false);
    while let Some(field) = mp.next_field().await.map_err(bad)? {
        match field.name().unwrap_or("") {
            "text" => text = Some(field.text().await.map_err(bad)?),
            "file" => {
                let name = clean_name(field.file_name().unwrap_or("file"));
                let mime = field.content_type().unwrap_or("application/octet-stream").to_string();
                file = Some((name, mime, field.bytes().await.map_err(bad)?));
            }
            "ttl" => ttl = field.text().await.ok().and_then(|s| s.parse().ok()).unwrap_or(3600),
            "burn" => burn = field.text().await.is_ok_and(|s| s == "1"),
            "private" => private = field.text().await.is_ok_and(|s| s == "1"),
            _ => {}
        }
    }
    if !TTLS.contains(&ttl) {
        ttl = 3600;
    }
    let (kind, name, mime, data) = match (file, text) {
        (Some((n, m, d)), _) if !d.is_empty() => ("file", n, m, d),
        (_, Some(t)) if !t.trim().is_empty() => {
            let first = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("Text");
            let name: String = first.trim().chars().take(60).collect();
            ("text", name, "text/plain".to_string(), Bytes::from(t))
        }
        _ => return Err((StatusCode::BAD_REQUEST, "Nothing to drop: add text or a file".into())),
    };
    if data.len() > MAX_ITEM {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "Too large: 25 MB max per drop".into()));
    }

    let mut s = st.lock().unwrap();
    if s.items.len() >= MAX_ITEMS || s.used() + data.len() > MAX_TOTAL {
        return Err((StatusCode::INSUFFICIENT_STORAGE, "The shelf is full. Delete something or wait for items to expire".into()));
    }
    let id = new_id(&s);
    let t = now();
    s.items.insert(id.clone(), Item { kind, name, mime, data, created: t, expires: t + ttl, burn, private });
    Ok(Json(serde_json::json!({ "id": id })))
}

/// GET /api/drops: public items, newest first, plus usage numbers.
async fn list(State(st): State<AppState>) -> impl IntoResponse {
    let s = st.lock().unwrap();
    let t = now();
    let mut v: Vec<Meta> = s.items.iter().filter(|(_, i)| !i.private && i.expires > t).map(|(id, i)| meta(id, i)).collect();
    v.sort_by(|a, b| b.created.cmp(&a.created));
    let body = serde_json::json!({ "items": v, "used": s.used(), "max": MAX_TOTAL, "count": s.items.len(), "max_item": MAX_ITEM });
    ([(header::CACHE_CONTROL, "no-store")], Json(body))
}

/// GET /api/drops/{id}: metadata only, never consumes a burn item.
async fn get_meta(State(st): State<AppState>, Path(id): Path<String>) -> Result<Json<Meta>, StatusCode> {
    let mut s = st.lock().unwrap();
    s.live(&id).map(|i| Json(meta(&id, i))).ok_or(StatusCode::NOT_FOUND)
}

async fn delete(State(st): State<AppState>, Path(id): Path<String>) -> StatusCode {
    match st.lock().unwrap().items.remove(&id) {
        Some(_) => StatusCode::NO_CONTENT,
        None => StatusCode::NOT_FOUND,
    }
}

#[derive(Deserialize)]
struct RawQ {
    dl: Option<String>,
}

/// GET /raw/{id}: the content. Burn items are removed as they are served.
/// Served sandboxed so uploaded content cannot script the shared origin.
async fn raw(State(st): State<AppState>, Path(id): Path<String>, Query(q): Query<RawQ>) -> Response {
    let item = {
        let mut s = st.lock().unwrap();
        let Some(it) = s.live(&id).cloned() else {
            return (StatusCode::NOT_FOUND, "Gone: this drop expired, burned, or never existed").into_response();
        };
        if it.burn {
            s.items.remove(&id);
        }
        it
    };
    let inline_img = INLINE_IMAGES.contains(&item.mime.as_str());
    let (ctype, inline) = if item.kind == "text" {
        ("text/plain; charset=utf-8".to_string(), true)
    } else if inline_img {
        (item.mime.clone(), true)
    } else {
        ("application/octet-stream".to_string(), false)
    };
    let disp = if inline && q.dl.is_none() { "inline" } else { "attachment" };
    let fname = if item.kind == "text" { format!("drop-{id}.txt") } else { item.name.clone() };
    let ascii: String = fname.chars().map(|c| if c.is_ascii_alphanumeric() || "._- ()".contains(c) { c } else { '_' }).collect();
    let mut h = HeaderMap::new();
    let mut set = |k, v: String| {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    };
    set(header::CONTENT_TYPE, ctype);
    set(header::CONTENT_DISPOSITION, format!("{disp}; filename=\"{ascii}\"; filename*=UTF-8''{}", percent_encode(&fname)));
    set(header::CACHE_CONTROL, "no-store".into());
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff".into());
    set(header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'; img-src 'self'; style-src 'unsafe-inline'".into());
    (h, item.data).into_response()
}

#[derive(Deserialize)]
struct QrQ {
    t: String,
}

/// GET /qr?t=...: an SVG QR code for a short text (the page sends its own link).
async fn qr(Query(q): Query<QrQ>) -> Response {
    if q.t.len() > 512 {
        return (StatusCode::BAD_REQUEST, "Text too long").into_response();
    }
    match qrcode::QrCode::new(q.t.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .dark_color(qrcode::render::svg::Color("#071018"))
                .light_color(qrcode::render::svg::Color("#e6edf7"))
                .build();
            ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "max-age=3600")], svg).into_response()
        }
        Err(_) => (StatusCode::BAD_REQUEST, "Cannot encode").into_response(),
    }
}

/// Drops expired items every 30 s so memory is returned promptly.
async fn reaper(st: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let t = now();
        st.lock().unwrap().items.retain(|_, i| i.expires > t);
    }
}

#[tokio::main]
async fn main() {
    let st: AppState = Arc::default();
    tokio::spawn(reaper(st.clone()));
    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("index.html")) }))
        .route("/api/drops", get(list).post(create))
        .route("/api/drops/{id}", get(get_meta).delete(delete))
        .route("/raw/{id}", get(raw))
        .route("/qr", get(qr))
        .layer(DefaultBodyLimit::max(MAX_ITEM + 64 * 1024))
        .with_state(st);
    let addr = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8451".into());
    let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("listening on {addr}");
    axum::serve(l, app).await.unwrap();
}
