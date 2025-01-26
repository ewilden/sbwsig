mod lobby;

use std::{
    env::{self, VarError},
    fs,
    sync::Arc,
};

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
use futures::{Sink, Stream, StreamExt};
use shuttle_axum::ShuttleAxum;
use tokio::{sync::mpsc, task::JoinHandle};
use tower_http::services::ServeDir;

use lobby::{CleanupWorkItem, InitialMessage, Lobbies, LobbiesInner};

struct State<S: Socket> {
    lobbies: Lobbies<S>,
    peer_ids: scc::HashSet<u32>,
    _cleanup_work: tokio::task::JoinHandle<()>,
}

impl<S: Socket> State<S> {
    fn new() -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let lobbies = Lobbies(Arc::new(LobbiesInner {
            lobbies: Default::default(),
            cleanup: sender,
        }));
        Self {
            lobbies: lobbies.clone(),
            peer_ids: Default::default(),
            _cleanup_work: tokio::spawn(async move {
                while let Some(work) = receiver.recv().await {
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
    if env::var("WRITE_SCHEMA") != Err(VarError::NotPresent) {
        fs::write(
            "schemas.txt",
            serde_json::to_string_pretty(&InitialMessage::Create { mesh: true }).unwrap(),
        )
        .expect("should succeed")
    }

    let state = Arc::new(State::<WebSocket>::new());

    let router = Router::new()
        .route("/websocket", get(websocket_handler))
        .nest_service("/", ServeDir::new("static"))
        .layer(Extension(state));

    Ok(router.into())
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    Extension(state): Extension<Arc<State<WebSocket>>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

async fn websocket(stream: WebSocket, state: Arc<State<WebSocket>>) {
    match handle_websocket(stream, state).await {
        Ok(opt_join_handle) => {
            if let Some(join_handle) = opt_join_handle {
                // Dropping this will not abort the task.
                let _: JoinHandle<()> = join_handle;
            }
        }
        Err(e) => {
            log::error!("{e:?}");
        }
    }
}

async fn handle_websocket<S: Socket>(
    mut socket: S,
    state: Arc<State<S>>,
) -> Result<Option<JoinHandle<()>>, Error> {
    log::info!("handle_websocket");
    let initial_message = loop {
        let message = match socket.next().await {
            None => return Ok(None),
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

    Ok(match initial_message {
        InitialMessage::Create { mesh } => {
            Some(state.lobbies.handle_create_lobby(peer, mesh).await?)
        }
        InitialMessage::Join { name } => {
            state.lobbies.handle_join_lobby(peer, &name).await?;
            None
        }
    })
}

trait Socket:
    'static
    + Send
    + Unpin
    + Sink<Message, Error = axum::Error>
    + Stream<Item = Result<Message, axum::Error>>
{
}
impl<
        T: 'static
            + Send
            + Unpin
            + Sink<Message, Error = axum::Error>
            + Stream<Item = Result<Message, axum::Error>>,
    > Socket for T
{
}

#[cfg(test)]
mod test {
    use std::{pin::Pin, task::Poll};

    use super::*;
    use axum::extract::ws::Message;
    use futures::FutureExt as _;
    use serde_json::json;
    use tokio::sync::mpsc;

    struct TestSocket {
        name: &'static str,
        sender: mpsc::UnboundedSender<Message>,
        receiver: mpsc::UnboundedReceiver<Result<Message, axum::Error>>,
    }

    impl TestSocket {
        fn new_pair(name: &'static str) -> (Self, TestSocketOtherEnd) {
            let (sender, other_receiver) = mpsc::unbounded_channel();
            let (other_sender, receiver) = mpsc::unbounded_channel();

            (
                TestSocket {
                    name,
                    sender,
                    receiver,
                },
                TestSocketOtherEnd {
                    sender: other_sender,
                    receiver: other_receiver,
                },
            )
        }
    }

    struct TestSocketOtherEnd {
        sender: mpsc::UnboundedSender<Result<Message, axum::Error>>,
        receiver: mpsc::UnboundedReceiver<Message>,
    }

    impl Stream for TestSocket {
        type Item = Result<Message, axum::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            match self.receiver.poll_recv(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(received) => {
                    eprintln!("test socket {} received: {received:#?}", self.name);
                    Poll::Ready(received)
                }
            }
        }
    }

    impl Sink<Message> for TestSocket {
        type Error = axum::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            eprintln!("test socket {} sending: {item:#?}", self.name);
            self.sender.send(item).map_err(|err| axum::Error::new(err))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn _impls_socket() {
        let (test_socket, _other_end) = TestSocket::new_pair("host");
        let _socket: &dyn Socket = &test_socket;
    }

    #[tokio::test]
    async fn handles_open_close() {
        let state = Arc::new(State::new());
        let (socket, fixture) = TestSocket::new_pair("host");
        let handler_task = tokio::spawn(handle_websocket(socket, state.clone()));
        drop(fixture);
        if let Some(join_handle) = handler_task
            .await
            .expect("should complete normally")
            .expect("should not get error")
        {
            join_handle.await.expect("should complete normally")
        }
        assert!(state.lobbies.0.lobbies.is_empty());
    }

    #[tokio::test]
    async fn basic_functionality() {
        let state = Arc::new(State::new());
        let (socket, mut fixture) = TestSocket::new_pair("host");
        let handler_task = tokio::spawn(handle_websocket(socket, state.clone()));
        fixture
            .sender
            .send(Ok(Message::Text(
                serde_json::to_string(&InitialMessage::Create { mesh: false })
                    .expect("should serialize successfully"),
            )))
            .expect("should succeed");
        let received = fixture
            .receiver
            .recv()
            .await
            .expect("should receive")
            .into_text()
            .expect("should be text");
        let received = serde_json::from_str::<serde_json::Value>(&received)
            .expect("should parse JSON successfully");
        let lobby_name = received["payload"]["CreatedLobby"]["name"]
            .as_str()
            .expect("should be string");
        assert_eq!(
            received,
            json!({
                "payload": {
                    "CreatedLobby": {
                        "name": lobby_name
                    }
                }
            })
        );
        let received = fixture
            .receiver
            .recv()
            .await
            .expect("should receive")
            .into_text()
            .expect("should be text");
        let received = serde_json::from_str::<serde_json::Value>(&received)
            .expect("should parse JSON successfully");
        assert_eq!(
            received,
            json!({
                "payload": {
                    "Id": {
                        "assigned": 1,
                        "mesh": false
                    }
                }
            })
        );
        let (socket2, mut fixture2) = TestSocket::new_pair("peer1");
        let handler2_task = tokio::spawn(
            handle_websocket(socket2, state.clone())
                .map(|opt_handle| assert!(opt_handle.expect("should succeed").is_none())),
        );

        fixture2
            .sender
            .send(Ok(Message::Text(
                serde_json::to_string(&InitialMessage::Join {
                    name: lobby_name.to_string(),
                })
                .expect("should serialize successfully"),
            )))
            .expect("should succeed");
        handler2_task.await.expect("should complete normally");

        let received = fixture2
            .receiver
            .recv()
            .await
            .expect("should receive")
            .into_text()
            .expect("should be text");
        let received = serde_json::from_str::<serde_json::Value>(&received)
            .expect("should parse JSON successfully");
        let peer_id = received["payload"]["Id"]["assigned"]
            .as_u64()
            .expect("should be int");
        assert_eq!(
            received,
            json!({
                "payload": {
                    "Id": {
                        "assigned": peer_id,
                        "mesh": false
                    }
                }
            })
        );

        drop(fixture);
        drop(fixture2);
        if let Some(lobby_join_handle) = handler_task
            .await
            .expect("should complete normally")
            .expect("should not get error")
        {
            lobby_join_handle.await.expect("should complete normally");
        }

        assert!(state.lobbies.0.lobbies.is_empty());
    }
}
