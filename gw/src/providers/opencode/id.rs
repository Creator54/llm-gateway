//! Opencode id encoding.
//!
//! `docs/provider-specs/opencode-direct.md`: the free tier accepts a request only
//! when the session id is a descending encoded id with a non-zero counter. The
//! encoding mirrors `~/opencode/packages/schema/src/identifier.ts`: 12 hex chars
//! of a 48-bit value, then 14 random chars.

/// Alphabet used by opencode for the random tail.
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Encodes `descending` (already counted) as opencode's 12 hex chars.
fn encode(value: u64) -> String {
    let value = value & 0xffff_ffff_ffff;
    let mut out = String::with_capacity(12);
    for shift in (0..6).rev() {
        out.push_str(&format!("{:02x}", (value >> (shift * 8)) & 0xff));
    }
    out
}

/// The 12 hex chars for a millisecond timestamp, counter and direction.
pub fn stamp(ms: u64, counter: u64, descending: bool) -> String {
    let current = (ms << 12) | (counter & 0xfff);
    if descending {
        encode(!current)
    } else {
        encode(current)
    }
}

/// 14 random chars from `entropy` (a hash), so ids stay deterministic in tests.
fn tail(entropy: &[u8]) -> String {
    (0..14)
        .map(|i| {
            let byte = entropy.get(i % entropy.len().max(1)).copied().unwrap_or(0);
            ALPHABET[usize::from(byte) % ALPHABET.len()] as char
        })
        .collect()
}

/// A full session id: `ses_` plus stamp and tail.
pub fn session(ms: u64, counter: u64, entropy: &[u8]) -> String {
    format!("ses_{}{}", stamp(ms, counter, true), tail(entropy))
}

/// A full request id: `msg_` plus stamp and tail. Message ids ascend, unlike sessions.
pub fn request(ms: u64, counter: u64, entropy: &[u8]) -> String {
    format!("msg_{}{}", stamp(ms, counter, false), tail(entropy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_descend_and_messages_ascend_like_the_cli() {
        // Decoded from a captured opencode 1.19.5 turn: `ses_e161929b7ffe…`, `msg_1e9e6d65e001…`.
        assert_eq!(stamp(8_219_186_760, 1, true), "e161929b7ffe");
        assert!(request(8_219_186_782, 1, b"x").starts_with("msg_1e9e6d65e001"));
    }

    #[test]
    fn stamp_is_twelve_hex_chars() {
        let stamp = stamp(1_700_000_000_000, 1, true);
        assert_eq!(stamp.len(), 12);
        assert!(stamp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn descending_counter_matches_fixture() {
        // ms = 0x1800000000000, counter 1, descending: ~(1 << 12 | 1) truncated to 48 bits.
        let value = !(((0x180_0000_0000_u64) << 12) | 1) & 0xffff_ffff_ffff;
        assert_eq!(stamp(0x180_0000_0000, 1, true), encode(value));
        assert_ne!(
            stamp(0x180_0000_0000, 1, true),
            stamp(0x180_0000_0000, 0, true)
        );
    }

    #[test]
    fn ascending_differs_from_descending() {
        assert_ne!(
            stamp(1_700_000_000_000, 1, true),
            stamp(1_700_000_000_000, 1, false)
        );
    }

    #[test]
    fn ids_have_the_expected_shape() {
        let sid = session(1_700_000_000_000, 1, b"entropy-seed-01");
        let rid = request(1_700_000_000_000, 2, b"entropy-seed-02");
        assert_eq!(sid.len(), 4 + 12 + 14);
        assert_eq!(rid.len(), 4 + 12 + 14);
        assert!(sid.starts_with("ses_"));
        assert!(rid.starts_with("msg_"));
        assert!(sid[16..].chars().all(|c| ALPHABET.contains(&(c as u8))));
    }

    #[test]
    fn tail_is_deterministic_and_varies_with_entropy() {
        assert_eq!(tail(b"seed-a"), tail(b"seed-a"));
        assert_ne!(tail(b"seed-a"), tail(b"seed-b"));
        assert_eq!(tail(b"").len(), 14);
    }
}
