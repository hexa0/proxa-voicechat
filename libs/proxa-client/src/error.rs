use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProxaError {
	#[error("failed to initialize audio backend: {0}")]
	AudioInit(String),

	#[error("failed to create client object: {0}")]
	ClientInit(String),

	#[error("failed to change audio channels: {0}")]
	ChannelSwitch(String),

	#[error("failed to change bitrate: {0}")]
	BitrateChange(String),

	#[error("network session error: {0}")]
	Network(String),

	#[error("internal logic error: {0}")]
	Internal(String),

	#[error("room name is empty or invalid")]
	InvalidRoom,
}

pub type Result<T, E = ProxaError> = std::result::Result<T, E>;
