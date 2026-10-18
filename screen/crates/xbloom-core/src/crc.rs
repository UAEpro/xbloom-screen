//! CRC-16/KERMIT — the checksum on every xBloom frame.
//!
//! Polynomial `0x1021`, init `0`, reflected input and output, no final XOR (check value
//! `0x2189` for `b"123456789"`). It is computed over the whole frame minus the trailing two
//! CRC bytes and stored little-endian.
//!
//! This is the reflected (LSB-first) form of the algorithm, so it uses the bit-reversed
//! polynomial `0x8408`; it is equivalent to the Python reference, which reflects each byte,
//! runs the MSB-first `0x1021` loop and reflects the result.

/// CRC-16/KERMIT of `data`.
pub fn crc16_kermit(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_check_value() {
        assert_eq!(crc16_kermit(b"123456789"), 0x2189);
        assert_eq!(crc16_kermit(b""), 0);
    }
}
