//! Lowercase hex, for keys and nonces in JSON.

pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    text.chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// `text` as exactly `N` bytes.
pub fn decode_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    decode(text)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_round_trips() {
        assert_eq!(super::encode(&[0, 1, 0xab, 0xff]), "0001abff");
        assert_eq!(super::decode("0001ABff").unwrap(), vec![0, 1, 0xab, 0xff]);
        assert_eq!(super::decode("abc"), None);
        assert_eq!(super::decode("zz"), None);
        assert_eq!(super::decode_array::<2>("0001"), Some([0, 1]));
        assert_eq!(super::decode_array::<3>("0001"), None);
    }
}
