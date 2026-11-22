//! TOTP (RFC 6238) and recovery codes. SHA-1, 6 digits, 30 second steps: what every
//! authenticator app supports.

use hmac::{Hmac, Mac};
use rand::Rng;
use sha1::Sha1;

pub const STEP_SECS: u64 = 30;
pub const DIGITS: u32 = 6;
pub const SECRET_BYTES: usize = 20;
pub const RECOVERY_CODE_COUNT: usize = 10;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn new_secret() -> Vec<u8> {
    let mut b = vec![0u8; SECRET_BYTES];
    rand::rng().fill(&mut b[..]);
    b
}

/// RFC 4648 base32 without padding.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buf = 0u32;
    let mut bits = 0;
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.chars().filter(|c| *c != ' ' && *c != '=') {
        let v = B32.iter().position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let bin = ((h[off] as u32 & 0x7f) << 24) | ((h[off + 1] as u32) << 16) | ((h[off + 2] as u32) << 8) | (h[off + 3] as u32);
    bin % 10u32.pow(DIGITS)
}

pub fn step_for(unix_secs: u64) -> u64 {
    unix_secs / STEP_SECS
}

pub fn code_at(secret: &[u8], unix_secs: u64) -> String {
    format!("{:0width$}", hotp(secret, step_for(unix_secs)), width = DIGITS as usize)
}

/// Accepts the current step and one either side (clock drift). Returns the step that
/// matched, which the caller stores so the same code cannot be used twice. `last_step` is
/// the last step already used, and anything at or before it is refused.
pub fn verify(secret: &[u8], code: &str, unix_secs: u64, last_step: Option<i64>) -> Option<i64> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if digits.len() != DIGITS as usize || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = step_for(unix_secs) as i64;
    let mut found = None;
    // Check all three steps regardless of an early match, so the time taken does not say
    // which one matched.
    for step in [now - 1, now, now + 1] {
        if step < 0 {
            continue;
        }
        let expected = format!("{:0width$}", hotp(secret, step as u64), width = DIGITS as usize);
        if subtle::ConstantTimeEq::ct_eq(expected.as_bytes(), digits.as_bytes()).into()
            && last_step.is_none_or(|l| step > l)
            && found.is_none()
        {
            found = Some(step);
        }
    }
    found
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn otpauth_uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        pct(issuer),
        pct(account),
        base32_encode(secret),
        pct(issuer)
    )
}

const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// `xxxxx-xxxxx`, 10 characters from a 31 character alphabet (about 49 bits).
pub fn new_recovery_code() -> String {
    let mut rng = rand::rng();
    let mut pick =
        |n: usize| -> String { (0..n).map(|_| RECOVERY_ALPHABET[rng.random_range(0..RECOVERY_ALPHABET.len())] as char).collect() };
    format!("{}-{}", pick(5), pick(5))
}

/// What is hashed and compared: lower case, no dashes or spaces.
pub fn normalize_recovery_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// True when the text has the shape of a recovery code rather than a 6 digit code.
pub fn looks_like_recovery_code(code: &str) -> bool {
    let n = normalize_recovery_code(code);
    n.len() == 10 && n.bytes().all(|b| RECOVERY_ALPHABET.contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, SHA-1, secret "12345678901234567890", 8 digits there, so we
    // compare the last 6.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc6238_vectors() {
        for (t, full) in
            [(59u64, "94287082"), (1111111109, "07081804"), (1111111111, "14050471"), (1234567890, "89005924"), (2000000000, "69279037")]
        {
            assert_eq!(code_at(RFC_SECRET, t), full[2..], "t={t}");
        }
    }

    #[test]
    fn base32_round_trip_and_known_value() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw 6ytb oi").unwrap(), b"foobar");
        assert!(base32_decode("1").is_none());
        let s = new_secret();
        assert_eq!(base32_decode(&base32_encode(&s)).unwrap(), s);
    }

    #[test]
    fn window_and_replay() {
        let t = 1_700_000_000u64;
        let code = code_at(RFC_SECRET, t);
        let step = step_for(t) as i64;
        assert_eq!(verify(RFC_SECRET, &code, t, None), Some(step));
        assert_eq!(verify(RFC_SECRET, &code, t + 30, None), Some(step), "one step late is fine");
        assert_eq!(verify(RFC_SECRET, &code, t - 30, None), Some(step), "one step early is fine");
        assert_eq!(verify(RFC_SECRET, &code, t + 90, None), None, "three steps away is not");
        assert_eq!(verify(RFC_SECRET, &code, t, Some(step)), None, "same step twice is a replay");
        assert_eq!(verify(RFC_SECRET, &code, t, Some(step - 1)), Some(step));
        assert_eq!(verify(RFC_SECRET, "000000x", t, None), None);
        assert_eq!(verify(RFC_SECRET, "12345", t, None), None);
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert_eq!(verify(RFC_SECRET, &spaced, t, None), Some(step));
    }

    #[test]
    fn uri_shape() {
        let u = otpauth_uri("Codehaven", "alice", RFC_SECRET);
        assert!(u.starts_with("otpauth://totp/Codehaven:alice?secret="));
        assert!(u.contains("&issuer=Codehaven&algorithm=SHA1&digits=6&period=30"));
        let u = otpauth_uri("My Haven", "a@b.co", RFC_SECRET);
        assert!(u.starts_with("otpauth://totp/My%20Haven:a%40b.co?"));
    }

    #[test]
    fn recovery_codes() {
        let c = new_recovery_code();
        assert_eq!(c.len(), 11);
        assert_eq!(c.as_bytes()[5], b'-');
        assert!(looks_like_recovery_code(&c));
        assert!(!looks_like_recovery_code("123456"));
        assert_eq!(normalize_recovery_code(&c.to_uppercase()), normalize_recovery_code(&c));
        assert_ne!(new_recovery_code(), new_recovery_code());
    }
}
