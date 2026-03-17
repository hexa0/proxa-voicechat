use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use proxa_client::{ProxaError, ProxaResult};

use proxa_client::client::AudioBackend;
use proxa_client::types::AudioDevice;

pub struct CpalBackendImpl {
    host: std::sync::Arc<cpal::Host>,
}

impl AudioBackend for CpalBackendImpl {
    // currently device enumeration is bugged in cpal on pipewire, once that is fixed by cpal this needs to be retested

    fn enumerate_input_devices(&self) -> Vec<AudioDevice> {
        self.host
            .input_devices()
            .map(|devices| {
                devices
                    .map(|d| {
                        #[allow(deprecated)]
                        let name = d.name().unwrap_or_else(|_| "Unknown".to_string());
                        let id = format!("{:?}", d.id());
                        AudioDevice { name, id }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn enumerate_output_devices(&self) -> Vec<AudioDevice> {
        self.host
            .output_devices()
            .map(|devices| {
                devices
                    .map(|d| {
                        #[allow(deprecated)]
                        let name = d.name().unwrap_or_else(|_| "Unknown".to_string());
                        let id = format!("{:?}", d.id());
                        AudioDevice { name, id }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set_input_device(&self, _id: &str) -> proxa_client::ProxaResult<()> {
        // TODO: implement device switching
        Ok(())
    }

    fn set_output_device(&self, _id: &str) -> proxa_client::ProxaResult<()> {
        // TODO: implement device switching
        Ok(())
    }

    fn get_current_input_device(&self) -> Option<AudioDevice> {
        self.host.default_input_device().map(|d| {
            #[allow(deprecated)]
            let name = d.name().unwrap_or_else(|_| "Unknown".to_string());
            let id = format!("{:?}", d.id());
            AudioDevice { name, id }
        })
    }

    fn get_current_output_device(&self) -> Option<AudioDevice> {
        self.host.default_output_device().map(|d| {
            #[allow(deprecated)]
            let name = d.name().unwrap_or_else(|_| "Unknown".to_string());
            let id = format!("{:?}", d.id());
            AudioDevice { name, id }
        })
    }
}

pub struct AudioBackendState {
    _streams: (cpal::Stream, cpal::Stream),
}

static BACKEND: parking_lot::Mutex<Option<AudioBackendState>> = parking_lot::const_mutex(None);

pub fn init() -> ProxaResult<()> {
    let mut guard = BACKEND.lock();
    if guard.is_some() {
        return Ok(());
    }

    let host = {
        #[cfg(target_os = "linux")]
        {
            cpal::host_from_id(cpal::HostId::PipeWire).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(target_os = "windows")]
        {
            cpal::host_from_id(cpal::HostId::Wasapi).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(target_os = "macos")]
        {
            cpal::default_host()
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            cpal::default_host()
        }
    };

    let host = std::sync::Arc::new(host);

    let backend_impl = CpalBackendImpl { host: host.clone() };
    proxa_client::client::register_audio_backend(Box::new(backend_impl));

    let input_device = host
        .default_input_device()
        .ok_or_else(|| ProxaError::AudioInit("no default input device".to_string()))?;
    let output_device = host
        .default_output_device()
        .ok_or_else(|| ProxaError::AudioInit("no default output device".to_string()))?;

    // name is deprecated so we'll need to switch off of it eventually

    #[allow(deprecated)]
    let in_name = input_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());
    #[allow(deprecated)]
    let out_name = output_device
        .name()
        .unwrap_or_else(|_| "Unknown".to_string());

    log::info!("audio input device: {}", in_name);
    log::info!("audio output device: {}", out_name);

    let mut input_config: cpal::StreamConfig = input_device
        .default_input_config()
        .map_err(|e| ProxaError::AudioInit(e.to_string()))?
        .into();
    let mut output_config: cpal::StreamConfig = output_device
        .default_output_config()
        .map_err(|e| ProxaError::AudioInit(e.to_string()))?
        .into();
    output_config.channels = 2;

    input_config.sample_rate = 48000;
    input_config.buffer_size = cpal::BufferSize::Fixed(256);
    output_config.sample_rate = 48000;
    output_config.buffer_size = cpal::BufferSize::Fixed(256);

    let hw_in_channels = input_config.channels as usize;
    let hw_out_channels = output_config.channels as usize;

    let input_data_fn = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        let client_opt = {
            let guard = proxa_client::client::ACTIVE_CLIENT.lock();
            guard.as_ref().and_then(|w| w.upgrade())
        };

        if let Some(client) = client_opt {
            let client_channels = if client.get_channels() == opus::Channels::Stereo {
                2
            } else {
                1
            };
            if hw_in_channels == client_channels {
                client.push_audio(data);
            } else if hw_in_channels == 1 && client_channels == 2 {
                let stereo: Vec<f32> = data.iter().flat_map(|&s| vec![s, s]).collect();
                client.push_audio(&stereo);
            } else if hw_in_channels > 1 && client_channels == 1 {
                let mono: Vec<f32> = data
                    .chunks_exact(hw_in_channels)
                    .map(|c| {
                        let mut sum = 0.0;
                        for s in c {
                            sum += *s;
                        }
                        sum / hw_in_channels as f32
                    })
                    .collect();
                client.push_audio(&mono);
            } else {
                let stereo: Vec<f32> = data
                    .chunks_exact(hw_in_channels)
                    .flat_map(|c| vec![c[0], c[1.min(hw_in_channels - 1)]])
                    .collect();
                client.push_audio(&stereo);
            }
        }
    };

    let output_data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let client_opt = {
            let guard = proxa_client::client::ACTIVE_CLIENT.lock();
            guard.as_ref().and_then(|w| w.upgrade())
        };

        if let Some(client) = client_opt {
            if hw_out_channels == 2 {
                client.pop_audio(data);
            } else if hw_out_channels == 1 {
                let frames = data.len();
                let mut stereo = vec![0.0f32; frames * 2];
                client.pop_audio(&mut stereo);
                for (i, frame) in stereo.chunks_exact(2).enumerate() {
                    data[i] = (frame[0] + frame[1]) / 2.0;
                }
            } else {
                let frames = data.len() / hw_out_channels;
                let mut stereo = vec![0.0f32; frames * 2];
                client.pop_audio(&mut stereo);
                for (i, frame) in data.chunks_exact_mut(hw_out_channels).enumerate() {
                    frame[0] = stereo[i * 2];
                    frame[1] = stereo[i * 2 + 1];
                    for j in 2..hw_out_channels {
                        frame[j] = 0.0;
                    }
                }
            }
        } else {
            for s in data.iter_mut() {
                *s = 0.0;
            }
        }
    };

    let err_fn = |err| log::error!("Audio stream error: {}", err);

    let mut input_stream_result =
        input_device.build_input_stream(input_config.clone(), input_data_fn.clone(), err_fn, None);
    if input_stream_result.is_err() {
        input_config.buffer_size = cpal::BufferSize::Default;
        input_stream_result =
            input_device.build_input_stream(input_config.clone(), input_data_fn, err_fn, None);
    }
    let input_stream = input_stream_result.map_err(|e| ProxaError::AudioInit(e.to_string()))?;

    let mut output_stream_result = output_device.build_output_stream(
        output_config.clone(),
        output_data_fn.clone(),
        err_fn,
        None,
    );
    if output_stream_result.is_err() {
        output_config.buffer_size = cpal::BufferSize::Default;
        output_stream_result =
            output_device.build_output_stream(output_config.clone(), output_data_fn, err_fn, None);
    }
    let output_stream = output_stream_result.map_err(|e| ProxaError::AudioInit(e.to_string()))?;

    input_stream
        .play()
        .map_err(|e| ProxaError::AudioInit(e.to_string()))?;
    output_stream
        .play()
        .map_err(|e| ProxaError::AudioInit(e.to_string()))?;

    *guard = Some(AudioBackendState {
        _streams: (input_stream, output_stream),
    });

    log::info!("hardware audio backend initialized successfully");

    Ok(())
}
