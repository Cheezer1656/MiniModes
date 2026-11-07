//! The editor's JSON API and its login link.
//!
//! Requests authenticate with the `mm_editor` session cookie. Requests that
//! change something must also carry `X-Minimodes: 1`, which a cross-site
//! form can't send, and a cross-origin `Origin` is always refused.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use mm_db::editor::{self, AddOutcome, RenameOutcome, Role, SaveOutcome};
use mm_proto::GameId;
use mm_proto::scripts::check_path;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{WebRequest, WebState, secure_headers};

const COOKIE: &str = "mm_editor";
/// Sources are capped at 2 MB in total; JSON escaping can grow them.
const BODY_LIMIT: usize = 6 << 20;
/// Open consoles re-check access this often.
const RECHECK: Duration = Duration::from_secs(30);

pub fn routes() -> Router<WebState> {
    Router::new()
        .route("/session", get(session))
        .route("/logout", post(logout))
        .route("/games/{game}/files", get(list_files))
        .route("/games/{game}/files/{*path}", get(read_file).put(save_file).delete(delete_file))
        .route("/games/{game}/rename", post(rename_file))
        .route("/games/{game}/history/{*path}", get(history))
        .route("/games/{game}/revisions/{id}", get(revision))
        .route("/games/{game}/reload", post(reload))
        .route("/games/{game}/collaborators", get(collaborators).post(add_collaborator))
        .route("/games/{game}/collaborators/{player}", axum::routing::delete(remove_collaborator))
        .route("/games/{game}/console", get(console))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn(guard))
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self(status, msg.into())
    }
}

impl From<mm_db::Error> for ApiError {
    fn from(e: mm_db::Error) -> Self {
        tracing::error!("editor database error: {e}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "database error; try again")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Refuses cross-origin requests, and changes without the custom header.
async fn guard(request: Request, next: Next) -> Response {
    let headers = request.headers();
    if let Some(origin) = headers.get(header::ORIGIN) {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let origin_host = origin
            .to_str()
            .ok()
            .and_then(|o| o.split_once("://"))
            .map(|(_, rest)| rest)
            .unwrap_or("");
        if origin_host != host {
            return ApiError::new(StatusCode::FORBIDDEN, "cross-origin request").into_response();
        }
    }
    let changes = !matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if changes && headers.get("x-minimodes").is_none_or(|v| v != "1") {
        return ApiError::new(StatusCode::FORBIDDEN, "missing X-Minimodes header").into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cookie_value(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value)
}

/// The signed-in player.
pub struct Session {
    pub player: Uuid,
    pub name: String,
    hash: Vec<u8>,
}

impl FromRequestParts<WebState> for Session {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &WebState) -> ApiResult<Self> {
        let unauthorized = || ApiError::new(StatusCode::UNAUTHORIZED, "not signed in; run /script edit in your draft");
        let secret = cookie_value(&parts.headers).ok_or_else(unauthorized)?;
        let hash = editor::hash_secret(secret);
        let (player, name) = editor::session(&state.db, &hash).await?.ok_or_else(unauthorized)?;
        Ok(Self { player, name, hash })
    }
}

async fn access(state: &WebState, session: &Session, game: i64) -> ApiResult<Role> {
    editor::role(&state.db, game, session.player)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such game, or you can't edit it"))
}

fn checked(path: &str) -> ApiResult<&str> {
    check_path(path).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    Ok(path)
}

#[derive(Deserialize)]
pub struct RedeemQuery {
    t: String,
}

fn link_error(message: &str) -> Response {
    let page = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
         <title>Minimodes editor</title>\
         <body style='font:16px system-ui;max-width:32rem;margin:4rem auto;padding:0 16px'>\
         <h1>Can't open the editor</h1><p>{message}</p>\
         <p>Run <code>/script edit</code> in your draft for a fresh link.</p>"
    );
    let mut response = (StatusCode::BAD_REQUEST, Html(page)).into_response();
    secure_headers(response.headers_mut());
    response
}

/// `/edit?t=…`: trades a one-time link for a session cookie.
pub async fn redeem(State(state): State<WebState>, Query(q): Query<RedeemQuery>) -> Response {
    let result = async {
        let Some((player, game)) = editor::redeem_token(&state.db, &editor::hash_secret(&q.t)).await? else {
            return Ok(None);
        };
        let (secret, hash) = editor::new_secret();
        editor::create_session(&state.db, &hash, player).await?;
        Ok::<_, mm_db::Error>(Some((secret, game)))
    }
    .await;
    match result {
        Ok(Some((secret, game))) => {
            let mut cookie = format!(
                "{COOKIE}={secret}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
                editor::SESSION_TTL.as_secs()
            );
            if state.secure_cookies {
                cookie.push_str("; Secure");
            }
            let mut response = Redirect::to(&format!("/editor/?game={game}")).into_response();
            let headers = response.headers_mut();
            headers.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).expect("cookie is ASCII"));
            secure_headers(headers);
            response
        }
        Ok(None) => link_error("This link has expired or was already used."),
        Err(e) => {
            tracing::error!("redeeming an editor link: {e}");
            link_error("Something went wrong on our side.")
        }
    }
}

async fn session(State(state): State<WebState>, session: Session) -> ApiResult<Json<Value>> {
    let owned = mm_db::owned_games(&state.db, session.player).await?;
    let shared = editor::shared_games(&state.db, session.player).await?;
    let games: Vec<Value> = owned
        .iter()
        .map(|g| (g, Role::Owner))
        .chain(shared.iter().map(|g| (g, Role::Collaborator)))
        .map(|(g, role)| {
            json!({
                "id": g.id,
                "name": g.name,
                "owner": g.owner_name,
                "kind": mm_db::kind_to_str(g.kind()),
                "role": role.as_str(),
            })
        })
        .collect();
    Ok(Json(json!({
        "player": { "uuid": session.player, "name": session.name },
        "games": games,
    })))
}

async fn logout(State(state): State<WebState>, session: Session) -> ApiResult<Response> {
    editor::delete_session(&state.db, &session.hash).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("mm_editor=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    );
    Ok(response)
}

async fn list_files(State(state): State<WebState>, session: Session, Path(game): Path<i64>) -> ApiResult<Json<Value>> {
    access(&state, &session, game).await?;
    let files = editor::list_files(&state.db, game).await?;
    Ok(Json(Value::Array(
        files
            .into_iter()
            .map(|f| {
                json!({
                    "path": f.path,
                    "size": f.size,
                    "version": f.version,
                    "updated_at": f.updated_unix,
                    "updated_by": f.updated_by,
                })
            })
            .collect(),
    )))
}

async fn read_file(
    State(state): State<WebState>,
    session: Session,
    Path((game, path)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    access(&state, &session, game).await?;
    let file = editor::file(&state.db, game, checked(&path)?)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such file"))?;
    Ok(Json(json!({
        "path": path,
        "source": file.source,
        "version": file.version,
        "updated_at": file.updated_unix,
        "updated_by": file.updated_by,
    })))
}

#[derive(Deserialize)]
struct SaveBody {
    source: String,
    /// The version the edit started from (0 for a new file). Omit to
    /// overwrite whatever is there.
    base: Option<i64>,
}

async fn save_file(
    State(state): State<WebState>,
    session: Session,
    Path((game, path)): Path<(i64, String)>,
    Json(body): Json<SaveBody>,
) -> ApiResult<Response> {
    access(&state, &session, game).await?;
    let path = checked(&path)?;
    let outcome = editor::save_file(&state.db, game, path, &body.source, session.player, body.base).await?;
    Ok(match outcome {
        SaveOutcome::Saved { version } => Json(json!({ "version": version })).into_response(),
        SaveOutcome::Conflict { version, by } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("{} saved this file since you opened it", by.as_deref().unwrap_or("someone")),
                "version": version,
                "updated_by": by,
            })),
        )
            .into_response(),
        SaveOutcome::NoGame => ApiError::new(StatusCode::NOT_FOUND, "no such game").into_response(),
        SaveOutcome::TooManyFiles => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("a game can have at most {} files", mm_proto::scripts::MAX_FILES),
        )
        .into_response(),
        SaveOutcome::TooLarge { total } => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "scripts would total {} KB; the limit is {} KB",
                total.div_ceil(1024),
                mm_proto::scripts::MAX_TOTAL_BYTES / 1024
            ),
        )
        .into_response(),
    })
}

async fn delete_file(
    State(state): State<WebState>,
    session: Session,
    Path((game, path)): Path<(i64, String)>,
) -> ApiResult<StatusCode> {
    access(&state, &session, game).await?;
    if editor::delete_file(&state.db, game, checked(&path)?, session.player).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(StatusCode::NOT_FOUND, "no such file"))
    }
}

#[derive(Deserialize)]
struct RenameBody {
    from: String,
    to: String,
}

async fn rename_file(
    State(state): State<WebState>,
    session: Session,
    Path(game): Path<i64>,
    Json(body): Json<RenameBody>,
) -> ApiResult<StatusCode> {
    access(&state, &session, game).await?;
    let (from, to) = (checked(&body.from)?, checked(&body.to)?);
    match editor::rename_file(&state.db, game, from, to, session.player).await? {
        RenameOutcome::Renamed => Ok(StatusCode::NO_CONTENT),
        RenameOutcome::Missing => Err(ApiError::new(StatusCode::NOT_FOUND, "no such file")),
        RenameOutcome::Exists => Err(ApiError::new(StatusCode::CONFLICT, format!("`{to}` already exists"))),
    }
}

async fn history(
    State(state): State<WebState>,
    session: Session,
    Path((game, path)): Path<(i64, String)>,
) -> ApiResult<Json<Value>> {
    access(&state, &session, game).await?;
    let revisions = editor::history(&state.db, game, checked(&path)?).await?;
    Ok(Json(Value::Array(
        revisions
            .into_iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "size": r.size,
                    "deleted": r.size.is_none(),
                    "updated_at": r.updated_unix,
                    "updated_by": r.updated_by,
                })
            })
            .collect(),
    )))
}

async fn revision(
    State(state): State<WebState>,
    session: Session,
    Path((game, id)): Path<(i64, i64)>,
) -> ApiResult<Json<Value>> {
    access(&state, &session, game).await?;
    let (path, source) = editor::revision(&state.db, game, id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such revision"))?;
    Ok(Json(json!({ "path": path, "source": source })))
}

async fn reload(State(state): State<WebState>, session: Session, Path(game): Path<i64>) -> ApiResult<Json<Value>> {
    access(&state, &session, game).await?;
    let (reply, running) = oneshot::channel();
    let _ = state.lobby.send(WebRequest::Reload {
        game: GameId(game),
        by: session.name.clone(),
        reply,
    });
    let running = running.await.unwrap_or(false);
    Ok(Json(json!({ "running": running })))
}

async fn collaborators(
    State(state): State<WebState>,
    session: Session,
    Path(game): Path<i64>,
) -> ApiResult<Json<Value>> {
    let role = access(&state, &session, game).await?;
    let owner = mm_db::game(&state.db, game)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such game"))?;
    let members = editor::collaborators(&state.db, game).await?;
    Ok(Json(json!({
        "owner": { "uuid": owner.owner, "name": owner.owner_name },
        "collaborators": members.iter().map(|m| json!({ "uuid": m.uuid, "name": m.username })).collect::<Vec<_>>(),
        "can_manage": role == Role::Owner,
        "max": editor::MAX_COLLABORATORS,
    })))
}

#[derive(Deserialize)]
struct AddBody {
    name: String,
}

async fn add_collaborator(
    State(state): State<WebState>,
    session: Session,
    Path(game): Path<i64>,
    Json(body): Json<AddBody>,
) -> ApiResult<Json<Value>> {
    if access(&state, &session, game).await? != Role::Owner {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "only the owner can invite collaborators"));
    }
    let name = body.name.trim();
    let bad = |msg: String| Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, msg));
    match editor::add_collaborator(&state.db, game, name, session.player).await? {
        AddOutcome::Added(member) => {
            let _ = state.lobby.send(WebRequest::AccessChanged { player: member.uuid });
            Ok(Json(json!({ "uuid": member.uuid, "name": member.username })))
        }
        AddOutcome::UnknownPlayer => bad(format!("nobody named `{name}` has played here yet")),
        AddOutcome::IsOwner => bad("you already own this game".to_owned()),
        AddOutcome::Already => bad(format!("`{name}` is already a collaborator")),
        AddOutcome::Full => bad(format!("a game can have at most {} collaborators", editor::MAX_COLLABORATORS)),
    }
}

/// The owner removes anyone; collaborators can remove themselves.
async fn remove_collaborator(
    State(state): State<WebState>,
    session: Session,
    Path((game, player)): Path<(i64, Uuid)>,
) -> ApiResult<StatusCode> {
    if access(&state, &session, game).await? != Role::Owner && player != session.player {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "only the owner can remove collaborators"));
    }
    if !editor::remove_collaborator(&state.db, game, player).await? {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not a collaborator"));
    }
    let _ = state.lobby.send(WebRequest::AccessChanged { player });
    Ok(StatusCode::NO_CONTENT)
}

/// Streams the game's console: `{type: "history", lines}` first, then one
/// `{type: "line", …}` per line, and `{type: "skipped", count}` if this
/// client fell behind.
async fn console(
    State(state): State<WebState>,
    session: Session,
    Path(game): Path<i64>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    access(&state, &session, game).await?;
    Ok(ws.on_upgrade(move |socket| stream_console(socket, state, session.player, game)))
}

async fn stream_console(socket: WebSocket, state: WebState, player: Uuid, game: i64) {
    let (recent, mut lines) = state.console.subscribe(GameId(game));
    let (mut tx, mut rx) = socket.split();
    let send = |value: Value| Message::Text(value.to_string().into());
    if tx.send(send(json!({ "type": "history", "lines": recent }))).await.is_err() {
        return;
    }
    let mut recheck = tokio::time::interval(RECHECK);
    recheck.tick().await;
    loop {
        let out = tokio::select! {
            line = lines.recv() => match line {
                Ok(line) => {
                    let mut value = serde_json::to_value(line).unwrap_or_default();
                    value["type"] = "line".into();
                    value
                }
                Err(RecvError::Lagged(count)) => json!({ "type": "skipped", "count": count }),
                Err(RecvError::Closed) => return,
            },
            msg = rx.next() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
            _ = recheck.tick() => match editor::role(&state.db, game, player).await {
                Ok(Some(_)) => continue,
                Ok(None) => {
                    let _ = tx.send(send(json!({ "type": "revoked" }))).await;
                    return;
                }
                Err(_) => continue,
            },
        };
        if tx.send(send(out)).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_session_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; mm_editor=secret; b=2"));
        assert_eq!(cookie_value(&headers), Some("secret"));
        let mut other = HeaderMap::new();
        other.append(header::COOKIE, HeaderValue::from_static("mm_editorx=nope"));
        assert_eq!(cookie_value(&other), None);
    }
}
