//! Small pure helpers: ID grouping, PIN generation, time formatting.

use std::time::{SystemTime, UNIX_EPOCH};

/// Keep only ASCII digits.
pub fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Group a device id in threes from the left: "309731599215" -> "309 731 599 215".
pub fn group_id(s: &str) -> String {
    let d = digits(s);
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "just now", "5 minutes ago", "3 hours ago", "2 days ago".
pub fn ago(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    let plural = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match d {
        0..=59 => "just now".into(),
        60..=3599 => plural(d / 60, "minute"),
        3600..=86_399 => plural(d / 3600, "hour"),
        86_400..=2_591_999 => plural(d / 86_400, "day"),
        _ => plural(d / 2_592_000, "month"),
    }
}

/// Local wall-clock "HH:MM" (via the Win32 local time on Windows).
pub fn local_hhmm() -> String {
    #[cfg(windows)]
    {
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        format!("{:02}:{:02}", t.wHour, t.wMinute)
    }
    #[cfg(not(windows))]
    {
        let s = now_secs();
        format!("{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60)
    }
}

/// A random PIN from an unambiguous alphabet (no 0/O/1/l/I).
pub fn generate_pin(len: usize) -> String {
    const ALPHA: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut seed = rand_seed();
    (0..len)
        .map(|_| {
            // xorshift64*
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            let r = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
            ALPHA[(r >> 33) as usize % ALPHA.len()] as char
        })
        .collect()
}

fn rand_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the OS RNG per process.
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish() | 1
}

/// Validate a host PIN chosen by the user.
pub fn check_pin(pin: &str) -> Result<(), &'static str> {
    if pin.chars().count() < 6 {
        return Err("Use at least 6 characters.");
    }
    if pin.chars().any(char::is_whitespace) {
        return Err("Spaces are not allowed.");
    }
    if pin.len() > 64 {
        return Err("Use at most 64 characters.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_ids() {
        assert_eq!(group_id("309731599215"), "309 731 599 215");
        assert_eq!(group_id("309 73"), "309 73");
        assert_eq!(group_id("12a3-4"), "123 4");
        assert_eq!(group_id(""), "");
        assert_eq!(group_id("1234567890123"), "123 456 789 012 3");
    }

    #[test]
    fn ago_buckets() {
        assert_eq!(ago(100, 110), "just now");
        assert_eq!(ago(0, 60), "1 minute ago");
        assert_eq!(ago(0, 7200), "2 hours ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
    }

    #[test]
    fn pins() {
        let p = generate_pin(8);
        assert_eq!(p.len(), 8);
        assert!(check_pin(&p).is_ok());
        assert!(check_pin("abc").is_err());
        assert!(check_pin("abc def").is_err());
    }
}
