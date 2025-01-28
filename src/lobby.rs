use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::ws::Message;
use color_eyre::eyre::eyre;
use color_eyre::eyre::Context as _;
use color_eyre::eyre::Error;
use derivative::Derivative;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
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
    next_id: u32,
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
            next_id: 2,
        }
    }
    fn get_peer_id(peer: &Peer<S>) -> u32 {
        peer.id
    }

    async fn join(&mut self, new_peer: SplitSink<S, Message>, is_host: bool) -> Result<u32, Error> {
        let assigned = if is_host {
            1
        } else {
            let id = self.next_id;
            self.next_id += 1;
            id
        };
        let mut new_peer = Peer::new(assigned, new_peer);
        new_peer
            .send(&LobbyMessage::Id {
                assigned,
                mesh: self.mesh,
            })
            .await?;
        for (_, peer) in &mut self.peers {
            peer.send(&LobbyMessage::PeerConnect { peer_id: assigned })
                .await?;
            new_peer
                .send(&LobbyMessage::PeerConnect {
                    peer_id: Self::get_peer_id(peer),
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
            peer.send(&LobbyMessage::PeerDisconnect { peer_id: assigned })
                .await?;
        }

        Ok((None, leaving_peer))
    }

    async fn relay(&mut self, relay_message: RelayMessage, src_id: u32) -> Result<(), Error> {
        let RelayMessage {
            kind,
            dest_id,
            data,
        } = relay_message;
        let Some(peer) = self.peers.get_mut(&dest_id) else {
            return Err(eyre!("no such peer {dest_id} to relay to"));
        };
        peer.send(&LobbyMessage::RelayedMessage {
            kind,
            data,
            peer_id: src_id,
        })
        .await
    }

    async fn handle_work_item(
        &mut self,
        work_item: LobbyWorkItem<S>,
    ) -> Result<WorkItemOk<S>, WorkItemErr> {
        match work_item {
            LobbyWorkItem::NewPeer(new_peer) => {
                let NewPeer { websocket } = new_peer;
                let (tx, rx) = websocket.split();
                let id = self
                    .join(tx, false /* is_host */)
                    .await
                    .context("failed to join new peer to lobby")
                    .map_err(WorkItemErr::NonFatal)?;
                Ok(WorkItemOk::InsertNewPeer(id, rx))
            }
            LobbyWorkItem::PeerMessage((id, opt_rx_result)) => {
                match opt_rx_result {
                    None => {
                        // Peer hung up on us.
                        // Remove it from the map.
                        if id == 1 {
                            Err(WorkItemErr::FatalForLobby(eyre!("host hung up")))
                        } else {
                            Err(WorkItemErr::FatalForPeer(id, eyre!("peer hung up")))
                        }
                    }
                    Some(result) => match result {
                        Err(e) => {
                            let e = eyre!("error receiving message: {e:?}");
                            if id == 1 {
                                return Err(WorkItemErr::FatalForLobby(e));
                            } else {
                                return Err(WorkItemErr::FatalForPeer(id, e));
                            }
                        }
                        Ok(message) => match message {
                            Message::Text(message) => {
                                // All remaining messages should be relay messages.
                                let relay_message: RelayMessage =
                                    serde_json::from_str(message.as_str())
                                        .context("failed to parse as RelayMessage")
                                        .map_err(|e| WorkItemErr::FatalForPeer(id, e))?;
                                self.relay(relay_message, id)
                                    .await
                                    .context("failed to relay message")
                                    .map_err(WorkItemErr::NonFatal)?;
                                Ok(WorkItemOk::Noop)
                            }
                            Message::Binary(_vec) => Err(WorkItemErr::FatalForPeer(
                                id,
                                eyre!("should never get binary websocket message"),
                            )),
                            Message::Ping(_) | Message::Pong(_) | Message::Close(_) => {
                                Ok(WorkItemOk::Noop)
                            }
                        },
                    },
                }
            }
        }
    }
}

/// If we receive this, we need to drain all the peers
/// from the lobby and close all their websockets.
#[must_use]
struct CloseLobby;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "payload")]
enum LobbyMessage {
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
        peer_id: u32,
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

#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
enum LobbyWorkItem<S: Socket> {
    NewPeer(#[derivative(Debug = "ignore")] NewPeer<S>),
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

#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
enum WorkItemOk<S: Socket> {
    InsertNewPeer(u32, #[derivative(Debug = "ignore")] SplitStream<S>),
    Noop,
}

#[derive(Debug)]
enum WorkItemErr {
    NonFatal(Error),
    FatalForPeer(u32, Error),
    FatalForLobby(Error),
}

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
        host.send(&LobbyMessage::CreatedLobby {
            name: lobby.name.clone(),
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
            log::debug!("work item: {work_item:?}");

            let result = lobby.handle_work_item(work_item).await;
            match result {
                Ok(ok) => match ok {
                    WorkItemOk::InsertNewPeer(id, rx) => {
                        peer_rx.get_mut().insert(
                            id,
                            rx.map(Some)
                                .chain(futures::stream::once(futures::future::ready(None))),
                        );
                    }
                    WorkItemOk::Noop => (),
                },
                Err(err) => match err {
                    WorkItemErr::NonFatal(err) => {
                        log::info!("non-fatal error handling work item: {err:?}");
                    }
                    WorkItemErr::FatalForPeer(id, err) => {
                        log::info!("hanging up on peer {id} due to fatal error {err:?}");
                        let _ = peer_rx.get_mut().remove(&id);
                        let result = lobby
                            .leave(id)
                            .await
                            .context("failed to leave lobby with errored peer");
                        let (close, _peer) = match result {
                            Err(e) => {
                                log::warn!("error leaving lobby with errored peer: {e:?}");
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
                    WorkItemErr::FatalForLobby(err) => {
                        log::info!("closing lobby due to fatal error: {err:?}");
                        return Ok(());
                    }
                },
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
                    log::info!("lobby task exited with error {e:?}");
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
#[serde(tag = "kind", content = "payload")]
pub(crate) enum InitialMessage {
    Create { mesh: bool },
    Join { name: String },
}
