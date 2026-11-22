//! Turkish tax identifiers: VKN (Vergi Kimlik Numarası, 10 digits) and TCKN
//! (T.C. Kimlik Numarası, 11 digits) with their check-digit algorithms.
//!
//! The placeholders `11111111111` (consumer without TCKN) and `2222222222`
//! (buyer without a Turkish tax id) are **not** valid identifiers here: they
//! are written by HostingOS itself (see
//! [`hos_core::integrations::einvoice::CONSUMER_TCKN`]) and must never be
//! accepted from a form as a real number.

use hos_core::integrations::einvoice::{CONSUMER_TCKN, FOREIGN_VKN};

/// Digits only (spaces, dots, dashes removed). `None` when anything else is
/// in the input.
pub fn normalize(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    for c in input.trim().chars() {
        match c {
            '0'..='9' => out.push(c),
            ' ' | '.' | '-' => {}
            _ => return None,
        }
    }
    Some(out)
}

fn digits(s: &str) -> Option<Vec<u32>> {
    s.chars().map(|c| c.to_digit(10)).collect()
}

/// Whether `vkn` (digits) is a checksum-valid VKN. The placeholder
/// `2222222222` is rejected.
pub fn is_valid_vkn(vkn: &str) -> bool {
    if vkn.len() != 10 || vkn == FOREIGN_VKN {
        return false;
    }
    let Some(d) = digits(vkn) else {
        return false;
    };
    let mut sum = 0u32;
    for (i, digit) in d.iter().take(9).enumerate() {
        let tmp = (digit + 9 - i as u32) % 10;
        let v = if tmp == 0 {
            0
        } else {
            let v = (tmp * 2u32.pow(9 - i as u32)) % 9;
            if v == 0 { 9 } else { v }
        };
        sum += v;
    }
    (10 - sum % 10) % 10 == d[9]
}

/// Whether `tckn` (digits) is a checksum-valid TCKN. The placeholder
/// `11111111111` is rejected.
pub fn is_valid_tckn(tckn: &str) -> bool {
    if tckn.len() != 11 || tckn == CONSUMER_TCKN {
        return false;
    }
    let Some(d) = digits(tckn) else {
        return false;
    };
    if d[0] == 0 {
        return false;
    }
    let odd = d[0] + d[2] + d[4] + d[6] + d[8];
    let even = d[1] + d[3] + d[5] + d[7];
    let tenth = (odd * 7 + 10 * 10 - even) % 10;
    let eleventh = d[..10].iter().sum::<u32>() % 10;
    d[9] == tenth && d[10] == eleventh
}

/// A tax number field may hold a VKN (companies) or a TCKN (sole
/// proprietors use their TCKN as tax number).
pub fn is_valid_tax_number(value: &str) -> bool {
    match value.len() {
        10 => is_valid_vkn(value),
        11 => is_valid_tckn(value),
        _ => false,
    }
}

/// Check digit of the first nine VKN digits (test helper / generator).
pub fn vkn_check_digit(first9: &str) -> Option<u32> {
    if first9.len() != 9 {
        return None;
    }
    (0..10).find(|c| is_valid_vkn(&format!("{first9}{c}")))
}

/// The two TCKN check digits for the first nine digits (test helper).
pub fn tckn_check_digits(first9: &str) -> Option<String> {
    let d = digits(first9).filter(|d| d.len() == 9 && d[0] != 0)?;
    let odd = d[0] + d[2] + d[4] + d[6] + d[8];
    let even = d[1] + d[3] + d[5] + d[7];
    let tenth = (odd * 7 + 100 - even) % 10;
    let eleventh = (d.iter().sum::<u32>() + tenth) % 10;
    Some(format!("{tenth}{eleventh}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tckn_algorithm() {
        // Widely used synthetic test number.
        assert!(is_valid_tckn("10000000146"));
        assert!(!is_valid_tckn("10000000145"));
        assert!(!is_valid_tckn("00000000146"), "first digit 0");
        assert!(
            !is_valid_tckn(CONSUMER_TCKN),
            "placeholder is not a real TCKN"
        );
        assert!(!is_valid_tckn("1000000014"), "length");
        assert!(!is_valid_tckn("1000000014a"));
        for first9 in ["123456789", "987654321", "555555555", "100000001"] {
            let tail = tckn_check_digits(first9).unwrap();
            assert!(is_valid_tckn(&format!("{first9}{tail}")), "{first9}{tail}");
        }
    }

    #[test]
    fn vkn_algorithm() {
        for first9 in ["123456789", "000000001", "987654321", "470003152"] {
            let c = vkn_check_digit(first9).unwrap();
            let vkn = format!("{first9}{c}");
            assert!(is_valid_vkn(&vkn), "{vkn}");
            // Exactly one check digit is valid.
            let wrong = (c + 1) % 10;
            assert!(!is_valid_vkn(&format!("{first9}{wrong}")));
        }
        assert!(!is_valid_vkn(FOREIGN_VKN));
        assert!(!is_valid_vkn("123456789"));
        assert!(!is_valid_vkn("12345678901"));
        assert!(is_valid_tax_number("1234567890"));
        assert!(is_valid_tax_number("10000000146"));
        assert!(!is_valid_tax_number("123"));
    }

    #[test]
    fn known_vkn_vector() {
        // Worked by hand: for 123456789 every `(d + 9 - i) % 10` is 0, so the
        // sum is 0 and the check digit is 0.
        assert_eq!(vkn_check_digit("123456789"), Some(0));
        for first9 in ["470003152", "987654321", "000000001", "314159265"] {
            assert_eq!(vkn_check_digit(first9), Some(compute_reference(first9)));
        }
    }

    /// Independent, literal transcription of the GİB algorithm.
    fn compute_reference(first9: &str) -> u32 {
        let d: Vec<u32> = first9.chars().map(|c| c.to_digit(10).unwrap()).collect();
        let mut sum = 0;
        for (i, digit) in d.iter().enumerate().take(9) {
            let tmp = (digit + (9 - i as u32)) % 10;
            let mut v = 0;
            if tmp != 0 {
                v = (tmp * 2u32.pow(9 - i as u32)) % 9;
                if v == 0 {
                    v = 9;
                }
            }
            sum += v;
        }
        (10 - (sum % 10)) % 10
    }

    #[test]
    fn normalize_input() {
        assert_eq!(normalize(" 123 456-78.90 ").as_deref(), Some("1234567890"));
        assert_eq!(normalize("12a"), None);
    }
}
