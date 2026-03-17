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
const BUFFER_FRAMES: u32 = 1024;

// soundd protocol (must match toyos_abi::audio)
const MSG_AUDIO_OPEN: u32 = 1;
const MSG_AUDIO_OPENED: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioOpenRequest {
    sample_rate: u32,
    channels: u16,
    format: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioOpenResponse {
    stream_id: u32,
    shm_token: u32,
    ring_size: u32,
}

pub struct Host;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Device;

pub struct Stream {
    playing: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
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
                min: 256,
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
                min: 256,
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
        let buffer_frames = match config.buffer_size {
            crate::BufferSize::Fixed(n) => n,
            crate::BufferSize::Default => BUFFER_FRAMES,
        };
        let sample_size = sample_format.sample_size();
        let buffer_samples = buffer_frames as usize * channels;
        let buffer_bytes = buffer_samples * sample_size;

        // Connect to soundd and request a stream
        let control = connect_soundd();
        let req = AudioOpenRequest {
            sample_rate: sample_rate as u32,
            channels: channels as u16,
            format: 0, // S16LE
        };
        toyos_abi::ipc::send(control, MSG_AUDIO_OPEN, &req).expect("soundd not responding");

        // soundd allocates shared memory and sends us the token
        let (msg_type, resp): (u32, AudioOpenResponse) =
            toyos_abi::ipc::recv(control).expect("soundd not responding");
        assert_eq!(msg_type, MSG_AUDIO_OPENED);

        // Map the shared memory ring
        let shm = toyos_abi::shm::SharedMemory::map(resp.shm_token, resp.ring_size as usize);

        let playing = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let playing2 = playing.clone();
        let alive2 = alive.clone();

        let thread = std::thread::Builder::new()
            .name("cpal-toyos-audio".to_string())
            .spawn(move || {
                let ring = unsafe { &*(shm.as_ptr() as *const toyos_abi::ring::RingHeader) };
                let mut buffer = vec![0u8; buffer_bytes];
                while alive2.load(Ordering::Relaxed) {
                    if !playing2.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }

                    let now = StreamInstant::new(0, 0);
                    let info = OutputCallbackInfo::new(OutputStreamTimestamp {
                        callback: now,
                        playback: now,
                    });

                    crate::host::fill_with_equilibrium(&mut buffer, sample_format);
                    let mut data = unsafe {
                        Data::from_parts(buffer.as_mut_ptr() as *mut (), buffer_samples, sample_format)
                    };
                    data_callback(&mut data, &info);

                    // Find the last non-zero byte — only write actual audio to the ring.
                    // If the callback produced nothing (VecDeque empty), skip the write
                    // entirely. soundd handles empty rings by outputting silence.
                    let content_len = buffer.iter().rposition(|&b| b != 0)
                        .map(|pos| (pos + 2) & !1)  // round up to sample boundary
                        .unwrap_or(0)
                        .min(buffer.len());

                    if content_len > 0 {
                        let mut written = 0;
                        while written < content_len {
                            let n = ring.write(&buffer[written..content_len]);
                            written += n;
                            if n == 0 {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                        // Pace to hardware rate: sleep proportional to samples written.
                        // This is what CoreAudio/ALSA do — call the callback at the
                        // rate the hardware consumes, not as fast as possible.
                        let samples_written = content_len / (channels * sample_size);
                        let sleep_nanos = samples_written as u64 * 1_000_000_000 / sample_rate as u64;
                        std::thread::sleep(Duration::from_nanos(sleep_nanos));
                    } else {
                        // App has no audio right now — sleep for one period and retry.
                        let period_nanos = buffer_frames as u64 * 1_000_000_000 / sample_rate as u64;
                        std::thread::sleep(Duration::from_nanos(period_nanos));
                    }

                }

                ring.close_writer();
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
        })
    }
}

fn connect_soundd() -> toyos_abi::Fd {
    for _ in 0..100 {
        if let Ok(fd) = toyos_abi::syscall::connect("soundd") {
            return fd;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("soundd not found");
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        self.playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        self.playing.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Device> {
        if self.yielded {
            None
        } else {
            self.yielded = true;
            Some(Device)
        }
    }
}
