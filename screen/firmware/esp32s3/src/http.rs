//! The admin web server: ESP-IDF's `esp_http_server` adapted to the shared
//! [`xbloom_app::api`] router — the same API and the same embedded admin panel as the
//! Raspberry Pi app.
//!
//! Uploads stream from the socket straight onto the data partition. While the setup hotspot is
//! on, requests for other host names (phones probing for a captive portal) are redirected to
//! the panel, so it opens as a sign-in page right after joining the hotspot.

use std::io::{self, Read};
use std::sync::Arc;

use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method as HttpMethod;
use esp_idf_svc::io::{EspIOError, Write as _};
use esp_idf_svc::sys;
use xbloom_app::api::{self, Body, HttpRequest, Method};
use xbloom_app::App;

use crate::net::{Network, AP_IP};

type EspRequest<'r, 'c> = Request<&'r mut EspHttpConnection<'c>>;

struct Req<'a, 'r, 'c> {
    method: Method,
    path: String,
    query: String,
    inner: &'a mut EspRequest<'r, 'c>,
}

impl Read for Req<'_, '_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).map_err(|e| io::Error::other(e.to_string()))
    }
}

impl HttpRequest for Req<'_, '_, '_> {
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
        self.inner.header(name).map(str::to_string)
    }
    fn body(&mut self) -> &mut dyn Read {
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// True for a request addressed to the screen itself (an IP literal, optionally with a port).
fn is_own_host(host: &str) -> bool {
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    name.parse::<std::net::Ipv4Addr>().is_ok()
}

fn serve(app: &App, net: &Network, mut request: EspRequest<'_, '_>) -> Result<(), EspIOError> {
    if net.hotspot_on() && request.header("Host").is_some_and(|h| !is_own_host(h)) {
        let location = format!("http://{AP_IP}/");
        request.into_response(302, Some("Found"), &[("Location", location.as_str()), ("Cache-Control", "no-store")])?;
        return Ok(());
    }
    let method = match request.method() {
        HttpMethod::Get | HttpMethod::Head => Method::Get,
        HttpMethod::Post => Method::Post,
        HttpMethod::Put => Method::Put,
        HttpMethod::Delete => Method::Delete,
        _ => Method::Other,
    };
    let uri = request.uri().to_string();
    let (path, query) = uri.split_once('?').unwrap_or((&uri, ""));
    let mut req = Req { method, path: path.to_string(), query: query.to_string(), inner: &mut request };
    let res = api::handle(app, &mut req);

    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", res.content_type)];
    headers.extend(res.headers.iter().map(|(k, v)| (*k, v.as_str())));
    let mut out = request.into_response(res.status, Some(reason(res.status)), &headers)?;
    match res.body {
        Body::Empty => {}
        Body::Bytes(b) => out.write_all(&b)?,
        Body::Static(b) => out.write_all(b)?,
        Body::File(p) => {
            let mut f = match std::fs::File::open(&p) {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("http: {}: {e}", p.display());
                    return Ok(());
                }
            };
            let mut buf = vec![0u8; 8 * 1024];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.write_all(&buf[..n])?,
                    Err(e) => {
                        log::warn!("http: reading {}: {e}", p.display());
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Start the admin server on port 80. Keep the returned server alive.
pub fn start(app: Arc<App>, net: Arc<Network>) -> Result<EspHttpServer<'static>, EspIOError> {
    let conf = Configuration {
        http_port: 80,
        // The API parses JSON/YAML and hashes PINs: give the handler task room, in PSRAM.
        stack_size: 20 * 1024,
        task_caps: sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT,
        max_open_sockets: 7,
        max_uri_handlers: 8,
        max_resp_headers: 16,
        lru_purge_enable: true,
        uri_match_wildcard: true,
        ..Default::default()
    };
    let mut server = EspHttpServer::new(&conf)?;
    for method in [HttpMethod::Get, HttpMethod::Head, HttpMethod::Post, HttpMethod::Put, HttpMethod::Delete] {
        let (app, net) = (Arc::clone(&app), Arc::clone(&net));
        server.fn_handler("/*", method, move |req| serve(&app, &net, req))?;
    }
    log::info!("http: admin panel on port 80");
    Ok(server)
}
