use anyhow::Result;
use std::collections::BTreeMap;
use crate::types::OPUS_SAMPLE_RATE;

pub struct PeerState {
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
}

impl PeerState {
    pub fn new(channels: opus::Channels) -> Result<Self> {
        Ok(Self {
            volume: 0.0,
            decoder: opus::Decoder::new(OPUS_SAMPLE_RATE, channels)?,
            buffer: Vec::new(),
            jitter_buffer: BTreeMap::new(),
            next_decode_seq: 0,
            target_jitter_frames: 0,
            is_buffering: true,
            total_expected: 0,
            total_lost: 0,
            good_frames: 0,
            stat_expected: 0,
            stat_lost: 0,
            reported_loss_rate: 0.0,
            is_silenced: true,
            awaiting_first_packet: true,
        })
    }

    pub fn refill_buffer(
        &mut self,
        pcm_len: usize,
        num_channels: usize,
        spf: usize,
        _is_stereo: bool,
    ) {
        let slack_samples = (5 * OPUS_SAMPLE_RATE as usize / 1000) * num_channels;

        while self.buffer.len() < pcm_len + slack_samples {
            if !self.is_buffering
                && !self.jitter_buffer.contains_key(&self.next_decode_seq)
                && !self.jitter_buffer.contains_key(&(self.next_decode_seq + 1))
                && self.buffer.len() >= pcm_len
            {
                break;
            }

            if self.is_buffering {
                let current_window = self
                    .jitter_buffer
                    .keys()
                    .next_back()
                    .zip(self.jitter_buffer.keys().next())
                    .map_or(0, |(&max, &min)| max.saturating_sub(min) as usize);

                if current_window >= self.target_jitter_frames {
                    self.is_buffering = false;
                    if let Some((&seq, _)) = self.jitter_buffer.iter().next() {
                        self.next_decode_seq = seq;
                    }
                } else {
                    self.buffer.extend(std::iter::repeat(0.0).take(spf));
                    self.volume = 0.0;
                    break;
                }
            }

            let mut decoded = vec![0.0f32; spf];
            let mut valid_decode = false;

            if !self.is_buffering {
                self.total_expected += 1;
                self.stat_expected += 1;
                let newest_seq = self.jitter_buffer.keys().next_back().cloned();
                let current_window = newest_seq.map_or(0, |max| max.saturating_sub(self.next_decode_seq) as usize);

                if !self.is_silenced && newest_seq.is_some() && newest_seq.unwrap() > self.next_decode_seq + 5 {
                    if let Some(&first_seq) = self.jitter_buffer.keys().next() {
                        self.next_decode_seq = first_seq;
                    }
                }

                let should_catch_up = current_window > self.target_jitter_frames + 1;
                let mut fast_forwarded = false;

                if should_catch_up
                    && self.jitter_buffer.contains_key(&self.next_decode_seq)
                    && self.jitter_buffer.contains_key(&(self.next_decode_seq + 1))
                {
                    if let Some(dec) = self.decode_crossfade(spf, num_channels) {
                        decoded = dec;
                        valid_decode = true;
                        fast_forwarded = true;
                        self.good_frames += 2;
                    } else if should_catch_up {
                        self.next_decode_seq += 1;
                        continue;
                    }
                }

                if !fast_forwarded {
                    if let Some(payload) = self.jitter_buffer.remove(&self.next_decode_seq) {
                        if let Ok(len) = self.decoder.decode_float(&payload, &mut decoded, false) {
                            valid_decode = true;
                            decoded.truncate(len * num_channels);
                        }
                        self.next_decode_seq += 1;
                        self.good_frames += 1;
                    } else {
                        self.good_frames = self.good_frames.saturating_sub(5);
                        if let Some(dec) = self.decode_plc_fec(num_channels, spf) {
                            decoded = dec;
                            valid_decode = true;
                        }
                        self.next_decode_seq += 1;
                        if !self.is_silenced {
                            self.total_lost += 1;
                            self.stat_lost += 1;
                        }
                    }
                }

                if self.good_frames > 40 {
                    self.good_frames -= 1;
                    self.target_jitter_frames = self.target_jitter_frames.saturating_sub(1).max(0);
                }
                self.jitter_buffer.retain(|&k, _| k >= self.next_decode_seq);
            }

            if valid_decode {
                let mut sum_sq = 0.0;
                for &sample in &decoded {
                    sum_sq += sample * sample;
                }
                self.volume = (sum_sq / decoded.len() as f32).sqrt();
                self.buffer.extend_from_slice(&decoded);
            } else if self.is_buffering || self.buffer.is_empty() {
                self.volume = 0.0;
                self.buffer.extend(std::iter::repeat(0.0).take(spf));
            }
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
                let angle = t * std::f32::consts::PI / 2.0;
                dec1[i] = dec1[i] * angle.cos() + dec2[i] * angle.sin();
            }
            Some(dec1)
        } else {
            None
        }
    }

    fn decode_plc_fec(&mut self, num_channels: usize, spf: usize) -> Option<Vec<f32>> {
        let mut decoded = vec![0.0f32; spf];
        let fec_available = self.jitter_buffer.contains_key(&(self.next_decode_seq + 1));

        if fec_available {
            let payload_n_plus_1 = self.jitter_buffer.get(&(self.next_decode_seq + 1)).unwrap();
            if let Ok(len) = self.decoder.decode_float(payload_n_plus_1, &mut decoded, true) {
                decoded.truncate(len * num_channels);
                return Some(decoded);
            }
        } else {
            if let Ok(len) = self.decoder.decode_float(&[], &mut decoded, false) {
                decoded.truncate(len * num_channels);
                if !self.is_silenced {
                    self.target_jitter_frames = self.target_jitter_frames.saturating_add(1).min(1000);
                }
                if self.jitter_buffer.is_empty() && self.target_jitter_frames > 0 && !self.is_silenced {
                    self.is_buffering = true;
                }
                return Some(decoded);
            }
        }
        None
    }
}
