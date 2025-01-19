mod lobby;

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket},
        WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Extension, Router,
};
use chrono::{DateTime, Utc};
use color_eyre::eyre::eyre;
use color_eyre::eyre::Context as _;
use color_eyre::eyre::Error;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use shuttle_axum::ShuttleAxum;
use tokio::{
    sync::{watch, Mutex},
    time::sleep,
};
use tower_http::services::ServeDir;

use lobby::{InitialMessage, Lobbies, Peer};

#[derive(Default)]
struct State {
    lobbies: Lobbies,
    peer_ids: scc::HashSet<u32>,
}

impl State {
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
    let state = Arc::new(Mutex::new(State::default()));

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

async fn handle_websocket(stream: WebSocket, state: Arc<State>) -> Result<(), Error> {
    // By splitting we can send and receive at the same time.
    let (sender, mut receiver) = stream.split();

    let initial_message = loop {
        let message = match receiver.next().await {
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
    let peer = Peer::new(peer_id, sender);

    match initial_message {
        InitialMessage::Create { mesh } => state.lobbies.handle_create_lobby(peer, mesh).await?,
        InitialMessage::Join { name } => state.lobbies.handle_join_lobby(peer, &name).await?,
    }

    // Next: loop over each incoming message (skip non-text ones other than bailing on binary)
    // Handle each message using a corresponding call on Lobbies. Mainly we're relaying.
    // When the websocket close, use the Lobbies stuff to handle close.
    async fn handle_leaving(peer_id: u32, state: Arc<State>) -> Result<(), Error> {
        // find lobby by peer & remove peer
        state.lobbies.handle_close_lobby(peer_id).await;
        // clean up Peer ID from active peers
        state
            .peer_ids
            .remove(&peer_id)
            .map_err(|e| eyre!("failed to remove peer: {e}"))?;
        Ok(())
    }

    loop {
        match receiver.next().await {
            None => break,
            Some(result) => match result {
                Ok(message) => match message {
                    Message::Text(text) => {
                        if let Err(e) = state.lobbies.handle_message(peer_id, &text).await {
                            log::error!("error handling message: {e}");
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        log::error!("received unexpected binary message");
                        break;
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) => continue,
                    Message::Pong(_) => continue,
                },
                Err(e) => {
                    log::error!("error receiving message: {e}");
                    break;
                }
            },
        }
    }

    handle_leaving(peer_id, state.clone())
        .await
        .unwrap_or_else(|e| {
            log::error!("error during leave handling: {e}");
        });

    Ok(())
}
