//! Minimal RFC 7798 HEVC RTP payloader.
//!
//! UU's official viewers accept single-layer non-interleaved HEVC. Parameter
//! sets are repeated in-band before each IRAP, so no aggregation packets are
//! required; oversized NAL units use FU packets.

use bytes::Bytes;

pub fn payload(mtu: usize, annex_b: &[u8]) -> Result<Vec<Bytes>, String> {
    if mtu < 4 {
        return Err("HEVC payload MTU too small".to_owned());
    }
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= annex_b.len() {
        let prefix = if annex_b[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if annex_b[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, index + prefix));
        index += prefix;
    }
    if starts.is_empty() {
        return Err("HEVC access unit has no Annex-B start code".to_owned());
    }
    let mut out = Vec::new();
    for (nal_index, &(_, start)) in starts.iter().enumerate() {
        let mut end = starts
            .get(nal_index + 1)
            .map_or(annex_b.len(), |next| next.0);
        while end > start && annex_b[end - 1] == 0 {
            end -= 1;
        }
        let nal = &annex_b[start..end];
        if nal.len() < 2 || nal[0] & 0x80 != 0 || nal[1] & 7 == 0 {
            return Err("invalid HEVC NAL header".to_owned());
        }
        let kind = (nal[0] >> 1) & 63;
        if kind >= 48 {
            return Err("HEVC Annex-B contains an RTP packet type".to_owned());
        }
        if nal.len() <= mtu {
            out.push(Bytes::copy_from_slice(nal));
            continue;
        }
        let chunks = nal[2..].chunks(mtu - 3);
        let last = chunks.len().saturating_sub(1);
        for (chunk_index, chunk) in chunks.enumerate() {
            let mut fu = Vec::with_capacity(3 + chunk.len());
            // Preserve F, LayerId and TID; replace only the NAL type with FU=49.
            fu.extend_from_slice(&[
                (nal[0] & 0x81) | (49 << 1),
                nal[1],
                kind | if chunk_index == 0 { 0x80 } else { 0 }
                    | if chunk_index == last { 0x40 } else { 0 },
            ]);
            fu.extend_from_slice(chunk);
            out.push(fu.into());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nals_and_fragments_large_units() {
        assert_eq!(
            payload(
                1200,
                &[0, 0, 0, 1, 64, 1, 9, 0, 0, 1, 66, 1, 8, 0, 0, 1, 38, 1, 7]
            )
            .unwrap(),
            vec![
                Bytes::from_static(&[64, 1, 9]),
                Bytes::from_static(&[66, 1, 8]),
                Bytes::from_static(&[38, 1, 7])
            ]
        );
        let header = [0x27, 0x2b];
        let data: Vec<u8> = (0..2501).map(|i| (i % 251 + 1) as u8).collect();
        let mut unit = vec![0, 0, 0, 1];
        unit.extend(header);
        unit.extend(&data);
        let packets = payload(1200, &unit).unwrap();
        assert!(packets.len() > 1);
        for packet in &packets {
            assert!(packet.len() <= 1200);
            assert_eq!((packet[0] >> 1) & 63, 49);
        }
    }
}
