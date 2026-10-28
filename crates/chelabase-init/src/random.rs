//! Random values, all from the operating system's CSPRNG (`getrandom`).

use base64::Engine;

/// `N` random bytes. Panics only if the OS RNG is unavailable, where no secret can be made.
pub fn bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the operating system's random number generator");
    out
}

/// 32 random bytes as 43 base64url characters (`[A-Za-z0-9_-]`).
pub fn text32() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes::<32>())
}

/// `len` characters drawn uniformly from `alphabet` (rejection sampling, no modulo bias).
pub fn from_alphabet(alphabet: &[u8], len: usize) -> String {
    assert!(!alphabet.is_empty() && alphabet.len() <= 256);
    let n = alphabet.len();
    // The largest multiple of n that fits in a byte: bytes at or above it are dropped.
    let limit = 256 - (256 % n);
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in bytes::<64>() {
            if (b as usize) < limit && out.len() < len {
                out.push(alphabet[b as usize % n] as char);
            }
        }
    }
    out
}

pub const ALPHANUMERIC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_differ_and_stay_in_their_alphabet() {
        assert_ne!(text32(), text32());
        assert_eq!(text32().len(), 43);
        let s = from_alphabet(b"AB", 200);
        assert_eq!(s.len(), 200);
        assert!(s.contains('A') && s.contains('B'));
        assert!(
            from_alphabet(ALPHANUMERIC, 32)
                .bytes()
                .all(|b| b.is_ascii_alphanumeric())
        );
    }
}
