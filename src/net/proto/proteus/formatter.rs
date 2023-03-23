use bytes::{Bytes, BytesMut};

use crate::net::{
    proto::proteus::message::OvertMessage, Deserialize, Deserializer, Serialize, Serializer,
};

#[derive(Clone, Copy)]
pub struct Formatter {
    // All proteus messages can be formatted without extra state.
}

impl Formatter {
    pub fn new() -> Formatter {
        Formatter {}
    }
}

impl Serializer<OvertMessage> for Formatter {
    fn serialize_frame(&mut self, src: OvertMessage) -> Bytes {
        src.serialize()
    }
}

impl Deserializer<OvertMessage> for Formatter {
    fn deserialize_frame(&mut self, src: &mut std::io::Cursor<&BytesMut>) -> Option<OvertMessage> {
        OvertMessage::deserialize(src)
    }
}