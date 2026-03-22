use crate::assets;
use crate::error::ProxaError;
use crate::error::Result as ProxaResult;
use parking_lot::Mutex;
use proxa_protocol::{ClientMessage, ServerAudioPacket, ServerMessage};
use quinn::Endpoint;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub static ACTIVE_CLIENT: parking_lot::Mutex<Option<Weak<ProxaClient>>> =
	parking_lot::const_mutex(None);

use crate::aec3::AecWrapper;
use crate::encode::{EncodeState, run_encode_task};
use crate::peer::PeerState;
use crate::types::{AudioDevice, DenoiseMethod, OPUS_SAMPLE_RATE, SILENCE_BITRATE};

pub trait AudioBackend: Send + Sync {
	fn enumerate_input_devices(&self) -> Vec<AudioDevice>;
	fn enumerate_output_devices(&self) -> Vec<AudioDevice>;
	fn set_input_device(&self, id: &str) -> ProxaResult<()>;
	fn set_output_device(&self, id: &str) -> ProxaResult<()>;
	fn get_current_input_device(&self) -> Option<AudioDevice>;
	fn get_current_output_device(&self) -> Option<AudioDevice>;
}

pub static AUDIO_BACKEND_PROVIDER: parking_lot::Mutex<Option<Box<dyn AudioBackend>>> =
	parking_lot::const_mutex(None);

pub fn register_audio_backend(backend: Box<dyn AudioBackend>) {
	*AUDIO_BACKEND_PROVIDER.lock() = Some(backend);
}

pub struct SfxState {
	pub pcm: Vec<f32>,
	pub position: usize,
}

pub struct ClientState {
	pub peers: HashMap<u32, PeerState>,
	pub channels: opus::Channels,
	pub samples_per_frame: usize,
	pub global_loss_rate: f32,
	pub mute_output: bool,
	pub report_tx: Option<mpsc::UnboundedSender<ClientMessage>>,
	pub sfx: Option<SfxState>,
}

impl ClientState {
	pub fn handle_audio_packet(&mut self, packet: ServerAudioPacket) {
		if packet.payload.len() > 1200 {
			// matches MAX_AUDIO_PACKET_SIZE
			log::warn!(
				"received oversized audio packet from peer {}",
				packet.peer_id
			);
			return;
		}
		if let Some(peer) = self.peers.get_mut(&packet.peer_id) {
			peer.total_bytes_received += packet.payload.len() as u64;
			if peer.awaiting_first_packet {
				peer.next_decode_seq = packet.sequence;
				peer.awaiting_first_packet = false;
			} else if peer.is_buffering && packet.sequence < peer.next_decode_seq {
				peer.next_decode_seq = packet.sequence;
			}
			if packet.sequence + 1000 >= peer.next_decode_seq {
				if packet.sequence < peer.next_decode_seq {
					// packet arrived after we already performed PLC (Loss Concealment) for it.
					// if it was counted as "lost", let's undo that stat as it's just "late" (jitter).
					if peer.lost_sequences.remove(&packet.sequence) {
						peer.stat_lost = peer.stat_lost.saturating_sub(1);
						peer.total_lost = peer.total_lost.saturating_sub(1);
					}

					// for late packets, increment jitter target if we're well into the stream.
					if !peer.is_buffering && peer.played_frames_since_silence > 50 {
						peer.target_jitter_frames = (peer.target_jitter_frames + 1).min(100);
					}
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
			ServerMessage::PeerJoined { peer_id, channels } => {
				let opus_channels = if channels >= 2 {
					opus::Channels::Stereo
				} else {
					opus::Channels::Mono
				};
				if let Ok(peer) = PeerState::new(opus_channels) {
					self.peers.insert(peer_id, peer);
					self.play_sfx("sounds/PeerJoin.opus");
				}
			}
			ServerMessage::PeerLeft { peer_id } => {
				self.peers.remove(&peer_id);
				self.play_sfx("sounds/PeerLeave.opus");
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
						// TOTAL SYNC: start every sentence from a perfect clean state.
						peer.is_buffering = true;
						peer.awaiting_first_packet = true;
						peer.buffer.clear();
						peer.jitter_buffer.clear(); // clear all stale packets from previous session

						// we will snap sequence when the first packet arrives in handle_audio_packet
						peer.next_decode_seq = 0;
						peer.resample_index = 0.0;
						peer.current_rate = 1.0;
						peer.target_jitter_frames = 3;
						peer.smoothed_current_latency = peer.target_jitter_frames as f32 + 0.5;
						peer.was_plc = false;

						peer.played_frames_since_silence = 0;
						peer.good_frames = 0;
						peer.stat_expected = 0;
						peer.stat_lost = 0;

						let _ = peer.decoder.reset_state();
					} else if !peer.is_silenced && silenced {
						peer.jitter_buffer.clear();
						peer.awaiting_first_packet = true;
						peer.is_buffering = true;
					}
					peer.is_silenced = silenced;
				}
			}
			ServerMessage::PeerChannels { peer_id, channels } => {
				if let Some(peer) = self.peers.get_mut(&peer_id) {
					let opus_channels = if channels >= 2 {
						opus::Channels::Stereo
					} else {
						opus::Channels::Mono
					};
					if let Ok(new_decoder) = opus::Decoder::new(OPUS_SAMPLE_RATE, opus_channels) {
						peer.decoder = new_decoder;
						peer.channels = opus_channels;
						peer.buffer.clear();
						peer.jitter_buffer.clear();
						peer.is_buffering = true;
						peer.awaiting_first_packet = true;
					}
				}
			}
			ServerMessage::PeerPing { peer_id, ping_ms } => {
				if let Some(peer) = self.peers.get_mut(&peer_id) {
					peer.ping = ping_ms;
				}
			}
			_ => {}
		}
	}

	pub fn play_sfx(&mut self, path: &str) {
		if let Some(pcm) = assets::get_sfx_pcm(path) {
			self.sfx = Some(SfxState { pcm, position: 0 });
		}
	}
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
	pub server_host: String,
	pub room_name: String,
	pub channels: opus::Channels,
	pub frame_duration_ms: f32,
	pub use_low_delay: bool,
	pub allow_self_signed: bool,
}

pub struct ProxaClient {
	pub state: Arc<Mutex<ClientState>>,
	pub encode_state: Arc<Mutex<EncodeState>>,
	pub mic_tx: mpsc::UnboundedSender<Vec<f32>>,
	pub far_end_tx: mpsc::UnboundedSender<Vec<f32>>,
	_encode_task: JoinHandle<()>,
	_network_task: JoinHandle<()>,
	_disconnect_tx: mpsc::Sender<()>,
	pub connection_slot: Arc<parking_lot::RwLock<Option<quinn::Connection>>>,
}

impl ProxaClient {
	pub fn new(config: ClientConfig) -> ProxaResult<Arc<Self>> {
		log::info!(
			"connecting to {} room '{}' (mic: {:?}, frame: {}ms, low_delay: {})",
			config.server_host,
			config.room_name,
			config.channels,
			config.frame_duration_ms,
			config.use_low_delay
		);

		let server_host_owned = config.server_host.clone();
		let room_name_owned = config.room_name.clone();
		let mut endpoint = Endpoint::client(
			"0.0.0.0:0"
				.parse::<std::net::SocketAddr>()
				.map_err(|e| ProxaError::Internal(e.to_string()))?,
		)
		.map_err(|e| ProxaError::Internal(e.to_string()))?;

		let models_search_paths = [
			std::path::PathBuf::from("models"),
			std::path::PathBuf::from("model"),
			std::path::PathBuf::from("dfn3"),
		];

		let mut dfn3_model_path_opt = None;
		let mut dfn3_engine_opt = None;

		let global_engine = crate::dfn3::GLOBAL_DFN3_ENGINE.lock();
		if let Some((p, e)) = &*global_engine {
			dfn3_model_path_opt = Some(p.clone());
			dfn3_engine_opt = Some(e.clone());
		}
		drop(global_engine);

		if dfn3_engine_opt.is_none() {
			for path in &models_search_paths {
				if path.exists() {
					dfn3_model_path_opt = Some(path.clone());
					log::info!(
						"DeepFilterNet3 models discovered for lazy loading at {:?}",
						path
					);
					break;
				}
			}
		}

		let quic_config = if config.allow_self_signed {
			crate::quic::make_client_config_self_signed()
		} else {
			crate::quic::make_client_config()
		};

		let mut transport_config = quinn::TransportConfig::default();
		transport_config
			.max_idle_timeout(Some(std::time::Duration::from_secs(1).try_into().unwrap()));
		transport_config.keep_alive_interval(Some(std::time::Duration::from_millis(250)));

		let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(quic_config)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));
		client_config.transport_config(Arc::new(transport_config));
		endpoint.set_default_client_config(client_config);

		let app_mode = if config.use_low_delay {
			opus::Application::LowDelay
		} else {
			opus::Application::Audio
		};

		let mut encoder = opus::Encoder::new(OPUS_SAMPLE_RATE, config.channels, app_mode)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		encoder
			.set_bitrate(opus::Bitrate::Bits(32000))
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		encoder
			.set_signal(opus::Signal::Voice)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		encoder
			.set_inband_fec(true)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		encoder
			.set_packet_loss_perc(0)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;

		let num_channels = if config.channels == opus::Channels::Stereo {
			2
		} else {
			1
		};
		let samples_per_frame =
			(OPUS_SAMPLE_RATE as f32 * config.frame_duration_ms * num_channels as f32 / 1000.0)
				as usize;

		let (mic_tx, mic_rx) = mpsc::unbounded_channel::<Vec<f32>>();
		let (far_end_tx, far_end_rx) = mpsc::unbounded_channel::<Vec<f32>>();
		let (report_tx, mut report_rx) = mpsc::unbounded_channel();

		let state = Arc::new(Mutex::new(ClientState {
			peers: HashMap::new(),
			channels: config.channels,
			samples_per_frame,
			global_loss_rate: 0.0,
			mute_output: false,
			report_tx: Some(report_tx),
			sfx: None,
		}));

		encoder
			.set_bitrate(opus::Bitrate::Bits(SILENCE_BITRATE.min(32000)))
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		encoder
			.set_inband_fec(false)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;
		encoder
			.set_packet_loss_perc(0)
			.map_err(|e| ProxaError::Internal(e.to_string()))?;

		let encode_state = Arc::new(Mutex::new(EncodeState {
			mic_buffer: Vec::new(),
			far_end_buffer: Vec::new(),
			encoder,
			channels: config.channels,
			samples_per_frame,
			simulated_outbound_loss: 0.0,
			simulated_outbound_jitter: 0.0,
			denoise_method: DenoiseMethod::Off,
			echo_cancellation_enabled: false,
			rnnoise_state_left: Some(*nnnoiseless::DenoiseState::new()),
			rnnoise_state_right: Some(*nnnoiseless::DenoiseState::new()),
			aec: Some(AecWrapper::new(OPUS_SAMPLE_RATE)),
			dfn3_engine: dfn3_engine_opt,
			dfn3_model_path: dfn3_model_path_opt,
			dfn3_loading: false,
			frame_duration_ms: config.frame_duration_ms,
			use_low_delay: config.use_low_delay,
			next_send_sequence: 0,
			volume: 0.0,
			target_bitrate: 32000,
			last_voice_time: std::time::Instant::now(),
			is_throttled: true,
			actual_loss_perc: 0,
			simulated_jitter_current_delay: 0.0,
			simulated_jitter_drift: 0.0,
			auto_normalize: false,
			current_gain: 1.0,
			total_bytes_sent: 0,
		}));

		let state_clone = state.clone();
		let encode_state_clone = encode_state.clone();
		let connection_slot = Arc::new(parking_lot::RwLock::new(None));

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
			connection_slot.clone(),
		));

		let connection_slot_task = connection_slot.clone();
		let _network_task = tokio::spawn(async move {
			let connection_slot = connection_slot_task;
			loop {
				// re-resolve in case of DNS change during runtime or retry loop
				let host_port = if server_host_owned.contains(':') {
					server_host_owned.clone()
				} else {
					format!("{}:39201", server_host_owned)
				};

				let target_addr = match tokio::net::lookup_host(&host_port).await {
					Ok(addrs) => {
						let resolved: Vec<std::net::SocketAddr> = addrs.collect();
						let mut ip = resolved.iter().find(|a| a.is_ipv6()).copied();
						if ip.is_none() {
							ip = resolved.first().copied();
						}
						match ip {
							Some(addr) => addr,
							None => {
								log::error!("Failed to resolve server hostname");
								tokio::time::sleep(std::time::Duration::from_secs(2)).await;
								continue;
							}
						}
					}
					Err(e) => {
						log::error!("Failed to resolve server hostname: {}", e);
						tokio::time::sleep(std::time::Duration::from_secs(2)).await;
						continue;
					}
				};

				let connection = match endpoint.connect(target_addr, "localhost") {
					Ok(connecting) => match connecting.await {
						Ok(conn) => conn,
						Err(err) => {
							log::error!("Connection to remote peer failed: {}", err);
							tokio::time::sleep(std::time::Duration::from_secs(2)).await;
							continue;
						}
					},
					Err(err) => {
						log::error!("Failed connecting to remote peer endpoint: {}", err);
						tokio::time::sleep(std::time::Duration::from_secs(2)).await;
						continue;
					}
				};

				// clear previous state entirely
				{
					let mut s = state_clone.lock();
					s.peers.clear();
					s.global_loss_rate = 0.0;
				}

				*connection_slot.write() = Some(connection.clone());

				let (mut ctrl_send, mut ctrl_recv) = match connection.open_bi().await {
					Ok((s, r)) => (s, r),
					Err(e) => {
						log::error!("Failed opening controller port logic stream: {}", e);
						*connection_slot.write() = None;
						tokio::time::sleep(std::time::Duration::from_secs(2)).await;
						continue;
					}
				};

				let join_msg = bincode::serialize(&ClientMessage::JoinRoom {
					room_name: room_name_owned.clone(),
					channels: if config.channels == opus::Channels::Stereo {
						2
					} else {
						1
					},
				})
				.unwrap();
				let _ = ctrl_send
					.write_all(&(join_msg.len() as u32).to_le_bytes())
					.await;
				let _ = ctrl_send.write_all(&join_msg).await;

				let mut len_buf = [0u8; 4];
				if let Err(e) = ctrl_recv.read_exact(&mut len_buf).await {
					log::error!("Control stream error (len init read): {}", e);
					*connection_slot.write() = None;
					tokio::time::sleep(std::time::Duration::from_secs(2)).await;
					continue;
				}

				let len = u32::from_le_bytes(len_buf) as usize;
				let mut msg_buf = vec![0u8; len];
				if let Err(e) = ctrl_recv.read_exact(&mut msg_buf).await {
					log::error!("Control stream error (msg init read): {}", e);
					*connection_slot.write() = None;
					tokio::time::sleep(std::time::Duration::from_secs(2)).await;
					continue;
				}

				match bincode::deserialize::<ServerMessage>(&msg_buf) {
					Ok(ServerMessage::RoomJoined {
						peer_id,
						channels: _,
					}) => {
						log::info!(
							"joined room '{}' with peer ID: {}",
							room_name_owned,
							peer_id
						);
					}
					Ok(ServerMessage::Error(e)) => {
						log::error!("server refused connection: {}", e);
						*connection_slot.write() = None;
						tokio::time::sleep(std::time::Duration::from_secs(2)).await;
						continue;
					}
					_ => {
						log::error!("unexpected initial message from server");
						*connection_slot.write() = None;
						tokio::time::sleep(std::time::Duration::from_secs(2)).await;
						continue;
					}
				}

				let conn_clone = connection.clone();
				let mut disconnect_triggered = false;

				loop {
					tokio::select! {
						_ = disconnect_rx.recv() => {
							let msg = bincode::serialize(&ClientMessage::LeaveRoom).unwrap();
							let _ = ctrl_send.write_all(&(msg.len() as u32).to_le_bytes()).await;
							let _ = ctrl_send.write_all(&msg).await;
							disconnect_triggered = true;
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
							if len > 65536 { // matches MAX_CONTROL_MESSAGE_SIZE
								log::warn!("server sent oversized control message: {} bytes", len);
								break;
							}
							let mut msg_buf = vec![0u8; len];
							if ctrl_recv.read_exact(&mut msg_buf).await.is_err() { break; }
							if let Ok(msg) = bincode::deserialize::<ServerMessage>(&msg_buf) {
								state_clone.lock().handle_server_message(msg, &encode_state_clone);
							}
						}
					}
				}

				*connection_slot.write() = None;
				if disconnect_triggered {
					log::info!("disconnect loop explicit exit executed.");
					break;
				} else {
					log::warn!("connection interrupted. attempting to reconnect...");
					state_clone.lock().play_sfx("sounds/Error.opus");
					tokio::time::sleep(std::time::Duration::from_millis(500)).await;
				}
			}
		});

		log::info!("ProxaClient::new initialized and started tasks");
		let client = Arc::new(Self {
			state,
			encode_state,
			mic_tx,
			far_end_tx,
			_encode_task,
			_network_task,
			_disconnect_tx: disconnect_tx,
			connection_slot,
		});

		*ACTIVE_CLIENT.lock() = Some(Arc::downgrade(&client));

		log::info!("client object created");
		Ok(client)
	}

	pub fn set_simulated_outbound_loss(&self, loss_pct: f32) {
		let mut state = self.encode_state.lock();
		state.simulated_outbound_loss = loss_pct.clamp(0.0, 1.0);
	}
	pub fn set_simulated_outbound_jitter(&self, jitter_ms: f32) {
		let mut state = self.encode_state.lock();
		state.simulated_outbound_jitter = jitter_ms.max(0.0);
	}
	pub fn set_mute_output(&self, mute: bool) {
		self.state.lock().mute_output = mute;
	}

	pub fn set_bitrate(&self, bitrate: i32) -> ProxaResult<()> {
		let mut state = self.encode_state.lock();
		state.target_bitrate = bitrate;
		if !state.is_throttled {
			state
				.encoder
				.set_bitrate(opus::Bitrate::Bits(bitrate))
				.map_err(|e| ProxaError::BitrateChange(e.to_string()))?;
		}

		log::info!("set bitrate to {} bps", bitrate);

		Ok(())
	}

	pub fn set_channels(&self, channels: opus::Channels) -> ProxaResult<()> {
		let (frame_duration, low_delay) = {
			let s = self.encode_state.lock();
			(s.frame_duration_ms, s.use_low_delay)
		};

		let num_channels = if channels == opus::Channels::Stereo {
			2
		} else {
			1
		};
		let samples_per_frame =
			(OPUS_SAMPLE_RATE as f32 * frame_duration * num_channels as f32 / 1000.0) as usize;

		// update encode state
		{
			let mut encode = self.encode_state.lock();
			if encode.channels == channels {
				return Ok(());
			}

			let app_mode = if low_delay {
				opus::Application::LowDelay
			} else {
				opus::Application::Audio
			};

			let mut new_encoder =
				// we opt to use opus::Application::Audio instead of opus::Application::Voip to prevent a high pass destroying the low-end, it sounds really bad on good microphones which capture the low-end correctly
			opus::Encoder::new(OPUS_SAMPLE_RATE, channels, app_mode)
				.map_err(|e| ProxaError::ChannelSwitch(e.to_string()))?;
			new_encoder
				.set_bitrate(opus::Bitrate::Bits(encode.target_bitrate))
				.map_err(|e| ProxaError::ChannelSwitch(e.to_string()))?;
			new_encoder
				// hint to the encoder that the signal is a voice, unlike setting Voip this won't destroy the low-end, this just makes the encoding more effecient
				.set_signal(opus::Signal::Voice)
				.map_err(|e| ProxaError::ChannelSwitch(e.to_string()))?;
			new_encoder
				.set_inband_fec(true)
				.map_err(|e| ProxaError::ChannelSwitch(e.to_string()))?;
			new_encoder
				.set_packet_loss_perc(encode.actual_loss_perc)
				.map_err(|e| ProxaError::ChannelSwitch(e.to_string()))?;

			encode.encoder = new_encoder;
			encode.channels = channels;
			encode.samples_per_frame = samples_per_frame;
			encode.mic_buffer.clear(); // clear buffer to prevent sample alignment issues
		}

		// update client state
		{
			let mut state = self.state.lock();
			state.channels = channels;
			state.samples_per_frame = samples_per_frame;

			if let Some(tx) = &state.report_tx {
				let _ = tx.send(ClientMessage::SetChannels(num_channels as u8));
			}

			for peer in state.peers.values_mut() {
				peer.buffer.clear();
				peer.jitter_buffer.clear();
				peer.is_buffering = true;
				peer.awaiting_first_packet = true;
			}
		}

		log::info!("switched to {:?} mic audio mode", channels);

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
	pub fn set_echo_cancellation_enabled(&self, enabled: bool) {
		self.encode_state.lock().echo_cancellation_enabled = enabled;
	}
	pub fn load_dfn3_models<P: AsRef<Path>>(&self, path: P) -> ProxaResult<()> {
		self.encode_state
			.lock()
			.load_dfn3_models(path)
			.map_err(|e| ProxaError::Internal(e.to_string()))
	}
	pub fn get_denoise_method(&self) -> DenoiseMethod {
		self.encode_state.lock().denoise_method
	}
	pub fn get_echo_cancellation_enabled(&self) -> bool {
		self.encode_state.lock().echo_cancellation_enabled
	}
	pub fn set_auto_normalize(&self, enabled: bool) {
		let mut state = self.encode_state.lock();
		state.auto_normalize = enabled;
		if !enabled {
			state.current_gain = 1.0;
		}
	}
	pub fn get_auto_normalize(&self) -> bool {
		self.encode_state.lock().auto_normalize
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
		let is_muted = state.mute_output;
		let num_channels = 2; // output is always stereo
		let report_tx = state.report_tx.clone();

		for (&peer_id, peer) in state.peers.iter_mut() {
			// refill the buffer using PeerState logic (always called to handle jitter target decay)
			// ensure we always request an even number of samples to keep L/R alignment
			let needed_samples = (((pcm.len() as f32 * 1.1) as usize + 8) / 2) * 2;
			peer.refill_buffer(needed_samples, num_channels, true);

			if peer.is_silenced && peer.jitter_buffer.is_empty() && peer.buffer.is_empty() {
				continue;
			}

			let frame_samples = peer.last_frame_size;
			let pcm_frames = peer.buffer.len() as f32 / (frame_samples as f32 * 2.0);
			let newest_seq = peer.jitter_buffer.keys().next_back().cloned();
			let jitter_frames = newest_seq.map_or(0, |max| {
				max.saturating_add(1).saturating_sub(peer.next_decode_seq)
			}) as f32;

			let current = pcm_frames + jitter_frames;

			// EMA smoothing (0.05 speed) for the current latency measure to avoid phase jumps
			peer.smoothed_current_latency = peer.smoothed_current_latency * 0.95 + current * 0.05;

			let target = peer.target_jitter_frames as f32 + 0.5;
			let diff = peer.smoothed_current_latency - target;

			// 2.0 frame dead-zone for voice chat (prevents typical pitch oscillation)
			// Also, ONLY activate rate control after the session has settled for 1 second.
			let target_rate = if diff.abs() < 2.0 || peer.played_frames_since_silence < 100 {
				1.0
			} else {
				let p_gain = 0.0005; // Very gentle correction
				1.0 + (diff * p_gain).clamp(-0.01, 0.01)
			};

			// ultra-slow EMA smoothing (0.01 speed) for phase stability
			peer.current_rate = peer.current_rate * 0.99 + target_rate * 0.01;

			// snap to 1.0 if the EMA is very close to perfect to avoid micro-drifts
			if (peer.current_rate - 1.0).abs() < 0.00005 {
				peer.current_rate = 1.0;
			}

			peer.pull_samples(pcm, peer.current_rate);

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

		if let Some(ref mut sfx) = state.sfx {
			let to_copy = (pcm.len()).min(sfx.pcm.len() - sfx.position);
			if to_copy > 0 {
				for i in 0..to_copy {
					pcm[i] += sfx.pcm[sfx.position + i];
				}
				sfx.position += to_copy;
			}
			if sfx.position >= sfx.pcm.len() {
				state.sfx = None;
			}
		}

		if is_muted {
			for sample in pcm.iter_mut() {
				*sample = 0.0;
			}
		}

		let _ = self.far_end_tx.send(pcm.to_vec());
	}

	pub fn get_peer_stats(&self) -> Vec<(u32, f32, usize, u64, u32)> {
		self.state
			.lock()
			.peers
			.iter()
			.map(|(&k, v)| {
				let frame_samples = v.last_frame_size;
				let _pcm_frames = v.buffer.len() as f32 / (frame_samples as f32 * 2.0);
				let newest_seq = v.jitter_buffer.keys().next_back().cloned();
				let _jitter_frames = newest_seq.map_or(0, |max| {
					max.saturating_add(1).saturating_sub(v.next_decode_seq)
				}) as f32;

				let total_frames = v.smoothed_current_latency.round();
				(
					k,
					v.volume,
					total_frames as usize,
					v.total_bytes_received,
					v.ping,
				)
			})
			.collect()
	}

	pub fn get_ping(&self) -> Option<Duration> {
		self.connection_slot.read().as_ref().map(|c| c.rtt())
	}

	pub fn get_total_bytes_sent(&self) -> u64 {
		self.encode_state.lock().total_bytes_sent
	}

	pub fn get_local_volume(&self) -> f32 {
		self.encode_state.lock().volume
	}

	pub fn get_voice_state(&self) -> crate::types::VoiceState {
		let state = self.encode_state.lock();
		if state.is_throttled {
			crate::types::VoiceState::Silenced
		} else if state.volume > crate::VOICE_THRESHOLD {
			crate::types::VoiceState::Speaking
		} else {
			crate::types::VoiceState::Waiting
		}
	}

	pub fn get_max_loss_rate(&self) -> f32 {
		self.state.lock().global_loss_rate
	}
	pub fn leave(&self) {
		let _ = self._disconnect_tx.try_send(());
	}

	pub fn enumerate_input_devices() -> Vec<AudioDevice> {
		AUDIO_BACKEND_PROVIDER
			.lock()
			.as_ref()
			.map(|b| b.enumerate_input_devices())
			.unwrap_or_default()
	}

	pub fn enumerate_output_devices() -> Vec<AudioDevice> {
		AUDIO_BACKEND_PROVIDER
			.lock()
			.as_ref()
			.map(|b| b.enumerate_output_devices())
			.unwrap_or_default()
	}

	pub fn set_input_device(id: &str) -> ProxaResult<()> {
		AUDIO_BACKEND_PROVIDER
			.lock()
			.as_ref()
			.ok_or_else(|| ProxaError::AudioInit("no audio backend registered".to_string()))?
			.set_input_device(id)
	}

	pub fn set_output_device(id: &str) -> ProxaResult<()> {
		AUDIO_BACKEND_PROVIDER
			.lock()
			.as_ref()
			.ok_or_else(|| ProxaError::AudioInit("no audio backend registered".to_string()))?
			.set_output_device(id)
	}

	pub fn get_current_input_device() -> Option<AudioDevice> {
		AUDIO_BACKEND_PROVIDER
			.lock()
			.as_ref()
			.and_then(|b| b.get_current_input_device())
	}

	pub fn get_current_output_device() -> Option<AudioDevice> {
		AUDIO_BACKEND_PROVIDER
			.lock()
			.as_ref()
			.and_then(|b| b.get_current_output_device())
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
