use camrtsp_core::{
    AccessUnitSink, CamRtspError, CameraDescriptor, NativeVideoPipeline, NegotiatedStreamConfig,
    Result, StreamConfig,
};

#[derive(Default)]
pub struct NativeCamera {
    inner: platform::PlatformPipeline,
}

impl NativeVideoPipeline for NativeCamera {
    fn enumerate(&self) -> Result<Vec<CameraDescriptor>> {
        self.inner.enumerate()
    }

    fn start(
        &mut self,
        config: StreamConfig,
        sink: AccessUnitSink,
    ) -> Result<NegotiatedStreamConfig> {
        self.inner.start(config, sink)
    }

    fn request_keyframe(&self) -> Result<()> {
        self.inner.request_keyframe()
    }

    fn stop(&mut self) -> Result<()> {
        self.inner.stop()
    }
}

#[cfg(target_os = "macos")]
#[allow(clippy::useless_transmute)]
mod platform {
    use super::*;
    use bytes::Bytes;
    use camrtsp_core::{
        CameraDescriptor, CameraFormat, CameraId, CameraPosition, CodecConfig, EncodedAccessUnit,
        PixelFormat,
    };
    use cidre::{
        arc, av,
        av::capture::{
            Session, VideoDataOutput, VideoDataOutputSampleBufDelegate,
            VideoDataOutputSampleBufDelegateImpl,
        },
        cf, cm, define_obj_type, dispatch, ns, objc,
        vt::{
            self,
            compression::{encoder_spec_keys, profile_level},
            compression_properties::keys,
        },
    };
    use std::ffi::c_void;

    #[derive(Default)]
    pub struct PlatformPipeline {
        running: Option<RunningPipeline>,
    }

    struct RunningPipeline {
        session: arc::R<Session>,
        #[allow(dead_code)]
        input: arc::R<av::CaptureDeviceInput>,
        #[allow(dead_code)]
        output: arc::R<VideoDataOutput>,
        #[allow(dead_code)]
        delegate: arc::R<CaptureDelegate>,
        #[allow(dead_code)]
        queue: arc::R<dispatch::Queue>,
        encoder: arc::R<vt::CompressionSession>,
        // The callback receives this address from VideoToolbox. Owning it here keeps
        // that context valid until `complete_all` has drained every callback.
        #[allow(dead_code)]
        context: Box<EncoderContext>,
    }

    struct EncoderContext {
        sink: AccessUnitSink,
        default_duration_90khz: u32,
    }

    struct CaptureDelegateInner {
        encoder: arc::R<vt::CompressionSession>,
    }

    define_obj_type!(
        CaptureDelegate + VideoDataOutputSampleBufDelegateImpl,
        CaptureDelegateInner,
        CAMRTSP_CAPTURE_DELEGATE
    );

    impl VideoDataOutputSampleBufDelegate for CaptureDelegate {}

    #[objc::add_methods]
    impl VideoDataOutputSampleBufDelegateImpl for CaptureDelegate {
        extern "C" fn impl_capture_output_did_output_sample_buf_from_connection(
            &mut self,
            _cmd: Option<&objc::Sel>,
            _output: &av::CaptureOutput,
            sample_buf: &cm::SampleBuf,
            _connection: &av::CaptureConnection,
        ) {
            let Some(image) = sample_buf.image_buf() else {
                return;
            };
            let mut flags = None;
            let _ = self.inner().encoder.enc_frame(
                image,
                sample_buf.pts(),
                sample_buf.duration(),
                &mut flags,
            );
        }
    }

    impl PlatformPipeline {
        pub fn enumerate(&self) -> Result<Vec<CameraDescriptor>> {
            let cameras = av::CaptureDevice::devices()
                .iter()
                .filter(|device| {
                    device.is_connected() && device.has_media_type(av::MediaType::video())
                })
                .map(|device| CameraDescriptor {
                    id: CameraId(device.unique_id().to_string()),
                    name: device.localized_name().to_string(),
                    position: CameraPosition::External,
                    formats: Vec::new(),
                })
                .collect::<Vec<_>>();
            if cameras.is_empty() {
                Err(CamRtspError("no camera devices are available".into()))
            } else {
                Ok(cameras)
            }
        }

        pub fn start(
            &mut self,
            config: StreamConfig,
            sink: AccessUnitSink,
        ) -> Result<NegotiatedStreamConfig> {
            if self.running.is_some() {
                return Err(CamRtspError("camera pipeline is already running".into()));
            }
            let device_id = ns::String::with_str(&config.camera.0);
            let device = av::CaptureDevice::with_unique_id(&device_id)
                .ok_or_else(|| CamRtspError(format!("camera '{}' was not found", config.camera)))?;
            let input = av::CaptureDeviceInput::with_device(&device)
                .map_err(|error| CamRtspError(format!("unable to open camera: {error}")))?;
            let actual = choose_preset(&device, config.requested_width, config.requested_height);
            let fps = config.requested_fps.max(1);
            let bitrate = config.bitrate.resolve(actual.0, actual.1, fps);
            let context = Box::new(EncoderContext {
                sink,
                default_duration_90khz: 90_000 / fps,
            });
            let context_ptr = Box::into_raw(context);
            let (encoder, hardware_encoder) = match new_encoder(
                actual.0,
                actual.1,
                fps,
                bitrate,
                config.gop_seconds,
                context_ptr,
                true,
            ) {
                Ok(encoder) => (encoder, true),
                Err(error) => {
                    match new_encoder(
                        actual.0,
                        actual.1,
                        fps,
                        bitrate,
                        config.gop_seconds,
                        context_ptr,
                        false,
                    ) {
                        Ok(encoder) => (encoder, false),
                        Err(fallback_error) => {
                            unsafe {
                                drop(Box::from_raw(context_ptr));
                            }
                            return Err(CamRtspError(format!(
                                "hardware VideoToolbox setup failed ({error}); software fallback failed ({fallback_error})"
                            )));
                        }
                    }
                }
            };
            let delegate = CaptureDelegate::with(CaptureDelegateInner {
                encoder: encoder.clone(),
            });
            let queue = dispatch::Queue::serial_with_ar_pool();
            let mut output = VideoDataOutput::new();
            output.set_always_discard_late_video_frames(true);
            output.set_sample_buf_delegate(Some(delegate.as_ref()), Some(&queue));
            let mut session = Session::new();
            session.configure(|session| {
                if session.can_add_input(&input) {
                    session.add_input(&input);
                }
                if session.can_add_output(&output) {
                    session.add_output(&output);
                }
                let preset = preset_for(actual.0, actual.1);
                if session.can_set_session_preset(preset) {
                    let _ = session.set_session_preset(preset);
                }
            });
            session.start_running();
            let context = unsafe { Box::from_raw(context_ptr) };
            let descriptor = CameraDescriptor {
                id: config.camera,
                name: device.localized_name().to_string(),
                position: CameraPosition::External,
                formats: vec![CameraFormat {
                    width: actual.0,
                    height: actual.1,
                    fps_num: fps,
                    fps_den: 1,
                    pixel_format: PixelFormat::Nv12,
                }],
            };
            self.running = Some(RunningPipeline {
                session,
                input,
                output,
                delegate,
                queue,
                encoder: encoder.clone(),
                context,
            });
            Ok(NegotiatedStreamConfig {
                camera: descriptor,
                width: actual.0,
                height: actual.1,
                fps_num: fps,
                fps_den: 1,
                pixel_format: PixelFormat::Nv12,
                encoder_name: "VideoToolbox H.264".into(),
                hardware_encoder,
            })
        }

        pub fn request_keyframe(&self) -> Result<()> {
            Err(CamRtspError(
                "VideoToolbox keyframe requests are not wired yet".into(),
            ))
        }

        pub fn stop(&mut self) -> Result<()> {
            if let Some(mut running) = self.running.take() {
                running.session.stop_running();
                let _ = running.encoder.complete_all();
                running.encoder.invalidate();
            }
            Ok(())
        }
    }

    impl Drop for PlatformPipeline {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    fn choose_preset(device: &av::CaptureDevice, width: u32, height: u32) -> (u32, u32) {
        let candidates = [(1920, 1080), (1280, 720), (960, 540), (640, 480)];
        candidates
            .iter()
            .copied()
            .filter(|(candidate_width, candidate_height)| {
                device.supports_preset(preset_for(*candidate_width, *candidate_height))
            })
            .min_by_key(|(candidate_width, candidate_height)| {
                candidate_width.abs_diff(width) as u64 + candidate_height.abs_diff(height) as u64
            })
            .unwrap_or((640, 480))
    }

    fn preset_for(width: u32, height: u32) -> &'static av::CaptureSessionPreset {
        match (width, height) {
            (1920, 1080) => av::CaptureSessionPreset::_1920x1080(),
            (1280, 720) => av::CaptureSessionPreset::_1280x720(),
            (960, 540) => av::CaptureSessionPreset::_960x540(),
            _ => av::CaptureSessionPreset::_640x480(),
        }
    }

    fn new_encoder(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        gop_seconds: u32,
        context: *mut EncoderContext,
        require_hardware: bool,
    ) -> Result<arc::R<vt::CompressionSession>> {
        let mut hardware_spec = cf::DictionaryMut::with_capacity(1);
        let true_value = cf::Boolean::value_true();
        if require_hardware {
            hardware_spec.insert(
                encoder_spec_keys::require_hw_accelerated_video_encoder(),
                true_value,
            );
        }
        let mut encoder = vt::CompressionSession::new(
            width,
            height,
            cm::VideoCodec::H264,
            require_hardware.then_some(&hardware_spec),
            None,
            None,
            Some(encoded_frame),
            context,
        )
        .map_err(|error| CamRtspError(format!("VideoToolbox H.264 setup failed: {error:?}")))?;
        let false_value = cf::Boolean::value_false();
        let mut properties = cf::DictionaryMut::with_capacity(7);
        properties.insert(keys::real_time(), true_value);
        properties.insert(keys::allow_frame_reordering(), false_value);
        properties.insert(
            keys::avarage_bit_rate(),
            &cf::Number::from_i32(bitrate as i32),
        );
        properties.insert(
            keys::expected_frame_rate(),
            &cf::Number::from_i32(fps as i32),
        );
        properties.insert(
            keys::max_key_frame_interval(),
            &cf::Number::from_i32((fps * gop_seconds.max(1)) as i32),
        );
        properties.insert(keys::profile_lvl(), profile_level::h264::main_auto_lvl());
        encoder.set_props(&properties).map_err(|error| {
            CamRtspError(format!("VideoToolbox configuration failed: {error:?}"))
        })?;
        encoder
            .prepare()
            .map_err(|error| CamRtspError(format!("VideoToolbox prepare failed: {error:?}")))?;
        Ok(encoder)
    }

    extern "C" fn encoded_frame(
        context: *mut EncoderContext,
        _source: *mut c_void,
        status: cidre::os::Status,
        _flags: vt::EncodeInfoFlags,
        buffer: Option<&cm::SampleBuf>,
    ) {
        if status.is_err() || context.is_null() {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let context = unsafe { &*context };
            let Some(buffer) = buffer else { return };
            if buffer.is_key_frame()
                && let Some(config) = codec_config(buffer)
            {
                context.sink.set_codec_config(config);
            }
            let Some(data) = buffer.data_buf() else {
                return;
            };
            let Ok(bytes) = data.as_slice() else { return };
            let nal_units = split_avcc(bytes)
                .into_iter()
                .map(Bytes::from)
                .collect::<Vec<_>>();
            if nal_units.is_empty() {
                return;
            }
            let pts = buffer.pts();
            let pts_90khz = ((pts.value as i128 * 90_000i128) / pts.scale as i128) as u32;
            context.sink.publish(EncodedAccessUnit {
                nal_units,
                pts_90khz,
                duration_90khz: context.default_duration_90khz,
                keyframe: buffer.is_key_frame(),
            });
        }));
    }

    fn codec_config(buffer: &cm::SampleBuf) -> Option<CodecConfig> {
        let description = buffer.format_desc()? as &cm::VideoFormatDesc;
        let avcc = description.avcc()?;
        parse_avcc_config(&avcc)
    }

    fn parse_avcc_config(data: &[u8]) -> Option<CodecConfig> {
        if data.len() < 7 || data[0] != 1 {
            return None;
        }
        let profile_level_id = [data[1], data[2], data[3]];
        let mut cursor = 5;
        let sps_count = (data[cursor] & 0x1f) as usize;
        cursor += 1;
        if sps_count == 0 || cursor + 2 > data.len() {
            return None;
        }
        let sps_len = u16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
        cursor += 2;
        let sps = data.get(cursor..cursor + sps_len)?.to_vec();
        cursor += sps_len;
        let pps_count = *data.get(cursor)? as usize;
        cursor += 1;
        if pps_count == 0 || cursor + 2 > data.len() {
            return None;
        }
        let pps_len = u16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
        cursor += 2;
        let pps = data.get(cursor..cursor + pps_len)?.to_vec();
        Some(CodecConfig {
            sps: Bytes::from(sps),
            pps: Bytes::from(pps),
            profile_level_id,
        })
    }

    fn split_avcc(data: &[u8]) -> Vec<Vec<u8>> {
        let mut cursor = 0;
        let mut nal_units = Vec::new();
        while cursor + 4 <= data.len() {
            let length = u32::from_be_bytes(data[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            let Some(nal) = data.get(cursor..cursor + length) else {
                break;
            };
            nal_units.push(nal.to_vec());
            cursor += length;
        }
        nal_units
    }
}

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod platform;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    #[derive(Default)]
    pub struct PlatformPipeline;

    impl PlatformPipeline {
        pub fn enumerate(&self) -> Result<Vec<CameraDescriptor>> {
            Err(CamRtspError(
                "this platform backend has not been compiled yet".into(),
            ))
        }

        pub fn start(
            &mut self,
            _: StreamConfig,
            _: AccessUnitSink,
        ) -> Result<NegotiatedStreamConfig> {
            Err(CamRtspError(
                "this platform backend has not been compiled yet".into(),
            ))
        }

        pub fn request_keyframe(&self) -> Result<()> {
            Err(CamRtspError(
                "this platform backend has not been compiled yet".into(),
            ))
        }

        pub fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
}
