//! The web script editor, served by the lobby over HTTP (Caddy adds TLS in
//! production).
//!
//! `/script edit` in a draft creates a one-time link to `/edit?t=…`, which
//! is exchanged for a session cookie. The page under `/editor/` then talks
//! to the JSON API under `/api/`. Saves go to Postgres; reloads go through
//! the coordinator to the shard running the draft, whose script output comes
//! back over the console WebSocket.

mod api;
pub mod console;

use std::net::SocketAddr;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use chunkedge::prelude::*;
use chunkedge::UniqueId;
use mm_core::Tokio;
use mm_db::Pool;
use mm_proto::GameId;
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::account::RefreshAccount;
use crate::coordinator::Coordinator;
pub use console::{ConsoleHub, Line};

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Loopback address; Caddy proxies the public hostname here.
    pub bind: SocketAddr,
    /// Mark the session cookie `Secure` (the public URL is HTTPS).
    #[serde(default = "default_secure")]
    pub secure_cookies: bool,
}

fn default_secure() -> bool {
    true
}

/// Asks the game side of the lobby to do something.
pub enum WebRequest {
    /// Restart a draft's scripts; answers whether the draft is running.
    Reload {
        game: GameId,
        by: String,
        reply: oneshot::Sender<bool>,
    },
    /// Who may edit a game changed; refresh this player's game list if online.
    AccessChanged { player: Uuid },
}

#[derive(Clone)]
pub struct WebState {
    pub db: Pool,
    pub console: ConsoleHub,
    pub lobby: UnboundedSender<WebRequest>,
    pub secure_cookies: bool,
}

#[derive(Resource)]
struct WebRequests(UnboundedReceiver<WebRequest>);

/// Shared with the coordinator, which feeds it shard diagnostics.
#[derive(Resource, Clone, Default)]
pub struct Console(pub ConsoleHub);

pub struct WebPlugin {
    pub config: Option<WebConfig>,
}

impl Plugin for WebPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Console>();
        let Some(config) = self.config.clone() else {
            return;
        };
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(WebRequests(rx))
            .insert_resource(WebSettings { config, tx })
            .add_systems(Startup, start)
            .add_systems(Update, handle_requests);
    }
}

#[derive(Resource)]
struct WebSettings {
    config: WebConfig,
    tx: UnboundedSender<WebRequest>,
}

fn start(settings: Res<WebSettings>, console: Res<Console>, db: Res<crate::account::Db>, tokio: Res<Tokio>) {
    let state = WebState {
        db: db.0.clone(),
        console: console.0.clone(),
        lobby: settings.tx.clone(),
        secure_cookies: settings.config.secure_cookies,
    };
    let bind = settings.config.bind;
    tokio.0.spawn(async move {
        let result = async {
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!("web editor listening on http://{bind}");
            axum::serve(listener, router(state)).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::error!("web editor failed: {e:#}");
            std::process::exit(1);
        }
    });
}

fn handle_requests(
    mut requests: ResMut<WebRequests>,
    coordinator: Res<Coordinator>,
    players: Query<(Entity, &UniqueId), With<Client>>,
    mut refresh: MessageWriter<RefreshAccount>,
) {
    while let Ok(request) = requests.0.try_recv() {
        match request {
            WebRequest::Reload { game, by, reply } => {
                let _ = reply.send(coordinator.reload_draft(game, &by));
            }
            WebRequest::AccessChanged { player } => {
                for (entity, uuid) in &players {
                    if uuid.0 == player {
                        refresh.write(RefreshAccount(entity));
                    }
                }
            }
        }
    }
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/editor/") }))
        .route("/edit", get(api::redeem))
        .route("/favicon.ico", get(|| async { axum::http::StatusCode::NO_CONTENT }))
        .route("/editor", get(|| async { Redirect::permanent("/editor/") }))
        .route("/editor/", get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }))
        .route("/editor/editor.js", get(|| async { asset("text/javascript; charset=utf-8", EDITOR_JS) }))
        .route("/editor/editor.css", get(|| async { asset("text/css; charset=utf-8", EDITOR_CSS) }))
        .route("/editor/minimodes.d.luau", get(|| async { asset("text/plain; charset=utf-8", TYPES) }))
        .nest("/api", api::routes())
        .with_state(state)
}

const INDEX_HTML: &str = include_str!("../../../../web/editor/index.html");
const EDITOR_JS: &str = include_str!("../../../../web/editor/editor.js");
const EDITOR_CSS: &str = include_str!("../../../../web/editor/editor.css");
const TYPES: &str = include_str!("../../../../crates/mm_script/minimodes.d.luau");

/// Monaco loads from jsDelivr; everything else is same-origin.
const CSP: &str = "default-src 'self'; \
    script-src 'self' https://cdn.jsdelivr.net; \
    style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; \
    font-src 'self' data: https://cdn.jsdelivr.net; \
    worker-src 'self' blob:; \
    connect-src 'self'; \
    img-src 'self' data:; \
    frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    secure_headers(headers);
    response
}

fn secure_headers(headers: &mut axum::http::HeaderMap) {
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}
