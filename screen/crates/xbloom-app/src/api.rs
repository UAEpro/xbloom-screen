//! The admin HTTP API and the embedded admin panel, independent of any HTTP server.
//!
//! Each platform adapts its server to [`HttpRequest`] / [`Response`] and calls [`handle`]:
//! `tiny_http` on the Raspberry Pi, `esp_http_server` on the ESP32. The routes are the same
//! everywhere, so the admin panel (`screen/admin/`, embedded at build time) works unchanged.
//!
//! ## Security
//!
//! * Everything under `/api/` except `info`, `setup` and `login` needs a session: the
//!   `xb_session` cookie (HttpOnly, SameSite=Strict) or `Authorization: Bearer <token>`.
//! * Mutating requests must carry `X-XBloom: 1`. Browsers can't add a custom header to a
//!   cross-site request without a CORS preflight (which this server never approves), so this
//!   blocks CSRF from other pages on the LAN.
//! * PIN guesses go through the web channel's throttle; PIN hashes and the Wi-Fi password are
//!   never returned.
//! * The API can't start a brew. Brewing dispenses hot water, so it only happens at the
//!   screen. It can **cancel** one.

use std::io::Read;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use xbloom_core::recipe::RawRecipe;
use xbloom_core::Recipe;

use crate::app::{attempt_ok, App, Data, PinPurpose};
use crate::auth::{Attempt, Channel};
use crate::recipes::{to_yaml, RecipeMeta, StoredRecipe};
use crate::settings::SettingsPatch;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// GET / HEAD.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// DELETE.
    Delete,
    /// Anything else.
    Other,
}

/// What the router needs from a request.
pub trait HttpRequest {
    /// The method.
    fn method(&self) -> Method;
    /// The path, without the query string.
    fn path(&self) -> &str;
    /// The raw query string (no `?`).
    fn query(&self) -> &str;
    /// A header value (case-insensitive name).
    fn header(&self, name: &str) -> Option<String>;
    /// The request body.
    fn body(&mut self) -> &mut dyn Read;
}

/// A response body.
#[derive(Debug)]
pub enum Body {
    /// Nothing.
    Empty,
    /// Owned bytes.
    Bytes(Vec<u8>),
    /// Embedded bytes (admin panel assets, already gzip-compressed).
    Static(&'static [u8]),
    /// Stream a file.
    File(PathBuf),
}

/// A response.
#[derive(Debug)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: &'static str,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
    /// The body.
    pub body: Body,
}

impl Response {
    fn json<T: Serialize>(status: u16, v: &T) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: vec![("Cache-Control", "no-store".into())],
            body: Body::Bytes(serde_json::to_vec(v).unwrap_or_default()),
        }
    }

    fn ok<T: Serialize>(v: &T) -> Self {
        Self::json(200, v)
    }

    fn error(status: u16, msg: impl Into<String>) -> Self {
        Self::json(status, &json!({ "error": msg.into() }))
    }

    fn text(content_type: &'static str, body: String) -> Self {
        Self { status: 200, content_type, headers: vec![], body: Body::Bytes(body.into_bytes()) }
    }

    fn header(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// Most bytes read for a JSON request body (recipe imports included).
const MAX_JSON: u64 = 512 * 1024;
const SESSION_COOKIE: &str = "xb_session";

fn read_body(req: &mut dyn HttpRequest) -> Result<Vec<u8>, Response> {
    let mut buf = Vec::new();
    req.body()
        .take(MAX_JSON + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Response::error(400, format!("read error: {e}")))?;
    if buf.len() as u64 > MAX_JSON {
        return Err(Response::error(413, "request too large"));
    }
    Ok(buf)
}

fn read_json<T: DeserializeOwned>(req: &mut dyn HttpRequest) -> Result<T, Response> {
    let buf = read_body(req)?;
    serde_json::from_slice(&buf).map_err(|e| Response::error(400, format!("invalid JSON: {e}")))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn session_token(req: &dyn HttpRequest) -> Option<String> {
    if let Some(auth) = req.header("authorization") {
        if let Some(t) = auth.strip_prefix("Bearer ") {
            return Some(t.trim().to_string());
        }
    }
    req.header("cookie")?.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == SESSION_COOKIE).then(|| v.to_string())
    })
}

fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200")
}

fn is_mutation(m: Method) -> bool {
    matches!(m, Method::Post | Method::Put | Method::Delete)
}

/// Route one request.
pub fn handle(app: &App, req: &mut dyn HttpRequest) -> Response {
    let method = req.method();
    let path = req.path().to_string();
    if !path.starts_with("/api/") {
        return serve_asset(req, &path);
    }
    if is_mutation(method) && req.header("x-xbloom").as_deref() != Some("1") {
        return Response::error(403, "missing X-XBloom header");
    }
    let segs: Vec<&str> = path.trim_start_matches("/api/").trim_end_matches('/').split('/').collect();
    // Public endpoints.
    match (method, segs.as_slice()) {
        (Method::Get, ["info"]) => return info(app, req),
        (Method::Post, ["setup"]) => return setup(app, req),
        (Method::Post, ["login"]) => return login(app, req),
        _ => {}
    }
    let token = session_token(req);
    let authed = token.as_deref().is_some_and(|t| app.auth.lock().unwrap().touch_session(t));
    if !authed {
        return Response::error(401, "log in with the admin PIN");
    }
    let res = route(app, req, method, &segs, token.as_deref().unwrap_or_default());
    res.unwrap_or_else(|e| e)
}

type R = Result<Response, Response>;

fn route(app: &App, req: &mut dyn HttpRequest, method: Method, segs: &[&str], token: &str) -> R {
    use Method::*;
    let query = req.query().to_string();
    Ok(match (method, segs) {
        (Post, ["logout"]) => {
            app.auth.lock().unwrap().end_session(token);
            Response::ok(&json!({"ok": true})).header("Set-Cookie", format!("{SESSION_COOKIE}=; Path=/; Max-Age=0"))
        }
        (Get, ["status"]) => {
            let presets = app.presets.lock().unwrap().presets.clone();
            Response::ok(&json!({
                "machine": app.machine_status(),
                "presets_dirty": presets.dirty(),
                "time": crate::util::now_unix(),
            }))
        }
        // ── settings & PINs ──
        (Get, ["settings"]) => Response::ok(&app.settings().public()),
        (Put, ["settings"]) => {
            let patch: SettingsPatch = read_json(req)?;
            let s = app.update_settings(|s| {
                s.apply(patch);
            });
            Response::ok(&s.public())
        }
        (Post, ["pin", "admin"]) => {
            #[derive(Deserialize)]
            struct B {
                current: String,
                new: String,
            }
            let b: B = read_json(req)?;
            app.change_admin_pin(Channel::Web, &b.current, &b.new).map_err(|e| Response::error(400, e))?;
            // The PIN change ended every session; issue a fresh one for this browser.
            let tok = app.auth.lock().unwrap().new_session();
            Response::ok(&json!({"ok": true, "token": tok})).header("Set-Cookie", session_cookie(&tok))
        }
        (Post, ["pin", "user"]) => {
            #[derive(Deserialize)]
            struct B {
                admin_pin: String,
                new: Option<String>,
            }
            let b: B = read_json(req)?;
            app.set_user_pin(Channel::Web, &b.admin_pin, b.new.as_deref()).map_err(|e| Response::error(400, e))?;
            Response::ok(&json!({"ok": true}))
        }
        // ── machine ──
        (Post, ["machine", "scan"]) => {
            app.scan();
            Response::ok(&json!({"ok": true}))
        }
        (Post, ["machine", "connect"]) => {
            #[derive(Deserialize, Default)]
            struct B {
                address: Option<String>,
            }
            let b: B = if read_body_is_empty(req) { B::default() } else { read_json(req)? };
            app.connect(b.address);
            Response::ok(&json!({"ok": true}))
        }
        (Post, ["machine", "disconnect"]) => {
            app.disconnect();
            Response::ok(&json!({"ok": true}))
        }
        (Post, ["machine", "forget"]) => {
            app.forget_machine();
            Response::ok(&json!({"ok": true}))
        }
        (Post, ["brew", "cancel"]) => {
            app.cancel_brew();
            Response::ok(&json!({"ok": true}))
        }
        // ── recipes ──
        (Get, ["recipes"]) => Response::ok(&app.recipes.lock().unwrap().list()),
        (Post, ["recipes"]) => {
            let b: RecipeBody = read_json(req)?;
            let recipe = b.recipe()?;
            let r = app.add_recipe(recipe, b.meta.unwrap_or_default()).map_err(|e| Response::error(400, e))?;
            Response::json(201, &r)
        }
        (Post, ["recipes", "validate"]) => {
            let v: Value = read_json(req)?;
            let v = v.get("recipe").cloned().unwrap_or(v);
            match parse_recipe_value(v) {
                Ok(r) => Response::ok(
                    &json!({"ok": true, "total_water_ml": r.total_water_ml(), "ratio": r.effective_ratio()}),
                ),
                Err(e) => Response::ok(&json!({"ok": false, "error": e})),
            }
        }
        (Post, ["recipes", "import"]) => {
            let text =
                String::from_utf8(read_body(req)?).map_err(|_| Response::error(400, "the file isn't UTF-8 text"))?;
            let (added, errors) = app.recipes.lock().unwrap().import(&text).map_err(|e| Response::error(400, e))?;
            if !added.is_empty() {
                app.changed(Data::Recipes);
            }
            Response::ok(&json!({"imported": added.len(), "errors": errors}))
        }
        (Post, ["recipes", "reorder"]) => {
            #[derive(Deserialize)]
            struct B {
                ids: Vec<String>,
            }
            let b: B = read_json(req)?;
            app.recipes.lock().unwrap().reorder(&b.ids);
            app.changed(Data::Recipes);
            Response::ok(&json!({"ok": true}))
        }
        (Get, ["recipes", "export.json"]) => {
            let list = app.recipes.lock().unwrap().list();
            Response::ok(&json!({"recipes": list}))
                .header("Content-Disposition", "attachment; filename=\"xbloom-recipes.json\"")
        }
        (Get, ["recipes", id]) => Response::ok(&get_recipe(app, id)?),
        (Get, ["recipes", id, "export.yaml"]) => {
            let r = get_recipe(app, id)?;
            let file = format!("{}.yaml", crate::util::slug(&r.recipe.name));
            Response::text("text/yaml; charset=utf-8", to_yaml(&r.recipe))
                .header("Content-Disposition", format!("attachment; filename=\"{file}\""))
        }
        (Put, ["recipes", id]) => {
            let b: RecipeBody = read_json(req)?;
            let recipe = b.recipe()?;
            let r = app.update_recipe(id, recipe, b.meta).map_err(|e| Response::error(400, e))?;
            Response::ok(&r)
        }
        (Post, ["recipes", id, "duplicate"]) => {
            let r = app.recipes.lock().unwrap().duplicate(id).map_err(|e| Response::error(404, e))?;
            app.changed(Data::Recipes);
            Response::json(201, &r)
        }
        (Delete, ["recipes", id]) => {
            if !app.delete_recipe(id) {
                return Err(Response::error(404, "no such recipe"));
            }
            Response::ok(&json!({"ok": true}))
        }
        // ── dial presets ──
        (Get, ["presets"]) => {
            let p = app.presets.lock().unwrap().presets.clone();
            Response::ok(&json!({
                "slots": p.slots, "pushed_at": p.pushed_at, "dirty": p.dirty(),
                "scale": app.settings().machine.preset_scale,
            }))
        }
        (Put, ["presets"]) => {
            #[derive(Deserialize)]
            struct B {
                slots: [Option<String>; 3],
                scale: Option<[bool; 3]>,
            }
            let b: B = read_json(req)?;
            app.set_presets(b.slots).map_err(|e| Response::error(400, e))?;
            if let Some(sc) = b.scale {
                app.update_settings(|s| s.machine.preset_scale = sc);
            }
            Response::ok(&json!({"ok": true}))
        }
        (Post, ["presets", "push"]) => {
            app.push_presets().map_err(|e| Response::error(400, e))?;
            Response::ok(&json!({"ok": true}))
        }
        // ── history ──
        (Get, ["history"]) => Response::ok(&app.history.lock().unwrap().list()),
        (Get, ["history", "export.csv"]) => {
            Response::text("text/csv; charset=utf-8", app.history.lock().unwrap().to_csv())
                .header("Content-Disposition", "attachment; filename=\"xbloom-history.csv\"")
        }
        (Get, ["history", id]) => {
            Response::ok(&app.history.lock().unwrap().get(id).ok_or_else(|| Response::error(404, "no such brew"))?)
        }
        (Delete, ["history"]) => {
            app.history.lock().unwrap().clear();
            app.changed(Data::History);
            Response::ok(&json!({"ok": true}))
        }
        (Delete, ["history", id]) => {
            if !app.history.lock().unwrap().delete(id) {
                return Err(Response::error(404, "no such brew"));
            }
            app.changed(Data::History);
            Response::ok(&json!({"ok": true}))
        }
        // ── screensaver media ──
        (Get, ["media"]) => Response::ok(&app.media.list()),
        (Post, ["media"]) => {
            let name = query_param(&query, "name").ok_or_else(|| Response::error(400, "missing ?name="))?;
            let mut up = app.media.begin_upload(&name).map_err(|e| Response::error(400, e.to_string()))?;
            up.copy_from(req.body()).map_err(|e| Response::error(413, e.to_string()))?;
            let info = up.finish().map_err(|e| Response::error(400, e.to_string()))?;
            app.changed(Data::Media);
            Response::json(201, &info)
        }
        (Get, ["media", name]) => {
            let p = app.media.path(name).ok_or_else(|| Response::error(404, "no such media"))?;
            let ct = if name.ends_with(".jpg") { "image/jpeg" } else { "application/octet-stream" };
            Response { status: 200, content_type: ct, headers: vec![], body: Body::File(p) }
        }
        (Get, ["media", name, "thumb.jpg"]) => {
            let bytes = app.media.thumbnail(name).map_err(|e| Response::error(404, e.to_string()))?;
            Response {
                status: 200,
                content_type: "image/jpeg",
                headers: vec![("Cache-Control", "max-age=60".into())],
                body: Body::Bytes(bytes),
            }
        }
        (Delete, ["media", name]) => {
            if !app.media.delete(name) {
                return Err(Response::error(404, "no such media"));
            }
            let video = app.settings().screensaver.video;
            if video.as_deref() == Some(*name) {
                app.update_settings(|s| s.screensaver.video = None);
            }
            app.changed(Data::Media);
            Response::ok(&json!({"ok": true}))
        }
        // ── device ──
        (Post, ["device", "lock"]) => {
            app.lock_screen();
            Response::ok(&json!({"ok": true}))
        }
        (Post, ["device", "restart"]) => {
            app.hooks().restart();
            Response::ok(&json!({"ok": true}))
        }
        (Get, ["backup"]) => Response::ok(&backup(app)).header(
            "Content-Disposition",
            format!("attachment; filename=\"xbloom-screen-backup-{}.json\"", crate::util::now_unix()),
        ),
        (Post, ["restore"]) => {
            let b: Backup = read_json(req)?;
            restore(app, b).map_err(|e| Response::error(400, e))?;
            Response::ok(&json!({"ok": true}))
        }
        _ => return Err(Response::error(404, "no such endpoint")),
    })
}

fn read_body_is_empty(req: &dyn HttpRequest) -> bool {
    req.header("content-length").is_some_and(|v| v.trim() == "0")
}

fn get_recipe(app: &App, id: &str) -> Result<StoredRecipe, Response> {
    app.recipes.lock().unwrap().get(id).ok_or_else(|| Response::error(404, "no such recipe"))
}

fn parse_recipe_value(v: Value) -> Result<Recipe, String> {
    serde_json::from_value::<RawRecipe>(v).map_err(|e| e.to_string()).and_then(|r| Recipe::from_raw(r).map_err(|e| e.0))
}

#[derive(Deserialize)]
struct RecipeBody {
    recipe: Value,
    meta: Option<RecipeMeta>,
}

impl RecipeBody {
    fn recipe(&self) -> Result<Recipe, Response> {
        parse_recipe_value(self.recipe.clone()).map_err(|e| Response::error(400, e))
    }
}

fn info(app: &App, req: &dyn HttpRequest) -> Response {
    let s = app.settings();
    // Lets the panel skip straight to the dashboard (or the login) without probing.
    let authed = session_token(req).is_some_and(|t| app.auth.lock().unwrap().touch_session(&t));
    let storage = app.hooks().storage().map(|(free, total)| json!({"free": free, "total": total}));
    Response::ok(&json!({
        "device_name": s.device_name,
        "version": app.platform.version,
        "platform": app.platform.name,
        "setup_required": app.needs_setup(),
        "screen": {"width": app.platform.screen.0, "height": app.platform.screen.1},
        "video": app.platform.video,
        "max_upload": app.platform.max_upload,
        "can_rotate": app.platform.can_rotate,
        "storage": storage,
        "simulator": s.machine.simulator,
        "theme": s.appearance.theme,
        "accent": s.appearance.accent,
        "web_locked_s": app.auth.lock().unwrap().locked_for(Channel::Web).map(|d| d.as_secs()),
        "authed": authed,
    }))
}

fn setup(app: &App, req: &mut dyn HttpRequest) -> Response {
    #[derive(Deserialize)]
    struct B {
        pin: String,
    }
    let b: B = match read_json(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    if let Err(e) = app.setup_admin_pin(&b.pin) {
        return Response::error(409, e);
    }
    let tok = app.auth.lock().unwrap().new_session();
    Response::ok(&json!({"ok": true, "token": tok})).header("Set-Cookie", session_cookie(&tok))
}

fn login(app: &App, req: &mut dyn HttpRequest) -> Response {
    #[derive(Deserialize)]
    struct B {
        pin: String,
    }
    let b: B = match read_json(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    if app.needs_setup() {
        return Response::error(409, "set an admin PIN first");
    }
    match app.check_pin(Channel::Web, PinPurpose::Admin, &b.pin) {
        Attempt::Ok => {
            let tok = app.auth.lock().unwrap().new_session();
            Response::ok(&json!({"ok": true, "token": tok})).header("Set-Cookie", session_cookie(&tok))
        }
        a @ Attempt::Wrong { remaining } => {
            Response::json(401, &json!({"error": attempt_ok(a).unwrap_err(), "remaining": remaining}))
        }
        a @ Attempt::Locked { retry_after } => Response::json(
            429,
            &json!({"error": attempt_ok(a).unwrap_err(), "retry_after_s": retry_after.as_secs().max(1)}),
        )
        .header("Retry-After", retry_after.as_secs().max(1).to_string()),
    }
}

/// Exported configuration (no PINs, no Wi-Fi password).
#[derive(Debug, Serialize, Deserialize)]
struct Backup {
    format: String,
    settings: Value,
    recipes: Vec<StoredRecipe>,
    presets: [Option<String>; 3],
}

fn backup(app: &App) -> Backup {
    let mut settings = app.settings().public();
    if let Some(o) = settings.as_object_mut() {
        o.remove("wifi");
        o.remove("setup_complete");
    }
    Backup {
        format: "xbloom-screen-backup/1".into(),
        settings,
        recipes: app.recipes.lock().unwrap().list(),
        presets: app.presets.lock().unwrap().presets.slots.clone(),
    }
}

fn restore(app: &App, b: Backup) -> Result<(), String> {
    if !b.format.starts_with("xbloom-screen-backup/") {
        return Err("not an xBloom Screen backup".into());
    }
    let mut settings = b.settings;
    // Only fields a patch may set; derived "has_*" flags are dropped.
    if let Some(sec) = settings.get_mut("security").and_then(Value::as_object_mut) {
        sec.retain(|k, _| matches!(k.as_str(), "lock_mode" | "auto_lock_s" | "allow_remote_start" | "hold_to_brew_ms"));
    }
    if let Some(m) = settings.get_mut("machine").and_then(Value::as_object_mut) {
        m.retain(|k, _| matches!(k.as_str(), "auto_connect" | "simulator" | "preset_scale"));
    }
    if let Some(o) = settings.as_object_mut() {
        o.retain(|k, _| {
            matches!(k.as_str(), "device_name" | "timezone" | "appearance" | "screensaver" | "security" | "machine")
        });
    }
    let patch: SettingsPatch = serde_json::from_value(settings).map_err(|e| format!("settings: {e}"))?;
    app.recipes.lock().unwrap().replace_all(b.recipes)?;
    app.update_settings(|s| {
        s.apply(patch);
    });
    let _ = app.set_presets(b.presets);
    app.changed(Data::Recipes);
    Ok(())
}

fn serve_asset(req: &dyn HttpRequest, path: &str) -> Response {
    let path = if path == "/" { "/index.html" } else { path };
    let Some(asset) = ASSETS.iter().find(|a| a.path == path) else {
        return Response::error(404, "not found");
    };
    if req.header("if-none-match").as_deref() == Some(asset.etag) {
        return Response {
            status: 304,
            content_type: asset.mime,
            headers: vec![("ETag", asset.etag.into())],
            body: Body::Empty,
        };
    }
    Response {
        status: 200,
        content_type: asset.mime,
        headers: vec![
            ("Content-Encoding", "gzip".into()),
            ("ETag", asset.etag.into()),
            ("Cache-Control", "no-cache".into()),
            ("X-Content-Type-Options", "nosniff".into()),
            ("Referrer-Policy", "no-referrer".into()),
            (
                "Content-Security-Policy",
                "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'".into(),
            ),
        ],
        body: Body::Static(asset.gz),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_app;

    struct Req {
        method: Method,
        path: String,
        query: String,
        headers: Vec<(String, String)>,
        body: std::io::Cursor<Vec<u8>>,
    }

    impl Req {
        fn new(method: Method, url: &str, body: &str) -> Self {
            let (path, query) = url.split_once('?').unwrap_or((url, ""));
            Self {
                method,
                path: path.into(),
                query: query.into(),
                headers: vec![("x-xbloom".into(), "1".into())],
                body: std::io::Cursor::new(body.as_bytes().to_vec()),
            }
        }
        fn auth(mut self, tok: &str) -> Self {
            self.headers.push(("authorization".into(), format!("Bearer {tok}")));
            self
        }
    }

    impl HttpRequest for Req {
        fn method(&self) -> Method {
            self.method
        }
        fn path(&self) -> &str {
            &self.path
        }
        fn query(&self) -> &str {
            &self.query
        }
        fn header(&self, name: &str) -> Option<String> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
        }
        fn body(&mut self) -> &mut dyn Read {
            &mut self.body
        }
    }

    fn body_json(r: &Response) -> Value {
        match &r.body {
            Body::Bytes(b) => serde_json::from_slice(b).unwrap(),
            other => panic!("not a JSON body: {other:?}"),
        }
    }

    #[test]
    fn setup_login_and_crud_flow() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), false);
        // Public info says setup is needed; authed endpoints refuse.
        let info = handle(&app, &mut Req::new(Method::Get, "/api/info", ""));
        assert_eq!(body_json(&info)["setup_required"], true);
        assert_eq!(handle(&app, &mut Req::new(Method::Get, "/api/recipes", "")).status, 401);
        // CSRF header is required on mutations.
        let mut no_hdr = Req::new(Method::Post, "/api/setup", r#"{"pin":"2580"}"#);
        no_hdr.headers.clear();
        assert_eq!(handle(&app, &mut no_hdr).status, 403);
        let r = handle(&app, &mut Req::new(Method::Post, "/api/setup", r#"{"pin":"2580"}"#));
        assert_eq!(r.status, 200);
        assert!(r.headers.iter().any(|(k, v)| *k == "Set-Cookie" && v.contains("HttpOnly")));
        assert_eq!(handle(&app, &mut Req::new(Method::Post, "/api/setup", r#"{"pin":"1111"}"#)).status, 409);
        // Wrong then right PIN.
        let bad = handle(&app, &mut Req::new(Method::Post, "/api/login", r#"{"pin":"0000"}"#));
        assert_eq!(bad.status, 401);
        let ok = handle(&app, &mut Req::new(Method::Post, "/api/login", r#"{"pin":"2580"}"#));
        let tok = body_json(&ok)["token"].as_str().unwrap().to_string();
        // Settings never leak secrets.
        let s = handle(&app, &mut Req::new(Method::Get, "/api/settings", "").auth(&tok));
        let text = String::from_utf8(match s.body {
            Body::Bytes(b) => b,
            _ => vec![],
        })
        .unwrap();
        assert!(!text.contains("salt") && text.contains("has_admin_pin"));
        // Recipes: list, import YAML, validate, export.
        let list = handle(&app, &mut Req::new(Method::Get, "/api/recipes", "").auth(&tok));
        assert_eq!(body_json(&list).as_array().unwrap().len(), 3);
        let yaml = "name: Kenya\ndose_g: 15\ngrind: 50\npours:\n  - {ml: 45, temp_c: 93, rpm: 100}\n  - {ml: 195, temp_c: 92, rpm: 100}\n";
        let imp = handle(&app, &mut Req::new(Method::Post, "/api/recipes/import", yaml).auth(&tok));
        assert_eq!(body_json(&imp)["imported"], 1);
        let v = handle(
            &app,
            &mut Req::new(Method::Post, "/api/recipes/validate", r#"{"name":"x","dose_g":30,"grind":1,"pours":[]}"#)
                .auth(&tok),
        );
        assert_eq!(body_json(&v)["ok"], false);
        let id = body_json(&handle(&app, &mut Req::new(Method::Get, "/api/recipes", "").auth(&tok)))[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let y = handle(&app, &mut Req::new(Method::Get, &format!("/api/recipes/{id}/export.yaml"), "").auth(&tok));
        assert_eq!(y.content_type, "text/yaml; charset=utf-8");
        // Presets need all three.
        let p = handle(&app, &mut Req::new(Method::Post, "/api/presets/push", "").auth(&tok));
        assert_eq!(p.status, 400);
        // No brew-start endpoint exists.
        assert_eq!(handle(&app, &mut Req::new(Method::Post, "/api/brew/start", "{}").auth(&tok)).status, 404);
        // Logout kills the session.
        handle(&app, &mut Req::new(Method::Post, "/api/logout", "").auth(&tok));
        assert_eq!(handle(&app, &mut Req::new(Method::Get, "/api/status", "").auth(&tok)).status, 401);
        app.shutdown();
    }

    #[test]
    fn admin_panel_is_served_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path(), false);
        let r = handle(&app, &mut Req::new(Method::Get, "/", ""));
        assert_eq!(r.status, 200);
        assert!(r.headers.iter().any(|(k, v)| *k == "Content-Encoding" && v == "gzip"));
        let etag = r.headers.iter().find(|(k, _)| *k == "ETag").unwrap().1.clone();
        let mut again = Req::new(Method::Get, "/", "");
        again.headers.push(("if-none-match".into(), etag));
        assert_eq!(handle(&app, &mut again).status, 304);
        app.shutdown();
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("my%20clip.xbv"), "my clip.xbv");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
