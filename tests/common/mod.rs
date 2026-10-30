//! Test harness: an app on a temp database plus a tiny cookie-tracking client.
#![allow(dead_code)]

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
        let app = Self::new().await;
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

    pub async fn get(&mut self, uri: &str) -> Resp {
        self.send(Method::GET, uri, &[], None).await
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
