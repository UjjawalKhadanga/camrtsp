//! Windows 10/11 camera capture using Media Foundation. All COM and Media
//! Foundation ownership is confined to this module and its MTA worker thread.

use super::*;
use bytes::Bytes;
use camrtsp_core::{
    CameraDescriptor, CameraFormat, CameraId, CameraPosition, CodecConfig, EncodedAccessUnit,
    PixelFormat,
};
use std::{
    mem::ManuallyDrop,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
    },
    core::{Error as WinError, HRESULT, IUnknownImpl, Interface, PWSTR, implement},
};

const CAPTURE_QUEUE_CAPACITY: usize = 16;

#[derive(Default)]
pub struct PlatformPipeline {
    running: Option<RunningPipeline>,
}

struct RunningPipeline {
    commands: Sender<WorkerCommand>,
    join: JoinHandle<()>,
}

enum WorkerCommand {
    RequestKeyframe,
    Stop,
}

struct CapturedSample {
    sample: IMFSample,
    timestamp: i64,
}

enum TransformEvent {
    NeedInput,
    HaveOutput,
    Failure(String),
}

#[implement(IMFAsyncCallback)]
struct TransformEventCallback {
    state: Arc<TransformEventCallbackState>,
}

struct TransformEventCallbackState {
    events: SyncSender<TransformEvent>,
    generator: Mutex<Option<IMFMediaEventGenerator>>,
    alive: AtomicBool,
}

impl TransformEventCallbackState {
    fn new(events: SyncSender<TransformEvent>) -> Arc<Self> {
        Arc::new(Self {
            events,
            generator: Mutex::new(None),
            alive: AtomicBool::new(true),
        })
    }

    fn install(&self, generator: IMFMediaEventGenerator, callback: IMFAsyncCallback) -> Result<()> {
        *self
            .generator
            .lock()
            .expect("transform event lock poisoned") = Some(generator.clone());
        unsafe { generator.BeginGetEvent(&callback, None) }
            .map_err(media_error("unable to begin asynchronous encoder events"))
    }

    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        self.generator
            .lock()
            .expect("transform event lock poisoned")
            .take();
    }
}

impl IMFAsyncCallback_Impl for TransformEventCallback_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }

    fn Invoke(&self, result: windows::core::Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let generator = self
            .state
            .generator
            .lock()
            .expect("transform event lock poisoned")
            .clone();
        let Some(generator) = generator else {
            return Ok(());
        };
        let Some(result) = result.cloned() else {
            return Ok(());
        };
        match unsafe { generator.EndGetEvent(&result) } {
            Ok(event) => {
                let event_type = unsafe { event.GetType() }.unwrap_or_default();
                let message = if event_type == METransformNeedInput.0 as u32 {
                    Some(TransformEvent::NeedInput)
                } else if event_type == METransformHaveOutput.0 as u32 {
                    Some(TransformEvent::HaveOutput)
                } else {
                    None
                };
                if let Some(message) = message {
                    if self.state.events.try_send(message).is_err() {
                        let _ = self.state.events.try_send(TransformEvent::Failure(
                            "asynchronous H.264 encoder event queue overflowed".into(),
                        ));
                    }
                }
            }
            Err(error) => {
                let _ = self.state.events.try_send(TransformEvent::Failure(format!(
                    "asynchronous H.264 encoder event failed: {error}"
                )));
            }
        }
        if self.state.alive.load(Ordering::Acquire) {
            let callback = self.to_interface::<IMFAsyncCallback>();
            let _ = unsafe { generator.BeginGetEvent(&callback, None) };
        }
        Ok(())
    }
}

#[implement(IMFSourceReaderCallback)]
struct SourceReaderCallback {
    state: Arc<SourceReaderCallbackState>,
}

struct SourceReaderCallbackState {
    samples: SyncSender<CapturedSample>,
    reader: Mutex<Option<IMFSourceReader>>,
    alive: AtomicBool,
    discontinuity: AtomicBool,
    failure: Mutex<Option<String>>,
}

impl SourceReaderCallbackState {
    fn new(samples: SyncSender<CapturedSample>) -> Arc<Self> {
        Arc::new(Self {
            samples,
            reader: Mutex::new(None),
            alive: AtomicBool::new(true),
            discontinuity: AtomicBool::new(false),
            failure: Mutex::new(None),
        })
    }

    fn set_reader(&self, reader: IMFSourceReader) {
        *self
            .reader
            .lock()
            .expect("source reader callback lock poisoned") = Some(reader);
    }

    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        self.reader
            .lock()
            .expect("source reader callback lock poisoned")
            .take();
    }

    fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .expect("source reader callback lock poisoned")
            .clone()
    }

    fn take_discontinuity(&self) -> bool {
        self.discontinuity.swap(false, Ordering::AcqRel)
    }

    fn request_next_sample(&self) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        let reader = self
            .reader
            .lock()
            .expect("source reader callback lock poisoned")
            .clone();
        if let Some(reader) = reader {
            // The asynchronous Source Reader contract permits requesting the
            // next sample from its callback. Failure is surfaced to the owner.
            if let Err(error) = unsafe {
                reader.ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
            } {
                *self
                    .failure
                    .lock()
                    .expect("source reader callback lock poisoned") =
                    Some(format!("Source Reader ReadSample failed: {error}"));
            }
        }
    }
}

impl IMFSourceReaderCallback_Impl for SourceReaderCallback_Impl {
    fn OnReadSample(
        &self,
        hrstatus: HRESULT,
        _stream_index: u32,
        stream_flags: u32,
        timestamp: i64,
        sample: windows::core::Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Ok(());
        }
        if !hrstatus.is_ok() || stream_flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            *self
                .state
                .failure
                .lock()
                .expect("source reader callback lock poisoned") = Some(format!(
                "camera capture failed with status {hrstatus:?}, flags {stream_flags:#x}"
            ));
            return Ok(());
        }
        if stream_flags
            & (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32
                | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32)
            != 0
        {
            *self
                .state
                .failure
                .lock()
                .expect("source reader callback lock poisoned") =
                Some("camera changed its media type while streaming".into());
            return Ok(());
        }
        if let Some(sample) = sample.cloned() {
            // Never block a Media Foundation callback. Full queues discard the
            // captured frame; the downstream encoder will be asked for an IDR.
            if self
                .state
                .samples
                .try_send(CapturedSample { sample, timestamp })
                .is_err()
            {
                self.state.discontinuity.store(true, Ordering::Release);
            }
        }
        self.state.request_next_sample();
        Ok(())
    }

    fn OnFlush(&self, _stream_index: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnEvent(
        &self,
        _stream_index: u32,
        _event: windows::core::Ref<IMFMediaEvent>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl PlatformPipeline {
    pub fn enumerate(&self) -> Result<Vec<CameraDescriptor>> {
        let _runtime = MediaFoundationRuntime::new()?;
        enumerate_devices()
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        sink: AccessUnitSink,
    ) -> Result<NegotiatedStreamConfig> {
        if self.running.is_some() {
            return Err(CamRtspError("camera pipeline is already running".into()));
        }
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let (commands, command_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("camrtsp-windows-media".into())
            .spawn(move || worker(config, sink, command_rx, startup_tx))
            .map_err(|error| {
                CamRtspError(format!("unable to start Media Foundation worker: {error}"))
            })?;
        match startup_rx.recv() {
            Ok(Ok(negotiated)) => {
                self.running = Some(RunningPipeline { commands, join });
                Ok(negotiated)
            }
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                let _ = join.join();
                Err(CamRtspError(
                    "Media Foundation worker exited during startup".into(),
                ))
            }
        }
    }

    pub fn request_keyframe(&self) -> Result<()> {
        let Some(running) = &self.running else {
            return Err(CamRtspError("camera pipeline is not running".into()));
        };
        running
            .commands
            .send(WorkerCommand::RequestKeyframe)
            .map_err(|_| CamRtspError("Media Foundation worker is no longer available".into()))
    }

    pub fn stop(&mut self) -> Result<()> {
        if let Some(running) = self.running.take() {
            let _ = running.commands.send(WorkerCommand::Stop);
            let _ = running.join.join();
        }
        Ok(())
    }
}

impl Drop for PlatformPipeline {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn worker(
    config: StreamConfig,
    sink: AccessUnitSink,
    commands: Receiver<WorkerCommand>,
    startup: SyncSender<Result<NegotiatedStreamConfig>>,
) {
    let result = worker_inner(config, sink, commands, &startup);
    if let Err(error) = result {
        let _ = startup.send(Err(error));
    }
}

fn worker_inner(
    config: StreamConfig,
    sink: AccessUnitSink,
    commands: Receiver<WorkerCommand>,
    startup: &SyncSender<Result<NegotiatedStreamConfig>>,
) -> Result<()> {
    let _runtime = MediaFoundationRuntime::new()?;
    let device = find_device(&config.camera)?;
    let source: IMFMediaSource = unsafe { device.activation.ActivateObject() }
        .map_err(media_error("unable to activate selected camera"))?;
    let (sample_tx, sample_rx) = mpsc::sync_channel(CAPTURE_QUEUE_CAPACITY);
    let callback_state = SourceReaderCallbackState::new(sample_tx);
    let callback_impl = SourceReaderCallback {
        state: callback_state.clone(),
    };
    let callback: IMFSourceReaderCallback = callback_impl.into();
    let attributes = create_attributes(4, "unable to create Source Reader attributes")?;
    unsafe {
        attributes
            .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)
            .map_err(media_error("unable to set Source Reader callback"))?;
        attributes
            .SetUINT32(&MF_LOW_LATENCY, 1)
            .map_err(media_error("unable to set low latency capture"))?;
        attributes
            .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
            .map_err(media_error("unable to enable hardware transforms"))?;
    }
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
        .map_err(media_error("unable to create Source Reader"))?;
    let selected = choose_media_type(&reader, &config)?;
    unsafe {
        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &selected.media_type,
            )
            .map_err(media_error("camera rejected selected media type"))?;
    }

    let mut mode = if selected.format.pixel_format == PixelFormat::H264 {
        let codec = codec_config_from_media_type(&selected.media_type)?;
        sink.set_codec_config(codec.clone());
        PipelineMode::Passthrough { codec }
    } else {
        PipelineMode::Encoder(MftEncoder::new(&selected, &config)?)
    };

    callback_state.set_reader(reader.clone());
    unsafe {
        reader
            .ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                None,
                None,
                None,
            )
            .map_err(media_error("unable to start camera capture"))?;
    }
    let negotiated = NegotiatedStreamConfig {
        camera: CameraDescriptor {
            id: config.camera,
            name: device.name,
            position: CameraPosition::External,
            formats: vec![selected.format.clone()],
        },
        width: selected.format.width,
        height: selected.format.height,
        fps_num: selected.format.fps_num,
        fps_den: selected.format.fps_den,
        pixel_format: selected.format.pixel_format,
        encoder_name: mode.name().into(),
        hardware_encoder: mode.hardware_encoder(),
    };
    startup
        .send(Ok(negotiated))
        .map_err(|_| CamRtspError("caller abandoned startup".into()))?;

    loop {
        if let Some(error) = callback_state.failure() {
            return Err(CamRtspError(error));
        }
        if callback_state.take_discontinuity() {
            // A raw-frame drop invalidates the encoder cadence. A force-keyframe
            // request is best effort because direct H.264 camera output cannot
            // expose this control through Media Foundation.
            let _ = mode.request_keyframe();
        }
        match commands.try_recv() {
            Ok(WorkerCommand::Stop) | Err(TryRecvError::Disconnected) => break,
            Ok(WorkerCommand::RequestKeyframe) => mode.request_keyframe()?,
            Err(TryRecvError::Empty) => {}
        }
        match sample_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(sample) => mode.consume(sample, &sink)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    callback_state.stop();
    let _ = mode.shutdown(&sink);
    unsafe {
        let _ = reader.Flush(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32);
        let _ = source.Shutdown();
    }
    Ok(())
}

enum PipelineMode {
    Passthrough { codec: CodecConfig },
    Encoder(MftEncoder),
}

/// Driver for synchronous Media Foundation H.264 encoder transforms.  It owns
/// the transform on the capture worker, so no COM object crosses a thread.
enum MftEncoder {
    Asynchronous(AsynchronousMftEncoder),
    Synchronous(SynchronousMftEncoder),
}

impl MftEncoder {
    fn new(selected: &SelectedMediaType, config: &StreamConfig) -> Result<Self> {
        match AsynchronousMftEncoder::new(selected, config) {
            Ok(encoder) => Ok(Self::Asynchronous(encoder)),
            Err(_) => SynchronousMftEncoder::new(selected, config).map(Self::Synchronous),
        }
    }

    fn consume(&mut self, captured: CapturedSample, sink: &AccessUnitSink) -> Result<()> {
        match self {
            Self::Asynchronous(encoder) => encoder.consume(captured, sink),
            Self::Synchronous(encoder) => encoder.consume(captured, sink),
        }
    }

    fn request_keyframe(&self) -> Result<()> {
        match self {
            Self::Asynchronous(encoder) => encoder.request_keyframe(),
            Self::Synchronous(encoder) => encoder.request_keyframe(),
        }
    }

    fn hardware_encoder(&self) -> bool {
        matches!(self, Self::Asynchronous(_))
    }

    fn shutdown(&mut self, sink: &AccessUnitSink) -> Result<()> {
        match self {
            Self::Asynchronous(encoder) => encoder.shutdown(sink),
            Self::Synchronous(encoder) => encoder.shutdown(sink),
        }
    }
}

/// Event-driven driver for hardware encoder MFTs. Media Foundation raises
/// NeedInput/HaveOutput on its work queue; the callback only enqueues those
/// events while this owning worker calls ProcessInput and ProcessOutput.
struct AsynchronousMftEncoder {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    output_stream_info: MFT_OUTPUT_STREAM_INFO,
    callback_state: Arc<TransformEventCallbackState>,
    events: Receiver<TransformEvent>,
    needs_input: bool,
}

impl AsynchronousMftEncoder {
    fn new(selected: &SelectedMediaType, config: &StreamConfig) -> Result<Self> {
        let activations = encoder_activations(
            selected.format.pixel_format,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        )?;
        let mut last_error = None;
        for activation in activations {
            match Self::from_activation(activation, selected, config) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            CamRtspError("no compatible asynchronous hardware H.264 encoder is installed".into())
        }))
    }

    fn from_activation(
        activation: IMFActivate,
        selected: &SelectedMediaType,
        config: &StreamConfig,
    ) -> Result<Self> {
        let transform: IMFTransform = unsafe { activation.ActivateObject() }
            .map_err(media_error("unable to activate hardware H.264 encoder"))?;
        let attributes = unsafe { transform.GetAttributes() }.map_err(media_error(
            "unable to query hardware H.264 encoder attributes",
        ))?;
        if unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 0 {
            return Err(CamRtspError(
                "hardware H.264 encoder did not expose asynchronous MFT support".into(),
            ));
        }
        unsafe {
            let _ = attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
        }
        let generator = transform
            .cast::<IMFMediaEventGenerator>()
            .map_err(media_error("hardware H.264 encoder has no event generator"))?;
        let output = h264_output_type(&selected.format, config)?;
        unsafe {
            transform.SetOutputType(0, &output, 0).map_err(media_error(
                "hardware H.264 encoder rejected its output type",
            ))?;
            transform
                .SetInputType(0, &selected.media_type, 0)
                .map_err(media_error(
                    "hardware H.264 encoder rejected the camera media type",
                ))?;
        }
        let output_stream_info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(media_error("unable to query hardware H.264 output"))?;
        let (event_tx, events) = mpsc::sync_channel(32);
        let callback_state = TransformEventCallbackState::new(event_tx);
        let callback_impl = TransformEventCallback {
            state: callback_state.clone(),
        };
        let callback: IMFAsyncCallback = callback_impl.into();
        callback_state.install(generator, callback)?;
        unsafe {
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(Self {
            codec_api: transform.cast::<ICodecAPI>().ok(),
            transform,
            output_stream_info,
            callback_state,
            events,
            needs_input: false,
        })
    }

    fn consume(&mut self, captured: CapturedSample, sink: &AccessUnitSink) -> Result<()> {
        self.pump_events(sink)?;
        if !self.needs_input {
            // The transform is still busy. Dropping this raw frame keeps the
            // capture callback non-blocking and bounds latency under load.
            return Ok(());
        }
        unsafe { self.transform.ProcessInput(0, &captured.sample, 0) }.map_err(media_error(
            "hardware H.264 encoder rejected an input sample",
        ))?;
        self.needs_input = false;
        self.pump_events(sink)
    }

    fn pump_events(&mut self, sink: &AccessUnitSink) -> Result<()> {
        loop {
            match self.events.try_recv() {
                Ok(TransformEvent::NeedInput) => self.needs_input = true,
                Ok(TransformEvent::HaveOutput) => self.produce_output(sink)?,
                Ok(TransformEvent::Failure(error)) => return Err(CamRtspError(error)),
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err(CamRtspError(
                        "hardware H.264 encoder event callback stopped unexpectedly".into(),
                    ));
                }
            }
        }
    }

    fn produce_output(&self, sink: &AccessUnitSink) -> Result<()> {
        let mut output = output_buffer(self.output_stream_info)?;
        let mut status = 0_u32;
        unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        }
        .map_err(media_error(
            "hardware H.264 encoder failed while producing output",
        ))?;
        let Some(sample) = take_output_sample(&mut output) else {
            return Ok(());
        };
        publish_encoded_sample(&self.transform, sample, sink)
    }

    fn request_keyframe(&self) -> Result<()> {
        let Some(codec_api) = &self.codec_api else {
            return Err(CamRtspError(
                "the selected H.264 encoder does not expose forced-keyframe control".into(),
            ));
        };
        let value = windows::Win32::System::Variant::VARIANT::from(true);
        unsafe { codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value) }.map_err(
            media_error("hardware H.264 encoder could not force a keyframe"),
        )
    }

    fn shutdown(&mut self, sink: &AccessUnitSink) -> Result<()> {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        let result = self.pump_events(sink);
        self.callback_state.stop();
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        result
    }
}

impl Drop for AsynchronousMftEncoder {
    fn drop(&mut self) {
        self.callback_state.stop();
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

fn output_buffer(info: MFT_OUTPUT_STREAM_INFO) -> Result<MFT_OUTPUT_DATA_BUFFER> {
    let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
        None
    } else {
        let sample = unsafe { MFCreateSample() }
            .map_err(media_error("unable to allocate encoder output sample"))?;
        let buffer = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1)) }
            .map_err(media_error("unable to allocate encoder output buffer"))?;
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(media_error("unable to attach encoder output buffer"))?;
        Some(sample)
    };
    Ok(MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(sample),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    })
}

fn publish_encoded_sample(
    transform: &IMFTransform,
    sample: IMFSample,
    sink: &AccessUnitSink,
) -> Result<()> {
    if let Ok(media_type) = unsafe { transform.GetOutputCurrentType(0) } {
        if let Ok(codec) = codec_config_from_media_type(&media_type) {
            sink.set_codec_config(codec);
        }
    }
    let bytes = sample_bytes(&sample)?;
    let nal_units = split_h264(&bytes);
    if nal_units.is_empty() {
        return Err(CamRtspError(
            "H.264 encoder emitted an empty access unit".into(),
        ));
    }
    if let Some(codec) = codec_config_from_nal_units(&nal_units) {
        sink.set_codec_config(codec);
    }
    let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0
        || nal_units
            .iter()
            .any(|nal| nal.first().is_some_and(|value| value & 0x1f == 5));
    let timestamp = unsafe { sample.GetSampleTime() }.unwrap_or(0);
    let duration = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
    sink.publish(EncodedAccessUnit {
        nal_units: nal_units.into_iter().map(Bytes::from).collect(),
        pts_90khz: hns_to_90khz(timestamp),
        duration_90khz: hns_to_90khz(duration),
        keyframe,
    });
    Ok(())
}

struct SynchronousMftEncoder {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    output_stream_info: MFT_OUTPUT_STREAM_INFO,
}

impl SynchronousMftEncoder {
    fn new(selected: &SelectedMediaType, config: &StreamConfig) -> Result<Self> {
        // Windows supplies a synchronous software H.264 MFT as the portable
        // fallback after hardware asynchronous MFT selection has failed.
        let activations = encoder_activations(
            selected.format.pixel_format,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        )?;
        let mut last_error = None;
        for activation in activations {
            match Self::from_activation(activation, selected, config) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            CamRtspError("no synchronous Media Foundation H.264 encoder is installed".into())
        }))
    }

    fn from_activation(
        activation: IMFActivate,
        selected: &SelectedMediaType,
        config: &StreamConfig,
    ) -> Result<Self> {
        let transform: IMFTransform = unsafe { activation.ActivateObject() }
            .map_err(media_error("unable to activate H.264 encoder"))?;
        let output = h264_output_type(&selected.format, config)?;
        unsafe {
            // Encoder MFTs require their H.264 output type before the raw input type.
            transform
                .SetOutputType(0, &output, 0)
                .map_err(media_error("H.264 encoder rejected its output type"))?;
            transform
                .SetInputType(0, &selected.media_type, 0)
                .map_err(media_error("H.264 encoder rejected the camera media type"))?;
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        let output_stream_info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(media_error("unable to query H.264 encoder output"))?;
        let codec_api = transform.cast::<ICodecAPI>().ok();
        Ok(Self {
            transform,
            codec_api,
            output_stream_info,
        })
    }

    fn consume(&mut self, captured: CapturedSample, sink: &AccessUnitSink) -> Result<()> {
        self.drain(sink)?;
        match unsafe { self.transform.ProcessInput(0, &captured.sample, 0) } {
            Ok(()) => self.drain(sink),
            Err(error) if error.code() == MF_E_NOTACCEPTING => {
                self.drain(sink)?;
                unsafe { self.transform.ProcessInput(0, &captured.sample, 0) }
                    .map_err(media_error("H.264 encoder rejected an input sample"))?;
                self.drain(sink)
            }
            Err(error) => Err(media_error("H.264 encoder rejected an input sample")(error)),
        }
    }

    fn drain(&mut self, sink: &AccessUnitSink) -> Result<()> {
        loop {
            let mut output = self.output_buffer()?;
            let mut status = 0_u32;
            match unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            } {
                Ok(()) => {
                    let sample = take_output_sample(&mut output);
                    let Some(sample) = sample else {
                        continue;
                    };
                    self.publish_sample(sample, sink)?;
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    drop_output_buffer(&mut output);
                    return Ok(());
                }
                Err(error) => {
                    drop_output_buffer(&mut output);
                    return Err(media_error("H.264 encoder failed while producing output")(
                        error,
                    ));
                }
            }
        }
    }

    fn output_buffer(&self) -> Result<MFT_OUTPUT_DATA_BUFFER> {
        let sample =
            if self.output_stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
                None
            } else {
                let sample = unsafe { MFCreateSample() }
                    .map_err(media_error("unable to allocate encoder output sample"))?;
                let buffer = unsafe { MFCreateMemoryBuffer(self.output_stream_info.cbSize.max(1)) }
                    .map_err(media_error("unable to allocate encoder output buffer"))?;
                unsafe { sample.AddBuffer(&buffer) }
                    .map_err(media_error("unable to attach encoder output buffer"))?;
                Some(sample)
            };
        Ok(MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        })
    }

    fn publish_sample(&self, sample: IMFSample, sink: &AccessUnitSink) -> Result<()> {
        if let Ok(media_type) = unsafe { self.transform.GetOutputCurrentType(0) } {
            if let Ok(codec) = codec_config_from_media_type(&media_type) {
                sink.set_codec_config(codec);
            }
        }
        let bytes = sample_bytes(&sample)?;
        let nal_units = split_h264(&bytes);
        if nal_units.is_empty() {
            return Err(CamRtspError(
                "H.264 encoder emitted an empty access unit".into(),
            ));
        }
        if let Some(codec) = codec_config_from_nal_units(&nal_units) {
            sink.set_codec_config(codec);
        }
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0
            || nal_units
                .iter()
                .any(|nal| nal.first().is_some_and(|value| value & 0x1f == 5));
        let timestamp = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let duration = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
        sink.publish(EncodedAccessUnit {
            nal_units: nal_units.into_iter().map(Bytes::from).collect(),
            pts_90khz: hns_to_90khz(timestamp),
            duration_90khz: hns_to_90khz(duration),
            keyframe,
        });
        Ok(())
    }

    fn request_keyframe(&self) -> Result<()> {
        let Some(codec_api) = &self.codec_api else {
            return Err(CamRtspError(
                "the selected H.264 encoder does not expose forced-keyframe control".into(),
            ));
        };
        let value = windows::Win32::System::Variant::VARIANT::from(true);
        unsafe { codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value) }
            .map_err(media_error("H.264 encoder could not force a keyframe"))
    }

    fn shutdown(&mut self, sink: &AccessUnitSink) -> Result<()> {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        self.drain(sink)?;
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        Ok(())
    }
}

impl Drop for SynchronousMftEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

impl PipelineMode {
    fn consume(&mut self, captured: CapturedSample, sink: &AccessUnitSink) -> Result<()> {
        match self {
            Self::Passthrough { codec } => {
                if let Some(config) = codec_config_from_sample(&captured.sample)? {
                    *codec = config.clone();
                    sink.set_codec_config(config);
                }
                let bytes = sample_bytes(&captured.sample)?;
                let nal_units = split_h264(&bytes);
                if nal_units.is_empty() {
                    return Err(CamRtspError(
                        "camera emitted an empty H.264 access unit".into(),
                    ));
                }
                let keyframe = unsafe { captured.sample.GetUINT32(&MFSampleExtension_CleanPoint) }
                    .unwrap_or(0)
                    != 0
                    || nal_units
                        .iter()
                        .any(|nal| nal.first().is_some_and(|value| value & 0x1f == 5));
                let duration = unsafe { captured.sample.GetSampleDuration() }.unwrap_or(0);
                sink.publish(EncodedAccessUnit {
                    nal_units: nal_units.into_iter().map(Bytes::from).collect(),
                    pts_90khz: hns_to_90khz(captured.timestamp),
                    duration_90khz: hns_to_90khz(duration),
                    keyframe,
                });
                Ok(())
            }
            Self::Encoder(encoder) => encoder.consume(captured, sink),
        }
    }

    fn request_keyframe(&self) -> Result<()> {
        match self {
            Self::Passthrough { .. } => Err(CamRtspError(
                "the selected camera exposes H.264 directly and does not support forced keyframes"
                    .into(),
            )),
            Self::Encoder(encoder) => encoder.request_keyframe(),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Passthrough { .. } => "Media Foundation native H.264 pass-through",
            Self::Encoder(encoder) if encoder.hardware_encoder() => {
                "Media Foundation H.264 hardware encoder"
            }
            Self::Encoder(_) => "Media Foundation H.264 software encoder",
        }
    }

    fn hardware_encoder(&self) -> bool {
        matches!(self, Self::Encoder(encoder) if encoder.hardware_encoder())
    }

    fn shutdown(&mut self, sink: &AccessUnitSink) -> Result<()> {
        match self {
            Self::Passthrough { .. } => Ok(()),
            Self::Encoder(encoder) => encoder.shutdown(sink),
        }
    }
}

fn h264_output_type(format: &CameraFormat, config: &StreamConfig) -> Result<IMFMediaType> {
    let output = unsafe { MFCreateMediaType() }.map_err(media_error(
        "unable to create H.264 encoder output media type",
    ))?;
    let frame_size = (u64::from(format.width) << 32) | u64::from(format.height);
    let frame_rate = (u64::from(format.fps_num.max(1)) << 32) | u64::from(format.fps_den.max(1));
    let bitrate = config.bitrate.resolve(
        format.width,
        format.height,
        format.fps_num / format.fps_den.max(1),
    );
    unsafe {
        output
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(media_error("unable to set H.264 output major type"))?;
        output
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(media_error("unable to set H.264 output subtype"))?;
        output
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
            .map_err(media_error("unable to set H.264 output frame size"))?;
        output
            .SetUINT64(&MF_MT_FRAME_RATE, frame_rate)
            .map_err(media_error("unable to set H.264 output frame rate"))?;
        output
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(media_error("unable to set H.264 output bitrate"))?;
        output
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(media_error("unable to set progressive H.264 output"))?;
        output
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
            .map_err(media_error("unable to set H.264 Main profile"))?;
        output
            .SetUINT32(
                &MF_MT_MAX_KEYFRAME_SPACING,
                format
                    .fps_num
                    .saturating_mul(config.gop_seconds.max(1))
                    .saturating_div(format.fps_den.max(1)),
            )
            .map_err(media_error("unable to set H.264 keyframe interval"))?;
    }
    Ok(output)
}

fn encoder_activations(
    input_format: PixelFormat,
    flags: MFT_ENUM_FLAG,
) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: match input_format {
            PixelFormat::Nv12 => MFVideoFormat_NV12,
            PixelFormat::Yuy2 => MFVideoFormat_YUY2,
            _ => return Err(CamRtspError("unsupported raw encoder input format".into())),
        },
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut raw = ptr::null_mut();
    let mut count = 0_u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut raw,
            &mut count,
        )
    }
    .map_err(media_error("unable to enumerate H.264 encoder transforms"))?;
    let guard = CoTaskMemActivations {
        pointer: raw,
        count: count as usize,
    };
    let activations = guard
        .as_slice()
        .iter()
        .filter_map(Clone::clone)
        .collect::<Vec<_>>();
    Ok(activations)
}

fn take_output_sample(output: &mut MFT_OUTPUT_DATA_BUFFER) -> Option<IMFSample> {
    let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
    unsafe { ManuallyDrop::drop(&mut output.pEvents) };
    sample
}

fn drop_output_buffer(output: &mut MFT_OUTPUT_DATA_BUFFER) {
    unsafe {
        ManuallyDrop::drop(&mut output.pSample);
        ManuallyDrop::drop(&mut output.pEvents);
    }
}

struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
            .map_err(media_error("unable to initialize COM MTA"))?;
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            unsafe { CoUninitialize() };
            return Err(media_error("unable to initialize Media Foundation")(error));
        }
        Ok(Self)
    }
}

impl Drop for MediaFoundationRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

struct DeviceActivation {
    name: String,
    id: CameraId,
    activation: IMFActivate,
}

fn enumerate_devices() -> Result<Vec<CameraDescriptor>> {
    let activations = video_device_activations()?;
    let mut cameras = Vec::with_capacity(activations.len());
    for activation in activations {
        let formats = enumerate_formats(&activation.activation).unwrap_or_default();
        cameras.push(CameraDescriptor {
            id: activation.id,
            name: activation.name,
            position: CameraPosition::External,
            formats,
        });
    }
    if cameras.is_empty() {
        Err(CamRtspError(
            "no Windows video cameras are available".into(),
        ))
    } else {
        Ok(cameras)
    }
}

fn find_device(id: &CameraId) -> Result<DeviceActivation> {
    video_device_activations()?
        .into_iter()
        .find(|device| device.id == *id)
        .ok_or_else(|| CamRtspError(format!("camera '{id}' was not found")))
}

fn video_device_activations() -> Result<Vec<DeviceActivation>> {
    let attributes = create_attributes(1, "unable to create device attributes")?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(media_error("unable to select video capture devices"))?;
    let mut raw = ptr::null_mut();
    let mut count = 0_u32;
    unsafe { MFEnumDeviceSources(&attributes, &mut raw, &mut count) }
        .map_err(media_error("unable to enumerate video devices"))?;
    let guard = CoTaskMemActivations {
        pointer: raw,
        count: count as usize,
    };
    let mut result = Vec::with_capacity(guard.count);
    for activation in guard.as_slice() {
        let activation = activation.clone().ok_or_else(|| {
            CamRtspError("Media Foundation returned an invalid camera activation".into())
        })?;
        let name = allocated_string(&activation, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?;
        let id = allocated_string(
            &activation,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        )?;
        result.push(DeviceActivation {
            name,
            id: CameraId(id),
            activation,
        });
    }
    Ok(result)
}

struct CoTaskMemActivations {
    pointer: *mut Option<IMFActivate>,
    count: usize,
}

impl CoTaskMemActivations {
    fn as_slice(&self) -> &[Option<IMFActivate>] {
        if self.pointer.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.pointer, self.count) }
        }
    }
}

impl Drop for CoTaskMemActivations {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.pointer.cast())) };
    }
}

struct SelectedMediaType {
    media_type: IMFMediaType,
    format: CameraFormat,
}

fn enumerate_formats(activation: &IMFActivate) -> Result<Vec<CameraFormat>> {
    let source: IMFMediaSource = unsafe { activation.ActivateObject() }.map_err(media_error(
        "unable to activate camera for format enumeration",
    ))?;
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }
        .map_err(media_error("unable to create format Source Reader"))?;
    let mut formats = Vec::new();
    for index in 0.. {
        let media_type = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, index)
        } {
            Ok(media_type) => media_type,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(media_error("unable to inspect camera media type")(error)),
        };
        if let Ok(format) = camera_format(&media_type) {
            formats.push(format);
        }
    }
    unsafe {
        let _ = source.Shutdown();
    }
    Ok(formats)
}

fn choose_media_type(reader: &IMFSourceReader, config: &StreamConfig) -> Result<SelectedMediaType> {
    let mut candidates = Vec::new();
    for index in 0.. {
        let media_type = match unsafe {
            reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, index)
        } {
            Ok(media_type) => media_type,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(media_error("unable to enumerate camera media types")(error)),
        };
        if let Ok(format) = camera_format(&media_type) {
            candidates.push(SelectedMediaType { media_type, format });
        }
    }
    candidates
        .into_iter()
        .min_by_key(|candidate| format_score(&candidate.format, config))
        .ok_or_else(|| CamRtspError("camera has no H.264, NV12, or YUY2 capture format".into()))
}

fn camera_format(media_type: &IMFMediaType) -> Result<CameraFormat> {
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }
        .map_err(media_error("media type has no subtype"))?;
    let pixel_format = if subtype == MFVideoFormat_H264 || subtype == MFVideoFormat_H264_ES {
        PixelFormat::H264
    } else if subtype == MFVideoFormat_NV12 {
        PixelFormat::Nv12
    } else if subtype == MFVideoFormat_YUY2 {
        PixelFormat::Yuy2
    } else {
        return Err(CamRtspError("unsupported camera media subtype".into()));
    };
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(media_error("media type has no frame size"))?;
    let rate = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or((30_u64) << 32 | 1);
    Ok(CameraFormat {
        width: (size >> 32) as u32,
        height: size as u32,
        fps_num: (rate >> 32) as u32,
        fps_den: (rate as u32).max(1),
        pixel_format,
    })
}

fn format_score(format: &CameraFormat, config: &StreamConfig) -> (u8, u64, u64) {
    let kind = match format.pixel_format {
        PixelFormat::H264 => 0,
        PixelFormat::Nv12 => 1,
        PixelFormat::Yuy2 => 2,
        _ => 9,
    };
    let resolution = format.width.abs_diff(config.requested_width) as u64
        + format.height.abs_diff(config.requested_height) as u64;
    let requested = config.requested_fps as u64 * format.fps_den as u64;
    let actual = format.fps_num as u64;
    (kind, resolution, requested.abs_diff(actual))
}

fn codec_config_from_media_type(media_type: &IMFMediaType) -> Result<CodecConfig> {
    let size = unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }
        .map_err(|_| CamRtspError("H.264 camera media type did not provide SPS/PPS".into()))?;
    let mut blob = vec![0_u8; size as usize];
    unsafe { media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }
        .map_err(media_error("unable to read H.264 sequence header"))?;
    codec_config_from_sequence_header(&blob)
        .ok_or_else(|| CamRtspError("H.264 sequence header did not contain SPS and PPS".into()))
}

fn codec_config_from_sample(sample: &IMFSample) -> Result<Option<CodecConfig>> {
    let bytes = sample_bytes(sample)?;
    Ok(codec_config_from_nal_units(&split_h264(&bytes)))
}

fn codec_config_from_annex_b(bytes: &[u8]) -> Option<CodecConfig> {
    codec_config_from_nal_units(&split_h264(bytes))
}

fn codec_config_from_sequence_header(bytes: &[u8]) -> Option<CodecConfig> {
    codec_config_from_annex_b(bytes).or_else(|| codec_config_from_avcc(bytes))
}

fn codec_config_from_nal_units(units: &[Vec<u8>]) -> Option<CodecConfig> {
    let sps = units
        .iter()
        .find(|nal| nal.first().is_some_and(|value| value & 0x1f == 7))?
        .clone();
    let pps = units
        .iter()
        .find(|nal| nal.first().is_some_and(|value| value & 0x1f == 8))?
        .clone();
    let profile_level_id = [*sps.get(1)?, *sps.get(2)?, *sps.get(3)?];
    Some(CodecConfig {
        sps: Bytes::from(sps),
        pps: Bytes::from(pps),
        profile_level_id,
    })
}

/// Parses the AVCDecoderConfigurationRecord layout used by many Media
/// Foundation encoders for `MF_MT_MPEG_SEQUENCE_HEADER`.
fn codec_config_from_avcc(bytes: &[u8]) -> Option<CodecConfig> {
    if bytes.len() < 7 || bytes[0] != 1 {
        return None;
    }
    let mut offset = 5;
    let sps_count = usize::from(bytes[offset] & 0x1f);
    offset += 1;
    let mut sps = None;
    for _ in 0..sps_count {
        let length = usize::from(u16::from_be_bytes(
            bytes.get(offset..offset + 2)?.try_into().ok()?,
        ));
        offset += 2;
        let unit = bytes.get(offset..offset.checked_add(length)?)?.to_vec();
        offset += length;
        if unit.first().is_some_and(|value| value & 0x1f == 7) {
            sps = Some(unit);
        }
    }
    let pps_count = usize::from(*bytes.get(offset)?);
    offset += 1;
    let mut pps = None;
    for _ in 0..pps_count {
        let length = usize::from(u16::from_be_bytes(
            bytes.get(offset..offset + 2)?.try_into().ok()?,
        ));
        offset += 2;
        let unit = bytes.get(offset..offset.checked_add(length)?)?.to_vec();
        offset += length;
        if unit.first().is_some_and(|value| value & 0x1f == 8) {
            pps = Some(unit);
        }
    }
    let sps = sps?;
    let pps = pps?;
    Some(CodecConfig {
        profile_level_id: [*sps.get(1)?, *sps.get(2)?, *sps.get(3)?],
        sps: Bytes::from(sps),
        pps: Bytes::from(pps),
    })
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(media_error("unable to access H.264 sample buffer"))?;
    let mut pointer = ptr::null_mut();
    let mut length = 0_u32;
    unsafe { buffer.Lock(&mut pointer, None, Some(&mut length)) }
        .map_err(media_error("unable to lock H.264 sample buffer"))?;
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length as usize) }.to_vec();
    unsafe {
        let _ = buffer.Unlock();
    }
    Ok(bytes)
}

fn split_h264(bytes: &[u8]) -> Vec<Vec<u8>> {
    let starts = (0..bytes.len())
        .filter(|&index| {
            bytes[index..].starts_with(&[0, 0, 1]) || bytes[index..].starts_with(&[0, 0, 0, 1])
        })
        .collect::<Vec<_>>();
    if !starts.is_empty() {
        return starts
            .iter()
            .enumerate()
            .filter_map(|(index, start)| {
                let prefix = if bytes[*start..].starts_with(&[0, 0, 0, 1]) {
                    4
                } else {
                    3
                };
                let end = starts.get(index + 1).copied().unwrap_or(bytes.len());
                (*start + prefix < end).then(|| bytes[*start + prefix..end].to_vec())
            })
            .collect();
    }
    let mut units = Vec::new();
    let mut offset = 0;
    while offset + 4 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let Some(end) = offset.checked_add(length) else {
            return Vec::new();
        };
        let Some(unit) = bytes.get(offset..end) else {
            return Vec::new();
        };
        units.push(unit.to_vec());
        offset = end;
    }
    units
}

fn hns_to_90khz(value: i64) -> u32 {
    (value.max(0) as u64).saturating_mul(9).saturating_div(1000) as u32
}

fn allocated_string(attributes: &IMFAttributes, key: &windows::core::GUID) -> Result<String> {
    let mut value = PWSTR::null();
    let mut length = 0_u32;
    unsafe { attributes.GetAllocatedString(key, &mut value, &mut length) }
        .map_err(media_error("unable to read camera attribute"))?;
    let text = unsafe { value.to_string() }
        .map_err(|error| CamRtspError(format!("camera attribute is not valid UTF-16: {error}")))?;
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    Ok(text)
}

fn create_attributes(capacity: u32, context: &'static str) -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, capacity) }.map_err(media_error(context))?;
    attributes.ok_or_else(|| {
        CamRtspError(format!(
            "{context}: Media Foundation returned no attribute store"
        ))
    })
}

fn media_error(context: &'static str) -> impl FnOnce(WinError) -> CamRtspError {
    move |error| {
        let detail = if error.code().0 == 0x80070005_u32 as i32 {
            "camera access was denied; enable desktop camera access in Windows Settings > Privacy & security > Camera".to_string()
        } else {
            error.to_string()
        };
        CamRtspError(format!("{context}: {detail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_h264_and_timestamp_units() {
        let units = split_h264(&[0, 0, 0, 1, 0x67, 0x4d, 0, 0x1f, 0, 0, 1, 0x68, 0xee]);
        assert_eq!(units, vec![vec![0x67, 0x4d, 0, 0x1f], vec![0x68, 0xee]]);
        assert_eq!(hns_to_90khz(10_000_000), 90_000);
    }

    #[test]
    fn parses_avcc_codec_configuration() {
        let config = codec_config_from_avcc(&[
            1, 0x4d, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x4d, 0, 0x1f, 1, 0, 2, 0x68, 0xee,
        ])
        .expect("valid avcC header");
        assert_eq!(config.sps.as_ref(), [0x67, 0x4d, 0, 0x1f]);
        assert_eq!(config.pps.as_ref(), [0x68, 0xee]);
        assert_eq!(config.profile_level_id, [0x4d, 0, 0x1f]);
    }

    #[test]
    #[ignore = "requires the Windows Media Foundation software H.264 encoder"]
    fn encodes_synthetic_nv12_with_the_software_mft() {
        let _runtime = MediaFoundationRuntime::new().expect("Media Foundation startup");
        let format = CameraFormat {
            width: 64,
            height: 64,
            fps_num: 30,
            fps_den: 1,
            pixel_format: PixelFormat::Nv12,
        };
        let media_type = unsafe { MFCreateMediaType() }.expect("NV12 media type");
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .unwrap();
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .unwrap();
            media_type
                .SetUINT64(&MF_MT_FRAME_SIZE, (64_u64 << 32) | 64)
                .unwrap();
            media_type
                .SetUINT64(&MF_MT_FRAME_RATE, (30_u64 << 32) | 1)
                .unwrap();
            media_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .unwrap();
        }
        let selected = SelectedMediaType { media_type, format };
        let config = StreamConfig {
            camera: CameraId("synthetic".into()),
            requested_width: 64,
            requested_height: 64,
            requested_fps: 30,
            bitrate: camrtsp_core::BitrateMode::BitsPerSecond(250_000),
            gop_seconds: 1,
        };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = frames.clone();
        let sink = AccessUnitSink::new(
            move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            },
            |_| {},
        );
        let mut encoder =
            SynchronousMftEncoder::new(&selected, &config).expect("Windows software H.264 encoder");
        for index in 0..12_i64 {
            encoder
                .consume(synthetic_nv12_sample(index), &sink)
                .expect("synthetic NV12 encoding");
        }
        encoder.shutdown(&sink).expect("encoder drain");
        assert!(
            frames.load(Ordering::Relaxed) > 0,
            "encoder produced H.264 output"
        );
    }

    fn synthetic_nv12_sample(index: i64) -> CapturedSample {
        let sample = unsafe { MFCreateSample() }.expect("synthetic sample");
        let buffer = unsafe { MFCreateMemoryBuffer(64 * 64 * 3 / 2) }.expect("synthetic buffer");
        let mut data = ptr::null_mut();
        unsafe {
            buffer.Lock(&mut data, None, None).unwrap();
            std::ptr::write_bytes(data, (index as u8).wrapping_mul(17), 64 * 64);
            std::ptr::write_bytes(data.add(64 * 64), 128, 64 * 64 / 2);
            buffer.SetCurrentLength(64 * 64 * 3 / 2).unwrap();
            buffer.Unlock().unwrap();
            sample.AddBuffer(&buffer).unwrap();
            sample.SetSampleTime(index * 333_333).unwrap();
            sample.SetSampleDuration(333_333).unwrap();
        }
        CapturedSample {
            sample,
            timestamp: index * 333_333,
        }
    }
}
