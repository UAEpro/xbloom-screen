//! The admin web server: `tiny_http` adapted to the shared [`xbloom_app::api`] router.
//!
//! A few worker threads pull requests off one listener; uploads stream straight to disk, so a
//! large screensaver video never sits in memory.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use tiny_http::{Header, Method as HttpMethod, Request, Response, Server, StatusCode};
use xbloom_app::api::{self, Body, HttpRequest, Method};
use xbloom_app::App;

struct Req<'a> {
    method: Method,
    path: String,
    query: String,
    inner: &'a mut Request,
}

impl HttpRequest for Req<'_> {
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
        self.inner
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|h| h.value.to_string())
    }
    fn body(&mut self) -> &mut dyn Read {
        self.inner.as_reader()
    }
}

fn method(m: &HttpMethod) -> Method {
    match m {
        HttpMethod::Get | HttpMethod::Head => Method::Get,
        HttpMethod::Post => Method::Post,
        HttpMethod::Put => Method::Put,
        HttpMethod::Delete => Method::Delete,
        _ => Method::Other,
    }
}

fn serve(app: &App, mut request: Request) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let mut req = Req { method: method(request.method()), path, query, inner: &mut request };
    let res = api::handle(app, &mut req);
    let mut headers = vec![Header::from_bytes("Content-Type", res.content_type).expect("valid header")];
    for (k, v) in &res.headers {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            headers.push(h);
        }
    }
    let status = StatusCode(res.status);
    let result = match res.body {
        Body::Empty => request.respond(Response::new(status, headers, std::io::empty(), Some(0), None)),
        Body::Bytes(b) => {
            let n = b.len();
            request.respond(Response::new(status, headers, std::io::Cursor::new(b), Some(n), None))
        }
        Body::Static(b) => request.respond(Response::new(status, headers, b, Some(b.len()), None)),
        Body::File(p) => match std::fs::File::open(&p) {
            Ok(f) => {
                let n = f.metadata().map(|m| m.len() as usize).ok();
                request.respond(Response::new(status, headers, f, n, None))
            }
            Err(_) => request.respond(Response::from_string("not found").with_status_code(404)),
        },
    };
    if let Err(e) = result {
        log::debug!("http: client went away: {e}");
    }
}

/// Start the admin server on `addr` with `workers` threads.
pub fn start(app: Arc<App>, addr: SocketAddr, workers: usize) -> std::io::Result<()> {
    let server = Arc::new(Server::http(addr).map_err(std::io::Error::other)?);
    log::info!("admin panel on http://{addr}");
    for i in 0..workers.max(1) {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        thread::Builder::new().name(format!("xbloom-http-{i}")).spawn(move || {
            for request in server.incoming_requests() {
                serve(&app, request);
            }
        })?;
    }
    Ok(())
}
