use crate::{
    AppState, ClientHandle, ClientRole, PendingLogin, RegisterOutcome, RoomState, ServerMessage,
    UnregisterOutcome, random_token,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

impl AppState {
    pub(crate) async fn register(
        &self,
        room_handle: &str,
        role: ClientRole,
        preferred_client_id: Option<&str>,
        tx: mpsc::Sender<ServerMessage>,
        penalized: Arc<AtomicBool>,
    ) -> Result<RegisterOutcome, String> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        self.prune_expired_rooms(&mut inner, now);
        let occupied_client_ids = {
            let Some(room) = inner.rooms.get(room_handle) else {
                return Err(String::from("channel not found"));
            };
            room.agents
                .keys()
                .chain(room.zookeepers.keys())
                .cloned()
                .collect::<HashSet<_>>()
        };
        let preferred_client_id = preferred_client_id
            .filter(|client_id| !client_id.is_empty() && !occupied_client_ids.contains(*client_id));
        let client_id = match preferred_client_id {
            Some(client_id) => client_id.to_owned(),
            None => loop {
                inner.next_client_id += 1;
                let candidate = format!("{}-{}", role.as_prefix(), inner.next_client_id);
                if !occupied_client_ids.contains(&candidate) {
                    break candidate;
                }
            },
        };
        let mut initial_messages = Vec::new();
        let mut broadcasts = Vec::new();

        let Some(room) = inner.rooms.get_mut(room_handle) else {
            return Err(String::from("channel not found"));
        };
        room.last_active_at = now;

        let agents = room.agents.keys().cloned().collect::<Vec<_>>();
        let zookeepers = room.zookeepers.keys().cloned().collect::<Vec<_>>();
        initial_messages.push(ServerMessage::Connected {
            client_id: client_id.clone(),
            role: role.clone(),
            agents,
            zookeepers,
        });

        match role {
            ClientRole::Agent => {
                room.agents.insert(
                    client_id.clone(),
                    ClientHandle {
                        tx,
                        penalized: Arc::clone(&penalized),
                    },
                );
                for zookeeper in room.zookeepers.values() {
                    broadcasts.push((
                        zookeeper.clone(),
                        ServerMessage::PeerJoined {
                            role: ClientRole::Agent,
                            client_id: client_id.clone(),
                        },
                    ));
                }
            }
            ClientRole::Zookeeper => {
                room.zookeepers.insert(
                    client_id.clone(),
                    ClientHandle {
                        tx,
                        penalized: Arc::clone(&penalized),
                    },
                );
                for agent in room.agents.values() {
                    broadcasts.push((
                        agent.clone(),
                        ServerMessage::PeerJoined {
                            role: ClientRole::Zookeeper,
                            client_id: client_id.clone(),
                        },
                    ));
                }
            }
        }

        Ok(RegisterOutcome {
            client_id,
            initial_messages,
            broadcasts,
        })
    }

    pub(crate) async fn unregister(
        &self,
        room_handle: &str,
        role: ClientRole,
        client_id: &str,
    ) -> UnregisterOutcome {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        self.prune_expired_rooms(&mut inner, now);
        let mut broadcasts = Vec::new();

        if let Some(room) = inner.rooms.get_mut(room_handle) {
            match role {
                ClientRole::Agent => {
                    let removed = room.agents.remove(client_id).is_some();
                    if removed {
                        room.last_active_at = now;
                        for zookeeper in room.zookeepers.values() {
                            broadcasts.push((
                                zookeeper.clone(),
                                ServerMessage::PeerLeft {
                                    role: ClientRole::Agent,
                                    client_id: client_id.to_owned(),
                                },
                            ));
                        }
                    }
                }
                ClientRole::Zookeeper => {
                    let removed = room.zookeepers.remove(client_id).is_some();
                    if removed {
                        room.last_active_at = now;
                        for agent in room.agents.values() {
                            broadcasts.push((
                                agent.clone(),
                                ServerMessage::PeerLeft {
                                    role: ClientRole::Zookeeper,
                                    client_id: client_id.to_owned(),
                                },
                            ));
                        }
                    }
                }
            }
        }
        self.prune_expired_rooms(&mut inner, now);

        UnregisterOutcome { broadcasts }
    }

    pub(crate) async fn candidate_login_for_room(
        &self,
        room_handle: &str,
        credential_request: &[u8],
    ) -> Result<Option<PendingLogin>, String> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        self.prune_expired_rooms(&mut inner, now);
        let Some(password_file) = inner
            .rooms
            .get(room_handle)
            .map(|room| room.password_file.clone())
        else {
            return Ok(None);
        };
        drop(inner);

        let (credential_response, login_session) =
            self.pake
                .start_login(room_handle.as_bytes(), &password_file, credential_request)?;

        Ok(Some(PendingLogin {
            candidate_id: random_token()?,
            room_handle: room_handle.to_owned(),
            credential_response,
            login_session,
        }))
    }

    pub(crate) async fn store_room(
        &self,
        room_handle: String,
        password_file: Vec<u8>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        self.prune_expired_rooms(&mut inner, now);
        if inner.rooms.len() >= self.config.max_rooms {
            return Err(String::from("room quota exceeded"));
        }
        if inner.rooms.contains_key(&room_handle) {
            return Err(String::from("generated duplicate room handle"));
        }

        inner.rooms.insert(
            room_handle,
            RoomState {
                password_file,
                agents: HashMap::new(),
                zookeepers: HashMap::new(),
                last_active_at: now,
                signal_events: VecDeque::new(),
            },
        );
        Ok(())
    }

    pub(crate) async fn ensure_room_capacity(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        self.prune_expired_rooms(&mut inner, Instant::now());
        if inner.rooms.len() >= self.config.max_rooms {
            return Err(String::from("room quota exceeded"));
        }
        Ok(())
    }

    pub(crate) async fn check_auth_rate_limit(&self, ip: IpAddr) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        self.prune_expired_rooms(&mut inner, now);
        let events = inner.auth_events_by_ip.entry(ip).or_default();
        prune_event_window(events, self.config.auth_rate_window, now);
        if events.len() >= self.config.auth_rate_limit {
            return Err(String::from(
                "too many authentication attempts from this IP",
            ));
        }
        events.push_back(now);
        Ok(())
    }

    pub(crate) fn allows_agent_room_creation(&self) -> bool {
        self.config.allow_agent_room_creation
    }

    pub(crate) async fn forward_signal(
        &self,
        room_handle: &str,
        sender_id: &str,
        peer_id: &str,
        payload: Value,
    ) -> Result<(ClientHandle, ServerMessage), String> {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        self.prune_expired_rooms(&mut inner, now);
        let Some(room) = inner.rooms.get_mut(room_handle) else {
            return Err(String::from("channel not found"));
        };
        prune_event_window(
            &mut room.signal_events,
            self.config.room_signal_rate_window,
            now,
        );
        if room.signal_events.len() >= self.config.room_signal_rate_limit {
            return Err(String::from("room signaling rate limit exceeded"));
        }
        room.signal_events.push_back(now);
        room.last_active_at = now;

        let sender_role = if room.agents.contains_key(sender_id) {
            Some(ClientRole::Agent)
        } else if room.zookeepers.contains_key(sender_id) {
            Some(ClientRole::Zookeeper)
        } else {
            None
        };
        let Some(sender_role) = sender_role else {
            return Err(format!("sender `{sender_id}` is not connected"));
        };

        let peer_role = if room.agents.contains_key(peer_id) {
            Some(ClientRole::Agent)
        } else if room.zookeepers.contains_key(peer_id) {
            Some(ClientRole::Zookeeper)
        } else {
            None
        };
        let Some(peer_role) = peer_role else {
            return Err(format!("peer `{peer_id}` is not connected"));
        };

        if sender_role == ClientRole::Agent && peer_role == ClientRole::Agent {
            return Err(String::from("agent-to-agent signaling is not supported"));
        }

        let Some(peer) = room
            .agents
            .get(peer_id)
            .or_else(|| room.zookeepers.get(peer_id))
        else {
            return Err(format!("peer `{peer_id}` is not connected"));
        };
        let peer = peer.clone();

        Ok((
            peer,
            ServerMessage::Signal {
                peer_id: sender_id.to_owned(),
                payload,
            },
        ))
    }

    fn prune_expired_rooms(&self, inner: &mut crate::StateInner, now: Instant) {
        let room_ttl = self.config.room_ttl;
        inner.rooms.retain(|_, room| {
            if !room.agents.is_empty() || !room.zookeepers.is_empty() {
                return true;
            }

            now.saturating_duration_since(room.last_active_at) < room_ttl
        });
        inner.auth_events_by_ip.retain(|_, events| {
            prune_event_window(events, self.config.auth_rate_window, now);
            !events.is_empty()
        });
    }
}

fn prune_event_window(events: &mut VecDeque<Instant>, window: Duration, now: Instant) {
    while let Some(ts) = events.front().copied() {
        if now.saturating_duration_since(ts) < window {
            break;
        }
        let _ = events.pop_front();
    }
}
