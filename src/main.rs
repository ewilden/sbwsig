mod lobby;

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Extension, Router,
};
use color_eyre::eyre::eyre;
use color_eyre::eyre::Context as _;
use color_eyre::eyre::Error;
use futures::{channel::mpsc, StreamExt};
use shuttle_axum::ShuttleAxum;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;

use lobby::{CleanupWorkItem, InitialMessage, Lobbies, LobbiesInner};

struct State {
    lobbies: Lobbies,
    peer_ids: scc::HashSet<u32>,
    _cleanup_work: tokio::task::JoinHandle<()>,
}

impl State {
    fn new() -> Self {
        let (sender, mut receiver) = mpsc::unbounded();
        let lobbies = Lobbies(Arc::new(LobbiesInner {
            lobbies: Default::default(),
            cleanup: sender,
        }));
        Self {
            lobbies: lobbies.clone(),
            peer_ids: Default::default(),
            _cleanup_work: tokio::spawn(async move {
                while let Some(work) = receiver.next().await {
                    match work {
                        CleanupWorkItem::DeleteLobby(lobby_name) => {
                            lobbies.0.lobbies.remove_async(&lobby_name).await;
                        }
                    }
                }
            }),
        }
    }

    fn allocate_peer(&self) -> u32 {
        loop {
            let peer_id = rand::random::<u32>();
            match self.peer_ids.insert(peer_id) {
                Ok(()) => return peer_id,
                Err(_) => continue,
            }
        }
    }
}

#[shuttle_runtime::main]
async fn main() -> ShuttleAxum {
    let state = Arc::new(Mutex::new(State::new()));

    let router = Router::new()
        .route("/websocket", get(websocket_handler))
        .nest_service("/", ServeDir::new("static"))
        .layer(Extension(state));

    Ok(router.into())
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

async fn websocket(stream: WebSocket, state: Arc<State>) {
    match handle_websocket(stream, state).await {
        Ok(()) => (),
        Err(e) => {
            log::error!("{e:?}");
        }
    }
}

async fn handle_websocket(mut socket: WebSocket, state: Arc<State>) -> Result<(), Error> {
    // By splitting we can send and receive at the same time.
    let initial_message = loop {
        let message = match socket.next().await {
            None => return Ok(()),
            Some(result) => result.context("error receiving initial message")?,
        };
        let message = match message {
            Message::Text(message) => message,
            Message::Binary(_) => return Err(eyre!("expecting text message")),
            Message::Close(_) => return Err(eyre!("expecting text message")),
            Message::Ping(_) => continue,
            Message::Pong(_) => continue,
        };
        break serde_json::from_str::<InitialMessage>(message.as_str())
            .context("failed to parse initial message")?;
    };

    let peer_id = state.allocate_peer();
    let peer = lobby::NewPeer {
        id: peer_id,
        websocket: socket,
    };

    match initial_message {
        InitialMessage::Create { mesh } => state.lobbies.handle_create_lobby(peer, mesh).await?,
        InitialMessage::Join { name } => state.lobbies.handle_join_lobby(peer, &name).await?,
    }
    Ok(())
}
