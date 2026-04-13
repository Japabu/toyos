use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::traits::{DeviceTrait, HostTrait, StreamTrait};
use crate::{
    BuildStreamError, Data, DefaultStreamConfigError, DeviceDescription, DeviceDescriptionBuilder,
    DeviceId, DeviceIdError, DeviceNameError, DevicesError, InputCallbackInfo, OutputCallbackInfo,
    OutputStreamTimestamp, PauseStreamError, PlayStreamError, SampleFormat, SampleRate,
    StreamConfig, StreamError, StreamInstant, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange, SupportedStreamConfigsError,
};

const CHANNELS: u16 = 2;
const SAMPLE_RATE: SampleRate = 44100;

pub struct Host;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Device;

pub struct Stream {
    playing: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    creation: std::time::Instant,
    buffer_frames: u32,
}

crate::assert_stream_send!(Stream);
crate::assert_stream_sync!(Stream);

pub type SupportedInputConfigs = crate::iter::SupportedInputConfigs;
pub type SupportedOutputConfigs = crate::iter::SupportedOutputConfigs;

#[derive(Default)]
pub struct Devices {
    yielded: bool,
}

impl Host {
    pub fn new() -> Result<Self, crate::HostUnavailable> {
        Ok(Host)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Ok(Devices { yielded: false })
    }

    fn default_input_device(&self) -> Option<Device> {
        None
    }

    fn default_output_device(&self) -> Option<Device> {
        Some(Device)
    }
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn name(&self) -> Result<String, DeviceNameError> {
        Ok("ToyOS Audio".to_string())
    }

    fn description(&self) -> Result<DeviceDescription, DeviceNameError> {
        Ok(DeviceDescriptionBuilder::new("ToyOS Audio".to_string()).build())
    }

    fn id(&self) -> Result<DeviceId, DeviceIdError> {
        Ok(DeviceId(crate::platform::HostId::Toyos, String::new()))
    }

    fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        Ok(Vec::new().into_iter())
    }

    fn supported_output_configs(
        &self,
    ) -> Result<SupportedOutputConfigs, SupportedStreamConfigsError> {
        Ok(vec![SupportedStreamConfigRange::new(
            CHANNELS,
            SAMPLE_RATE,
            SAMPLE_RATE,
            SupportedBufferSize::Range {
                min: 128,
                max: 4096,
            },
            SampleFormat::I16,
        )]
        .into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Err(DefaultStreamConfigError::StreamTypeNotSupported)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Ok(SupportedStreamConfig::new(
            CHANNELS,
            SAMPLE_RATE,
            SupportedBufferSize::Range {
                min: 128,
                max: 4096,
            },
            SampleFormat::I16,
        ))
    }

    fn build_input_stream_raw<D, E>(
        &self,
        _config: StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Err(BuildStreamError::StreamConfigNotSupported)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        mut data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate;

        let audio = toyos::audio::AudioStream::open(
            sample_rate as u32,
            channels as u16,
            0, // S16LE
        ).map_err(|e| BuildStreamError::BackendSpecific {
            err: crate::BackendSpecificError {
                description: format!("failed to open audio stream: {e:?}"),
            },
        })?;

        let buffer_frames = audio.period_frames();
        let buffer_samples = buffer_frames as usize * channels;

        let playing = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let playing2 = playing.clone();
        let alive2 = alive.clone();

        let thread = std::thread::Builder::new()
            .name("cpal-toyos-audio".to_string())
            .spawn(move || {
                let now = StreamInstant::new(0, 0);
                let info = OutputCallbackInfo::new(OutputStreamTimestamp {
                    callback: now,
                    playback: now,
                });

                while alive2.load(Ordering::Relaxed) {
                    if !playing2.load(Ordering::Relaxed) {
                        let ptr = &*playing2 as *const AtomicBool as *const u32;
                        unsafe {
                            toyos_abi::syscall::futex_wait(ptr, 0, None);
                        }
                        continue;
                    }

                    audio.wait_and_fill(|buf| {
                        crate::host::fill_with_equilibrium(buf, sample_format);
                        let mut data = unsafe {
                            Data::from_parts(buf.as_mut_ptr() as *mut (), buffer_samples, sample_format)
                        };
                        data_callback(&mut data, &info);
                    });
                }

                audio.close();
            })
            .map_err(|e| BuildStreamError::BackendSpecific {
                err: crate::BackendSpecificError {
                    description: format!("failed to spawn audio thread: {e}"),
                },
            })?;

        Ok(Stream {
            playing,
            alive,
            thread: Some(thread),
            creation: std::time::Instant::now(),
            buffer_frames,
        })
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        self.playing.store(true, Ordering::Relaxed);
        unsafe {
            let ptr = &*self.playing as *const AtomicBool as *const u32;
            toyos_abi::syscall::futex_wake(ptr, 1);
        }
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        self.playing.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn buffer_size(&self) -> Result<crate::FrameCount, crate::StreamError> {
        Ok(self.buffer_frames)
    }

    fn now(&self) -> crate::StreamInstant {
        let d = self.creation.elapsed();
        crate::StreamInstant::new(d.as_secs(), d.subsec_nanos())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(th) = self.thread.take() {
            let _ = th.join();
        }
    }
}

impl Iterator for Devices {
    type Item = Device;
    fn next(&mut self) -> Option<Self::Item> {
        if self.yielded {
            None
        } else {
            self.yielded = true;
            Some(Device)
        }
    }
}
