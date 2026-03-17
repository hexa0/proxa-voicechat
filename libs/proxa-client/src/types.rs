
pub const OPUS_SAMPLE_RATE: u32 = 48000;
pub const FRAME_DURATION_MS: usize = 20;
pub const VOICE_THRESHOLD: f32 = 0.001;
pub const SILENCE_BITRATE: i32 = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseMethod {
    Off,
    RNNoise,
    DFN3,
}

impl DenoiseMethod {
    pub fn next(&self) -> Self {
        match self {
            DenoiseMethod::Off => DenoiseMethod::RNNoise,
            DenoiseMethod::RNNoise => DenoiseMethod::DFN3,
            DenoiseMethod::DFN3 => DenoiseMethod::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Waiting,
    Speaking,
    Silenced,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AudioDevice {
    pub name: String,
    pub id: String,
}
