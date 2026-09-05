use bytes::Bytes;
use serde::Serialize;
use std::{fmt, sync::Arc};

pub type Result<T> = std::result::Result<T, CamRtspError>;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CamRtspError(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct CameraId(pub String);

impl fmt::Display for CameraId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraPosition {
    Front,
    Back,
    #[default]
    External,
    Unspecified,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    Nv12,
    Yuy2,
    H264,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub pixel_format: PixelFormat,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CameraDescriptor {
    pub id: CameraId,
    pub name: String,
    pub position: CameraPosition,
    pub formats: Vec<CameraFormat>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitrateMode {
    Auto,
    BitsPerSecond(u32),
}

impl BitrateMode {
    pub fn resolve(&self, width: u32, height: u32, fps: u32) -> u32 {
        match self {
            Self::Auto => width
                .saturating_mul(height)
                .saturating_mul(fps)
                .saturating_div(10)
                .clamp(1_000_000, 12_000_000),
            Self::BitsPerSecond(value) => *value,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub camera: CameraId,
    pub requested_width: u32,
    pub requested_height: u32,
    pub requested_fps: u32,
    pub bitrate: BitrateMode,
    pub gop_seconds: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct NegotiatedStreamConfig {
    pub camera: CameraDescriptor,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub pixel_format: PixelFormat,
    pub encoder_name: String,
    pub hardware_encoder: bool,
}

#[derive(Clone, Debug)]
pub struct CodecConfig {
    pub sps: Bytes,
    pub pps: Bytes,
    pub profile_level_id: [u8; 3],
}

#[derive(Clone, Debug)]
pub struct EncodedAccessUnit {
    pub nal_units: Vec<Bytes>,
    pub pts_90khz: u32,
    pub duration_90khz: u32,
    pub keyframe: bool,
}

pub trait NativeVideoPipeline {
    fn enumerate(&self) -> Result<Vec<CameraDescriptor>>;
    fn start(
        &mut self,
        config: StreamConfig,
        sink: AccessUnitSink,
    ) -> Result<NegotiatedStreamConfig>;
    fn request_keyframe(&self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}

#[derive(Clone)]
pub struct AccessUnitSink {
    publish: Arc<dyn Fn(EncodedAccessUnit) + Send + Sync>,
    set_codec_config: Arc<dyn Fn(CodecConfig) + Send + Sync>,
}

impl AccessUnitSink {
    pub fn new(
        publish: impl Fn(EncodedAccessUnit) + Send + Sync + 'static,
        set_codec_config: impl Fn(CodecConfig) + Send + Sync + 'static,
    ) -> Self {
        Self {
            publish: Arc::new(publish),
            set_codec_config: Arc::new(set_codec_config),
        }
    }

    pub fn publish(&self, access_unit: EncodedAccessUnit) {
        (self.publish)(access_unit);
    }

    pub fn set_codec_config(&self, config: CodecConfig) {
        (self.set_codec_config)(config);
    }
}
