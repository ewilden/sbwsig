use std::collections::BTreeMap;

use axum::extract::ws::{Message, WebSocket};
use color_eyre::eyre::eyre;
use color_eyre::eyre::Context as _;
use color_eyre::eyre::Error;
use futures::stream::SplitSink;
use futures::SinkExt as _;
use rand::distributions::Alphanumeric;
use rand::Rng;

pub(crate) struct Peer {
    id: u32,
    websocket: SplitSink<WebSocket, Message>,
}

impl Peer {
    pub(crate) fn new(id: u32, websocket: SplitSink<WebSocket, Message>) -> Self {
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

pub(crate) struct Lobby {
    name: String,
    host: u32,
    mesh: bool,
    peers: BTreeMap<u32, Peer>,
    sealed: bool,
}

impl Lobby {
    fn new(host: u32, mesh: bool) -> Self {
        Lobby {
            name: rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(16)
                .map(char::from)
                .collect::<String>(),
            host,
            mesh,
            peers: Default::default(),
            sealed: false,
        }
    }
    fn get_peer_id(host: u32, peer: &Peer) -> u32 {
        if host == peer.id {
            1
        } else {
            peer.id
        }
    }

    async fn join(&mut self, mut new_peer: Peer) -> Result<(), Error> {
        let host = self.host;
        let assigned = Self::get_peer_id(host, &new_peer);
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
                        peer_id: Self::get_peer_id(host, peer),
                    },
                })
                .await?;
        }
        self.peers.insert(new_peer.id, new_peer);
        Ok(())
    }

    async fn leave(&mut self, peer_id: u32) -> Result<(Option<CloseLobby>, Peer), Error> {
        let host = self.host;
        let Some(leaving_peer) = self.peers.remove(&peer_id) else {
            return Err(eyre!(
                "peer {peer_id} tried to leave lobby {} without being in it",
                self.name
            ));
        };
        let assigned = Self::get_peer_id(host, &leaving_peer);
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

    async fn seal(&mut self, peer_id: u32) -> Result<SealedLobby, Error> {
        let host = self.host;
        if host != peer_id {
            return Err(eyre!("only the host can seal"));
        }
        self.sealed = true;
        for (_, peer) in &mut self.peers {
            peer.send(&LobbyMessage {
                payload: LobbyPayload::Seal,
            })
            .await?;
        }
        log::info!(
            "Peer {peer_id} sealed lobby {} with {} peers",
            &self.name,
            self.peers.len()
        );
        Ok(SealedLobby)
    }

    async fn relay(&mut self, relay_message: RelayMessage) -> Result<(), Error> {
        let RelayMessage {
            kind,
            dest_id,
            data,
        } = relay_message;
        let dest_id = if dest_id == 1 { self.host } else { dest_id };
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

/// If we receive this, the lobby has been sealed and
/// we should close all the connections once we're
/// confident everybody has joined.
/// TODO: Perhaps remove this functionality if we want
/// people to be able to join a lobby midway through.
#[must_use]
struct SealedLobby;

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

#[derive(Default)]
pub(crate) struct Lobbies {
    lobbies: scc::HashMap<String, Lobby>,
}

impl Lobbies {
    pub(crate) async fn handle_create_lobby(
        &self,
        mut peer: Peer,
        mesh: bool,
    ) -> Result<(), Error> {
        let mut occupied_entry = loop {
            let lobby = Lobby::new(peer.id, mesh);
            let entry = self.lobbies.entry_async(lobby.name.clone()).await;
            match entry {
                scc::hash_map::Entry::Occupied(_) => continue,
                scc::hash_map::Entry::Vacant(vacant_entry) => {
                    break vacant_entry.insert_entry(lobby)
                }
            }
        };
        let lobby = occupied_entry.get_mut();
        peer.send(&LobbyMessage {
            payload: LobbyPayload::CreatedLobby {
                name: lobby.name.clone(),
            },
        })
        .await?;
        lobby.join(peer).await?;
        Ok(())
    }

    pub(crate) async fn handle_join_lobby(&self, peer: Peer, name: &str) -> Result<(), Error> {
        let Some(mut lobby) = self.lobbies.get_async(name).await else {
            return Err(eyre!("no such lobby {name}"));
        };
        lobby.join(peer).await?;

        Ok(())
    }

    pub(crate) async fn handle_seal(&mut self, peer_id: u32, name: &str) -> Result<(), Error> {
        let Some(mut lobby) = self.lobbies.get_async(name).await else {
            return Err(eyre!("no such lobby {name}"));
        };
        let SealedLobby = lobby.seal(peer_id).await?;
        // TODO: handle SealedLobby
        Ok(())
    }

    pub(crate) async fn handle_relay_message(
        &mut self,
        relay_message: RelayMessage,
        name: &str,
    ) -> Result<(), Error> {
        let Some(mut lobby) = self.lobbies.get_async(name).await else {
            return Err(eyre!("no such lobby {name}"));
        };
        lobby.relay(relay_message).await
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) enum InitialMessage {
    Create { mesh: bool },
    Join { name: String },
}
