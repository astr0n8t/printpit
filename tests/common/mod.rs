//! Test harness: an app on a temp database plus a tiny cookie-tracking client.
#![allow(dead_code)]

pub mod moonraker;

use std::collections::HashMap;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

use printpit::auth::{Role, password};
use printpit::config::Config;
use printpit::db::users;
use printpit::state::AppState;
use printpit::web;

pub const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
pub const ADMIN: &str = "admin";
pub const ADMIN_PW: &str = "correct horse battery";

pub struct TestApp {
    pub state: AppState,
    pub router: Router,
    _dir: TempDir,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_env(&[("PRINTPIT_SECURE_COOKIES", "false")]).await
    }

    pub async fn with_env(extra: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap().to_string();
        let mut vars: HashMap<String, String> = HashMap::from([
            ("PRINTPIT_SECRET_KEY".into(), KEY.into()),
            ("PRINTPIT_DATA_DIR".into(), data_dir),
        ]);
        for (k, v) in extra {
            vars.insert(k.to_string(), v.to_string());
        }
        let config = Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
        let state = AppState::init(config).await.unwrap();
        let router = web::router(state.clone());
        Self {
            state,
            router,
            _dir: dir,
        }
    }

    /// An app whose first admin already exists, plus a client logged in as that admin.
    pub async fn with_admin() -> (Self, Client) {
        Self::with_admin_env(&[("PRINTPIT_SECURE_COOKIES", "false")]).await
    }

    pub async fn with_admin_env(env: &[(&str, &str)]) -> (Self, Client) {
        let app = Self::with_env(env).await;
        let mut c = app.client();
        c.setup(ADMIN, ADMIN_PW).await;
        (app, c)
    }

    pub fn client(&self) -> Client {
        Client {
            router: self.router.clone(),
            state: self.state.clone(),
            cookies: HashMap::new(),
        }
    }

    pub async fn create_user(&self, name: &str, pw: &str, role: Role) -> i64 {
        let hash = password::hash(pw).unwrap();
        users::create(&self.state.db, name, &hash, role).await.unwrap()
    }

    pub async fn scalar(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.state.db).await.unwrap()
    }

    pub async fn exec(&self, sql: &'static str) {
        sqlx::query(sql).execute(&self.state.db).await.unwrap();
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Resp {
    pub fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    pub fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    }

    /// The `_csrf` hidden-field value on the page.
    pub fn csrf(&self) -> String {
        let marker = r#"name="_csrf" value=""#;
        let start = self.body.find(marker).expect("page has a _csrf field") + marker.len();
        self.body[start..].split('"').next().unwrap().to_string()
    }
}

/// An open SSE response, read event by event.
pub struct SseStream {
    pub status: StatusCode,
    body: Body,
    buf: String,
}

impl SseStream {
    /// The raw body, for non-SSE streams such as MJPEG.
    pub fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    /// The next event's `data` (multi-line data joined with `\n`), or `None` on timeout/end.
    pub async fn next_event(&mut self, timeout: std::time::Duration) -> Option<String> {
        loop {
            if let Some(idx) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..idx + 2).collect();
                let data: Vec<&str> = raw
                    .lines()
                    .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
                    .collect();
                if data.is_empty() {
                    continue; // keep-alive comment
                }
                return Some(data.join("\n"));
            }
            let frame = tokio::time::timeout(timeout, self.body.frame())
                .await
                .ok()??
                .ok()?;
            if let Some(bytes) = frame.data_ref() {
                self.buf.push_str(&String::from_utf8_lossy(bytes));
            }
        }
    }

    /// Reads until an event for `target` (e.g. `live-1`) whose HTML contains `needle`.
    pub async fn wait_for(&mut self, target: &str, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let Some(event) = self.next_event(std::time::Duration::from_secs(10)).await else {
                break;
            };
            if let Some((id, html)) = event.split_once('\n')
                && id == target
                && html.contains(needle)
            {
                return html.to_string();
            }
        }
        panic!("no `{target}` event containing `{needle}`");
    }
}

pub struct Client {
    router: Router,
    state: AppState,
    pub cookies: HashMap<String, String>,
}

impl Client {
    pub async fn send(
        &mut self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<String>,
    ) -> Resp {
        let mut req = Request::builder().method(method).uri(uri);
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            req = req.header(header::COOKIE, cookie);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(b)),
            None => req.body(Body::empty()),
        };
        let res = self.router.clone().oneshot(req.unwrap()).await.unwrap();
        let resp = Resp {
            status: res.status(),
            headers: res.headers().clone(),
            body: String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned(),
        };
        for raw in resp.set_cookies() {
            let c = cookie::Cookie::parse(raw).unwrap();
            if c.max_age().is_some_and(|a| a.is_zero()) || c.value().is_empty() {
                self.cookies.remove(c.name());
            } else {
                self.cookies.insert(c.name().to_string(), c.value().to_string());
            }
        }
        resp
    }

    /// A multipart/form-data POST (htmx-style, with the CSRF header). `file` is
    /// `(filename, bytes)` and is sent last, like the upload form.
    pub async fn post_multipart(
        &mut self,
        uri: &str,
        csrf: &str,
        fields: &[(&str, &str)],
        file: Option<(&str, &[u8])>,
    ) -> Resp {
        const B: &str = "printpit-test-boundary";
        let mut body = Vec::new();
        for (k, v) in fields {
            body.extend_from_slice(
                format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
            );
        }
        if let Some((name, data)) = file {
            body.extend_from_slice(
                format!("--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{B}--\r\n").as_bytes());
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={B}"))
            .header("hx-request", "true")
            .header("x-csrf-token", csrf);
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            req = req.header(header::COOKIE, cookie);
        }
        let res = self
            .router
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        Resp {
            status: res.status(),
            headers: res.headers().clone(),
            body: String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned(),
        }
    }

    /// Opens a streaming response (SSE) without reading it to the end.
    pub async fn open_stream(&mut self, uri: &str) -> SseStream {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::ACCEPT, "text/event-stream");
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            req = req.header(header::COOKIE, cookie);
        }
        let res = self
            .router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        SseStream {
            status: res.status(),
            body: res.into_body(),
            buf: String::new(),
        }
    }

    pub async fn get(&mut self, uri: &str) -> Resp {
        self.send(Method::GET, uri, &[], None).await
    }

    pub async fn delete_hx(&mut self, uri: &str, csrf: &str) -> Resp {
        self.send(
            Method::DELETE,
            uri,
            &[("hx-request", "true"), ("x-csrf-token", csrf)],
            None,
        )
        .await
    }

    /// An htmx form post with the session CSRF header.
    pub async fn post_hx(&mut self, uri: &str, csrf: &str, fields: &[(&str, &str)]) -> Resp {
        self.post_with(uri, fields, &[("hx-request", "true"), ("x-csrf-token", csrf)])
            .await
    }

    pub async fn get_hx(&mut self, uri: &str) -> Resp {
        self.send(Method::GET, uri, &[("hx-request", "true")], None).await
    }

    pub async fn post(&mut self, uri: &str, fields: &[(&str, &str)]) -> Resp {
        self.post_with(uri, fields, &[]).await
    }

    pub async fn post_with(&mut self, uri: &str, fields: &[(&str, &str)], headers: &[(&str, &str)]) -> Resp {
        let body = serde_urlencoded::to_string(fields).unwrap();
        self.send(Method::POST, uri, headers, Some(body)).await
    }

    /// Completes first-run setup and leaves this client logged in.
    pub async fn setup(&mut self, username: &str, pw: &str) -> Resp {
        let page = self.get("/setup").await;
        let token = self.state.setup.token().expect("setup token");
        let res = self
            .post(
                "/setup",
                &[
                    ("_csrf", &page.csrf()),
                    ("token", &token),
                    ("username", username),
                    ("password", pw),
                    ("confirm", pw),
                ],
            )
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "setup failed: {}", res.body);
        res
    }

    pub async fn login(&mut self, username: &str, pw: &str) -> Resp {
        self.login_next(username, pw, None).await
    }

    pub async fn login_next(&mut self, username: &str, pw: &str, next: Option<&str>) -> Resp {
        let page = self.get("/login").await;
        let csrf = page.csrf();
        let mut fields = vec![("_csrf", csrf.as_str()), ("username", username), ("password", pw)];
        if let Some(n) = next {
            fields.push(("next", n));
        }
        self.post("/login", &fields).await
    }

    /// The session's CSRF token, read from a page.
    pub async fn session_csrf(&mut self) -> String {
        self.get("/ui/account").await.csrf()
    }
}
