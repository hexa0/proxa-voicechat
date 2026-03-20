use anyhow::Result;
use parking_lot::RwLock;
use rust_embed::RustEmbed;
use std::collections::HashMap;

#[derive(RustEmbed)]
#[folder = "resources/"]
pub struct EmbeddedResources;

pub static SFX_CACHE: RwLock<Option<HashMap<String, Vec<f32>>>> = parking_lot::const_rwlock(None);

pub fn get_sfx_pcm(path: &str) -> Option<Vec<f32>> {
	{
		let cache = SFX_CACHE.read();
		if let Some(ref map) = *cache {
			if let Some(pcm) = map.get(path) {
				return Some(pcm.clone());
			}
		}
	}

	let file = EmbeddedResources::get(path)?;
	let pcm = match decode_ogg_opus(&file.data) {
		Ok(pcm) => pcm,
		Err(e) => {
			log::error!("failed to decode SFX {}: {}", path, e);
			return None;
		}
	};

	let mut cache = SFX_CACHE.write();
	if cache.is_none() {
		*cache = Some(HashMap::new());
	}
	cache
		.as_mut()
		.unwrap()
		.insert(path.to_string(), pcm.clone());
	Some(pcm)
}

fn decode_ogg_opus(data: &[u8]) -> Result<Vec<f32>> {
	let mut reader = ogg::PacketReader::new(std::io::Cursor::new(data));
	// we use 48000 Stereo to match Proxa's output mixer
	let mut decoder = opus::Decoder::new(48000, opus::Channels::Stereo)?;
	let mut pcm = Vec::new();

	let mut packet_count = 0;
	while let Some(packet) = reader.read_packet()? {
		packet_count += 1;
		// skip first 2 packets (Id Header and Tags)
		if packet_count <= 2 {
			continue;
		}

		let mut out = [0.0f32; 5760 * 2]; // max frame size for 120ms at 48khz stereo
		if let Ok(len) = decoder.decode_float(&packet.data, &mut out, false) {
			pcm.extend_from_slice(&out[..len * 2]);
		}
	}

	Ok(pcm)
}
