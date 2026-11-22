//! TLD-specific requirements: registration periods, transfer rules, extra
//! registrant data, privacy availability and post-expiry timelines.
//!
//! [`builtin()`] is a core set (common gTLDs, `.tr` family, `.uk`, `.eu`, `.us`,
//! `.ca`, `.de`, `.fr`, `.nl`, `.au`, popular ccTLDs used as generics). It is
//! data, not code: copy it with [`TldRules::with_builtin`] and
//! [`TldRules::insert`] overrides (e.g. loaded from the `tlds` table as JSON —
//! [`TldRule`] is serde-compatible).
//!
//! ## Extra field keys
//!
//! Requirements are expressed as registrar-neutral keys that the storefront
//! collects and the registrar implementations translate. They are looked up in
//! `contacts.registrant.extra` first and then in `DomainRegistration::extra`;
//! the key `tax_id` refers to [`ContactInfo::tax_id`].
//!
//! | key | meaning |
//! |---|---|
//! | `tax_id` | TR: T.C. kimlik no (TCKN) for individuals, vergi no (VKN) for organisations, passport number for foreigners |
//! | `tr_document` | TR: supporting document reference (documented second levels such as `av.tr`, `bel.tr`) |
//! | `us_nexus_category` | `.us` nexus: `C11`, `C12`, `C21`, `C31`, `C32` |
//! | `us_nexus_country` | `.us` nexus country for `C31`/`C32` |
//! | `us_purpose` | `.us` application purpose: `P1`…`P5` |
//! | `eu_citizenship_country` | `.eu`: EU citizenship of a registrant living outside the EU/EEA |
//! | `uk_legal_type` | `.uk` registrant type (`IND`, `LTD`, `PLC`, …) |
//! | `uk_company_number` | `.uk` company registration number (companies) |
//! | `ca_legal_type` | `.ca` CIRA legal type (`CCT`, `CCO`, `RES`, …) |
//! | `au_eligibility_type` / `au_eligibility_id` | `.au` eligibility (ABN/ACN/TM) |
//!
//! [`ContactInfo::tax_id`]: hos_core::integrations::registrar::ContactInfo::tax_id

use std::collections::BTreeMap;
use std::sync::LazyLock;

use hos_core::IntegrationError;
use hos_core::integrations::registrar::DomainRegistration;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Input type of an [`ExtraField`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "options", rename_all = "snake_case")]
pub enum ExtraFieldKind {
    /// Free text.
    Text,
    /// One of the `(value, label)` options.
    Select(Vec<(String, String)>),
    /// Checkbox / agreement.
    Bool,
    /// A document (upload reference or document number).
    Document,
}

/// One additional datum a TLD requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraField {
    /// Key in `extra` (see the module docs).
    pub key: String,
    /// Form label (English; the web layer translates by key).
    pub label: String,
    /// Input type.
    pub kind: ExtraFieldKind,
    /// Whether the registration must be refused without it.
    pub required: bool,
    /// Help text.
    pub help: Option<String>,
}

/// Rules for one TLD (public suffix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TldRule {
    /// TLD without leading dot (`com.tr`).
    pub tld: String,
    /// Minimum registration period in years.
    pub min_years: u32,
    /// Maximum registration period in years (also the cap for renewals).
    pub max_years: u32,
    /// Incoming transfers need an EPP auth code.
    pub transfer_requires_auth_code: bool,
    /// A transfer adds one year to the expiry.
    pub transfer_adds_year: bool,
    /// A registrar WHOIS privacy / ID-protection service may be ordered.
    pub privacy_allowed: bool,
    /// The registry accepts Turkish-script IDN labels.
    pub turkish_idn: bool,
    /// Days after expiry during which a normal renewal is possible.
    pub grace_days: u32,
    /// Days of redemption (restore fee) after the grace period.
    pub redemption_days: u32,
    /// Registrant must be in one of these ISO countries (empty = anywhere).
    #[serde(default)]
    pub registrant_countries: Vec<String>,
    /// An extra key that waives [`registrant_countries`](Self::registrant_countries)
    /// when present (e.g. EU citizenship for `.eu`).
    #[serde(default)]
    pub country_waiver_key: Option<String>,
    /// Additional data to collect.
    #[serde(default)]
    pub extra_fields: Vec<ExtraField>,
    /// Free-form notes for staff.
    #[serde(default)]
    pub notes: Option<String>,
}

impl TldRule {
    /// Defaults for a generic TLD: 1–10 years, EPP transfer adding a year,
    /// privacy allowed, 30 days grace + 30 days redemption.
    pub fn generic(tld: &str) -> Self {
        Self {
            tld: tld.to_owned(),
            min_years: 1,
            max_years: 10,
            transfer_requires_auth_code: true,
            transfer_adds_year: true,
            privacy_allowed: true,
            turkish_idn: false,
            grace_days: 30,
            redemption_days: 30,
            registrant_countries: Vec::new(),
            country_waiver_key: None,
            extra_fields: Vec::new(),
            notes: None,
        }
    }

    /// Errors unless `years` is within `min_years..=max_years`.
    pub fn validate_years(&self, years: u32) -> Result<(), IntegrationError> {
        if years < self.min_years || years > self.max_years {
            return Err(IntegrationError::InvalidInput(format!(
                ".{} can be registered for {}–{} years (requested {years})",
                self.tld, self.min_years, self.max_years
            )));
        }
        Ok(())
    }

    /// Required extra fields that are missing for `request`.
    pub fn missing_fields(&self, request: &DomainRegistration) -> Vec<&ExtraField> {
        self.extra_fields
            .iter()
            .filter(|f| f.required && lookup_extra(request, &f.key).is_none())
            .collect()
    }

    /// Validates period, required extra fields and registrant country.
    pub fn validate_registration(
        &self,
        request: &DomainRegistration,
    ) -> Result<(), IntegrationError> {
        self.validate_years(request.years)?;
        let missing = self.missing_fields(request);
        if !missing.is_empty() {
            let keys: Vec<&str> = missing.iter().map(|f| f.key.as_str()).collect();
            return Err(IntegrationError::InvalidInput(format!(
                ".{} requires additional registrant data: {}",
                self.tld,
                keys.join(", ")
            )));
        }
        for f in &self.extra_fields {
            if let (ExtraFieldKind::Select(options), Some(v)) =
                (&f.kind, lookup_extra(request, &f.key))
                && !options
                    .iter()
                    .any(|(value, _)| value.eq_ignore_ascii_case(&v))
            {
                return Err(IntegrationError::InvalidInput(format!(
                    "{} has an invalid value for .{}",
                    f.key, self.tld
                )));
            }
        }
        if !self.registrant_countries.is_empty() {
            let country = request
                .contacts
                .registrant
                .address
                .country_code
                .to_ascii_uppercase();
            let waived = self
                .country_waiver_key
                .as_deref()
                .is_some_and(|k| lookup_extra(request, k).is_some());
            if !waived && !self.registrant_countries.contains(&country) {
                return Err(IntegrationError::InvalidInput(format!(
                    "the registrant of a .{} domain must be located in an eligible country",
                    self.tld
                )));
            }
        }
        Ok(())
    }
}

/// Looks up an extra key for a registration: registrant contact `extra` first,
/// then the request `extra`; `tax_id` also reads [`ContactInfo::tax_id`].
/// Non-string scalars are stringified; blanks count as missing.
///
/// [`ContactInfo::tax_id`]: hos_core::integrations::registrar::ContactInfo::tax_id
pub fn lookup_extra(request: &DomainRegistration, key: &str) -> Option<String> {
    if key == "tax_id"
        && let Some(t) = request.contacts.registrant.tax_id.as_deref()
        && !t.trim().is_empty()
    {
        return Some(t.trim().to_owned());
    }
    extra_str(&request.contacts.registrant.extra, key).or_else(|| extra_str(&request.extra, key))
}

/// Reads `value[key]` as a trimmed non-empty string (numbers/bools stringified).
pub fn extra_str(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A set of rules keyed by TLD.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TldRules {
    rules: BTreeMap<String, TldRule>,
}

impl TldRules {
    /// An empty set (every lookup falls back to [`TldRule::generic`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// A mutable copy of the built-in set.
    pub fn with_builtin() -> Self {
        builtin().clone()
    }

    /// Adds or replaces a rule.
    pub fn insert(&mut self, rule: TldRule) {
        self.rules.insert(rule.tld.to_ascii_lowercase(), rule);
    }

    /// Exact lookup (`com.tr`).
    pub fn get(&self, tld: &str) -> Option<&TldRule> {
        self.rules.get(tld.trim_start_matches('.'))
    }

    /// Effective rule: exact entry, else the entry of the closest parent suffix
    /// (`gen.tr` → `tr`) re-labelled, else [`TldRule::generic`].
    pub fn rule(&self, tld: &str) -> TldRule {
        let tld = tld.trim_start_matches('.').to_ascii_lowercase();
        if let Some(r) = self.rules.get(&tld) {
            return r.clone();
        }
        let mut parent = tld.as_str();
        while let Some((_, rest)) = parent.split_once('.') {
            if let Some(r) = self.rules.get(rest) {
                let mut r = r.clone();
                r.tld.clone_from(&tld);
                return r;
            }
            parent = rest;
        }
        TldRule::generic(&tld)
    }

    /// All explicit rules, ordered by TLD.
    pub fn iter(&self) -> impl Iterator<Item = &TldRule> {
        self.rules.values()
    }

    /// Number of explicit rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// `true` when there are no explicit rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The built-in rule set.
pub fn builtin() -> &'static TldRules {
    static RULES: LazyLock<TldRules> = LazyLock::new(build_builtin);
    &RULES
}

/// Shorthand for `builtin().rule(tld)`.
pub fn rule_for(tld: &str) -> TldRule {
    builtin().rule(tld)
}

const EU_EEA: &[&str] = &[
    "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT", "LV",
    "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE", "IS", "LI", "NO",
];

fn field(key: &str, label: &str, kind: ExtraFieldKind, required: bool, help: &str) -> ExtraField {
    ExtraField {
        key: key.to_owned(),
        label: label.to_owned(),
        kind,
        required,
        help: (!help.is_empty()).then(|| help.to_owned()),
    }
}

fn select(options: &[(&str, &str)]) -> ExtraFieldKind {
    ExtraFieldKind::Select(
        options
            .iter()
            .map(|(v, l)| ((*v).to_owned(), (*l).to_owned()))
            .collect(),
    )
}

#[allow(clippy::too_many_lines)]
fn build_builtin() -> TldRules {
    let mut set = TldRules::new();

    // ---- generic TLDs ------------------------------------------------------
    for tld in [
        "com", "net", "org", "info", "biz", "xyz", "online", "site", "store", "tech", "shop",
        "club", "pro", "name", "mobi", "cloud", "live", "space", "website", "agency",
    ] {
        let mut r = TldRule::generic(tld);
        // Verisign accepts Turkish-script IDNs for .com/.net.
        r.turkish_idn = matches!(tld, "com" | "net");
        set.insert(r);
    }
    for tld in ["app", "dev", "page"] {
        let mut r = TldRule::generic(tld);
        r.notes = Some("HSTS-preloaded TLD: sites must be served over HTTPS.".into());
        set.insert(r);
    }
    for (tld, max) in [
        ("io", 10),
        ("co", 5),
        ("me", 10),
        ("tv", 10),
        ("cc", 10),
        ("ai", 10),
    ] {
        let mut r = TldRule::generic(tld);
        r.max_years = max;
        if tld == "ai" {
            r.min_years = 2;
            r.notes = Some(".ai registrations and renewals are in 2-year terms.".into());
        }
        set.insert(r);
    }

    // ---- Türkiye (TRABIS) ---------------------------------------------------
    let tr_id = field(
        "tax_id",
        "T.C. Kimlik No / Vergi No",
        ExtraFieldKind::Text,
        true,
        "TCKN for individuals, VKN for organisations, passport number for foreign registrants.",
    );
    let tr_base = |tld: &str, documented: bool| {
        let mut r = TldRule::generic(tld);
        r.max_years = 5;
        r.privacy_allowed = false;
        r.transfer_adds_year = false;
        r.grace_days = 30;
        r.redemption_days = 0;
        r.extra_fields.push(tr_id.clone());
        if documented {
            r.extra_fields.push(field(
                "tr_document",
                "Supporting document",
                ExtraFieldKind::Document,
                true,
                "Document proving the right to this second-level domain (TRABIS documented category).",
            ));
        }
        r.notes =
            Some("Registered via TRABIS; registrant identity is verified by the registry.".into());
        r
    };
    for tld in [
        "tr", "com.tr", "net.tr", "org.tr", "biz.tr", "info.tr", "web.tr", "gen.tr", "tv.tr",
        "name.tr",
    ] {
        set.insert(tr_base(tld, false));
    }
    for tld in [
        "av.tr", "dr.tr", "bel.tr", "pol.tr", "edu.tr", "k12.tr", "kep.tr", "gov.tr", "tsk.tr",
    ] {
        set.insert(tr_base(tld, true));
    }

    // ---- United Kingdom (Nominet) --------------------------------------------
    let uk_type = field(
        "uk_legal_type",
        "Registrant type",
        select(&[
            ("IND", "UK individual"),
            ("FIND", "Non-UK individual"),
            ("LTD", "UK limited company"),
            ("PLC", "UK public limited company"),
            ("PTNR", "UK partnership"),
            ("STRA", "UK sole trader"),
            ("LLP", "UK limited liability partnership"),
            ("IP", "UK industrial/provident registered company"),
            ("SCH", "UK school"),
            ("RCHAR", "UK registered charity"),
            ("GOV", "UK government body"),
            ("CRC", "UK corporation by royal charter"),
            ("STAT", "UK statutory body"),
            ("OTHER", "UK entity that does not fit any of the above"),
            ("FCORP", "Non-UK corporation"),
            ("FOTHER", "Non-UK entity that does not fit any of the above"),
        ]),
        false,
        "",
    );
    let uk_number = field(
        "uk_company_number",
        "Company registration number",
        ExtraFieldKind::Text,
        false,
        "Required by Nominet for LTD, PLC, LLP, IP, SCH and RCHAR registrants.",
    );
    for tld in ["uk", "co.uk", "org.uk", "me.uk", "ltd.uk", "plc.uk"] {
        let mut r = TldRule::generic(tld);
        r.transfer_requires_auth_code = false;
        r.transfer_adds_year = false;
        r.privacy_allowed = false;
        r.grace_days = 90;
        r.redemption_days = 0;
        r.extra_fields = vec![uk_type.clone(), uk_number.clone()];
        r.notes = Some("Transfers are IPS tag changes (no EPP code, no year added); Nominet redacts individuals' data.".into());
        set.insert(r);
    }

    // ---- European Union (EURid) ---------------------------------------------
    let mut eu = TldRule::generic("eu");
    eu.privacy_allowed = false;
    eu.grace_days = 0;
    eu.redemption_days = 40;
    eu.registrant_countries = EU_EEA.iter().map(|c| (*c).to_owned()).collect();
    eu.country_waiver_key = Some("eu_citizenship_country".into());
    eu.extra_fields.push(field(
        "eu_citizenship_country",
        "EU citizenship (country)",
        ExtraFieldKind::Text,
        false,
        "Only for EU citizens residing outside the EU/EEA.",
    ));
    set.insert(eu);

    // ---- United States (.us nexus) ------------------------------------------
    let mut us = TldRule::generic("us");
    us.privacy_allowed = false;
    us.extra_fields = vec![
        field(
            "us_nexus_category",
            "Nexus category",
            select(&[
                ("C11", "US citizen"),
                ("C12", "Permanent resident of the US"),
                ("C21", "US organisation"),
                (
                    "C31",
                    "Foreign entity regularly engaged in lawful activities in the US",
                ),
                ("C32", "Foreign entity with an office or facility in the US"),
            ]),
            true,
            "",
        ),
        field(
            "us_nexus_country",
            "Nexus country",
            ExtraFieldKind::Text,
            false,
            "ISO country code; required for C31/C32.",
        ),
        field(
            "us_purpose",
            "Application purpose",
            select(&[
                ("P1", "Business use for profit"),
                (
                    "P2",
                    "Non-profit business, club, association, religious organisation",
                ),
                ("P3", "Personal use"),
                ("P4", "Educational purposes"),
                ("P5", "Government purposes"),
            ]),
            true,
            "",
        ),
    ];
    us.notes = Some("WHOIS privacy is prohibited for .us.".into());
    set.insert(us);

    // ---- Canada (CIRA) --------------------------------------------------------
    let mut ca = TldRule::generic("ca");
    ca.privacy_allowed = false;
    ca.grace_days = 45;
    ca.extra_fields.push(field(
        "ca_legal_type",
        "CIRA legal type",
        select(&[
            ("CCO", "Canadian corporation"),
            ("CCT", "Canadian citizen"),
            ("RES", "Permanent resident of Canada"),
            ("GOV", "Government or government entity in Canada"),
            ("EDU", "Canadian educational institution"),
            ("ASS", "Canadian unincorporated association"),
            ("HOP", "Canadian hospital"),
            ("PRT", "Partnership registered in Canada"),
            ("TDM", "Trade-mark registered in Canada"),
            ("TRD", "Canadian trade union"),
            ("PLT", "Canadian political party"),
            ("LAM", "Canadian library, archive or museum"),
            ("TRS", "Trust established in Canada"),
            ("ABO", "Aboriginal peoples"),
            ("INB", "Indian band"),
            (
                "LGR",
                "Legal representative of a Canadian citizen or permanent resident",
            ),
            ("OMK", "Official mark registered in Canada"),
            ("MAJ", "Her/His Majesty the Queen/King"),
        ]),
        true,
        "Canadian Presence Requirements apply.",
    ));
    set.insert(ca);

    // ---- Germany, France, Netherlands -----------------------------------------
    let mut de = TldRule::generic("de");
    de.max_years = 1;
    de.privacy_allowed = false;
    de.transfer_adds_year = false;
    de.grace_days = 0;
    de.redemption_days = 30;
    de.notes = Some("DENIC renews yearly; deleted names enter a 30-day redemption period.".into());
    set.insert(de);

    let mut fr = TldRule::generic("fr");
    fr.privacy_allowed = false;
    fr.registrant_countries = EU_EEA.iter().map(|c| (*c).to_owned()).collect();
    fr.redemption_days = 30;
    fr.grace_days = 0;
    set.insert(fr);

    let mut nl = TldRule::generic("nl");
    nl.max_years = 1;
    nl.privacy_allowed = false;
    nl.transfer_adds_year = false;
    nl.grace_days = 0;
    nl.redemption_days = 40;
    set.insert(nl);

    // ---- Australia --------------------------------------------------------------
    let au_fields = vec![
        field(
            "au_eligibility_type",
            "Eligibility ID type",
            select(&[
                ("ABN", "Australian Business Number"),
                ("ACN", "Australian Company Number"),
                ("TM", "Australian trademark"),
                ("OTHER", "Other"),
            ]),
            true,
            "",
        ),
        field(
            "au_eligibility_id",
            "Eligibility ID",
            ExtraFieldKind::Text,
            true,
            "",
        ),
    ];
    for tld in ["com.au", "net.au", "org.au"] {
        let mut r = TldRule::generic(tld);
        r.max_years = 5;
        r.privacy_allowed = false;
        r.transfer_adds_year = false;
        r.redemption_days = 0;
        r.extra_fields.clone_from(&au_fields);
        set.insert(r);
    }
    let mut au = TldRule::generic("au");
    au.max_years = 5;
    au.privacy_allowed = false;
    au.redemption_days = 0;
    au.notes = Some(
        "Direct .au requires an Australian presence (citizenship, residency or ABN/ACN).".into(),
    );
    set.insert(au);

    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::integrations::registrar::{ContactInfo, DomainContacts};
    use serde_json::json;

    fn reg(domain: &str, years: u32, country: &str, extra: Value) -> DomainRegistration {
        let contact = ContactInfo {
            first_name: "Ada".into(),
            address: hos_core::integrations::PostalAddress {
                country_code: country.into(),
                ..Default::default()
            },
            ..ContactInfo::default()
        };
        DomainRegistration {
            domain: domain.into(),
            years,
            contacts: DomainContacts::same(contact),
            extra,
            ..DomainRegistration::default()
        }
    }

    #[test]
    fn lookups_and_fallbacks() {
        let rules = builtin();
        assert!(rules.len() > 40);
        assert_eq!(rules.rule("com").max_years, 10);
        assert!(rules.rule("com").turkish_idn);
        assert_eq!(rules.rule("com.tr").max_years, 5);
        // Unknown .tr second level inherits the .tr rule.
        let r = rules.rule("example.tr");
        assert_eq!(r.tld, "example.tr");
        assert_eq!(r.max_years, 5);
        // Unknown TLD gets the generic rule.
        assert_eq!(rules.rule(".museum"), TldRule::generic("museum"));
        assert!(!rules.rule("co.uk").transfer_requires_auth_code);
    }

    #[test]
    fn validates_periods_and_requirements() {
        let r = rule_for("com.tr");
        assert!(r.validate_years(6).is_err());
        let missing = reg("ornek.com.tr", 1, "TR", json!({}));
        assert!(r.validate_registration(&missing).is_err());
        let mut ok = reg("ornek.com.tr", 1, "TR", json!({}));
        ok.contacts.registrant.tax_id = Some("12345678901".into());
        assert!(r.validate_registration(&ok).is_ok());

        let us = rule_for("us");
        let bad = reg(
            "x.us",
            1,
            "US",
            json!({"us_nexus_category": "C99", "us_purpose": "P1"}),
        );
        assert!(us.validate_registration(&bad).is_err());
        let good = reg(
            "x.us",
            1,
            "US",
            json!({"us_nexus_category": "c11", "us_purpose": "P1"}),
        );
        assert!(us.validate_registration(&good).is_ok());

        let eu = rule_for("eu");
        assert!(
            eu.validate_registration(&reg("x.eu", 1, "TR", json!({})))
                .is_err()
        );
        assert!(
            eu.validate_registration(&reg("x.eu", 1, "DE", json!({})))
                .is_ok()
        );
        assert!(
            eu.validate_registration(&reg(
                "x.eu",
                1,
                "TR",
                json!({"eu_citizenship_country": "DE"})
            ))
            .is_ok()
        );
        let ai = rule_for("ai");
        assert!(ai.validate_years(1).is_err());
    }

    #[test]
    fn extendable_and_serializable() {
        let mut rules = TldRules::with_builtin();
        let mut custom = TldRule::generic("com.tr");
        custom.max_years = 2;
        rules.insert(custom);
        assert_eq!(rules.rule("com.tr").max_years, 2);
        assert_eq!(builtin().rule("com.tr").max_years, 5);
        let json = serde_json::to_value(rules.rule("us")).unwrap();
        assert_eq!(json["extra_fields"][0]["kind"]["type"], "select");
        let back: TldRule = serde_json::from_value(json).unwrap();
        assert_eq!(back, rules.rule("us"));
    }
}
