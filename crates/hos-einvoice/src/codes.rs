//! GİB code lists HostingOS validates against, and legal-number helpers.
//!
//! Source: `UBL-TR_Codelist.xml` / `UBL-TR_Common_Schematron.xml` of the GİB
//! e-Fatura package (<https://ebelge.gib.gov.tr/dosyalar/kilavuzlar/e-FaturaPaketi.zip>,
//! read 2026-11-18).

use std::fmt;

/// `TaxExemptionReasonCodeType` — every exemption code GİB accepts.
const EXEMPTION_CODES: &[&str] = &[
    "001", "101", "102", "103", "104", "105", "106", "107", "108", "151", "201", "202", "204",
    "205", "206", "207", "208", "209", "211", "212", "213", "214", "215", "216", "217", "218",
    "219", "220", "221", "223", "225", "226", "227", "228", "229", "230", "231", "232", "233",
    "234", "235", "236", "237", "238", "239", "240", "241", "242", "250", "301", "302", "303",
    "304", "305", "306", "307", "309", "310", "311", "312", "313", "314", "315", "316", "317",
    "318", "319", "320", "321", "322", "323", "324", "325", "326", "327", "328", "329", "330",
    "331", "332", "333", "334", "335", "336", "337", "338", "340", "341", "342", "343", "344",
    "350", "351", "501", "555", "801", "802", "803", "804", "805", "806", "807", "808", "809",
    "810", "811", "812", "701", "702", "703", "704",
];

/// `istisnaTaxExemptionReasonCodeType` — codes that make the invoice type
/// `ISTISNA` (except 555).
const ISTISNA_CODES: &[&str] = &[
    "001", "101", "102", "103", "104", "105", "106", "107", "108", "201", "202", "204", "205",
    "206", "207", "208", "209", "211", "212", "213", "214", "215", "216", "217", "218", "219",
    "220", "221", "223", "225", "226", "227", "228", "229", "230", "231", "232", "233", "234",
    "235", "236", "237", "238", "239", "240", "241", "242", "250", "301", "302", "303", "304",
    "305", "306", "307", "308", "309", "310", "311", "312", "313", "314", "315", "316", "317",
    "318", "319", "320", "321", "322", "323", "324", "325", "326", "327", "328", "329", "330",
    "331", "332", "333", "334", "335", "336", "337", "338", "339", "340", "341", "342", "343",
    "344", "350", "501",
];

/// `WithholdingTaxTypeWithPercent` — withholding code + percent pairs.
const WITHHOLDING_WITH_PERCENT: &[&str] = &[
    "60130", "60140", "60290", "60350", "60370", "60450", "60550", "60690", "60790", "60890",
    "60950", "60970", "61090", "61190", "61270", "61290", "61370", "61390", "61450", "61550",
    "61570", "61650", "61770", "61870", "61970", "62070", "62190", "62290", "62350", "62420",
    "62530", "62620", "65090", "65050", "65070", "65020", "65030", "62740", "62750", "801100",
    "802100", "803100", "804100", "805100", "806100", "807100", "808100", "809100", "810100",
    "811100", "812100", "813100", "814100", "815100", "816100", "817100", "818100", "819100",
    "820100", "821100", "822100", "823100", "824100", "825100",
];

/// What an exemption code means for the invoice type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExemptionClass {
    /// Not a GİB code.
    Unknown,
    /// Full / partial exemption: invoice type `ISTISNA`.
    Istisna,
    /// 351 / 555 etc.: allowed on a `SATIS` invoice.
    Satis,
    /// Special base (`OZELMATRAH`, 801–812) — not issued by HostingOS.
    OzelMatrah,
    /// Export-registered (`IHRACKAYITLI`, 701–704) — not issued by HostingOS.
    IhracKayitli,
}

/// Classifies an exemption code.
pub fn exemption_class(code: &str) -> ExemptionClass {
    if !EXEMPTION_CODES.contains(&code) {
        // 308 / 339 are only valid for investment-incentive invoices.
        return ExemptionClass::Unknown;
    }
    if code != "555" && ISTISNA_CODES.contains(&code) {
        return ExemptionClass::Istisna;
    }
    match code.as_bytes().first() {
        Some(b'8') => ExemptionClass::OzelMatrah,
        Some(b'7') => ExemptionClass::IhracKayitli,
        _ => ExemptionClass::Satis,
    }
}

/// Whether `code` (601…627, 801…825) with `percent` is a valid withholding.
pub fn is_valid_withholding(code: &str, percent: &str) -> bool {
    WITHHOLDING_WITH_PERCENT.contains(&format!("{code}{percent}").as_str())
}

/// A 3-character document series (`[A-Z0-9]{3}`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Series(String);

impl Series {
    /// Validates and upper-cases a series.
    pub fn parse(value: &str) -> Option<Self> {
        let v = value.trim().to_ascii_uppercase();
        (v.len() == 3
            && v.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
        .then_some(Self(v))
    }

    /// The three characters.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Series {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Formats the 16-character legal number `AAA` + year + 9-digit sequence.
/// `None` when the year or sequence does not fit.
pub fn legal_number(series: &Series, year: i32, seq: i64) -> Option<String> {
    ((2000..=2099).contains(&year) && (1..=999_999_999).contains(&seq))
        .then(|| format!("{series}{year}{seq:09}"))
}

/// Whether `number` matches GİB's `^[A-Z0-9]{3}20[0-9]{2}[0-9]{9}$`.
pub fn is_legal_number(number: &str) -> bool {
    let b = number.as_bytes();
    b.len() == 16
        && b[..3]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && &b[3..5] == b"20"
        && b[5..].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exemption_classes() {
        assert_eq!(exemption_class("301"), ExemptionClass::Istisna);
        assert_eq!(exemption_class("351"), ExemptionClass::Satis);
        assert_eq!(exemption_class("555"), ExemptionClass::Satis);
        assert_eq!(exemption_class("801"), ExemptionClass::OzelMatrah);
        assert_eq!(exemption_class("701"), ExemptionClass::IhracKayitli);
        assert_eq!(exemption_class("308"), ExemptionClass::Unknown);
        assert_eq!(exemption_class("999"), ExemptionClass::Unknown);
    }

    #[test]
    fn withholding_pairs() {
        assert!(is_valid_withholding("601", "40"));
        assert!(is_valid_withholding("624", "20"));
        assert!(!is_valid_withholding("601", "50"));
        assert!(!is_valid_withholding("999", "100"));
    }

    #[test]
    fn numbers() {
        let s = Series::parse("hos").unwrap();
        assert_eq!(legal_number(&s, 2026, 42).unwrap(), "HOS2026000000042");
        assert!(is_legal_number("HOS2026000000042"));
        assert!(!is_legal_number("HOS1926000000042"));
        assert!(!is_legal_number("HO2026000000042"));
        assert!(!is_legal_number("hos2026000000042"));
        assert!(Series::parse("AB").is_none());
        assert!(Series::parse("AB-").is_none());
        assert!(Series::parse("ÇAB").is_none());
        assert_eq!(legal_number(&s, 2026, 1_000_000_000), None);
    }
}
