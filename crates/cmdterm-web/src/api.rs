//! The HTTP routes: the web app's files, login, the account, machines and
//! terminals. The crate README lists every route.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post};
use cmdterm_remote::keys::PublicKey;
use cmdterm_remote::proto::{Msg, OpenSpec};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{self, hash_token, random_token, same_origin};
use crate::config::Provider;
use crate::store::{BrowserSession, Identity, Machine, User};
use crate::{ASSETS, App, Pairing, now, oidc, random_id};

type Shared = State<Arc<App>>;

/// How long a pairing code works.
const PAIRING_FOR: Duration = Duration::from_secs(10 * 60);
/// Letters of pairing codes: no 0, O, 1, I or L to misread.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// How often an idle terminal WebSocket is pinged, so proxies keep it.
const PING_EVERY: Duration = Duration::from_secs(30);
/// Largest message a browser may send on a terminal.
const MAX_INPUT: usize = 1 << 20;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/auth/options", get(options))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/auth/token", get(dev_page))
        .route("/api/auth/token", post(dev_login))
        .route("/auth/oidc/{provider}/start", get(oidc_start))
        .route("/auth/oidc/{provider}/callback", get(oidc_callback))
        .route("/api/me", get(me))
        .route("/api/me/password", post(set_password))
        .route("/api/me/identities/{provider}", delete(remove_identity))
        .route("/api/machines", get(machines))
        .route("/api/machines/pair", post(pair))
        .route(
            "/api/machines/{id}",
            patch(rename_machine).delete(remove_machine),
        )
        .route("/api/machines/{id}/projects", get(projects))
        .route("/api/machines/{id}/sessions", get(sessions))
        .route("/api/machines/{id}/sessions/{name}", delete(end_session))
        .route("/api/machines/{id}/terminal", get(terminal))
        .route("/v1/link/redeem", post(redeem))
        .fallback(asset)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// An error answer: `{"error": message}`.
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("scriptschnell-term-web: {error}");
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "something went wrong on the server",
    )
}

/// Refuses cross-site changes and adds the security headers to every
/// answer.
async fn guard(State(app): Shared, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let changes = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if path.starts_with("/api/") && changes && !same_origin(request.headers()) {
        return ApiError::new(StatusCode::FORBIDDEN, "requests must come from this site")
            .into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // Older Safari does not count WebSockets to the same host as 'self'.
    let public = app.config.public_url();
    let socket = public
        .strip_prefix("https://")
        .map(|host| format!("wss://{host}"))
        .or_else(|| {
            public
                .strip_prefix("http://")
                .map(|host| format!("ws://{host}"))
        })
        .unwrap_or_default();
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src 'self' {socket}; manifest-src 'self'; \
         worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if app.config.secure() {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

/// The files of the web app.
async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some((_, bytes)) = ASSETS.iter().find(|(name, _)| *name == path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let kind = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webmanifest") => "application/manifest+json",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            // The service worker keeps its own copies; the browser asks
            // again each time so a new version shows.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        *bytes,
    )
        .into_response()
}

// --- login -------------------------------------------------------------------

/// The logged-in user. Answers 401 for everyone else.
pub struct CurrentUser(pub User);

impl FromRequestParts<Arc<App>> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, ApiError> {
        signed_in(&parts.headers, app)
            .map(CurrentUser)
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "log in first"))
    }
}

fn signed_in(headers: &HeaderMap, app: &App) -> Option<User> {
    let token = auth::session_token(headers, app.config.secure())?;
    app.store.session_user(&hash_token(&token))
}

/// Starts a browser session for `user` and returns its cookie.
fn start_session(app: &App, user: &str) -> Result<HeaderValue, ApiError> {
    let token = random_token();
    let now = now();
    app.store
        .write(|d| {
            d.sessions.push(BrowserSession {
                hash: hash_token(&token),
                user: user.to_owned(),
                created: now,
                last_used: now,
            })
        })
        .map_err(internal)?;
    Ok(auth::session_cookie(app.config.secure(), &token))
}

async fn options(State(app): Shared) -> Json<serde_json::Value> {
    let providers: Vec<_> = app
        .config
        .oidc
        .iter()
        .map(|p| json!({"id": p.id, "name": p.name}))
        .collect();
    Json(json!({
        "password": app.config.password_login,
        "providers": providers,
        "dev": app.dev_token.is_some(),
    }))
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}

async fn login(
    State(app): Shared,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Login>,
) -> Result<Response, ApiError> {
    if !app.config.password_login {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "password login is off",
        ));
    }
    let keys = [
        format!("user:{}", body.username.trim().to_lowercase()),
        format!("ip:{}", peer.ip()),
    ];
    if let Some(wait) = app.throttle.wait(&keys) {
        let secs = wait.as_secs().max(1);
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, secs.to_string())],
            Json(json!({"error": "too many failed logins, wait a moment", "retry_after": secs})),
        )
            .into_response());
    }
    let user = app.store.user_by_name(body.username.trim());
    let hash = user.as_ref().and_then(|u| u.password.clone());
    let password = body.password;
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&password, hash.as_deref()))
        .await
        .map_err(internal)?;
    let Some(user) = user.filter(|_| ok) else {
        app.throttle.failed(&keys);
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "wrong user name or password",
        ));
    };
    app.throttle.succeeded(&keys[..1]);
    let cookie = start_session(&app, &user.id)?;
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"name": user.name})),
    )
        .into_response())
}

async fn logout(State(app): Shared, headers: HeaderMap) -> Result<Response, ApiError> {
    let secure = app.config.secure();
    if let Some(token) = auth::session_token(&headers, secure) {
        let hash = hash_token(&token);
        app.store
            .write(|d| d.sessions.retain(|s| s.hash != hash))
            .map_err(internal)?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, auth::clear_cookie(secure))],
    )
        .into_response())
}

#[derive(Deserialize)]
struct DevToken {
    token: String,
}

/// The `--dev` login page. The token rides in the URL fragment
/// (`/auth/token#token=...`), which a GET never sends to the server, so it
/// stays out of logs and history; the page then POSTs it.
async fn dev_page() -> Response {
    let Some((_, bytes)) = ASSETS.iter().find(|(name, _)| *name == "token.html") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        *bytes,
    )
        .into_response()
}

/// `--dev` logs in with the token the login page POSTs. The same-origin
/// guard covers this route, so only the login page itself can call it.
async fn dev_login(State(app): Shared, Json(body): Json<DevToken>) -> Result<Response, ApiError> {
    let Some(expected) = &app.dev_token else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "dev login is off"));
    };
    if !auth::equal(hash_token(&body.token).as_bytes(), expected.as_bytes()) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "that login link is not valid",
        ));
    }
    let user = match app.store.user_by_name("dev") {
        Some(user) => user,
        None => {
            let user = User {
                id: random_id(),
                name: "dev".into(),
                password: None,
                identities: Vec::new(),
                created: now(),
            };
            app.store
                .write(|d| d.users.push(user.clone()))
                .map_err(internal)?;
            user
        }
    };
    let cookie = start_session(&app, &user.id)?;
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({"name": user.name})),
    )
        .into_response())
}

/// A safe account name from what the sign-in provider sent: no control
/// characters, at most 64 characters, never empty.
fn account_name(base: &str) -> String {
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(64).collect();
    if cleaned.trim().is_empty() {
        "user".to_owned()
    } else {
        cleaned
    }
}

fn provider<'a>(app: &'a App, id: &str) -> Result<&'a Provider, ApiError> {
    app.config
        .oidc
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such sign-in provider"))
}

fn login_error(message: &str) -> Response {
    Redirect::to(&format!("/#login?error={}", oidc::encode(message))).into_response()
}

async fn oidc_start(
    State(app): Shared,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    provider(&app, &id)?;
    let link_user = signed_in(&headers, &app).map(|u| u.id);
    let worker = app.clone();
    let url = tokio::task::spawn_blocking(move || {
        let provider = provider(&worker, &id).map_err(|e| e.1)?;
        worker.oidc.start(&worker.config, provider, link_user)
    })
    .await
    .map_err(internal)?;
    match url {
        Ok(url) => Ok(Redirect::to(&url).into_response()),
        Err(error) => {
            eprintln!("scriptschnell-term-web: sign-in: {error}");
            Ok(login_error("the sign-in provider cannot be reached"))
        }
    }
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn oidc_callback(
    State(app): Shared,
    Path(id): Path<String>,
    Query(query): Query<Callback>,
) -> Result<Response, ApiError> {
    let provider = provider(&app, &id)?.clone();
    if let Some(error) = query.error {
        return Ok(login_error(
            query.error_description.as_deref().unwrap_or(&error),
        ));
    }
    let (Some(code), Some(state)) = (query.code, query.state) else {
        return Ok(login_error("the sign-in came back incomplete"));
    };
    let Some(pending) = app.oidc.take(&state).filter(|p| p.provider == provider.id) else {
        return Ok(login_error("that sign-in expired, try again"));
    };
    let worker = app.clone();
    let finish_provider = provider.clone();
    let finish_pending = pending.clone();
    let claims = tokio::task::spawn_blocking(move || {
        worker
            .oidc
            .finish(&worker.config, &finish_provider, &finish_pending, &code)
    })
    .await
    .map_err(internal)?;
    let claims = match claims {
        Ok(claims) => claims,
        Err(error) => {
            eprintln!(
                "scriptschnell-term-web: sign-in with {}: {error}",
                provider.id
            );
            return Ok(login_error("the sign-in failed"));
        }
    };
    let identity = Identity {
        provider: provider.id.clone(),
        issuer: claims.issuer.clone(),
        subject: claims.subject.clone(),
        email: claims.email.clone(),
    };
    let owner = app.store.read(|d| {
        d.users
            .iter()
            .find(|u| {
                u.identities
                    .iter()
                    .any(|i| i.issuer == identity.issuer && i.subject == identity.subject)
            })
            .map(|u| u.id.clone())
    });

    // A logged-in user connects another identity.
    if let Some(user) = pending.link_user {
        if owner.as_ref().is_some_and(|o| *o != user) {
            return Ok(Redirect::to(&format!(
                "/#account?error={}",
                oidc::encode("that identity belongs to another account")
            ))
            .into_response());
        }
        if owner.is_none() {
            app.store
                .write(|d| {
                    if let Some(u) = d.users.iter_mut().find(|u| u.id == user) {
                        u.identities.push(identity);
                    }
                })
                .map_err(internal)?;
        }
        return Ok(Redirect::to("/#account").into_response());
    }

    let user = match owner {
        Some(user) => user,
        None => {
            let email = claims.email.as_deref().filter(|_| claims.email_verified);
            let Some(email) = email.filter(|e| oidc::allowed(&provider.allow, e)) else {
                return Ok(login_error("this account may not sign in here"));
            };
            let base = account_name(claims.preferred_username.as_deref().unwrap_or(email));
            app.store
                .write(|d| {
                    let mut name = base.clone();
                    let mut n = 2;
                    while d.users.iter().any(|u| u.name.eq_ignore_ascii_case(&name)) {
                        name = format!("{base}-{n}");
                        n += 1;
                    }
                    let user = User {
                        id: random_id(),
                        name,
                        password: None,
                        identities: vec![identity],
                        created: now(),
                    };
                    let id = user.id.clone();
                    d.users.push(user);
                    id
                })
                .map_err(internal)?
        }
    };
    let cookie = start_session(&app, &user)?;
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to("/#machines")).into_response())
}

// --- account -----------------------------------------------------------------

async fn me(CurrentUser(user): CurrentUser) -> Json<serde_json::Value> {
    let identities: Vec<_> = user
        .identities
        .iter()
        .map(|i| json!({"provider": i.provider, "email": i.email}))
        .collect();
    Json(json!({
        "name": user.name,
        "has_password": user.password.is_some(),
        "identities": identities,
    }))
}

#[derive(Deserialize)]
struct NewPassword {
    #[serde(default)]
    current: Option<String>,
    new: String,
}

async fn set_password(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<NewPassword>,
) -> Result<StatusCode, ApiError> {
    auth::check_new_password(&body.new).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    let keys = [
        format!("user:{}", user.name.to_lowercase()),
        format!("ip:{}", peer.ip()),
    ];
    if let Some(hash) = user.password.clone() {
        if app.throttle.wait(&keys).is_some() {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many failed attempts, wait a moment",
            ));
        }
        let current = body.current.unwrap_or_default();
        let ok = tokio::task::spawn_blocking(move || auth::verify_password(&current, Some(&hash)))
            .await
            .map_err(internal)?;
        if !ok {
            app.throttle.failed(&keys);
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "the current password is wrong",
            ));
        }
    }
    let new = body.new;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&new))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    app.store
        .write(|d| {
            if let Some(u) = d.users.iter_mut().find(|u| u.id == user.id) {
                u.password = Some(hash);
            }
        })
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_identity(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path(provider): Path<String>,
) -> Result<StatusCode, ApiError> {
    let left = user
        .identities
        .iter()
        .filter(|i| i.provider != provider)
        .count();
    if left == user.identities.len() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "no such identity"));
    }
    if left == 0 && user.password.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "set a password first, or you could not log in any more",
        ));
    }
    app.store
        .write(|d| {
            if let Some(u) = d.users.iter_mut().find(|u| u.id == user.id) {
                u.identities.retain(|i| i.provider != provider);
            }
        })
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// --- machines ----------------------------------------------------------------

fn machine_json(app: &App, machine: &Machine) -> serde_json::Value {
    json!({
        "id": machine.id,
        "name": machine.name,
        "online": app.hub.online(&machine.id),
        "last_seen": machine.last_seen,
        "fingerprint": PublicKey::parse(&machine.key).map(|k| k.fingerprint()),
        "projects": machine.projects,
        "sessions": machine.sessions,
    })
}

async fn machines(State(app): Shared, CurrentUser(user): CurrentUser) -> Json<serde_json::Value> {
    let list = app.store.read(|d| {
        d.machines
            .iter()
            .filter(|m| m.owner == user.id)
            .map(|m| machine_json(&app, m))
            .collect::<Vec<_>>()
    });
    Json(serde_json::Value::Array(list))
}

fn own_machine(app: &App, user: &User, id: &str) -> Result<Machine, ApiError> {
    app.store
        .machine_of(&user.id, id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such machine"))
}

/// A new pairing code, `XXXX-XXXX-XX`, and what it keeps for redeeming.
fn pairing_code() -> (String, String) {
    let bytes = auth::random_bytes::<10>();
    let plain: String = bytes
        .iter()
        .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
        .collect();
    let shown = format!("{}-{}-{}", &plain[..4], &plain[4..8], &plain[8..]);
    (shown, plain)
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

async fn pair(State(app): Shared, CurrentUser(user): CurrentUser) -> Json<serde_json::Value> {
    let (shown, plain) = pairing_code();
    {
        let mut pairings = app.pairings.lock().unwrap();
        pairings.retain(|_, p| p.until > Instant::now());
        // A few open codes per account are plenty.
        let mine: Vec<String> = pairings
            .iter()
            .filter(|(_, p)| p.user == user.id)
            .map(|(code, _)| code.clone())
            .collect();
        if mine.len() >= 5 {
            for code in mine {
                pairings.remove(&code);
            }
        }
        pairings.insert(
            plain,
            Pairing {
                user: user.id,
                until: Instant::now() + PAIRING_FOR,
            },
        );
    }
    Json(json!({
        "code": shown,
        "expires": now() + PAIRING_FOR.as_secs(),
        "command": format!("scriptschnell-term-remote link {} {shown}", app.config.public_url()),
    }))
}

#[derive(Deserialize)]
struct Redeem {
    code: String,
    name: String,
    key: String,
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "names have 1 to 64 characters",
        ));
    }
    Ok(name.to_owned())
}

/// `scriptschnell-term-remote link`: trades a pairing code for the link.
async fn redeem(
    State(app): Shared,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<Redeem>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let keys = [format!("redeem:{}", peer.ip())];
    if app.throttle.wait(&keys).is_some() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many wrong codes, wait a moment",
        ));
    }
    let name = valid_name(&body.name)?;
    let Some(key) = PublicKey::parse(&body.key) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "that is not a machine key",
        ));
    };
    let pairing = {
        let mut pairings = app.pairings.lock().unwrap();
        pairings
            .remove(&normalize_code(&body.code))
            .filter(|p| p.until > Instant::now())
    };
    let Some(pairing) = pairing else {
        app.throttle.failed(&keys);
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "that code is wrong, used or expired; get a new one in the web app",
        ));
    };
    let id = app
        .store
        .write(|d| {
            let existing = d
                .machines
                .iter_mut()
                .find(|m| PublicKey::parse(&m.key).as_ref() == Some(&key));
            match existing {
                Some(m) if m.owner != pairing.user => Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "this machine is linked to another account; remove it there first",
                )),
                Some(m) => {
                    m.name = name.clone();
                    Ok(m.id.clone())
                }
                None => {
                    let id = random_id();
                    d.machines.push(Machine {
                        id: id.clone(),
                        owner: pairing.user.clone(),
                        name: name.clone(),
                        key: key.to_line(""),
                        created: now(),
                        last_seen: None,
                        projects: Vec::new(),
                        sessions: Vec::new(),
                    });
                    Ok(id)
                }
            }
        })
        .map_err(internal)??;
    Ok(Json(json!({
        "machine": id,
        "server_key": app.hub.key_line(),
        "address": app.config.link_address(),
    })))
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn rename_machine(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(body): Json<Rename>,
) -> Result<StatusCode, ApiError> {
    own_machine(&app, &user, &id)?;
    let name = valid_name(&body.name)?;
    app.store
        .write(|d| {
            if let Some(m) = d.machines.iter_mut().find(|m| m.id == id) {
                m.name = name;
            }
        })
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_machine(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    own_machine(&app, &user, &id)?;
    app.store
        .write(|d| d.machines.retain(|m| m.id != id))
        .map_err(internal)?;
    app.hub.disconnect(&id);
    Ok(StatusCode::NO_CONTENT)
}

fn offline(error: String) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error)
}

async fn projects(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    own_machine(&app, &user, &id)?;
    let list = app.hub.projects(&id).await.map_err(offline)?;
    Ok(Json(json!(list)))
}

async fn sessions(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    own_machine(&app, &user, &id)?;
    let list = app.hub.sessions(&id).await.map_err(offline)?;
    Ok(Json(json!(list)))
}

async fn end_session(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path((id, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    own_machine(&app, &user, &id)?;
    if !app.hub.online(&id) {
        return Err(offline("the machine is offline".into()));
    }
    app.hub
        .end_session(&id, &name)
        .await
        .map_err(|e| ApiError::new(StatusCode::NOT_FOUND, e))?;
    let _ = app.hub.sessions(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

// --- terminals ---------------------------------------------------------------

#[derive(Deserialize)]
struct TerminalQuery {
    session: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default)]
    resume: Option<u64>,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

fn valid_session(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 128 && !name.chars().any(char::is_control)
}

async fn terminal(
    State(app): Shared,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(query): Query<TerminalQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // A page of another site must not reach terminals with this browser's
    // cookie.
    if !same_origin(&headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "requests must come from this site",
        ));
    }
    own_machine(&app, &user, &id)?;
    if !valid_session(&query.session) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "session names have 1 to 128 characters",
        ));
    }
    let link = app
        .hub
        .link(&id)
        .ok_or_else(|| offline("the machine is offline".into()))?;
    let spec = OpenSpec {
        pty: Some((query.cols.clamp(2, 1000), query.rows.clamp(2, 1000))),
        shell: true,
        cwd: query.cwd.unwrap_or_default(),
        session: query.session,
        resume: query.resume,
        ..OpenSpec::default()
    };
    Ok(ws
        .max_message_size(MAX_INPUT)
        .on_upgrade(move |socket| relay(socket, link, spec)))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

/// Passes a terminal between the browser and its session until either
/// side ends. The session keeps running when the browser leaves.
async fn relay(mut socket: WebSocket, link: Arc<crate::hub::Link>, spec: OpenSpec) {
    let mut channel = link.open(spec);
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            msg = channel.rx.recv() => {
                let (message, last) = match msg {
                    Some(Msg::Data(_, data) | Msg::Stderr(_, data)) => (Message::Binary(data.into()), false),
                    Some(Msg::Attached(_, offset, _)) => (text(json!({"type": "attached", "offset": offset})), false),
                    Some(Msg::Exit(_, code)) => (text(json!({"type": "exit", "code": code})), true),
                    Some(Msg::OpenFailed(_, message)) => (text(json!({"type": "error", "message": message})), true),
                    Some(_) => continue,
                    None => (text(json!({"type": "error", "message": "the machine went offline"})), true),
                };
                if socket.send(message).await.is_err() || last {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(data))) => channel.send(Msg::Data(0, data.to_vec())),
                Some(Ok(Message::Text(text))) => {
                    if let Ok(Control::Resize { cols, rows }) = serde_json::from_str(text.as_str()) {
                        channel.send(Msg::Resize(0, cols.clamp(2, 1000), rows.clamp(2, 1000)));
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(value.to_string().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_read_well_and_normalize() {
        let (shown, plain) = pairing_code();
        assert_eq!(shown.len(), 12);
        assert_eq!(normalize_code(&shown), plain);
        assert_eq!(normalize_code(&shown.to_lowercase()), plain);
        assert!(plain.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert!(valid_name("laptop").is_ok());
        assert!(valid_name(" ").is_err());
        assert!(!valid_session("a\nb"));
    }
}
