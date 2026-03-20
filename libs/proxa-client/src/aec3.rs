/// wrapper for AEC3's VoipAec3 to make it Send/Sync for shared state.
pub struct AecWrapper(pub aec3::voip::VoipAec3);
unsafe impl Send for AecWrapper {}
unsafe impl Sync for AecWrapper {}

impl AecWrapper {
	pub fn new(sample_rate: u32) -> Self {
		Self(
			aec3::voip::VoipAec3::builder(sample_rate as usize, 1, 1)
				.build()
				.expect("Failed to build AEC3"),
		)
	}
}

impl std::ops::Deref for AecWrapper {
	type Target = aec3::voip::VoipAec3;
	fn deref(&self) -> &Self::Target {
		&self.0
	}
}
impl std::ops::DerefMut for AecWrapper {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.0
	}
}
