use crate::aec3::AecWrapper;
use crate::dfn3::{DfEngine, load_dfn3_engine_internal};
use crate::types::{DenoiseMethod, SILENCE_BITRATE, VOICE_THRESHOLD};
use anyhow::Result;
use parking_lot::Mutex;
use proxa_protocol::ClientMessage;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct EncodeState {
	pub mic_buffer: Vec<f32>,
	pub far_end_buffer: Vec<f32>,
	pub encoder: opus::Encoder,
	pub channels: opus::Channels,
	pub samples_per_frame: usize,
	pub simulated_outbound_loss: f32,
	pub simulated_outbound_jitter: f32,
	pub denoise_method: DenoiseMethod,
	pub echo_cancellation_enabled: bool,
	pub rnnoise_state_left: Option<nnnoiseless::DenoiseState<'static>>,
	pub rnnoise_state_right: Option<nnnoiseless::DenoiseState<'static>>,
	pub aec: Option<AecWrapper>,
	pub dfn3_engine: Option<DfEngine>,
	pub dfn3_model_path: Option<std::path::PathBuf>,
	pub dfn3_loading: bool,
	pub frame_duration_ms: f32,
	pub use_low_delay: bool,
	pub next_send_sequence: u32,
	pub volume: f32,
	pub target_bitrate: i32,
	pub last_voice_time: std::time::Instant,
	pub is_throttled: bool,
	pub actual_loss_perc: i32,
	pub simulated_jitter_current_delay: f32,
	pub simulated_jitter_drift: f32,
	pub auto_normalize: bool,
	pub current_gain: f32,
	pub total_bytes_sent: u64,
}

impl EncodeState {
	pub fn load_dfn3_models(&mut self, path: impl AsRef<Path>) -> Result<()> {
		let path = path.as_ref().to_path_buf();
		if self.dfn3_model_path.as_ref() == Some(&path) && self.dfn3_engine.is_some() {
			return Ok(());
		}
		self.dfn3_model_path = Some(path);
		self.dfn3_engine = None;
		Ok(())
	}

	pub fn process_aec(&mut self, frame: &mut [f32]) {
		if !self.echo_cancellation_enabled {
			return;
		}

		let mic_channels = if self.channels == opus::Channels::Stereo {
			2
		} else {
			1
		};
		let speaker_channels = 2; // speaker output in Proxa is hardcoded to Stereo

		let capture_block_size = 480 * mic_channels;
		let render_block_size = 480 * speaker_channels;

		let num_blocks = frame.len() / capture_block_size;
		if num_blocks == 0 {
			return; // too small for AEC3 (needs 10ms)
		}

		let required_render = num_blocks * render_block_size;
		let mut render_data = self
			.far_end_buffer
			.drain(..self.far_end_buffer.len().min(required_render))
			.collect::<Vec<f32>>();

		// pad with zeros if we don't have enough render data
		if render_data.len() < required_render {
			render_data.resize(required_render, 0.0);
		}

		if let Some(ref mut aec) = self.aec {
			for (c_block, r_block) in frame
				.chunks_mut(capture_block_size)
				.zip(render_data.chunks(render_block_size))
			{
				let mut capture_mono = [0.0f32; 480];
				let mut render_mono = [0.0f32; 480];

				if mic_channels == 2 {
					for i in 0..480 {
						capture_mono[i] = (c_block[i * 2] + c_block[i * 2 + 1]) / 2.0;
					}
				} else {
					capture_mono.copy_from_slice(c_block);
				}

				for i in 0..480 {
					render_mono[i] = (r_block[i * 2] + r_block[i * 2 + 1]) / 2.0;
				}

				let mut out = [0.0f32; 480];
				let _ = aec.process(&capture_mono, Some(&render_mono), false, &mut out);

				if mic_channels == 2 {
					for i in 0..480 {
						c_block[i * 2] = out[i];
						c_block[i * 2 + 1] = out[i];
					}
				} else {
					c_block.copy_from_slice(&out);
				}
			}
		}
	}

	pub fn process_denoise(
		&mut self,
		frame: &mut [f32],
		encode_state_clone: &Arc<Mutex<EncodeState>>,
	) {
		match self.denoise_method {
			DenoiseMethod::RNNoise => self.process_rnnoise(frame),
			DenoiseMethod::DFN3 => self.process_dfn3(frame, encode_state_clone),
			DenoiseMethod::Off => {}
		}
	}

	fn process_rnnoise(&mut self, frame: &mut [f32]) {
		let channels = if self.channels == opus::Channels::Stereo {
			2
		} else {
			1
		};

		if let Some(mut d) = self.rnnoise_state_left.take() {
			for chunk in frame.chunks_mut(480 * channels) {
				if chunk.len() == 480 * channels {
					let mut mono = [0.0f32; 480];
					if channels == 2 {
						for i in 0..480 {
							mono[i] = (chunk[i * 2] + chunk[i * 2 + 1]) * 16384.0;
						}
					} else {
						for i in 0..480 {
							mono[i] = chunk[i] * 32768.0;
						}
					}

					let mut out = [0.0f32; 480];
					d.process_frame(&mut out, &mono);

					if channels == 2 {
						for i in 0..480 {
							chunk[i * 2] = out[i] / 32768.0;
							chunk[i * 2 + 1] = out[i] / 32768.0;
						}
					} else {
						for i in 0..480 {
							chunk[i] = out[i] / 32768.0;
						}
					}
				}
			}
			self.rnnoise_state_left = Some(d);
		}
	}

	fn process_dfn3(&mut self, frame: &mut [f32], encode_state_clone: &Arc<Mutex<EncodeState>>) {
		let channels = if self.channels == opus::Channels::Stereo {
			2
		} else {
			1
		};

		if self.dfn3_engine.is_none() && self.dfn3_model_path.is_some() && !self.dfn3_loading {
			let path = self.dfn3_model_path.clone().unwrap();
			let e_state = encode_state_clone.clone();
			self.dfn3_loading = true;
			tokio::task::spawn_blocking(move || {
				log::info!("lazy loading DFN3 models in background...");
				match load_dfn3_engine_internal(&path) {
					Ok(engine) => {
						let mut s = e_state.lock();
						s.dfn3_engine = Some(DfEngine(std::sync::Arc::new(
							parking_lot::Mutex::new(engine),
						)));
						s.dfn3_loading = false;
						log::info!("DFN3 AI engine loaded successfully in background");
					}
					Err(e) => {
						log::error!("failed to lazy load DFN3 models: {}", e);
						let mut s = e_state.lock();
						s.dfn3_loading = false;
						s.dfn3_model_path = None;
					}
				}
			});
		}

		if let Some(ref mut engine) = self.dfn3_engine {
			let mut engine_lock = engine.0.lock();
			let hop_size = engine_lock.hop_size;
			for chunk in frame.chunks_mut(hop_size * channels) {
				if chunk.len() == hop_size * channels {
					if channels == 2 {
						let mut left = vec![0.0f32; hop_size];
						for i in 0..hop_size {
							left[i] = (chunk[i * 2] + chunk[i * 2 + 1]) / 2.0;
						}
						let input = ndarray::Array2::from_shape_vec((1, hop_size), left).unwrap();
						let mut output = ndarray::Array2::zeros((1, hop_size));
						let _lsnr = engine_lock
							.process(input.view(), output.view_mut())
							.unwrap_or(0.0);
						let processed = output.row(0);
						for i in 0..hop_size {
							chunk[i * 2] = processed[i];
							chunk[i * 2 + 1] = processed[i];
						}
					} else {
						let input = ndarray::ArrayView2::from_shape((1, hop_size), chunk).unwrap();
						let mut output = ndarray::Array2::zeros((1, hop_size));
						let _lsnr = engine_lock.process(input, output.view_mut()).unwrap_or(0.0);
						chunk.copy_from_slice(output.row(0).as_slice().unwrap());
					}
				}
			}
		}
	}

	pub fn handle_vad(
		&mut self,
		frame: &mut [f32],
		report_tx: &Option<mpsc::UnboundedSender<ClientMessage>>,
	) -> bool {
		let mut max_abs = 0.0f32;
		let mut sum_sq = 0.0;
		for sample in frame.iter() {
			let abs = sample.abs();
			if abs > max_abs {
				max_abs = abs;
			}
			sum_sq += sample * sample;
		}
		let vol = (sum_sq / frame.len() as f32).sqrt();
		self.volume = max_abs;

		if self.auto_normalize {
			let target_peak = 0.8;
			// only adjust gain when we hear voice activity (to avoid noise floor boost)
			if max_abs > 0.0001 && vol > VOICE_THRESHOLD {
				let desired_gain = (target_peak / max_abs).clamp(0.1, 10.0);
				if desired_gain < self.current_gain {
					// extremely fast reduction (effectively a compressor attack) to prevent clipping
					self.current_gain = self.current_gain * 0.2 + desired_gain * 0.8;
				} else {
					// extremely slow and steady increase to avoid "pumping" and boosting background noise
					self.current_gain = self.current_gain * 0.999 + desired_gain * 0.001;
				}
			}

			for s in frame {
				*s = (*s * self.current_gain).clamp(-1.0, 1.0);
			}
			self.volume = (max_abs * self.current_gain).min(1.0);
		}

		if vol > VOICE_THRESHOLD {
			self.last_voice_time = std::time::Instant::now();
			if self.is_throttled {
				let bitrate = self.target_bitrate;
				let _ = self.encoder.set_bitrate(opus::Bitrate::Bits(bitrate));
				let _ = self.encoder.set_inband_fec(true);
				let loss_perc = self.actual_loss_perc;
				let _ = self.encoder.set_packet_loss_perc(loss_perc);
				self.is_throttled = false;
				let _ = self.encoder.reset_state();
				if let Some(tx) = report_tx {
					let _ = tx.send(ClientMessage::SetSilence(false));
				}
			}
		} else if !self.is_throttled
			&& self.last_voice_time.elapsed() > std::time::Duration::from_millis(500)
		{
			let low_bitrate = self.target_bitrate.min(SILENCE_BITRATE);
			let _ = self.encoder.set_bitrate(opus::Bitrate::Bits(low_bitrate));
			let _ = self.encoder.set_inband_fec(false);
			let _ = self.encoder.set_packet_loss_perc(0);
			self.is_throttled = true;
			if let Some(tx) = report_tx {
				let _ = tx.send(ClientMessage::SetSilence(true));
			}
		}

		self.is_throttled
	}
}

pub async fn run_encode_task(
	encode_state: Arc<Mutex<EncodeState>>,
	mut mic_rx: mpsc::UnboundedReceiver<Vec<f32>>,
	mut far_end_rx: mpsc::UnboundedReceiver<Vec<f32>>,
	report_tx: Option<mpsc::UnboundedSender<ClientMessage>>,
	connection_slot: Arc<parking_lot::RwLock<Option<quinn::Connection>>>,
) {
	let encode_state_clone = encode_state.clone();

	loop {
		tokio::select! {
			Some(pcm) = mic_rx.recv() => {
				let mut state = encode_state.lock();
				state.mic_buffer.extend_from_slice(&pcm);
			}
			Some(pcm) = far_end_rx.recv() => {
				let mut state = encode_state.lock();
				state.far_end_buffer.extend_from_slice(&pcm);
				let max_samples = 48000;
				let cur_len = state.far_end_buffer.len();
				if cur_len > max_samples {
					state.far_end_buffer.drain(..cur_len - max_samples);
				}
			}
			else => break,
		}

		let mut state = encode_state.lock();
		let spf = state.samples_per_frame;

		// update simulated jitter delay (Markov process)
		if state.simulated_outbound_jitter > 0.0 {
			// we use a random walk with some damping to simulate network delay variation
			let jitter = state.simulated_outbound_jitter;
			let target_drift = (rand::random::<f32>() - 0.5) * jitter * 0.2;
			state.simulated_jitter_drift = state.simulated_jitter_drift * 0.9 + target_drift;
			state.simulated_jitter_current_delay = (state.simulated_jitter_current_delay
				+ state.simulated_jitter_drift)
				.clamp(0.0, jitter);
		} else {
			state.simulated_jitter_current_delay = 0.0;
			state.simulated_jitter_drift = 0.0;
		}

		while state.mic_buffer.len() >= spf {
			let mut frame: Vec<f32> = state.mic_buffer.drain(..spf).collect();

			state.process_aec(&mut frame);
			state.process_denoise(&mut frame, &encode_state_clone);
			if state.handle_vad(&mut frame, &report_tx) {
				continue;
			}

			let seq = state.next_send_sequence;
			state.next_send_sequence += 1;

			if state.simulated_outbound_loss > 0.0
				&& rand::random::<f32>() < state.simulated_outbound_loss
			{
				continue;
			}

			let jitter = state.simulated_outbound_jitter;
			let current_delay = state.simulated_jitter_current_delay;

			let mut buf = vec![0u8; 1500];
			if let Ok(len) = state.encoder.encode_float(&frame, &mut buf) {
				buf.truncate(len);
				let pkt_bytes = proxa_protocol::ClientAudioPacket::serialize(seq, &buf);
				state.total_bytes_sent += pkt_bytes.len() as u64;
				if let Some(conn) = connection_slot.read().as_ref() {
					let conn = conn.clone();
					if jitter > 0.0 {
						tokio::spawn(async move {
							tokio::time::sleep(std::time::Duration::from_millis(
								current_delay as u64,
							))
							.await;
							let _ = conn.send_datagram(pkt_bytes.into());
						});
					} else {
						let _ = conn.send_datagram(pkt_bytes.into());
					}
				}
			}
		}
	}
}
