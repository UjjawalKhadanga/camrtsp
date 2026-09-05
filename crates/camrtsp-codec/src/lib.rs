#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    pub annex_b: Vec<u8>,
    pub timestamp: u32,
    pub keyframe: bool,
}

pub trait VideoEncoder {
    type Error;
    fn encode(&mut self, frame: &[u8], timestamp: u32) -> Result<EncodedFrame, Self::Error>;
}
