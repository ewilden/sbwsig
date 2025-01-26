use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use color_eyre::eyre::eyre;
use color_eyre::eyre::Context as _;
use color_eyre::eyre::Error;
use derivative::Derivative;
use futures::stream::SplitSink;
use futures::FutureExt as _;
use futures::SinkExt as _;
use futures::StreamExt;
use rand::distributions::Alphanumeric;
use rand::Rng;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::StreamMap;

use crate::Socket;
pub(crate) struct Peer<S: Socket> {
    id: u32,
    websocket: SplitSink<S, Message>,
}

impl<S: Socket> Peer<S> {
    pub(crate) fn new(id: u32, websocket: SplitSink<S, Message>) -> Self {
        Self { id, websocket }
    }
    async fn send(&mut self, msg: &LobbyMessage) -> Result<(), Error> {
        self.websocket
            .send(Message::Text(
                serde_json::to_string(msg).expect("should serialize to string"),
            ))
            .await
            .context("error sending message")
    }
}

pub(crate) struct Lobby<S: Socket> {
    name: String,
    mesh: bool,
    peers: BTreeMap<u32, Peer<S>>,
}

impl<S: Socket> Lobby<S> {
    fn new_name() -> String {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect::<String>()
    }

    fn new(name: String, mesh: bool) -> Self {
        Lobby {
            name,
            mesh,
            peers: Default::default(),
        }
    }
    fn get_peer_id(peer: &Peer<S>) -> u32 {
        peer.id
    }

    async fn join(&mut self, new_peer: SplitSink<S, Message>, is_host: bool) -> Result<u32, Error> {
        let assigned = if is_host {
            1
        } else {
            self.peers.keys().copied().max().unwrap_or(1u32) + 1
        };
        let mut new_peer = Peer::new(assigned, new_peer);
        new_peer
            .send(&LobbyMessage {
                payload: LobbyPayload::Id {
                    assigned,
                    mesh: self.mesh,
                },
            })
            .await?;
        for (_, peer) in &mut self.peers {
            peer.send(&LobbyMessage {
                payload: LobbyPayload::PeerConnect { peer_id: assigned },
            })
            .await?;
            new_peer
                .send(&LobbyMessage {
                    payload: LobbyPayload::PeerConnect {
                        peer_id: Self::get_peer_id(peer),
                    },
                })
                .await?;
        }
        self.peers.insert(new_peer.id, new_peer);
        Ok(assigned)
    }

    async fn leave(&mut self, peer_id: u32) -> Result<(Option<CloseLobby>, Peer<S>), Error> {
        let Some(leaving_peer) = self.peers.remove(&peer_id) else {
            return Err(eyre!(
                "peer {peer_id} tried to leave lobby {} without being in it",
                self.name
            ));
        };
        let assigned = Self::get_peer_id(&leaving_peer);
        let close = assigned == 1;
        if close {
            return Ok((Some(CloseLobby), leaving_peer));
        }

        for (_, peer) in &mut self.peers {
            peer.send(&LobbyMessage {
                payload: LobbyPayload::PeerDisconnect { peer_id: assigned },
            })
            .await?;
        }

        Ok((None, leaving_peer))
    }

    async fn relay(&mut self, relay_message: RelayMessage) -> Result<(), Error> {
        let RelayMessage {
            kind,
            dest_id,
            data,
        } = relay_message;
        let Some(peer) = self.peers.get_mut(&dest_id) else {
            return Err(eyre!("no such peer {dest_id} to relay to"));
        };
        peer.send(&LobbyMessage {
            payload: LobbyPayload::RelayedMessage { kind, data },
        })
        .await
    }
}

/// If we receive this, we need to drain all the peers
/// from the lobby and close all their websockets.
#[must_use]
struct CloseLobby;

#[derive(serde::Serialize, serde::Deserialize)]
struct LobbyMessage {
    payload: LobbyPayload,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum LobbyPayload {
    Id {
        assigned: u32,
        mesh: bool,
    },
    CreatedLobby {
        name: String,
    },
    JoinedLobby,
    PeerConnect {
        peer_id: u32,
    },
    PeerDisconnect {
        peer_id: u32,
    },
    Seal,
    RelayedMessage {
        kind: RelayMessageKind,
        data: String,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Inbound {
    Seal,
    Relay { relay_message: RelayMessage },
}

#[derive(serde::Serialize, serde::Deserialize)]
enum RelayMessageKind {
    Offer,
    Answer,
    Candidate,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct RelayMessage {
    kind: RelayMessageKind,
    dest_id: u32,
    data: String,
}

pub(crate) struct NewPeer<S: Socket> {
    pub(crate) websocket: S,
}

enum LobbyWorkItem<S: Socket> {
    NewPeer(NewPeer<S>),
    PeerMessage((u32, Option<Result<Message, axum::Error>>)),
}

pub(crate) enum CleanupWorkItem {
    DeleteLobby(String),
}

pub(crate) struct LobbiesInner<S: Socket> {
    pub(crate) lobbies: scc::HashMap<String, mpsc::Sender<NewPeer<S>>>,
    pub(crate) cleanup: mpsc::UnboundedSender<CleanupWorkItem>,
}

struct ClearLobbyOnDrop<S: Socket> {
    lobbies: Lobbies<S>,
    lobby_name: String,
}

impl<S: Socket> Drop for ClearLobbyOnDrop<S> {
    fn drop(&mut self) {
        let Self {
            lobbies,
            lobby_name,
        } = self;
        match lobbies
            .0
            .cleanup
            .send(CleanupWorkItem::DeleteLobby(lobby_name.clone()))
        {
            Ok(_) => (),
            Err(e) => {
                log::error!("cleanup receiver was dropped: {e:?}")
            }
        }
    }
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub(crate) struct Lobbies<S: Socket>(pub(crate) Arc<LobbiesInner<S>>);

impl<S: Socket> Lobbies<S> {
    async fn new_lobby_task(
        self,
        name: String,
        host_ws: S,
        mesh: bool,
        mut peer_receiver: mpsc::Receiver<NewPeer<S>>,
    ) -> Result<(), Error> {
        let _cleanup = ClearLobbyOnDrop {
            lobbies: self.clone(),
            lobby_name: name.clone(),
        };

        let host_id = 1;
        let mut lobby = Lobby::new(name, mesh);
        let mut peer_rx = StreamMap::<u32, _>::new().fuse();
        let (host_tx, host_rx) = host_ws.split();
        peer_rx.get_mut().insert(
            host_id,
            host_rx
                .map(Some)
                .chain(futures::stream::once(futures::future::ready(None))),
        );
        let mut host = Peer::new(host_id, host_tx);
        host.send(&LobbyMessage {
            payload: LobbyPayload::CreatedLobby {
                name: lobby.name.clone(),
            },
        })
        .await?;
        let Peer {
            id: _host_id,
            websocket: host_tx,
        } = host;
        lobby.join(host_tx, true /* is_host */).await?;

        loop {
            let work_item = {
                let mut peer_rx = peer_rx.by_ref().chain(futures::stream::pending());
                futures::select! {
                    opt_rx = peer_rx.next() => {
                        LobbyWorkItem::PeerMessage(opt_rx.expect("should not have ended"))
                    }
                    new_peer = peer_receiver.recv().fuse() => {
                        let Some(new_peer) = new_peer else {
                            log::info!("exiting because new-peer receiver was dropped");
                            return Ok(())
                        };
                        LobbyWorkItem::NewPeer(new_peer)
                    }
                }
            };
            match work_item {
                LobbyWorkItem::NewPeer(new_peer) => {
                    let NewPeer { websocket } = new_peer;
                    let (tx, rx) = websocket.split();
                    let id = lobby
                        .join(tx, false /* is_host */)
                        .await
                        .context("failed to join new peer to lobby")?;
                    peer_rx.get_mut().insert(
                        id,
                        rx.map(Some)
                            .chain(futures::stream::once(futures::future::ready(None))),
                    );
                }
                LobbyWorkItem::PeerMessage((id, opt_rx_result)) => {
                    match opt_rx_result {
                        None => {
                            // Peer hung up on us.
                            // Remove it from the map.
                            let _ = peer_rx.get_mut().remove(&id);
                            let result = lobby
                                .leave(id)
                                .await
                                .context("failed to leave lobby with hung-up peer");
                            let (close, _peer) = match result {
                                Err(e) => {
                                    log::error!("error leaving lobby with hung-up peer: {e:?}");
                                    continue;
                                }
                                Ok(ok) => ok,
                            };

                            if let Some(CloseLobby) = close {
                                // The lobby is closed, hang up on everybody.
                                log::info!("closing lobby {}", lobby.name);
                                return Ok(());
                            }
                        }
                        Some(result) => match result {
                            Err(e) => {
                                log::error!("error receiving: {e:?}");
                                // Peer errored.
                                // Remove it from the map.
                                let _ = peer_rx.get_mut().remove(&id);
                                let result = lobby
                                    .leave(id)
                                    .await
                                    .context("failed to leave lobby with errored peer");
                                let (close, _peer) = match result {
                                    Err(e) => {
                                        log::error!("error leaving lobby with errored peer: {e:?}");
                                        continue;
                                    }
                                    Ok(ok) => ok,
                                };
                                if let Some(CloseLobby) = close {
                                    // The lobby is closed, hang up on everybody.
                                    log::info!("closing lobby {}", lobby.name);
                                    return Ok(());
                                }
                            }
                            Ok(message) => match message {
                                Message::Text(message) => {
                                    // All remaining messages should be relay messages.
                                    let relay_message: RelayMessage =
                                        serde_json::from_str(message.as_str())
                                            .context("failed to parse as RelayMessage")?;
                                    lobby
                                        .relay(relay_message)
                                        .await
                                        .context("failed to relay message")?;
                                }
                                Message::Binary(_vec) => {
                                    // Wrong! Hang up on the peer.
                                    // For now just ignore.
                                    log::debug!("ignoring binary msg")
                                }
                                Message::Ping(_) => (),
                                Message::Pong(_) => (),
                                Message::Close(_) => (),
                            },
                        },
                    }
                }
            }
        }
    }

    pub(crate) async fn handle_create_lobby(
        &self,
        peer: NewPeer<S>,
        mesh: bool,
    ) -> Result<JoinHandle<()>, Error> {
        let (name, vacant_entry) = loop {
            let name = Lobby::<S>::new_name();
            let entry = self.0.lobbies.entry_async(name.clone()).await;
            match entry {
                scc::hash_map::Entry::Occupied(_) => continue,
                scc::hash_map::Entry::Vacant(vacant_entry) => {
                    break (name, vacant_entry);
                }
            }
        };
        // Arbitrary limit for backpressure.
        let (sender, receiver) = mpsc::channel(32);
        vacant_entry.insert_entry(sender);
        let me = self.clone();
        Ok(tokio::spawn(async move {
            match me
                .new_lobby_task(name, peer.websocket, mesh, receiver)
                .await
            {
                Ok(()) => (),
                Err(e) => {
                    log::error!("lobby task exited with error {e:?}");
                }
            }
        }))
    }

    pub(crate) async fn handle_join_lobby(
        &self,
        peer: NewPeer<S>,
        name: &str,
    ) -> Result<(), Error> {
        let Some(mut lobby) = self.0.lobbies.get_async(name).await else {
            return Err(eyre!("no such lobby {name}"));
        };
        lobby
            .get_mut()
            .send(peer)
            .await
            .map_err(|e| eyre!("error sending peer to lobby: {e:?}"))?;

        Ok(())
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) enum InitialMessage {
    Create { mesh: bool },
    Join { name: String },
}
