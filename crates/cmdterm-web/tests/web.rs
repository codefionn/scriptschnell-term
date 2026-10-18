//! End to end: a web server in this process, a `cmdterm-remote` server that
//! links to it with a pairing code, and a browser played by HTTP and
//! WebSocket clients.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cmdterm_remote::Paths;
use cmdterm_remote::server::Server;
use cmdterm_web::config::Config;
use cmdterm_web::hub::Hub;
use cmdterm_web::store::{Store, User};
use cmdterm_web::{App, api, auth, now};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

struct Web {
    url: String,
    host: String,
    agent: ureq::Agent,
    cookie: Option<String>,
}

impl Web {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value, Option<String>) {
        let url = format!("{}{path}", self.url);
        let mut request = ureq::http::Request::builder()
            .method(method)
            .uri(&url)
            .header("Origin", &self.url)
            .header("Content-Type", "application/json");
        if let Some(cookie) = &self.cookie {
            request = request.header("Cookie", cookie);
        }
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut response = self.agent.run(request.body(body).unwrap()).unwrap();
        let cookie = response
            .headers()
            .get("set-cookie")
            .map(|c| c.to_str().unwrap().split(';').next().unwrap().to_owned());
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        (response.status().as_u16(), value, cookie)
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs this test again with `SHELL=/bin/sh`, so the terminal's login
/// shell does not depend on the user's shell setup.
fn with_plain_shell(test: &str) -> bool {
    if std::env::var("SHELL").as_deref() == Ok("/bin/sh") {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env("SHELL", "/bin/sh")
        .env("ENV", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
    true
}

#[test]
fn a_linked_machine_serves_terminals_to_its_account_only() {
    if with_plain_shell("a_linked_machine_serves_terminals_to_its_account_only") {
        return;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();

    let store = Arc::new(Store::open(&dir.path().join("web")).unwrap());
    for name in ["alice", "mallory"] {
        let hash = auth::hash_password("password123").unwrap();
        store
            .write(|d| {
                d.users.push(User {
                    id: name.into(),
                    name: name.into(),
                    password: Some(hash),
                    identities: Vec::new(),
                    created: now(),
                })
            })
            .unwrap();
    }
    let identity = cmdterm_remote::keys::Identity::generate().unwrap();
    let hub = Hub::new(identity, store.clone(), runtime.handle().clone());
    let linked = hub.listen("127.0.0.1:0").unwrap();
    let config = Config {
        listen: address.to_string(),
        link_address: Some(linked.to_string()),
        ..Config::default()
    };
    let app = App::new(config, store, hub, None);
    let router = api::router(app).into_make_service_with_connect_info::<SocketAddr>();
    runtime.spawn(async move { axum::serve(listener, router).await });

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut web = Web {
        url: format!("http://{address}"),
        host: address.to_string(),
        agent,
        cookie: None,
    };

    // Nobody gets in without logging in, and a wrong password locks.
    assert_eq!(web.request("GET", "/api/machines", None).0, 401);
    let (status, _, _) = web.request(
        "POST",
        "/api/auth/login",
        Some(json!({"username": "alice", "password": "wrong"})),
    );
    assert_eq!(status, 401);
    let (status, body, cookie) = web.request(
        "POST",
        "/api/auth/login",
        Some(json!({"username": "Alice", "password": "password123"})),
    );
    assert_eq!(status, 200, "{body}");
    web.cookie = cookie;

    // Pages of other sites cannot change anything.
    let mut foreign = ureq::http::Request::builder()
        .method("POST")
        .uri(format!("{}/api/machines/pair", web.url))
        .header("Origin", "https://evil.example")
        .header("Content-Type", "application/json");
    foreign = foreign.header("Cookie", web.cookie.clone().unwrap());
    let response = web.agent.run(foreign.body(String::new()).unwrap()).unwrap();
    assert_eq!(response.status().as_u16(), 403);

    // Link a machine with a pairing code.
    let (status, pairing, _) = web.request("POST", "/api/machines/pair", Some(json!({})));
    assert_eq!(status, 200, "{pairing}");
    let code = pairing["code"].as_str().unwrap();
    assert!(pairing["command"].as_str().unwrap().contains(code));
    let paths = Paths::new(Some(dir.path().join("machine")));
    std::fs::create_dir_all(&paths.state).unwrap();
    std::fs::write(
        paths.app_state(),
        r#"{"projects": [{"id": 1, "name": "app", "env": {"kind": "local"}, "root": "/srv/app", "vcs": "git"}]}"#,
    )
    .unwrap();
    let link = cmdterm_remote::link::redeem(&paths, &web.url, code, "box", None).unwrap();
    assert_eq!(link.address, linked.to_string());
    // Codes work once.
    assert!(cmdterm_remote::link::redeem(&paths, &web.url, code, "box", None).is_err());

    // A wrong --fingerprint refuses the link before anything is pinned.
    let (_, pairing, _) = web.request("POST", "/api/machines/pair", Some(json!({})));
    let code = pairing["code"].as_str().unwrap();
    let error =
        cmdterm_remote::link::redeem(&paths, &web.url, code, "box", Some("SHA3-256:wrong"))
            .unwrap_err();
    assert!(error.to_string().contains("not linking"), "{error}");
    assert_eq!(cmdterm_remote::link::load(&paths).unwrap().len(), 1);
    // The right fingerprint links.
    let (_, pairing, _) = web.request("POST", "/api/machines/pair", Some(json!({})));
    let code = pairing["code"].as_str().unwrap();
    let expected = link.key().unwrap().fingerprint();
    assert!(
        cmdterm_remote::link::redeem(&paths, &web.url, code, "box", Some(&expected)).is_ok()
    );

    let server = Server::new(paths.clone()).unwrap();
    server.spawn("127.0.0.1:0").unwrap();
    wait_for("the machine to connect", || {
        web.request("GET", "/api/machines", None).1[0]["online"] == true
    });
    let machine = link.machine.clone();
    let (status, projects, _) =
        web.request("GET", &format!("/api/machines/{machine}/projects"), None);
    assert_eq!(status, 200, "{projects}");
    assert_eq!(projects[0]["root"], "/srv/app");

    // A terminal in a session, through the WebSocket.
    let terminal = format!(
        "ws://{}/api/machines/{machine}/terminal?session=t1&cols=80&rows=24",
        web.host
    );
    let open = |origin: &str| {
        let mut request = terminal.as_str().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
        request
            .headers_mut()
            .insert("Cookie", web.cookie.clone().unwrap().parse().unwrap());
        request
    };
    let refused = runtime.block_on(tokio_tungstenite::connect_async(open(
        "https://evil.example",
    )));
    assert!(refused.is_err(), "a foreign page reached the terminal");
    let (mut socket, _) = runtime
        .block_on(tokio_tungstenite::connect_async(open(&web.url)))
        .unwrap();
    let output = runtime.block_on(async {
        let mut output = Vec::new();
        let mut attached = false;
        let typed = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    tungstenite::Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).unwrap();
                        if value["type"] == "attached" && !attached {
                            attached = true;
                            socket
                                .send(tungstenite::Message::Binary(
                                    b"echo hel\"\"lo\r".to_vec().into(),
                                ))
                                .await
                                .unwrap();
                        }
                    }
                    tungstenite::Message::Binary(data) => {
                        output.extend_from_slice(&data);
                        if String::from_utf8_lossy(&output).contains("hello") {
                            return;
                        }
                    }
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            typed.is_ok(),
            "no echo: {}",
            String::from_utf8_lossy(&output)
        );
        let _ = socket.close(None).await;
        output
    });
    assert!(String::from_utf8_lossy(&output).contains("hello"));

    // The session keeps running after the browser left.
    wait_for("the session to be listed", || {
        let (_, sessions, _) =
            web.request("GET", &format!("/api/machines/{machine}/sessions"), None);
        sessions[0]["name"] == "t1" && sessions[0]["attached"] == false
    });

    // Another account sees nothing of it.
    let mut other = Web {
        url: web.url.clone(),
        host: web.host.clone(),
        agent: web.agent.clone(),
        cookie: None,
    };
    let (_, _, cookie) = other.request(
        "POST",
        "/api/auth/login",
        Some(json!({"username": "mallory", "password": "password123"})),
    );
    other.cookie = cookie;
    assert_eq!(other.request("GET", "/api/machines", None).1, json!([]));
    assert_eq!(
        other
            .request("GET", &format!("/api/machines/{machine}/sessions"), None)
            .0,
        404
    );

    // Ending the session, then removing the machine, which drops it.
    let (status, _, _) = web.request(
        "DELETE",
        &format!("/api/machines/{machine}/sessions/t1"),
        None,
    );
    assert_eq!(status, 204);
    let (status, _, _) = web.request("DELETE", &format!("/api/machines/{machine}"), None);
    assert_eq!(status, 204);
    assert_eq!(web.request("GET", "/api/machines", None).1, json!([]));
    server.stop();
}

/// `--dev`: the login page is served at `/auth/token` and carries the token
/// in the URL fragment, which the GET never sends to the server; the token
/// itself is POSTed to `/api/auth/token`, where the same-origin guard
/// applies, so it stays out of logged URLs and cannot be cross-site forged.
#[test]
fn dev_login_posts_the_token_from_the_fragment() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("web")).unwrap());
    let identity = cmdterm_remote::keys::Identity::generate().unwrap();
    let hub = Hub::new(identity, store.clone(), runtime.handle().clone());
    hub.listen("127.0.0.1:0").unwrap();
    let config = Config {
        listen: address.to_string(),
        ..Config::default()
    };
    let token = auth::random_token();
    let app = App::new(config, store, hub, Some(token.clone()));
    let router = api::router(app).into_make_service_with_connect_info::<SocketAddr>();
    runtime.spawn(async move { axum::serve(listener, router).await });

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut web = Web {
        url: format!("http://{address}"),
        host: address.to_string(),
        agent,
        cookie: None,
    };

    // The page loads and pulls in token.js; no query string is involved.
    let mut page = web
        .agent
        .get(&format!("{}/auth/token", web.url))
        .call()
        .unwrap();
    assert_eq!(page.status().as_u16(), 200);
    let html = page.body_mut().read_to_string().unwrap();
    assert!(html.contains("token.js"), "{html}");
    assert!(
        !web.url.contains(&token),
        "the page URL must not carry the token"
    );

    // A wrong token is refused, the right one sets the session cookie.
    let (status, body, _) = web.request("POST", "/api/auth/token", Some(json!({"token": "wrong"})));
    assert_eq!(status, 401, "{body}");
    let (status, body, cookie) =
        web.request("POST", "/api/auth/token", Some(json!({"token": token})));
    assert_eq!(status, 200, "{body}");
    assert!(
        cookie.as_deref().is_some_and(|c| c.starts_with("session=")),
        "{cookie:?}"
    );
    web.cookie = cookie;
    let (status, me, _) = web.request("GET", "/api/me", None);
    assert_eq!(status, 200);
    assert_eq!(me["name"], "dev");

    // Without an Origin header the guard refuses the POST.
    let response = web
        .agent
        .post(&format!("{}/api/auth/token", web.url))
        .header("Content-Type", "application/json")
        .send(json!({"token": "whatever"}).to_string())
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
}
