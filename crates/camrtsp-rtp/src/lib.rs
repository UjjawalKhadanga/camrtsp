pub fn packetize_h264(nal: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    if nal.is_empty() || mtu <= 2 {
        return vec![];
    }
    if nal.len() <= mtu {
        return vec![nal.to_vec()];
    }
    let header = nal[0];
    let fu_indicator = (header & 0xe0) | 28;
    let nal_type = header & 0x1f;
    let chunk = mtu - 2;
    nal[1..]
        .chunks(chunk)
        .enumerate()
        .map(|(i, data)| {
            let mut packet = Vec::with_capacity(data.len() + 2);
            let end = (i + 1) * chunk >= nal.len() - 1;
            packet.push(fu_indicator);
            packet.push(nal_type | if i == 0 { 0x80 } else { 0 } | if end { 0x40 } else { 0 });
            packet.extend_from_slice(data);
            packet
        })
        .collect()
}

pub fn rtp_packet(
    payload: &[u8],
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    marker: bool,
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(12 + payload.len());
    packet.extend_from_slice(&[0x80, 96 | if marker { 0x80 } else { 0 }]);
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_large_nal_as_fu_a() {
        let packets = packetize_h264(&[0x65, 1, 2, 3, 4, 5], 4);
        assert_eq!(
            packets,
            vec![
                vec![0x7c, 0x85, 1, 2],
                vec![0x7c, 0x05, 3, 4],
                vec![0x7c, 0x45, 5]
            ]
        );
    }
}
