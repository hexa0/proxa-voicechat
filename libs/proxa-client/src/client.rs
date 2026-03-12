use anyhow::{Context, Result};
use parking_lot::Mutex;
use proxa_protocol::{ClientMessage, ServerAudioPacket, ServerMessage};
use quinn::{Connection, Endpoint};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::aec3::AecWrapper;
use crate::encode::{EncodeState, run_encode_task};
use crate::peer::PeerState;
use crate::types::{DenoiseMethod, FRAME_DURATION_MS, OPUS_SAMPLE_RATE, SILENCE_BITRATE};

pub struct ClientState {
    pub peers: HashMap<u32, PeerState>,
    pub channels: opus::Channels,
    pub samples_per_frame: usize,
    pub global_loss_rate: f32,
    pub report_tx: Option<mpsc::UnboundedSender<ClientMessage>>,
}

impl ClientState {
    pub fn handle_audio_packet(&mut self, packet: ServerAudioPacket) {
        if let Some(peer) = self.peers.get_mut(&packet.peer_id) {
            if peer.awaiting_first_packet {
                peer.next_decode_seq = packet.sequence;
                peer.awaiting_first_packet = false;
            } else if peer.is_buffering && packet.sequence < peer.next_decode_seq {
                peer.next_decode_seq = packet.sequence;
            }
            if packet.sequence + 1000 >= peer.next_decode_seq {
                if packet.sequence < peer.next_decode_seq {
                    peer.target_jitter_frames = (peer.target_jitter_frames + 1).min(1000);
                }
                peer.jitter_buffer.insert(packet.sequence, packet.payload);
            }
            if peer.jitter_buffer.len() > 1000 {
                if let Some((&first_key, _)) = peer.jitter_buffer.iter().next() {
                    peer.jitter_buffer.remove(&first_key);
                }
            }
        }
    }

    pub fn handle_server_message(
        &mut self,
        msg: ServerMessage,
        encode_state: &Arc<Mutex<EncodeState>>,
    ) {
        match msg {
            ServerMessage::PeerJoined { peer_id } => {
                if let Ok(peer) = PeerState::new(self.channels) {
                    self.peers.insert(peer_id, peer);
                }
            }
            ServerMessage::PeerLeft { peer_id } => {
                self.peers.remove(&peer_id);
            }
            ServerMessage::TargetLossRate(rate) => {
                self.global_loss_rate = rate.clamp(0.0, 0.90);
                let pct = (rate * 100.0) as i32;
                let mut encode = encode_state.lock();
                encode.actual_loss_perc = pct;
                if !encode.is_throttled {
                    let _ = encode.encoder.set_packet_loss_perc(pct);
                }
            }
            ServerMessage::PeerSilence { peer_id, silenced } => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    if peer.is_silenced && !silenced {
                        if peer.jitter_buffer.is_empty() {
                            peer.is_buffering = true;
                            peer.awaiting_first_packet = true;
                            peer.buffer.clear();
                            let _ = peer.decoder.reset_state();
                        }
                    } else if !peer.is_silenced && silenced {
                        peer.jitter_buffer.clear();
                        peer.awaiting_first_packet = true;
                    }
                    peer.is_silenced = silenced;
                }
            }
            _ => {}
        }
    }
}

pub struct ProxaClient {
    pub connection: Connection,
    pub state: Arc<Mutex<ClientState>>,
    pub encode_state: Arc<Mutex<EncodeState>>,
    pub mic_tx: mpsc::UnboundedSender<Vec<f32>>,
    pub far_end_tx: mpsc::UnboundedSender<Vec<f32>>,
    _encode_task: JoinHandle<()>,
    _network_task: JoinHandle<()>,
    _disconnect_tx: mpsc::Sender<()>,
}

impl ProxaClient {
    pub async fn connect(
        server_host: &str,
        room_name: &str,
        channels: opus::Channels,
        allow_self_signed: bool,
    ) -> Result<Self> {
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;

        let host_port = if server_host.contains(':') {
            server_host.to_string()
        } else {
            format!("{}:39201", server_host)
        };
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&host_port).await?.collect();
        let mut target_addr = addrs.iter().find(|a| a.is_ipv6()).copied();
        if target_addr.is_none() {
            target_addr = addrs.first().copied();
        }
        let target_addr = target_addr.context("Failed to resolve server hostname")?;

        #[derive(Debug)]
        struct SkipServerVerification;
        impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
            fn verify_server_cert(
                &self,
                _end_entity: &rustls::pki_types::CertificateDer<'_>,
                _intermediates: &[rustls::pki_types::CertificateDer<'_>],
                _server_name: &rustls::pki_types::ServerName<'_>,
                _ocsp_response: &[u8],
                _now: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _m: &[u8],
                _c: &rustls::pki_types::CertificateDer<'_>,
                _d: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _m: &[u8],
                _c: &rustls::pki_types::CertificateDer<'_>,
                _d: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                rustls::crypto::ring::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }

        let mut rustls_config = if allow_self_signed {
            let mut config = rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth();
            config
                .dangerous()
                .set_certificate_verifier(std::sync::Arc::new(SkipServerVerification));
            config
        } else {
            let native_certs = rustls_native_certs::load_native_certs();
            let mut root_store = rustls::RootCertStore::empty();
            for cert in native_certs.certs {
                let _ = root_store.add(cert);
            }
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        rustls_config.alpn_protocols = vec![b"proxa-hq".to_vec()];
        let mut transport_config = quinn::TransportConfig::default();
        transport_config
            .max_idle_timeout(Some(std::time::Duration::from_secs(5).try_into().unwrap()));
        transport_config.keep_alive_interval(Some(std::time::Duration::from_secs(1)));

        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));
        client_config.transport_config(Arc::new(transport_config));
        endpoint.set_default_client_config(client_config);

        let connection = endpoint.connect(target_addr, "localhost")?.await?;
        let (mut ctrl_send, mut ctrl_recv) = connection.open_bi().await?;

        let join_msg = bincode::serialize(&ClientMessage::JoinRoom(room_name.to_string()))?;
        ctrl_send
            .write_all(&(join_msg.len() as u32).to_le_bytes())
            .await?;
        ctrl_send.write_all(&join_msg).await?;

        let mut len_buf = [0u8; 4];
        ctrl_recv.read_exact(&mut len_buf).await?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut msg_buf = vec![0u8; len];
        ctrl_recv.read_exact(&mut msg_buf).await?;

        let srv_msg: ServerMessage = bincode::deserialize(&msg_buf)?;
        match srv_msg {
            ServerMessage::RoomJoined { peer_id } => {
                log::info!("joined room with peer ID: {}", peer_id);
            }
            ServerMessage::Error(e) => {
                anyhow::bail!("server refused connection: {}", e);
            }
            _ => {
                anyhow::bail!("unexpected initial message from server");
            }
        }

        let mut encoder = opus::Encoder::new(OPUS_SAMPLE_RATE, channels, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(32000))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(0)?;

        let num_channels = if channels == opus::Channels::Stereo {
            2
        } else {
            1
        };
        let samples_per_frame =
            (OPUS_SAMPLE_RATE as usize * FRAME_DURATION_MS * num_channels) / 1000;

        let (mic_tx, mic_rx) = mpsc::unbounded_channel::<Vec<f32>>();
        let (far_end_tx, far_end_rx) = mpsc::unbounded_channel::<Vec<f32>>();
        let (report_tx, mut report_rx) = mpsc::unbounded_channel();

        let state = Arc::new(Mutex::new(ClientState {
            peers: HashMap::new(),
            channels,
            samples_per_frame,
            global_loss_rate: 0.0,
            report_tx: Some(report_tx),
        }));

        encoder.set_bitrate(opus::Bitrate::Bits(SILENCE_BITRATE.min(32000)))?;
        encoder.set_inband_fec(false)?;
        encoder.set_packet_loss_perc(0)?;

        let global_engine = crate::dfn3::GLOBAL_DFN3_ENGINE.lock();
        let (engine, path) = match &*global_engine {
            Some((p, e)) => (Some(e.clone()), Some(p.clone())),
            None => (None, None),
        };
        drop(global_engine);

        let encode_state = Arc::new(Mutex::new(EncodeState {
            mic_buffer: Vec::new(),
            far_end_buffer: Vec::new(),
            encoder,
            channels,
            samples_per_frame,
            simulated_loss: 0.0,
            denoise_method: DenoiseMethod::Off,
            aec_enabled: false,
            rnnoise_state_left: Some(*nnnoiseless::DenoiseState::new()),
            rnnoise_state_right: Some(*nnnoiseless::DenoiseState::new()),
            aec: Some(AecWrapper(
                aec3::voip::VoipAec3::builder(OPUS_SAMPLE_RATE as usize, 1, 1)
                    .build()
                    .expect("Failed to build AEC3"),
            )),
            dfn3_engine: engine,
            dfn3_model_path: path,
            dfn3_loading: false,
            next_send_sequence: 0,
            volume: 0.0,
            target_bitrate: 32000,
            last_voice_time: std::time::Instant::now() - std::time::Duration::from_secs(1),
            is_throttled: true,
            actual_loss_perc: 0,
        }));

        let state_clone = state.clone();
        let encode_state_clone = encode_state.clone();
        let conn_clone = connection.clone();
        let (disconnect_tx, mut disconnect_rx) = mpsc::channel(1);
        let report_tx_encode = state.lock().report_tx.clone();

        if let Some(ref tx) = report_tx_encode {
            let _ = tx.send(ClientMessage::SetSilence(true));
        }

        let _encode_task = tokio::spawn(run_encode_task(
            encode_state.clone(),
            mic_rx,
            far_end_rx,
            report_tx_encode,
            connection.clone(),
        ));

        let _network_task = tokio::spawn(async move {
            let mut ctrl_recv = ctrl_recv;
            let mut ctrl_send = ctrl_send;
            let mut len_buf = [0u8; 4];

            loop {
                tokio::select! {
                    _ = disconnect_rx.recv() => {
                         let msg = bincode::serialize(&ClientMessage::LeaveRoom).unwrap();
                         let _ = ctrl_send.write_all(&(msg.len() as u32).to_le_bytes()).await;
                         let _ = ctrl_send.write_all(&msg).await;
                         break;
                    }
                    Some(msg) = report_rx.recv() => {
                         let msg_bytes = bincode::serialize(&msg).unwrap();
                         let _ = ctrl_send.write_all(&(msg_bytes.len() as u32).to_le_bytes()).await;
                         let _ = ctrl_send.write_all(&msg_bytes).await;
                    }
                    datagram = conn_clone.read_datagram() => {
                        match datagram {
                            Ok(data) => {
                                if let Some(packet) = ServerAudioPacket::deserialize(&data) {
                                    state_clone.lock().handle_audio_packet(packet);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    res = ctrl_recv.read_exact(&mut len_buf) => {
                        if res.is_err() { break; }
                        let len = u32::from_le_bytes(len_buf) as usize;
                        let mut msg_buf = vec![0u8; len];
                        if ctrl_recv.read_exact(&mut msg_buf).await.is_err() { break; }
                        if let Ok(msg) = bincode::deserialize::<ServerMessage>(&msg_buf) {
                            state_clone.lock().handle_server_message(msg, &encode_state_clone);
                        }
                    }
                }
            }
        });

        log::info!("ProxaClient::connect finished");
        Ok(Self {
            connection,
            state,
            encode_state,
            mic_tx,
            far_end_tx,
            _encode_task,
            _network_task,
            _disconnect_tx: disconnect_tx,
        })
    }

    pub fn set_simulated_loss(&self, loss_pct: f32) {
        let mut state = self.encode_state.lock();
        state.simulated_loss = loss_pct.clamp(0.0, 1.0);
    }

    pub fn set_bitrate(&self, bitrate: i32) -> Result<()> {
        let mut state = self.encode_state.lock();
        state.target_bitrate = bitrate;
        if !state.is_throttled {
            state
                .encoder
                .set_bitrate(opus::Bitrate::Bits(bitrate))
                .context("Failed to set bitrate")?;
        }
        Ok(())
    }

    pub fn get_bitrate(&self) -> i32 {
        self.encode_state.lock().target_bitrate
    }
    pub fn is_silent(&self) -> bool {
        self.encode_state.lock().is_throttled
    }
    pub fn set_denoise_method(&self, method: DenoiseMethod) {
        self.encode_state.lock().denoise_method = method;
    }
    pub fn set_aec(&self, enabled: bool) {
        self.encode_state.lock().aec_enabled = enabled;
    }
    pub fn load_dfn3_models<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        self.encode_state.lock().load_dfn3_models(path)
    }
    pub fn get_denoise_method(&self) -> DenoiseMethod {
        self.encode_state.lock().denoise_method
    }
    pub fn get_aec_enabled(&self) -> bool {
        self.encode_state.lock().aec_enabled
    }
    pub fn get_channels(&self) -> opus::Channels {
        self.state.lock().channels
    }
    pub fn push_audio(&self, pcm: &[f32]) {
        let _ = self.mic_tx.send(pcm.to_vec());
    }

    pub fn pop_audio(&self, pcm: &mut [f32]) {
        for sample in pcm.iter_mut() {
            *sample = 0.0;
        }
        let mut state = self.state.lock();
        let client_channels = state.channels;
        let spf = state.samples_per_frame;
        let report_tx = state.report_tx.clone();

        for (&peer_id, peer) in state.peers.iter_mut() {
            if peer.is_silenced && peer.jitter_buffer.is_empty() {
                continue;
            }
            let num_channels = if client_channels == opus::Channels::Stereo {
                2
            } else {
                1
            };

            // refill the buffer using PeerState logic
            peer.refill_buffer(
                pcm.len(),
                num_channels,
                spf,
                client_channels == opus::Channels::Stereo,
            );

            let available = peer.buffer.len();
            let to_read = available.min(pcm.len());
            for i in 0..to_read {
                pcm[i] += peer.buffer[i];
            }
            peer.buffer.drain(..to_read);

            // report metrics if enough samples collected
            if peer.stat_expected >= 50 {
                let rate = peer.stat_lost as f32 / peer.stat_expected as f32;
                peer.stat_expected = 0;
                peer.stat_lost = 0;
                if (rate - peer.reported_loss_rate).abs() > 0.02 || rate == 0.0 || rate >= 1.0 {
                    peer.reported_loss_rate = rate;
                    if let Some(tx) = &report_tx {
                        let _ = tx.send(ClientMessage::ReportPeerLoss {
                            peer_id,
                            loss_rate: rate,
                        });
                    }
                }
            }
        }
        let _ = self.far_end_tx.send(pcm.to_vec());
    }

    pub fn get_peer_stats(&self) -> Vec<(u32, f32, usize)> {
        self.state
            .lock()
            .peers
            .iter()
            .map(|(&k, v)| (k, v.volume, v.target_jitter_frames))
            .collect()
    }

    pub fn get_local_stats(&self) -> f32 {
        self.encode_state.lock().volume
    }
    pub fn get_max_loss_rate(&self) -> f32 {
        self.state.lock().global_loss_rate
    }
    pub fn leave(&self) {
        let _ = self._disconnect_tx.try_send(());
    }

    pub fn auto_load_models(&self) {
        let models_search_paths = [
            std::path::PathBuf::from("models"),
            std::path::PathBuf::from("model"),
            std::path::PathBuf::from("dfn3"),
        ];

        let mut lock = self.encode_state.lock();
        if lock.dfn3_engine.is_some() {
            return;
        }

        for path in &models_search_paths {
            if path.exists() {
                match lock.load_dfn3_models(path) {
                    Ok(_) => {
                        log::info!("DeepFilterNet3 models loaded successfully");
                        break;
                    }
                    Err(e) => {
                        log::warn!(
                            "found DeepFilterNet3 models at {:?} but failed to load: {}",
                            path,
                            e
                        );
                    }
                }
            }
        }
    }
}

impl Drop for ProxaClient {
    fn drop(&mut self) {
        // persist DFN3 engine for next connection
        let s = self.encode_state.lock();
        if let (Some(path), Some(engine)) = (&s.dfn3_model_path, &s.dfn3_engine) {
            *crate::dfn3::GLOBAL_DFN3_ENGINE.lock() = Some((path.clone(), engine.clone()));
        }
        self.leave();
    }
}
