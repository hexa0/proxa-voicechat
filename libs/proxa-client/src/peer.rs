use crate::types::OPUS_SAMPLE_RATE;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

pub struct PeerState {
    pub channels: opus::Channels,
    pub volume: f32,
    pub decoder: opus::Decoder,
    pub buffer: Vec<f32>,
    pub jitter_buffer: BTreeMap<u32, Vec<u8>>,
    pub next_decode_seq: u32,
    pub target_jitter_frames: usize,
    pub is_buffering: bool,
    pub total_expected: u32,
    pub total_lost: u32,
    pub good_frames: u32,
    pub stat_expected: u32,
    pub stat_lost: u32,
    pub reported_loss_rate: f32,
    pub is_silenced: bool,
    pub awaiting_first_packet: bool,
    pub last_frame_size: usize,
    pub lost_sequences: BTreeSet<u32>,
    pub silence_frames: u32,
    pub played_frames_since_silence: u32,
    pub recent_jitter_max: f32,
    pub last_packet_time: Option<std::time::Instant>,
    pub missing_frames_streak: u32,
    pub resample_index: f32,
    pub current_rate: f32,
    pub was_plc: bool,
    pub smoothed_current_latency: f32,
}

impl PeerState {
    pub fn new(channels: opus::Channels) -> Result<Self> {
        Ok(Self {
            channels,
            volume: 0.0,
            decoder: opus::Decoder::new(OPUS_SAMPLE_RATE, channels)?,
            buffer: Vec::new(),
            jitter_buffer: BTreeMap::new(),
            next_decode_seq: 0,
            target_jitter_frames: 1,
            is_buffering: true,
            total_expected: 0,
            total_lost: 0,
            good_frames: 0,
            stat_expected: 0,
            stat_lost: 0,
            reported_loss_rate: 0.0,
            is_silenced: true,
            awaiting_first_packet: true,
            last_frame_size: (OPUS_SAMPLE_RATE as usize * 10) / 1000,
            lost_sequences: BTreeSet::new(),
            silence_frames: 0,
            played_frames_since_silence: 0,
            recent_jitter_max: 0.0,
            last_packet_time: None,
            missing_frames_streak: 0,
            resample_index: 0.0,
            current_rate: 1.0,
            was_plc: false,
            smoothed_current_latency: 2.0, // Start with a safe assumption
        })
    }

    pub fn refill_buffer(&mut self, pcm_len: usize, num_channels: usize, _is_stereo: bool) {
        let peer_channels = if self.channels == opus::Channels::Stereo {
            2
        } else {
            1
        };

        if self.is_silenced {
            self.silence_frames += 1;
            if self.silence_frames >= 100 {
                self.silence_frames = 0;
                self.target_jitter_frames = self.target_jitter_frames.saturating_sub(1).max(0);
            }
        } else {
            self.silence_frames = 0;
        }

        while self.buffer.len() < pcm_len {
            if self.awaiting_first_packet {
                // if we are waiting for the very first packet after silence,
                // don't advance sequence or decode silence/PLC yet.
                break;
            }

            if self.is_buffering {
                let newest = self.jitter_buffer.keys().next_back();
                let oldest = self.jitter_buffer.keys().next();
                let current_window = newest.zip(oldest).map_or(0, |(&max, &min)| {
                    (max.saturating_add(1).saturating_sub(min)) as usize
                });

                // use a safety margin during buffering to ensure we don't start "starved"
                let _target = self.target_jitter_frames.max(1);
                // Use a safety margin during initial buffering/underrun to avoid chattering
                if current_window >= 3 || (current_window >= 1 && self.missing_frames_streak > 3) {
                    self.is_buffering = false;
                    if let Some((&seq, _)) = self.jitter_buffer.iter().next() {
                        self.next_decode_seq = seq;
                    }
                } else {
                    // only force silence if we are truly silenced/waiting for start.
                    // otherwise, fall through to PLC logic so Opus can 'lag out' correctly.
                    let needed = pcm_len.saturating_sub(self.buffer.len());
                    self.buffer.extend(std::iter::repeat(0.0).take(needed));
                    self.volume = 0.0;
                    break;
                }
            }

            // normal processing
            let max_spf = (OPUS_SAMPLE_RATE as usize * 120 * peer_channels) / 1000;
            let mut decoded = vec![0.0f32; max_spf];
            let mut valid_decode = false;
            let mut fast_forwarded = false;

            // Sync stats: count every intended frame decode
            self.stat_expected += 1;
            self.total_expected += 1;

            let newest_seq = self.jitter_buffer.keys().next_back().cloned();
            let current_window = newest_seq.map_or(0, |max| {
                max.saturating_add(1).saturating_sub(self.next_decode_seq) as usize
            });

            if !self.is_silenced
                && newest_seq.is_some()
                && newest_seq.unwrap() > self.next_decode_seq + 30
            {
                if let Some(&first_seq) = self.jitter_buffer.keys().next() {
                    self.next_decode_seq = first_seq;
                }
            }

            let should_catch_up = self.played_frames_since_silence > 100
                && (current_window > self.target_jitter_frames + 5);

            if should_catch_up
                && self.jitter_buffer.contains_key(&self.next_decode_seq)
                && self.jitter_buffer.contains_key(&(self.next_decode_seq + 1))
            {
                if let Some(dec) = self.decode_crossfade(self.last_frame_size, peer_channels) {
                    decoded = dec;
                    valid_decode = true;
                    fast_forwarded = true;
                    self.stat_expected += 1;
                    self.total_expected += 1;
                    self.good_frames += 2;
                } else {
                    self.next_decode_seq += 1;
                    continue;
                }
            }

            if !fast_forwarded {
                if let Some(payload) = self.jitter_buffer.remove(&self.next_decode_seq) {
                    if let Ok(len) = self.decoder.decode_float(&payload, &mut decoded, false) {
                        valid_decode = true;
                        decoded.truncate(len * peer_channels);
                        self.last_frame_size = len;
                    }
                    self.next_decode_seq += 1;
                    self.good_frames += 1;
                    self.missing_frames_streak = 0;
                } else {
                    let newest_seq = self.jitter_buffer.keys().next_back().cloned();
                    let is_clearly_lost = newest_seq.map_or(false, |n| n > self.next_decode_seq);

                    if !self.is_silenced {
                        // attempt to fill with PLC/FEC if we are expecting audio
                        if let Some(dec) = self.decode_plc_fec(peer_channels) {
                            let len = dec.len() / peer_channels;
                            if len > 0 {
                                self.last_frame_size = len;
                                decoded = dec;
                                valid_decode = true;
                            }
                        }
                        self.next_decode_seq += 1;
                        self.missing_frames_streak += 1;
                        self.good_frames = self.good_frames.saturating_sub(2);

                        if !self.is_silenced {
                            self.total_lost += 1;
                            self.stat_lost += 1;
                            self.lost_sequences.insert(self.next_decode_seq - 1);
                        }

                        // Only re-buffer if we have a total dropout (150ms+)
                        if !is_clearly_lost && self.missing_frames_streak > 15 {
                            self.is_buffering = true;
                        }
                    } else {
                        // simply break and perform fade-out for silence/start
                        self.volume = 0.0;
                        if !self.buffer.is_empty() {
                            let fade_len = (160).min(self.buffer.len() / 2 * 2);
                            let base_idx = self.buffer.len() - fade_len;
                            for i in 0..fade_len {
                                let vol = (fade_len - 1 - i) as f32 / fade_len as f32;
                                self.buffer[base_idx + i] *= vol;
                            }
                        }
                        self.was_plc = true;
                        break;
                    }
                }
            }

            // safety: ensure we never have an odd length (L/R swap protection)
            if self.buffer.len() % 2 != 0 {
                self.buffer.pop();
            }

            // if we have plenty of packets, we shouldn't be buffering
            if self.is_buffering && self.jitter_buffer.len() > self.target_jitter_frames.max(1) {
                self.is_buffering = false;
            }

            if valid_decode {
                if !fast_forwarded {
                    self.played_frames_since_silence =
                        self.played_frames_since_silence.saturating_add(1).min(1000);
                }
                let mut sum_sq = 0.0;
                for &sample in &decoded {
                    sum_sq += sample * sample;
                }
                self.volume = (sum_sq / decoded.len() as f32).sqrt();

                if self.played_frames_since_silence < 2 && self.buffer.is_empty() && !decoded.is_empty() && !self.was_plc {
                    // Only fade in on the very first frame of a sentence to avoid choppiness
                    let fade_len = (128).min(decoded.len());
                    for i in 0..fade_len {
                        let vol = i as f32 / fade_len as f32;
                        decoded[i] *= vol;
                    }
                }

                let converted = if peer_channels == 1 && num_channels == 2 {
                    let mut v = Vec::with_capacity(decoded.len() * 2);
                    for &s in &decoded {
                        v.push(s);
                        v.push(s);
                    }
                    v
                } else if peer_channels == 2 && num_channels == 1 {
                    let mut v = Vec::with_capacity(decoded.len() / 2);
                    for chunk in decoded.chunks_exact(2) {
                        v.push((chunk[0] + chunk[1]) / 2.0);
                    }
                    v
                } else {
                    decoded
                };

                if self.was_plc && !self.buffer.is_empty() {
                    // truncate the laggy PLC tail and blend the new real packet for low-latency recovery
                    let overlap = (240).min(converted.len() / 2 * 2);
                    if self.buffer.len() > overlap {
                        let to_remove = self.buffer.len() - overlap;
                        self.buffer.drain(0..to_remove);
                    }
                    let actual_overlap = self.buffer.len();
                    for i in 0..actual_overlap {
                        let t = i as f32 / actual_overlap as f32;
                        self.buffer[i] = self.buffer[i] * (1.0 - t) + converted[i] * t;
                    }
                    if actual_overlap < converted.len() {
                        self.buffer.extend_from_slice(&converted[actual_overlap..]);
                    }
                    self.was_plc = false;
                } else {
                    self.buffer.extend_from_slice(&converted);
                    self.was_plc = fast_forwarded; // if we skipped packets to catch up, crossfade next time too
                }
            } else {
                self.volume = 0.0;
                // only insert as much silence as needed for this call to avoid over-filling
                // ensure needed is even to keep L/R alignment
                let needed = ((pcm_len.saturating_sub(self.buffer.len()) + 1) / 2) * 2;
                if needed > 0 {
                    // remove fade-out as it causes a fade-in when speech resumes
                    self.buffer.extend(std::iter::repeat(0.0).take(needed));
                    self.was_plc = true;
                }
                break;
            }

            // decrease target every 500ms of clean audio
            let ms_per_frame = (self.last_frame_size * 1000 / OPUS_SAMPLE_RATE as usize) as u32;
            let good_frames_target = if ms_per_frame > 0 {
                500 / ms_per_frame
            } else {
                50
            };

            if self.good_frames >= good_frames_target {
                self.good_frames = 0;
                self.target_jitter_frames = self.target_jitter_frames.saturating_sub(1).max(1);
            }
            self.jitter_buffer.retain(|&k, _| k >= self.next_decode_seq);
            let min_keep = self.next_decode_seq.saturating_sub(1000);
            self.lost_sequences.retain(|&k| k >= min_keep);
        }
    }

    fn decode_crossfade(&mut self, spf: usize, num_channels: usize) -> Option<Vec<f32>> {
        let payload1 = self.jitter_buffer.remove(&self.next_decode_seq).unwrap();
        self.next_decode_seq += 1;
        let payload2 = self.jitter_buffer.remove(&self.next_decode_seq).unwrap();
        self.next_decode_seq += 1;

        let mut dec1 = vec![0.0f32; spf];
        let mut dec2 = vec![0.0f32; spf];
        let ok1 = self.decoder.decode_float(&payload1, &mut dec1, false);
        let ok2 = self.decoder.decode_float(&payload2, &mut dec2, false);

        if let (Ok(l1), Ok(l2)) = (ok1, ok2) {
            let len1 = l1 * num_channels;
            let len2 = l2 * num_channels;
            dec1.truncate(len1);
            dec2.truncate(len2);

            let crossfade_len = len1.min(len2);
            for i in 0..crossfade_len {
                let t = i as f32 / crossfade_len as f32;
                // linear crossfade is better for correlated voice signals (no amplitude swell)
                dec1[i] = dec1[i] * (1.0 - t) + dec2[i] * t;
            }
            Some(dec1)
        } else {
            None
        }
    }

    fn decode_plc_fec(&mut self, num_channels: usize) -> Option<Vec<f32>> {
        // use a safe maximum for PLC (Opus can do up to 120ms)
        let max_spf = (OPUS_SAMPLE_RATE as usize * 120) / 1000;
        let mut decoded = vec![0.0f32; max_spf * num_channels];
        let fec_available = self.jitter_buffer.contains_key(&(self.next_decode_seq + 1));

        if fec_available {
            let payload_n_plus_1 = self.jitter_buffer.get(&(self.next_decode_seq + 1)).unwrap();
            if let Ok(len) = self
                .decoder
                .decode_float(payload_n_plus_1, &mut decoded, true)
            {
                decoded.truncate(len * num_channels);
                return Some(decoded);
            }
        }

        // default PLC (Packet Loss Concealment)
        // use last_frame_size if available, else 10ms default
        let plc_target_spf = if self.last_frame_size > 0 {
            self.last_frame_size
        } else {
            480
        };
        let mut plc_decoded = vec![0.0f32; plc_target_spf * num_channels];
        if let Ok(len) = self.decoder.decode_float(&[], &mut plc_decoded, false) {
            plc_decoded.truncate(len * num_channels);

            if !self.is_silenced {
                // very conservative growth: only if we have a significant streak of misses
                if self.missing_frames_streak > 8 && self.missing_frames_streak % 8 == 0 {
                    self.target_jitter_frames = (self.target_jitter_frames + 1).min(60);
                }
            }
            return Some(plc_decoded);
        }
        None
    }

    pub fn pull_samples(&mut self, output: &mut [f32], rate: f32) {
        let n_channels = 2;
        let mut out_idx = 0;

        while out_idx < output.len() {
            let base_idx = (self.resample_index.floor() as usize) * n_channels;

            // hermite 4-point resampler needs i-1, i, i+1, i+2
            if base_idx + 3 * n_channels >= self.buffer.len() {
                // if we run out, fill the rest with silent samples to avoid phase clicks
                let remaining = output.len() - out_idx;
                for i in 0..remaining {
                    output[out_idx + i] += 0.0;
                }
                break;
            }

            let f = self.resample_index - self.resample_index.floor();

            for ch in 0..n_channels {
                let y0 = if base_idx >= n_channels {
                    self.buffer[base_idx - n_channels + ch]
                } else {
                    self.buffer[base_idx + ch]
                };
                let y1 = self.buffer[base_idx + ch];
                let y2 = self.buffer[base_idx + n_channels + ch];
                let y3 = self.buffer[base_idx + 2 * n_channels + ch];

                // hermite interpolation (Tension=0, Bias=0)
                let c0 = y1;
                let c1 = 0.5 * (y2 - y0);
                let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
                let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);

                let sample = ((c3 * f + c2) * f + c1) * f + c0;
                output[out_idx + ch] += sample;
            }

            self.resample_index += rate;
            out_idx += n_channels;
        }

        let consumed_frames = self.resample_index.floor() as usize;
        if consumed_frames > 0 {
            if consumed_frames * n_channels <= self.buffer.len() {
                self.buffer.drain(..(consumed_frames * n_channels));
            } else {
                self.buffer.clear();
            }
            self.resample_index -= consumed_frames as f32;
        }
    }
}
