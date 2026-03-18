use anyhow::Result;
use dashmap::DashMap;
use proxa_protocol::{ClientAudioPacket, ClientMessage, ServerAudioPacket, ServerMessage};
use quinn::{Connection, Endpoint, SendStream, ServerConfig};
use rand::RngCore;
use rand::seq::SliceRandom;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

const MAX_CONTROL_MESSAGE_SIZE: usize = 65536;
const MAX_AUDIO_PACKET_SIZE: usize = 1200;
const PEER_BITRATE_LIMIT: usize = 93750; // 750kbps in bytes/sec

struct Room {
    // [peer_id] -> (Connection, ControlSender, LossReports, IsSilenced, Channels)
    peers: DashMap<
        u32,
        (
            Connection,
            Arc<tokio::sync::Mutex<SendStream>>,
            DashMap<u32, f32>,
            bool,
            u8,
        ),
    >,
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub port: u16,
    pub cert_path: Option<std::path::PathBuf>,
    pub key_path: Option<std::path::PathBuf>,
}

struct AppState {
    rooms: DashMap<String, Arc<Room>>,
    next_peer_id: AtomicU32,
    id_mask: u32,
    id_perm: [u8; 32],
}

impl AppState {
    fn scramble_id(&self, id: u32) -> u32 {
        // XOR with a random constant
        let xored = id ^ self.id_mask;

        // Bit-permutation (swap bits to specific random indices)
        let mut result = 0u32;
        for i in 0..32 {
            if (xored >> i) & 1 == 1 {
                result |= 1 << self.id_perm[i];
            }
        }
        result
    }
}

pub async fn start_relay_server(config: RelayConfig) -> Result<()> {
    let (cert_chain, key) =
        if let (Some(cert_p), Some(key_p)) = (&config.cert_path, &config.key_path) {
            log::info!("loading SSL certificate from {:?} and {:?}", cert_p, key_p);
            let cert_file = std::fs::File::open(cert_p)?;
            let mut cert_reader = std::io::BufReader::new(cert_file);
            let certs = rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

            let key_file = std::fs::File::open(key_p)?;
            let mut key_reader = std::io::BufReader::new(key_file);
            let key = rustls_pemfile::private_key(&mut key_reader)?
                .ok_or_else(|| anyhow::anyhow!("no private key found in {:?}", key_p))?;

            (certs, key)
        } else {
            log::info!("generating self-signed certificate for localhost...");
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
            let cert_der = cert.cert.der().to_vec();
            let priv_key = cert.signing_key.serialize_der();

            let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
            let key = rustls::pki_types::PrivatePkcs8KeyDer::from(priv_key).into();
            (cert_chain, key)
        };

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;

    server_crypto.alpn_protocols = vec![b"proxa-hq".to_vec()];
    let mut transport_config = quinn::TransportConfig::default();
    transport_config.max_idle_timeout(Some(std::time::Duration::from_secs(5).try_into().unwrap()));

    let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_config));
    server_config.transport_config(Arc::new(transport_config));

    let bind_addr = format!("[::]:{}", config.port);
    let endpoint = Endpoint::server(server_config, bind_addr.parse()?)?;
    log::info!("proxa Relay listening on {}...", bind_addr);

    let mut rng = rand::thread_rng();
    let id_mask = rng.next_u32();
    let mut id_perm: Vec<u8> = (0..32u8).collect();
    id_perm.shuffle(&mut rng);
    let id_perm: [u8; 32] = id_perm.try_into().unwrap();

    let state = Arc::new(AppState {
        rooms: DashMap::new(),
        next_peer_id: AtomicU32::new(1),
        id_mask,
        id_perm,
    });

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Ok(connection) = incoming.await {
                let raw_id = state.next_peer_id.fetch_add(1, Ordering::Relaxed);
                let peer_id = state.scramble_id(raw_id);
                if let Err(e) = handle_connection(connection, peer_id, state).await {
                    log::error!("connection {} error: {}", peer_id, e);
                }
            }
        });
    }

    Ok(())
}

async fn notify_peer(send_stream: &Arc<tokio::sync::Mutex<SendStream>>, msg: &ServerMessage) {
    if let Ok(bytes) = bincode::serialize(msg) {
        let mut stream = send_stream.lock().await;
        let _ = stream.write_all(&(bytes.len() as u32).to_le_bytes()).await;
        let _ = stream.write_all(&bytes).await;
    }
}

async fn handle_connection(
    connection: Connection,
    peer_id: u32,
    state: Arc<AppState>,
) -> Result<()> {
    let (ctrl_send, mut ctrl_recv) = connection.accept_bi().await?;
    let ctrl_send = Arc::new(tokio::sync::Mutex::new(ctrl_send));

    let mut len_buf = [0u8; 4];
    ctrl_recv.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_CONTROL_MESSAGE_SIZE {
        anyhow::bail!("control message too large: {} bytes", len);
    }
    let mut msg_buf = vec![0u8; len];
    ctrl_recv.read_exact(&mut msg_buf).await?;

    let client_msg: ClientMessage = bincode::deserialize(&msg_buf)?;

    let (room_name, initial_channels) = match client_msg {
        ClientMessage::JoinRoom {
            room_name,
            channels,
        } => (room_name, channels),
        _ => anyhow::bail!("expected JoinRoom message"),
    };

    let room = state
        .rooms
        .entry(room_name.clone())
        .or_insert_with(|| {
            Arc::new(Room {
                peers: DashMap::new(),
            })
        })
        .clone();

    log::info!("peer {} joined room {}", peer_id, room_name);

    notify_peer(
        &ctrl_send,
        &ServerMessage::RoomJoined {
            peer_id,
            channels: initial_channels,
        },
    )
    .await;

    // notify newcomer about existing peers and their silence state
    for entry in room.peers.iter() {
        let other_id = *entry.key();
        let (_other_conn, other_ctrl, _other_loss_map, other_silenced, other_channels) =
            entry.value();

        notify_peer(
            &ctrl_send,
            &ServerMessage::PeerJoined {
                peer_id: other_id,
                channels: *other_channels,
            },
        )
        .await;
        if *other_silenced {
            notify_peer(
                &ctrl_send,
                &ServerMessage::PeerSilence {
                    peer_id: other_id,
                    silenced: true,
                },
            )
            .await;
        }

        // notify existing peer about newcomer
        notify_peer(
            other_ctrl,
            &ServerMessage::PeerJoined {
                peer_id,
                channels: initial_channels,
            },
        )
        .await;
    }

    room.peers.insert(
        peer_id,
        (
            connection.clone(),
            ctrl_send.clone(),
            DashMap::new(),
            true,
            initial_channels,
        ),
    ); // defaults to silenced

    let conn_clone = connection.clone();
    let room_clone = room.clone();
    tokio::spawn(async move {
        let mut last_reset = std::time::Instant::now();
        let mut bytes_sent_recent = 0usize;

        while let Ok(data) = conn_clone.read_datagram().await {
            // packet size limit
            if data.len() > MAX_AUDIO_PACKET_SIZE {
                log::warn!(
                    "peer {} sent oversized packet ({} bytes), dropping",
                    peer_id,
                    data.len()
                );
                continue;
            }

            // rate limiting
            let now = std::time::Instant::now();
            if now.duration_since(last_reset).as_secs() >= 1 {
                bytes_sent_recent = 0;
                last_reset = now;
            }

            bytes_sent_recent += data.len();
            if bytes_sent_recent > PEER_BITRATE_LIMIT {
                // Drop if exceeding 750kbps
                continue;
            }

            if let Some(client_pkt) = ClientAudioPacket::deserialize(&data) {
                let pkt = ServerAudioPacket {
                    peer_id,
                    sequence: client_pkt.sequence,
                    payload: client_pkt.payload.to_vec(),
                };
                let b = pkt.serialize();
                for entry in room_clone.peers.iter() {
                    let other_id = *entry.key();
                    if other_id != peer_id {
                        let other_conn = &entry.value().0;
                        let _ = other_conn.send_datagram(b.clone().into());
                    }
                }
            }
        }
    });

    loop {
        let mut len_buf = [0u8; 4];
        if ctrl_recv.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_CONTROL_MESSAGE_SIZE {
            log::warn!(
                "peer {} sent oversized control message: {} bytes",
                peer_id,
                len
            );
            break;
        }
        let mut msg_buf = vec![0u8; len];
        if ctrl_recv.read_exact(&mut msg_buf).await.is_err() {
            break;
        }

        if let Ok(msg) = bincode::deserialize::<ClientMessage>(&msg_buf) {
            match msg {
                ClientMessage::LeaveRoom => break,
                ClientMessage::JoinRoom { .. } => {
                    // already joined, ignore
                }
                ClientMessage::ReportPeerLoss {
                    peer_id: sender_id,
                    loss_rate,
                } => {
                    // update the sender's loss map with this listener's reported rate
                    if let Some(snd_entry) = room.peers.get(&sender_id) {
                        snd_entry.2.insert(peer_id, loss_rate);

                        // calculate the overall TargetLossRate for the sender
                        let mut max_rate = 0.0f32;
                        for entry in snd_entry.2.iter() {
                            if *entry.value() > max_rate {
                                max_rate = *entry.value();
                            }
                        }

                        let target_msg = ServerMessage::TargetLossRate(max_rate);
                        notify_peer(&snd_entry.1, &target_msg).await;
                    }
                }
                ClientMessage::SetSilence(silenced) => {
                    // update relay state
                    if let Some(mut entry) = room.peers.get_mut(&peer_id) {
                        entry.3 = silenced;
                    }

                    let silence_msg = ServerMessage::PeerSilence { peer_id, silenced };
                    for entry in room.peers.iter() {
                        let other_id = *entry.key();
                        if other_id != peer_id {
                            let other_send = &entry.value().1;
                            notify_peer(other_send, &silence_msg).await;
                        }
                    }
                }
                ClientMessage::SetChannels(channels) => {
                    if let Some(mut entry) = room.peers.get_mut(&peer_id) {
                        entry.4 = channels;
                    }

                    let channels_msg = ServerMessage::PeerChannels { peer_id, channels };
                    for entry in room.peers.iter() {
                        let other_id = *entry.key();
                        if other_id != peer_id {
                            let other_send = &entry.value().1;
                            notify_peer(other_send, &channels_msg).await;
                        }
                    }
                }
            }
        }
    }

    room.peers.remove(&peer_id);
    for entry in room.peers.iter() {
        let _other_id = *entry.key();
        let (_other_conn, other_ctrl, other_loss_map, _other_silenced, _other_channels) =
            entry.value();

        // remove departing peer from this peer's records
        other_loss_map.remove(&peer_id);

        // recalculate max loss rate for this peer since a reporter left
        let mut max_rate = 0.0f32;
        for report in other_loss_map.iter() {
            if *report.value() > max_rate {
                max_rate = *report.value();
            }
        }
        let _ = notify_peer(other_ctrl, &ServerMessage::TargetLossRate(max_rate)).await;

        notify_peer(other_ctrl, &ServerMessage::PeerLeft { peer_id }).await;
    }
    log::info!("peer {} left room {}", peer_id, room_name);

    Ok(())
}
