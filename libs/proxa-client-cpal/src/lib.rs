use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use proxa_client::ProxaClient;
use std::sync::Arc;

pub struct AudioBackend {
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
}

pub fn start_audio_backend(
    client_slot: Arc<parking_lot::Mutex<Option<Arc<ProxaClient>>>>,
) -> Result<AudioBackend> {
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

    let input_device = host
        .default_input_device()
        .context("No input device available")?;
    let output_device = host
        .default_output_device()
        .context("No output device available")?;

    let input_config = input_device.default_input_config()?;
    let output_config = output_device.default_output_config()?;

    let hw_in_channels = input_config.channels() as usize;
    let hw_out_channels = output_config.channels() as usize;

    let build_config = |config: &cpal::SupportedStreamConfig| -> cpal::StreamConfig {
        let mut c: cpal::StreamConfig = config.clone().into();
        c.sample_rate = 48000;
        c.buffer_size = cpal::BufferSize::Fixed(256);
        c
    };

    let mut in_stream_config = build_config(&input_config);
    let mut out_stream_config = build_config(&output_config);

    // we assume 1 channel if no client is yet loaded, but callbacks adapt dynamically
    let client_slot_in = client_slot.clone();
    let input_data_fn = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        let guard = client_slot_in.lock();
        if let Some(client) = guard.as_ref() {
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

    let client_slot_out = client_slot.clone();
    let output_data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        let guard = client_slot_out.lock();
        if let Some(client) = guard.as_ref() {
            let client_channels = if client.get_channels() == opus::Channels::Stereo {
                2
            } else {
                1
            };
            if hw_out_channels == client_channels {
                client.pop_audio(data);
            } else if hw_out_channels > 1 && client_channels == 1 {
                let frames = data.len() / hw_out_channels;
                let mut mono = vec![0.0f32; frames];
                client.pop_audio(&mut mono);
                for (frame, mono_sample) in data.chunks_exact_mut(hw_out_channels).zip(mono.iter())
                {
                    for sample in frame.iter_mut() {
                        *sample = *mono_sample;
                    }
                }
            } else if hw_out_channels == 1 && client_channels == 2 {
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

    let mut input_stream = input_device.build_input_stream(
        in_stream_config.clone(),
        input_data_fn.clone(),
        err_fn,
        None,
    );
    if input_stream.is_err() {
        in_stream_config.buffer_size = cpal::BufferSize::Default;
        input_stream =
            input_device.build_input_stream(in_stream_config.clone(), input_data_fn, err_fn, None);
    }
    let input_stream = input_stream.context("Failed to build input stream")?;

    let mut output_stream = output_device.build_output_stream(
        out_stream_config.clone(),
        output_data_fn.clone(),
        err_fn,
        None,
    );
    if output_stream.is_err() {
        out_stream_config.buffer_size = cpal::BufferSize::Default;
        output_stream = output_device.build_output_stream(
            out_stream_config.clone(),
            output_data_fn,
            err_fn,
            None,
        );
    }
    let output_stream = output_stream.context("Failed to build output stream")?;

    input_stream.play()?;
    output_stream.play()?;

    Ok(AudioBackend {
        _input_stream: input_stream,
        _output_stream: output_stream,
    })
}
