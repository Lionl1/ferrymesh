//! `ferry web`: the node plus a chat in the browser.
//!
//! One HTML page (compiled into the binary, no build step) talks to a small JSON API and
//! gets new messages and receipts over server-sent events. Every API call needs the
//! token printed at startup: the link carries it in the URL fragment, which browsers do
//! not send to servers or put in `Referer`, and another web page cannot guess it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ferrymesh_core::{Config, Entropy, Event, OsEntropy, PublicIdentity};
use ferrymesh_net::HostSender;
use ferrymesh_net::host::now_ms;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::NetArgs;
use crate::history::{Entry, History};
use crate::home::Home;

const INDEX: &str = include_str!("web/index.html");

pub async fn serve(
    home: Home,
    net: NetArgs,
    listen: SocketAddr,
    token: Option<String>,
) -> Result<()> {
    let history = History::load(home.history())?;
    let me = home.identity()?.public();
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    let mut handle = crate::start_host(&home, net).await?;
    let token = token.unwrap_or_else(|| {
        let mut bytes = [0u8; 16];
        OsEntropy.fill(&mut bytes);
        hex::encode(bytes)
    });
    let app = App::new(home, me, handle.sender(), history, token.clone());

    let shown = if listen.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], listen.port()))
    } else {
        listen
    };
    if !listen.ip().is_loopback() {
        eprintln!(
            "warning: the web UI listens on {listen}; anyone on the network with the link \
             can read and send your messages"
        );
    }
    println!("web UI: http://{shown}/#{token}");

    let server = tokio::spawn(axum::serve(listener, router(app.clone())).into_future());
    loop {
        tokio::select! {
            event = handle.next_event() => match event {
                Some(event) => {
                    if let Err(e) = app.on_event(event) {
                        eprintln!("history: {e:#}");
                    }
                }
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    server.abort();
    handle.shutdown().await;
    Ok(())
}

pub struct App {
    home: Home,
    me: PublicIdentity,
    sender: HostSender,
    history: Mutex<History>,
    updates: broadcast::Sender<String>,
    token: String,
}

/// What open pages are told about.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Update<'a> {
    Message { entry: &'a Entry },
    Delivered { id: String },
}

impl App {
    pub fn new(
        home: Home,
        me: PublicIdentity,
        sender: HostSender,
        history: History,
        token: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            home,
            me,
            sender,
            history: Mutex::new(history),
            updates: broadcast::channel(256).0,
            token,
        })
    }

    /// Records a node event in the history and pushes it to open pages.
    pub fn on_event(&self, event: Event) -> Result<()> {
        match event {
            Event::Message {
                id,
                from,
                sent_at_ms,
                body,
            } => {
                let entry = Entry::incoming(id, &from, &body, sent_at_ms, now_ms());
                self.history.lock().unwrap().add(entry.clone())?;
                self.push(&Update::Message { entry: &entry });
            }
            Event::Delivered { id } => {
                if self.history.lock().unwrap().delivered(id)? {
                    self.push(&Update::Delivered { id: id.to_string() });
                }
            }
        }
        Ok(())
    }

    fn push(&self, update: &Update) {
        // No open page is not an error.
        let _ = self.updates.send(serde_json::to_string(update).unwrap());
    }

    fn authorized(&self, req: &Request) -> bool {
        let header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        // EventSource cannot set headers, so the event stream takes `?token=`.
        let query = req
            .uri()
            .query()
            .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("token=")));
        header
            .or(query)
            .is_some_and(|given| constant_time_eq(given.as_bytes(), self.token.as_bytes()))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/status", get(status))
        .route("/api/contacts", get(contacts).post(add_contact))
        .route("/api/messages", get(messages).post(send))
        .route("/api/events", get(events))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .merge(api)
        .with_state(app)
}

async fn auth(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if app.authorized(&req) {
        next.run(req).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "a valid token is required".into()).into_response()
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad_request(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, e.to_string())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Serialize)]
struct Me {
    card: String,
    address: String,
}

async fn me(State(app): State<Arc<App>>) -> Json<Me> {
    Json(Me {
        card: app.me.to_card(),
        address: app.me.address().to_string(),
    })
}

#[derive(Serialize)]
struct StatusView {
    neighbors: usize,
    stored: usize,
    pending: usize,
}

async fn status(State(app): State<Arc<App>>) -> Json<StatusView> {
    let s = app.sender.status().await;
    Json(StatusView {
        neighbors: s.neighbors,
        stored: s.stored,
        pending: s.pending,
    })
}

#[derive(Serialize)]
struct ContactView {
    /// `None` for someone who wrote to us but is not in the contacts.
    name: Option<String>,
    card: String,
    address: String,
}

async fn contacts(State(app): State<Arc<App>>) -> Result<Json<Vec<ContactView>>, ApiError> {
    let named = app.home.contacts().map_err(internal)?;
    let mut out: Vec<ContactView> = named
        .iter()
        .filter_map(|(name, card)| {
            let public = PublicIdentity::from_card(card).ok()?;
            Some(ContactView {
                name: Some(name.clone()),
                card: card.clone(),
                address: public.address().to_string(),
            })
        })
        .collect();
    let history = app.history.lock().unwrap();
    for entry in history.entries() {
        if out.iter().any(|c| c.card == entry.peer) {
            continue;
        }
        if let Ok(public) = PublicIdentity::from_card(&entry.peer) {
            out.push(ContactView {
                name: None,
                card: entry.peer.clone(),
                address: public.address().to_string(),
            });
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct NewContact {
    name: String,
    card: String,
}

async fn add_contact(
    State(app): State<Arc<App>>,
    Json(new): Json<NewContact>,
) -> Result<Json<ContactView>, ApiError> {
    let public = app
        .home
        .add_contact(new.name.trim(), &new.card)
        .map_err(bad_request)?;
    if public == app.me {
        return Err(bad_request("that is your own card"));
    }
    Ok(Json(ContactView {
        name: Some(new.name.trim().to_string()),
        card: public.to_card(),
        address: public.address().to_string(),
    }))
}

async fn messages(State(app): State<Arc<App>>) -> Json<Vec<Entry>> {
    Json(app.history.lock().unwrap().entries().to_vec())
}

#[derive(Deserialize)]
struct NewMessage {
    /// The recipient's contact card.
    to: String,
    text: String,
}

async fn send(
    State(app): State<Arc<App>>,
    Json(msg): Json<NewMessage>,
) -> Result<Json<Entry>, ApiError> {
    let text = msg.text.trim();
    if text.is_empty() {
        return Err(bad_request("the message is empty"));
    }
    if text.len() > Config::default().max_body {
        return Err(bad_request("the message is too long"));
    }
    let to = PublicIdentity::from_card(&msg.to).map_err(bad_request)?;
    let id = app
        .sender
        .send(to, text.as_bytes().to_vec())
        .await
        .map_err(bad_request)?;
    let entry = Entry::outgoing(id, &to, text, now_ms());
    app.history
        .lock()
        .unwrap()
        .add(entry.clone())
        .map_err(internal)?;
    // Other open tabs show it too.
    app.push(&Update::Message { entry: &entry });
    Ok(Json(entry))
}

async fn events(
    State(app): State<Arc<App>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(app.updates.subscribe()).map(|update| {
        let data = match update {
            Ok(json) => json,
            // The page fell behind: it reloads everything.
            Err(BroadcastStreamRecvError::Lagged(_)) => r#"{"type":"resync"}"#.to_string(),
        };
        Ok(SseEvent::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use ferrymesh_core::{Identity, Node};
    use ferrymesh_net::{Host, HostHandle, UdpConfig, UdpTransport};
    use http_body_util::BodyExt;
    use tokio::time::timeout;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "test-token";

    fn temp_home(tag: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("ferry-web-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = Home(dir);
        home.init().unwrap();
        home
    }

    async fn udp(peer: Option<SocketAddr>) -> UdpTransport {
        let mut t = UdpTransport::bind(UdpConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            broadcast: None,
            beacon_interval: Duration::from_millis(100),
            ..UdpConfig::default()
        })
        .await
        .unwrap();
        if let Some(peer) = peer {
            t.add_static_peer(peer);
        }
        t
    }

    fn host(node: Node, transport: UdpTransport) -> HostHandle {
        let mut host = Host::new(node);
        host.add_transport(transport);
        host.spawn()
    }

    async fn call(
        router: &Router,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {TOKEN}"));
        let body = match body {
            Some(json) => {
                req = req.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let resp = router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn chat_through_the_api() {
        let alice_home = temp_home("alice");
        let carol_home = temp_home("carol");
        let bob = udp(None).await;
        let bob_addr = bob.local_addr().unwrap();
        let mut bob = host(
            Node::new(Identity::generate(&mut OsEntropy), Config::default()),
            bob,
        );
        let mut alice = host(alice_home.node().unwrap(), udp(Some(bob_addr)).await);
        let mut carol = host(carol_home.node().unwrap(), udp(Some(bob_addr)).await);
        let carol_card = carol.public.to_card();

        let history = History::load(alice_home.history()).unwrap();
        let app = App::new(
            alice_home,
            alice.public,
            alice.sender(),
            history,
            TOKEN.into(),
        );
        let router = router(app.clone());
        let mut updates = app.updates.subscribe();

        // The page is public, the API is not.
        let page = router
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        for uri in ["/api/me", "/api/me?token=wrong", "/api/events"] {
            let resp = router
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
        let resp = router
            .clone()
            .oneshot(
                Request::get(format!("/api/me?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let (code, me) = call(&router, "GET", "/api/me", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(me["card"], alice.public.to_card());

        let (code, _) = call(
            &router,
            "POST",
            "/api/contacts",
            Some(serde_json::json!({"name": "two words", "card": carol_card})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, _) = call(
            &router,
            "POST",
            "/api/contacts",
            Some(serde_json::json!({"name": "carol", "card": carol_card})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);

        let (code, _) = call(
            &router,
            "POST",
            "/api/messages",
            Some(serde_json::json!({"to": carol_card, "text": "  "})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, sent) = call(
            &router,
            "POST",
            "/api/messages",
            Some(serde_json::json!({"to": carol_card, "text": "hi from the browser"})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(sent["delivered"], false);

        let got = timeout(Duration::from_secs(10), carol.next_event())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(got, Event::Message { ref body, .. } if body == b"hi from the browser"));
        let receipt = timeout(Duration::from_secs(10), alice.next_event())
            .await
            .unwrap()
            .unwrap();
        app.on_event(receipt).unwrap();

        // Open pages heard about the sent message and then the receipt.
        assert!(
            updates
                .recv()
                .await
                .unwrap()
                .contains("\"type\":\"message\"")
        );
        let delivered = updates.recv().await.unwrap();
        assert!(delivered.contains("\"type\":\"delivered\""), "{delivered}");

        let (_, history) = call(&router, "GET", "/api/messages", None).await;
        assert_eq!(history[0]["text"], "hi from the browser");
        assert_eq!(history[0]["delivered"], true);
        let (_, contacts) = call(&router, "GET", "/api/contacts", None).await;
        assert_eq!(contacts[0]["name"], "carol");

        // Carol writes back; Alice's page shows the message.
        carol
            .send(alice.public, b"hello, browser".to_vec())
            .await
            .unwrap();
        let back = timeout(Duration::from_secs(10), alice.next_event())
            .await
            .unwrap()
            .unwrap();
        app.on_event(back).unwrap();
        assert!(updates.recv().await.unwrap().contains("hello, browser"));
        let (_, status) = call(&router, "GET", "/api/status", None).await;
        assert_eq!(status["neighbors"], 1);

        assert!(bob.try_next_event().is_none());
        for handle in [alice, bob, carol] {
            handle.shutdown().await;
        }
        let _ = std::fs::remove_dir_all(&app.home.0);
        let _ = std::fs::remove_dir_all(&carol_home.0);
    }
}
