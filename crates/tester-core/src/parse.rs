//! Argument parsing for console commands.
//!
//! Every parser is total and returns `Option`: a malformed argument must
//! produce an `err` reply, never a silently-different value. An instrument that
//! quietly reinterprets your input is worse than one that refuses it.

/// Parse a non-negative integer.
///
/// Accepts `0x`/`0b` prefixes and `_` separators so bus patterns can be written
/// readably, e.g. `0b1010_1010`. Rejects overflow rather than wrapping.
pub fn u32_arg(s: &str) -> Option<u32> {
    let (radix, digits) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        (2, rest)
    } else {
        (10, s)
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: u32 = 0;
    let mut any = false;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(radix)?;
        acc = acc.checked_mul(radix)?.checked_add(d)?;
        any = true;
    }
    any.then_some(acc)
}

/// Parse a frequency in Hz, accepting `k`/`M` suffixes: `1M`, `115200`, `2k5`.
///
/// Digits after the suffix are a fraction of the multiplier, the way values are
/// written on schematics: `2k5` is 2500 and `2k05` is 2050.
pub fn hz_arg(s: &str) -> Option<u32> {
    let (mult, idx) = if let Some(i) = s.find(['M', 'm']) {
        (1_000_000u32, i)
    } else if let Some(i) = s.find(['k', 'K']) {
        (1_000u32, i)
    } else {
        return u32_arg(s);
    };

    let whole = u32_arg(&s[..idx])?;
    let mut value = whole.checked_mul(mult)?;

    let frac_str = &s[idx + 1..];
    if !frac_str.is_empty() {
        // Scale by digit position so `2k05` is 2.05k rather than 2.5k.
        let frac = u32_arg(frac_str)?;
        let mut scale = mult;
        for _ in 0..frac_str.len() {
            scale /= 10;
            if scale == 0 {
                return None; // more fraction digits than the suffix can express
            }
        }
        value = value.checked_add(frac.checked_mul(scale)?)?;
    }
    Some(value)
}

/// Parse one byte of a payload byte list.
///
/// Bare tokens are hex, because payloads are written as `48 65 6c 6c 6f`. An
/// explicit `0x` prefix is accepted too, so pasting either style works.
pub fn hex_byte_arg(s: &str) -> Option<u8> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

/// Parse a channel index against the bus width.
pub fn channel_arg(s: &str, bus_width: u8) -> Option<u8> {
    let n = u32_arg(s)?;
    (n < bus_width as u32).then_some(n as u8)
}

/// Parse a channel bitmask: either `0x...`/`0b...`/decimal, or the word `all`.
pub fn mask_arg(s: &str, bus_width: u8) -> Option<u16> {
    let all: u16 = if bus_width >= 16 {
        u16::MAX
    } else {
        (1u16 << bus_width) - 1
    };
    if s.eq_ignore_ascii_case("all") {
        return Some(all);
    }
    let v = u32_arg(s)?;
    let m = u16::try_from(v).ok()?;
    // Reject bits outside the bus rather than masking them off: a mask with a
    // typo'd extra bit should be a visible error, not a quietly different test.
    (m & !all == 0).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_with_radix_prefixes_and_separators() {
        assert_eq!(u32_arg("42"), Some(42));
        assert_eq!(u32_arg("0xff"), Some(255));
        assert_eq!(u32_arg("0XFF"), Some(255));
        assert_eq!(u32_arg("0b1010_1010"), Some(0xaa));
        assert_eq!(u32_arg("1_000_000"), Some(1_000_000));
    }

    #[test]
    fn malformed_integers_are_rejected_not_coerced() {
        assert_eq!(u32_arg(""), None);
        assert_eq!(u32_arg("0x"), None);
        assert_eq!(u32_arg("0b"), None);
        assert_eq!(u32_arg("_"), None);
        assert_eq!(u32_arg("12x"), None);
        assert_eq!(u32_arg("-1"), None);
        assert_eq!(u32_arg("0b12"), None, "2 is not a binary digit");
    }

    #[test]
    fn integer_overflow_is_rejected_not_wrapped() {
        assert_eq!(u32_arg("4294967295"), Some(u32::MAX));
        assert_eq!(u32_arg("4294967296"), None);
        assert_eq!(u32_arg("0x1_0000_0000"), None);
    }

    #[test]
    fn frequency_suffixes() {
        assert_eq!(hz_arg("115200"), Some(115_200));
        assert_eq!(hz_arg("1M"), Some(1_000_000));
        assert_eq!(hz_arg("1m"), Some(1_000_000));
        assert_eq!(hz_arg("10k"), Some(10_000));
        assert_eq!(hz_arg("2k5"), Some(2_500));
        assert_eq!(hz_arg("2k05"), Some(2_050));
        assert_eq!(hz_arg("1M5"), Some(1_500_000));
        assert_eq!(hz_arg("1M234567"), Some(1_234_567));
    }

    #[test]
    fn frequency_fraction_longer_than_suffix_is_rejected() {
        // `1k2345` would need a 0.1 Hz step; say so rather than truncating.
        assert_eq!(hz_arg("1k2345"), None);
    }

    #[test]
    fn hex_bytes() {
        assert_eq!(hex_byte_arg("00"), Some(0));
        assert_eq!(hex_byte_arg("ff"), Some(255));
        assert_eq!(hex_byte_arg("4"), Some(4));
        assert_eq!(hex_byte_arg("0x4a"), Some(0x4a));
        assert_eq!(hex_byte_arg("100"), None, "wider than a byte");
        assert_eq!(hex_byte_arg("gg"), None);
        assert_eq!(hex_byte_arg(""), None);
    }

    #[test]
    fn channels_are_bounds_checked() {
        assert_eq!(channel_arg("0", 16), Some(0));
        assert_eq!(channel_arg("15", 16), Some(15));
        assert_eq!(channel_arg("16", 16), None);
        assert_eq!(channel_arg("x", 16), None);
    }

    #[test]
    fn masks_reject_bits_outside_the_bus() {
        assert_eq!(mask_arg("all", 16), Some(0xffff));
        assert_eq!(mask_arg("ALL", 16), Some(0xffff));
        assert_eq!(mask_arg("0b1010", 16), Some(0b1010));
        assert_eq!(mask_arg("0xffff", 16), Some(0xffff));
        assert_eq!(mask_arg("0x10000", 16), None);
        // A narrower bus must reject the high bits too.
        assert_eq!(mask_arg("all", 8), Some(0x00ff));
        assert_eq!(mask_arg("0x100", 8), None);
    }
}
