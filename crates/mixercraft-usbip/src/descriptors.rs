//! USB descriptors of an emulated USB Audio Class 1 device: full speed,
//! 48 kHz, 16-bit stereo, one isochronous endpoint. A speaker device has an
//! adaptive OUT endpoint; a microphone device an asynchronous IN endpoint.

use crate::{CHANNELS, Direction, FRAMES_PER_MS, RATE};

/// pid.codes vendor id (open-source projects).
pub const VENDOR_ID: u16 = 0x1209;
/// pid.codes test product id. A product release needs its own PID.
pub const PRODUCT_ID: u16 = 0x0001;
/// Bytes per frame on the wire (16-bit stereo).
pub const FRAME_BYTES: usize = 2 * CHANNELS;
/// Largest isochronous packet: one frame more than nominal per 1 ms.
pub const MAX_PACKET: usize = (FRAMES_PER_MS + 1) * FRAME_BYTES;
/// The streaming endpoint number.
pub const STREAM_EP: u32 = 1;

/// Standard device descriptor.
#[must_use]
pub fn device() -> Vec<u8> {
    let [vl, vh] = VENDOR_ID.to_le_bytes();
    let [pl, ph] = PRODUCT_ID.to_le_bytes();
    vec![
        18, 1, // bLength, DEVICE
        0x00, 0x02, // bcdUSB 2.00 (full-speed device)
        0, 0, 0,  // class defined per interface
        64, // bMaxPacketSize0
        vl, vh, pl, ph, // idVendor, idProduct
        0x00, 0x01, // bcdDevice 1.00
        1, 2, 3, // iManufacturer, iProduct, iSerialNumber
        1, // bNumConfigurations
    ]
}

/// Configuration descriptor with all interface, class and endpoint
/// descriptors.
#[must_use]
pub fn configuration(direction: Direction) -> Vec<u8> {
    // Audio control: input terminal 1 -> output terminal 2.
    let (it_type, ot_type): (u16, u16) = match direction {
        Direction::Speaker => (0x0101, 0x0301),    // USB streaming -> speaker
        Direction::Microphone => (0x0201, 0x0101), // microphone -> USB streaming
    };
    let [itl, ith] = it_type.to_le_bytes();
    let [otl, oth] = ot_type.to_le_bytes();
    let channels = CHANNELS as u8;
    let input_terminal = [12, 0x24, 0x02, 1, itl, ith, 0, channels, 0x03, 0x00, 0, 0];
    let output_terminal = [9, 0x24, 0x03, 2, otl, oth, 0, 1, 0];
    let ac_total: u16 = 9 + 12 + 9;
    let [al, ah] = ac_total.to_le_bytes();
    let ac_header = [9, 0x24, 0x01, 0x00, 0x01, al, ah, 1, 1];
    let [r0, r1, r2, _] = RATE.to_le_bytes();
    let format = [11, 0x24, 0x02, 1, channels, 2, 16, 1, r0, r1, r2];
    let [ml, mh] = (MAX_PACKET as u16).to_le_bytes();
    let (ep_address, ep_attributes, terminal_link) = match direction {
        Direction::Speaker => (STREAM_EP as u8, 0x09, 1), // OUT, iso adaptive; feeds IT 1
        Direction::Microphone => (0x80 | STREAM_EP as u8, 0x05, 2), // IN, iso async; fed by OT 2
    };

    let mut d: Vec<u8> = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50];
    d.extend_from_slice(&[9, 4, 0, 0, 0, 1, 1, 0, 0]); // interface 0: audio control
    d.extend_from_slice(&ac_header);
    d.extend_from_slice(&input_terminal);
    d.extend_from_slice(&output_terminal);
    d.extend_from_slice(&[9, 4, 1, 0, 0, 1, 2, 0, 0]); // interface 1 alt 0: idle
    d.extend_from_slice(&[9, 4, 1, 1, 1, 1, 2, 0, 0]); // interface 1 alt 1: streaming
    d.extend_from_slice(&[7, 0x24, 0x01, terminal_link, 1, 0x01, 0x00]); // AS general, PCM
    d.extend_from_slice(&format);
    d.extend_from_slice(&[9, 5, ep_address, ep_attributes, ml, mh, 1, 0, 0]);
    d.extend_from_slice(&[7, 0x25, 0x01, 0x00, 0, 0, 0]);
    let [tl, th] = (d.len() as u16).to_le_bytes();
    d[2] = tl;
    d[3] = th;
    d
}

/// String descriptor `index` (0 = language list).
#[must_use]
pub fn string(index: u8, product: &str, serial: &str) -> Option<Vec<u8>> {
    let text = match index {
        0 => return Some(vec![4, 3, 0x09, 0x04]), // English (US)
        1 => "MixerCraft",
        2 => product,
        3 => serial,
        _ => return None,
    };
    let mut d = vec![0, 3];
    // A string descriptor holds at most 126 UTF-16 units.
    for u in text.encode_utf16().take(126) {
        d.extend_from_slice(&u.to_le_bytes());
    }
    d[0] = d.len() as u8;
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks the descriptor chain and returns (type, subtype-or-0) pairs.
    fn walk(d: &[u8]) -> Vec<(u8, u8)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < d.len() {
            let len = d[i] as usize;
            assert!(len >= 2 && i + len <= d.len(), "malformed descriptor at {i}");
            out.push((d[i + 1], if d[i + 1] >= 0x24 { d[i + 2] } else { 0 }));
            i += len;
        }
        out
    }

    #[test]
    fn configuration_lengths_are_consistent() {
        for dir in [Direction::Speaker, Direction::Microphone] {
            let c = configuration(dir);
            assert_eq!(usize::from(u16::from_le_bytes([c[2], c[3]])), c.len());
            let chain = walk(&c);
            assert_eq!(chain.iter().filter(|(t, _)| *t == 4).count(), 3, "AC + two AS alternates");
            assert_eq!(chain.iter().filter(|(t, _)| *t == 5).count(), 1, "one endpoint");
            // The class-specific AC total covers header + terminals.
            let ac = &c[18..];
            assert_eq!(u16::from_le_bytes([ac[5], ac[6]]), 30);
        }
        let mic = configuration(Direction::Microphone);
        let ep = mic.windows(2).position(|w| w == [9, 5]).unwrap();
        assert_eq!(mic[ep + 2], 0x81, "IN endpoint");
    }

    #[test]
    fn strings_are_utf16() {
        let s = string(2, "MixerCraft Game", "1").unwrap();
        assert_eq!(s[0] as usize, s.len());
        assert_eq!(s[0], 2 + 2 * 15);
        assert!(string(9, "x", "y").is_none());
        assert_eq!(device().len(), 18);
    }
}
