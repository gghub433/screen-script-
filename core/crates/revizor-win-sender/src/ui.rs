//! Local web UI server. Binds to 127.0.0.1 only; every API call must carry the
//! per-run token (injected into the page) and a matching Host header, which
//! stops other web pages from driving the app through the browser.

use crate::controller::{profile_from_str, Controller};
use crate::sources::Source;
use serde_json::json;
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX: &str = include_str!("../ui/index.html");

pub struct Ui {
    pub url: String,
    server: Arc<Server>,
}

pub fn token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    revizor_crypto::identity::hex(&b)
}

impl Ui {
    pub fn bind(port: u16) -> std::io::Result<(Self, u16)> {
        let server = Server::http(format!("127.0.0.1:{port}")).map_err(|e| std::io::Error::other(e.to_string()))?;
        let port = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
        let tok = token();
        Ok((Self { url: format!("http://127.0.0.1:{port}/?t={tok}"), server: Arc::new(server) }, port))
    }

    pub fn token(&self) -> String {
        self.url.rsplit("t=").next().unwrap_or_default().to_string()
    }

    /// Serves requests on the calling thread until the process exits.
    pub fn serve(&self, ctl: Arc<Controller>, sources: Arc<dyn Fn() -> Vec<Source> + Send + Sync>) {
        let tok = self.token();
        let port = self.url.split(':').nth(2).and_then(|p| p.split('/').next()).unwrap_or("0").to_string();
        for req in self.server.incoming_requests() {
            handle(req, &ctl, &sources, &tok, &port);
        }
    }
}

fn json_resp(code: u16, v: serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(v.to_string())
        .with_status_code(code)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap())
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn handle(mut req: Request, ctl: &Arc<Controller>, sources: &Arc<dyn Fn() -> Vec<Source> + Send + Sync>, tok: &str, port: &str) {
    let url = req.url().to_string();
    let host_ok = header(&req, "host").is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        let _ = req.respond(Response::from_string("forbidden").with_status_code(403));
        return;
    }
    let method = req.method().clone();
    if method == Method::Get && (url == "/" || url.starts_with("/?")) {
        let ok = url.contains(&format!("t={tok}"));
        let body = if ok { INDEX.replace("__TOKEN__", tok) } else { "Open Revizor from its own window.".to_string() };
        let _ = req.respond(
            Response::from_string(body)
                .with_status_code(if ok { 200 } else { 403 })
                .with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
                .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap())
                .with_header(Header::from_bytes("Content-Security-Policy", "default-src 'self' 'unsafe-inline'; connect-src 'self'").unwrap()),
        );
        return;
    }
    if header(&req, "x-revizor-token") != Some(tok) {
        let _ = req.respond(json_resp(403, json!({"error": "bad token"})));
        return;
    }
    let mut body = String::new();
    {
        use std::io::Read;
        let mut limited = req.as_reader().take(64 * 1024);
        let _ = limited.read_to_string(&mut body);
    }
    let arg: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let s = |k: &str| arg.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();

    let result: Result<serde_json::Value, String> = match (method, url.as_str()) {
        (Method::Get, "/api/state") => Ok(state(ctl, sources)),
        // Returns immediately; progress is `scanning` in /api/state (a TV scan takes a few seconds).
        (Method::Post, "/api/scan") => Ok(json!({ "started": ctl.scan_async(1500) })),
        (Method::Post, "/api/pair") => ctl.pair(&s("id"), &s("pin")).map(|_| json!({})).map_err(msg),
        (Method::Post, "/api/forget") => Ok(json!({ "removed": ctl.forget(&s("id")) })),
        (Method::Post, "/api/manual") => ctl.add_manual(&s("ip")).map(|_| json!({})).map_err(msg),
        (Method::Post, "/api/stop") => {
            ctl.stop();
            Ok(json!({}))
        }
        (Method::Post, "/api/start") => {
            let list = sources();
            match list.iter().find(|x| x.id == s("source")) {
                Some(src) => ctl.start(&s("receiver"), src, profile_from_str(&s("profile"))).map(|_| json!({})).map_err(msg),
                None => Err("That screen or window is no longer available. Refresh the list.".into()),
            }
        }
        _ => Err("not found".into()),
    };
    let _ = match result {
        Ok(v) => req.respond(json_resp(200, v)),
        Err(e) => req.respond(json_resp(400, json!({ "error": e }))),
    };
}

fn msg(e: crate::controller::ControlError) -> String {
    let crate::controller::ControlError::Msg(m) = e;
    m
}

fn state(ctl: &Controller, sources: &Arc<dyn Fn() -> Vec<Source> + Send + Sync>) -> serde_json::Value {
    json!({
        "device": { "name": ctl.device_name, "id": ctl.device_id() },
        "platform": if cfg!(windows) { "windows" } else { "other" },
        "scanning": ctl.scanning(),
        "receivers": ctl.receivers(),
        "sources": sources(),
        "stream": ctl.stream_view(),
        "trusted": ctl.trusted().into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
    })
}
