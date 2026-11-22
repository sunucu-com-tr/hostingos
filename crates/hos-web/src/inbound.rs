//! Inbound e-mail parsing for ticket piping (pure functions, no I/O):
//! the RFC 5322 / MIME message (via `mail-parser`) reduced to what a
//! ticket needs, automated-mail detection, reply tokens in plus addresses
//! and the Mailgun webhook signature. The HTTP side lives in
//! [`crate::routes::inbound_mail`].

use mail_parser::{MessageParser, MessageRef, PartKind};

/// Most attachments taken from one message (the rest are listed only).
pub const MAX_ATTACHMENTS: usize = 10;
/// Largest attachment taken.
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// Longest ticket body kept (characters).
pub const MAX_BODY_CHARS: usize = 20_000;
/// Longest subject kept (characters).
pub const MAX_SUBJECT_CHARS: usize = 200;
/// Mailgun signatures older / newer than this are refused (replay window).
pub const MAILGUN_WINDOW_SECS: i64 = 15 * 60;
/// Prefix of a reply token in the plus part of an address (`support+t…@`).
pub const TOKEN_PREFIX: char = 't';
/// Random bytes of a reply token (hex: 40 characters, lowercase so that
/// mail systems that lowercase addresses keep it intact).
const TOKEN_BYTES: usize = 20;

/// An attachment of a parsed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// What ticket piping needs from a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedMail {
    /// Message-ID without angle brackets.
    pub message_id: Option<String>,
    /// `From` address, lowercased.
    pub from_email: Option<String>,
    /// `From` display name.
    pub from_name: Option<String>,
    /// Subject (trimmed, capped).
    pub subject: String,
    /// Plain-text body without the quoted reply, capped.
    pub body: String,
    /// `To`, `Cc`, `Delivered-To`, `X-Original-To` addresses, lowercased.
    pub recipients: Vec<String>,
    /// Why the message looks automated (auto-reply, bounce, list, loop).
    pub automated: Option<&'static str>,
    /// Attachments within the limits.
    pub attachments: Vec<ParsedAttachment>,
    /// Names of every attachment (also those over the limits).
    pub attachment_names: Vec<String>,
    /// `Authentication-Results` header values, topmost first.
    pub auth_results: Vec<String>,
    /// `X-Mailgun-Spf` header values, topmost first.
    pub mailgun_spf: Vec<String>,
}

/// How the sender of a message is authenticated (tenant settings,
/// migration 0051). Only a passing result *for the From domain* makes the
/// sender a known client; everything else is an unverified sender.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SenderCheck {
    /// Nothing is trusted.
    #[default]
    None,
    /// The topmost `Authentication-Results` header with this authserv-id
    /// (the tenant's receiving MTA, which must remove forged copies of its
    /// own header, RFC 8601 §5); headers of any other authserv-id are
    /// ignored. Passing: `dmarc=pass` for the From domain, or `dkim=pass` /
    /// `spf=pass` for a domain aligned with it.
    AuthResults(String),
    /// Mailgun inbound route post. Mailgun's documentation (Receiving
    /// Messages via HTTP, Spam Filter) says: the webhook signature covers
    /// only `timestamp` + `token`, not the message; with the domain's spam
    /// filter set to "mark spam with MIME headers" Mailgun inserts
    /// `X-Mailgun-Spf` (`Pass`, `Neutral`, `Fail`, `SoftFail`) and
    /// `X-Mailgun-Dkim-Check-Result` (`Pass` / `Fail`, no signing domain
    /// given, so it cannot be tied to the From domain and is not used); the
    /// `sender` field is the SMTP `MAIL FROM`. Passing here: exactly one
    /// `X-Mailgun-Spf` header (a second one means the sender added its
    /// own), `Pass`, for an envelope sender aligned with the From domain.
    MailgunSpf {
        /// The post's `sender` field.
        envelope_sender: Option<String>,
    },
}

/// The domain of an address (lowercased), if any.
fn domain_of(address: &str) -> Option<String> {
    let address = address.trim().trim_matches(['<', '>']);
    let (_, domain) = address.rsplit_once('@').unwrap_or(("", address));
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    (domain.contains('.') && !domain.is_empty()).then_some(domain)
}

/// Relaxed alignment: the same domain, or one a subdomain of the other.
fn aligned(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

/// `Authentication-Results` value without comments: `(authserv-id,
/// [(method, result, [(property, value)])])`.
#[allow(clippy::type_complexity)]
fn parse_auth_results(
    value: &str,
) -> Option<(String, Vec<(String, String, Vec<(String, String)>)>)> {
    let mut text = String::with_capacity(value.len());
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => text.push(if c.is_whitespace() { ' ' } else { c }),
            _ => {}
        }
    }
    let mut parts = text.split(';');
    let authserv = parts
        .next()?
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();
    let mut results = Vec::new();
    for part in parts {
        let mut tokens = part.split_whitespace();
        let Some((method, result)) = tokens.next().and_then(|t| t.split_once('=')) else {
            continue;
        };
        let method = method
            .split('/')
            .next()
            .unwrap_or(method)
            .to_ascii_lowercase();
        let props = tokens
            .filter_map(|t| t.split_once('='))
            .map(|(k, v)| {
                (
                    k.to_ascii_lowercase(),
                    v.trim_matches('"').to_ascii_lowercase(),
                )
            })
            .collect();
        results.push((method, result.to_ascii_lowercase(), props));
    }
    Some((authserv, results))
}

/// The method by which sender authentication passed for the From domain
/// of `mail` (`dmarc`, `dkim`, `spf`), or `None`: the sender is not
/// verified.
pub fn authenticate_sender(mail: &ParsedMail, check: &SenderCheck) -> Option<&'static str> {
    let from_domain = domain_of(mail.from_email.as_deref()?)?;
    match check {
        SenderCheck::None => None,
        SenderCheck::AuthResults(id) => {
            let id = id.trim().to_ascii_lowercase();
            let (_, results) = mail
                .auth_results
                .iter()
                .filter_map(|v| parse_auth_results(v))
                .find(|(authserv, _)| *authserv == id)?;
            let prop = |props: &[(String, String)], key: &str| {
                props.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
            };
            for (method, result, props) in &results {
                if result != "pass" {
                    continue;
                }
                match method.as_str() {
                    "dmarc"
                        if prop(props, "header.from").as_deref() == Some(from_domain.as_str()) =>
                    {
                        return Some("dmarc");
                    }
                    "dkim" => {
                        let d = prop(props, "header.d")
                            .or_else(|| prop(props, "header.i").and_then(|i| domain_of(&i)));
                        if d.is_some_and(|d| aligned(&d, &from_domain)) {
                            return Some("dkim");
                        }
                    }
                    "spf" => {
                        let d = prop(props, "smtp.mailfrom").and_then(|m| domain_of(&m));
                        if d.is_some_and(|d| aligned(&d, &from_domain)) {
                            return Some("spf");
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        SenderCheck::MailgunSpf { envelope_sender } => {
            let [only] = mail.mailgun_spf.as_slice() else {
                return None;
            };
            let envelope = domain_of(envelope_sender.as_deref()?)?;
            (only.trim().eq_ignore_ascii_case("pass") && aligned(&envelope, &from_domain))
                .then_some("spf")
        }
    }
}

fn parser() -> MessageParser {
    MessageParser::new()
        .max_depth(8)
        .max_parts(100)
        .max_encoded_nesting(2)
}

fn header_text(m: &MessageRef<'_>, name: &str) -> Option<String> {
    m.headers()
        .get(name)
        .map(|h| String::from_utf8_lossy(h.raw_value()).trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Lowercased bare address of `<a@b>` / `a@b` text (`None` without `@`).
pub fn bare_address(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => &s[a + 1..b],
        _ => s,
    };
    let s = s.trim().to_ascii_lowercase();
    let (local, domain) = s.split_once('@')?;
    if local.is_empty()
        || domain.is_empty()
        || s.len() > 254
        || s.chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';'))
    {
        return None;
    }
    Some(s)
}

/// Why a message looks automated: auto-replies, bounces, mailing lists,
/// bulk mail and our own loop marker. `None`: written by a person.
fn automated(m: &MessageRef<'_>, from: Option<&str>) -> Option<&'static str> {
    if header_text(m, "Auto-Submitted").is_some_and(|v| !v.eq_ignore_ascii_case("no")) {
        return Some("auto_submitted");
    }
    if header_text(m, "X-Autoreply").is_some()
        || header_text(m, "X-Autorespond").is_some()
        || header_text(m, "X-Autoresponder").is_some()
    {
        return Some("auto_reply");
    }
    if header_text(m, "Precedence").is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "bulk" | "junk" | "list" | "auto_reply"
        )
    }) {
        return Some("bulk");
    }
    if header_text(m, "List-Id").is_some() || header_text(m, "List-Unsubscribe").is_some() {
        return Some("mailing_list");
    }
    if header_text(m, "X-Loop").is_some_and(|v| v.to_ascii_lowercase().contains("hostingos")) {
        return Some("loop");
    }
    if m.root_part().is_content_type("multipart", "report") {
        return Some("bounce");
    }
    if header_text(m, "Return-Path").is_some_and(|v| v.replace(' ', "") == "<>") {
        return Some("bounce");
    }
    if let Some(local) = from.and_then(|f| f.split('@').next()) {
        let local = local.split('+').next().unwrap_or(local);
        if matches!(local, "mailer-daemon" | "postmaster") {
            return Some("bounce");
        }
        if matches!(
            local,
            "noreply" | "no-reply" | "donotreply" | "do-not-reply"
        ) {
            return Some("no_reply_sender");
        }
    }
    None
}

/// Cuts the quoted previous message: from an "On … wrote:" / "… yazdı:"
/// line, an Outlook "-----Original Message-----" / "From:" block, or a
/// trailing run of `>` lines.
pub fn strip_quoted(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let lower = t.to_lowercase();
        let attribution = (lower.ends_with("wrote:") || lower.ends_with("yazdı:"))
            && lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .is_none_or(|l| l.trim_start().starts_with('>'));
        let outlook = t.starts_with("-----Original Message-----")
            || t.starts_with("-----Orijinal İleti-----")
            || t.starts_with("________________________________");
        if attribution || outlook {
            end = i;
            break;
        }
    }
    // A trailing block of quoted lines.
    while end > 0 {
        let t = lines[end - 1].trim();
        if t.is_empty() || t.starts_with('>') {
            end -= 1;
        } else {
            break;
        }
    }
    lines[..end].join("\n").trim().to_owned()
}

/// Plain text as ticket Markdown: line breaks kept (hard breaks).
pub fn text_to_markdown(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut out = String::with_capacity(text.len() + lines.len() * 2);
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        let next_text = lines.get(i + 1).is_some_and(|n| !n.is_empty());
        if !line.is_empty() && next_text {
            out.push_str("  ");
        }
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

fn cap(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Parses a raw RFC 5322 message. `None`: not a message.
pub fn parse(raw: &[u8]) -> Option<ParsedMail> {
    let message = parser().parse(raw)?;
    let root = message.root();
    if !root.headers().has_known() {
        return None;
    }
    let from = root.from().and_then(|l| l.first());
    let from_email = from.and_then(|m| m.address()).and_then(bare_address);
    let from_name = from
        .and_then(|m| m.name())
        .map(|n| cap(n.trim(), 120))
        .filter(|n| !n.is_empty());
    let mut recipients: Vec<String> = Vec::new();
    for list in root.all_to().chain(root.all_cc()) {
        for m in list.mailboxes() {
            if let Some(a) = m.address().and_then(bare_address) {
                recipients.push(a);
            }
        }
    }
    for name in ["Delivered-To", "X-Original-To", "Envelope-To"] {
        for h in root.headers().all(name) {
            if let Some(a) = bare_address(&String::from_utf8_lossy(h.raw_value())) {
                recipients.push(a);
            }
        }
    }
    // A set (addresses are lower-cased by `bare_address`), first one first.
    let mut seen = std::collections::HashSet::new();
    recipients.retain(|a| seen.insert(a.clone()));
    let text: Vec<String> = root.text_bodies().map(|t| t.into_owned()).collect();
    let body = cap(
        &strip_quoted(&text.join("\n\n").replace('\r', "")),
        MAX_BODY_CHARS,
    );
    let mut attachments = Vec::new();
    let mut attachment_names = Vec::new();
    for part in root.attachments() {
        let name = part
            .attachment_name()
            .map(|n| cap(n.trim(), 150))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| match part.kind() {
                PartKind::Message(_) => "message.eml".to_owned(),
                _ => "attachment.bin".to_owned(),
            });
        attachment_names.push(name.clone());
        if matches!(part.kind(), PartKind::Message(_) | PartKind::Multipart) {
            continue;
        }
        if attachments.len() >= MAX_ATTACHMENTS || part.decoded_len() > MAX_ATTACHMENT_BYTES {
            continue;
        }
        let bytes = part.decoded().into_owned();
        if bytes.is_empty() || bytes.len() > MAX_ATTACHMENT_BYTES {
            continue;
        }
        attachments.push(ParsedAttachment {
            filename: name,
            bytes,
        });
    }
    let header_values = |name: &str| -> Vec<String> {
        root.headers()
            .all(name)
            .map(|h| cap(String::from_utf8_lossy(h.raw_value()).trim(), 2000))
            .take(20)
            .collect()
    };
    let auth_results = header_values("Authentication-Results");
    let mailgun_spf = header_values("X-Mailgun-Spf");
    Some(ParsedMail {
        auth_results,
        mailgun_spf,
        message_id: root
            .message_id()
            .map(|id| cap(id.trim().trim_matches(['<', '>']), 250))
            .filter(|id| !id.is_empty()),
        automated: automated(&root, from_email.as_deref()),
        from_email,
        from_name,
        subject: cap(root.subject().unwrap_or_default().trim(), MAX_SUBJECT_CHARS),
        body,
        recipients,
        attachments,
        attachment_names,
    })
}

/// A new reply token (40 lowercase hex characters).
pub fn new_reply_token() -> String {
    hos_core::crypto::random_bytes::<TOKEN_BYTES>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `local+t<token>@domain` for a base address `local@domain` (an existing
/// plus part is replaced). `None`: not an address.
pub fn reply_address(base: &str, token: &str) -> Option<String> {
    let base = bare_address(base)?;
    let (local, domain) = base.split_once('@')?;
    let user = local.split('+').next().unwrap_or(local);
    Some(format!("{user}+{TOKEN_PREFIX}{token}@{domain}"))
}

/// The reply token in the plus part of `address`, if well-formed.
pub fn reply_token(address: &str) -> Option<String> {
    let address = bare_address(address)?;
    let (local, _) = address.split_once('@')?;
    let (_, detail) = local.split_once('+')?;
    let token = detail.strip_prefix(TOKEN_PREFIX)?;
    (token.len() == TOKEN_BYTES * 2 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

/// The address without its plus part (`support+x@d` → `support@d`).
pub fn without_detail(address: &str) -> Option<String> {
    let address = bare_address(address)?;
    let (local, domain) = address.split_once('@')?;
    let user = local.split('+').next().unwrap_or(local);
    Some(format!("{user}@{domain}"))
}

/// A ticket number (`TCK-123456`) in a subject.
pub fn ticket_number_in(subject: &str) -> Option<String> {
    let upper = subject.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut from = 0;
    while let Some(i) = upper[from..].find("TCK-") {
        let start = from + i;
        let digits = &bytes[start + 4..];
        let n = digits.iter().take_while(|b| b.is_ascii_digit()).count();
        let boundary_before = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        if n == 6 && boundary_before {
            return Some(upper[start..start + 10].to_owned());
        }
        from = start + 4;
    }
    None
}

/// Checks a Mailgun webhook signature: HMAC-SHA256 of `timestamp + token`
/// with the signing key, hex, compared in constant time; the timestamp
/// must be within [`MAILGUN_WINDOW_SECS`] of `now`.
pub fn verify_mailgun(
    signing_key: &str,
    timestamp: &str,
    token: &str,
    signature: &str,
    now: i64,
) -> Result<(), &'static str> {
    let ts: i64 = timestamp.trim().parse().map_err(|_| "bad timestamp")?;
    if (now - ts).abs() > MAILGUN_WINDOW_SECS {
        return Err("stale timestamp");
    }
    if token.len() < 16 || token.len() > 200 {
        return Err("bad token");
    }
    let message = format!("{}{token}", timestamp.trim());
    if hos_core::crypto::verify_hmac_sha256_hex(
        signing_key.as_bytes(),
        message.as_bytes(),
        signature.trim(),
    ) {
        Ok(())
    } else {
        Err("bad signature")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = "From: \"Ayşe Yılmaz\" <Ayse@Example.COM>\r\n\
To: Destek <destek+t0123456789abcdef0123456789abcdef01234567@example.net>\r\n\
Subject: Re: [TCK-123456] Sunucu yavaş\r\n\
Message-ID: <abc-1@mail.example.com>\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Merhaba,\r\n\
sorun devam ediyor.\r\n\
\r\n\
On Mon, 2 Nov 2026 at 10:00, Destek <destek@example.net> wrote:\r\n\
> Önceki yanıt\r\n\
> ikinci satır\r\n";

    #[test]
    fn parses_a_plain_reply() {
        let m = parse(PLAIN.as_bytes()).expect("parsed");
        assert_eq!(m.message_id.as_deref(), Some("abc-1@mail.example.com"));
        assert_eq!(m.from_email.as_deref(), Some("ayse@example.com"));
        assert_eq!(m.from_name.as_deref(), Some("Ayşe Yılmaz"));
        assert_eq!(m.subject, "Re: [TCK-123456] Sunucu yavaş");
        assert_eq!(m.body, "Merhaba,\nsorun devam ediyor.");
        assert_eq!(m.automated, None);
        assert_eq!(
            m.recipients,
            vec!["destek+t0123456789abcdef0123456789abcdef01234567@example.net".to_owned()]
        );
        assert_eq!(
            reply_token(&m.recipients[0]).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(ticket_number_in(&m.subject).as_deref(), Some("TCK-123456"));
    }

    #[test]
    fn parses_multipart_with_attachments_and_legacy_charset() {
        let raw = "From: musteri@example.com\r\n\
To: destek@example.net\r\n\
Subject: =?ISO-8859-9?Q?=DEikayet_=FDlet?=\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: multipart/alternative; boundary=\"b2\"\r\n\
\r\n\
--b2\r\n\
Content-Type: text/plain; charset=windows-1254\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
=DDyi g=FCnler\r\n\
--b2\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>İyi günler</p>\r\n\
--b2--\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"fatura.pdf\"\r\n\
Content-Disposition: attachment; filename=\"fatura.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--b1--\r\n";
        let m = parse(raw.as_bytes()).expect("parsed");
        assert_eq!(m.subject, "Şikayet ılet");
        assert_eq!(m.body, "İyi günler");
        assert_eq!(m.attachment_names, vec!["fatura.pdf".to_owned()]);
        assert_eq!(m.attachments.len(), 1);
        assert!(m.attachments[0].bytes.starts_with(b"%PDF-1.4"));
        assert_eq!(m.message_id, None);
    }

    #[test]
    fn html_only_bodies_become_text() {
        let raw = "From: a@example.com\r\nSubject: x\r\nContent-Type: text/html\r\n\r\n<p>Bir</p><p>&lt;iki&gt;</p>\r\n";
        let m = parse(raw.as_bytes()).expect("parsed");
        assert!(
            m.body.contains("Bir") && m.body.contains("<iki>"),
            "{:?}",
            m.body
        );
        assert!(!m.body.contains("<p>"));
    }

    #[test]
    fn detects_automated_mail() {
        let base = "From: x@example.com\r\nSubject: s\r\n";
        for (header, reason) in [
            ("Auto-Submitted: auto-replied\r\n", "auto_submitted"),
            ("X-Autoreply: yes\r\n", "auto_reply"),
            ("Precedence: bulk\r\n", "bulk"),
            ("List-Id: <liste.example.com>\r\n", "mailing_list"),
            ("X-Loop: hostingos\r\n", "loop"),
            ("Return-Path: <>\r\n", "bounce"),
            (
                "Content-Type: multipart/report; report-type=delivery-status; boundary=z\r\n",
                "bounce",
            ),
        ] {
            let raw = format!("{base}{header}\r\nbody\r\n");
            assert_eq!(
                parse(raw.as_bytes()).unwrap().automated,
                Some(reason),
                "{header}"
            );
        }
        let raw = "From: MAILER-DAEMON@example.com\r\nSubject: s\r\n\r\nx\r\n";
        assert_eq!(parse(raw.as_bytes()).unwrap().automated, Some("bounce"));
        let raw = "From: no-reply@example.com\r\nSubject: s\r\n\r\nx\r\n";
        assert_eq!(
            parse(raw.as_bytes()).unwrap().automated,
            Some("no_reply_sender")
        );
        let raw = "From: a@example.com\r\nAuto-Submitted: no\r\nSubject: s\r\n\r\nx\r\n";
        assert_eq!(parse(raw.as_bytes()).unwrap().automated, None);
    }

    #[test]
    fn rejects_non_messages_and_limits_attachments() {
        assert!(parse(b"").is_none());
        assert!(parse(b"just some text without headers").is_none());
        let mut raw = String::from(
            "From: a@example.com\r\nSubject: many\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nx\r\n",
        );
        for i in 0..(MAX_ATTACHMENTS + 3) {
            raw.push_str(&format!(
                "--b\r\nContent-Type: text/plain; name=\"f{i}.txt\"\r\nContent-Disposition: attachment; filename=\"f{i}.txt\"\r\n\r\nveri {i}\r\n"
            ));
        }
        raw.push_str("--b--\r\n");
        let m = parse(raw.as_bytes()).unwrap();
        assert_eq!(m.attachment_names.len(), MAX_ATTACHMENTS + 3);
        assert_eq!(m.attachments.len(), MAX_ATTACHMENTS);
    }

    #[test]
    fn quoted_text_is_cut() {
        assert_eq!(
            strip_quoted("Tamam.\n\n2 Kas 2026 Pzt, Destek yazdı:\n> eski"),
            "Tamam."
        );
        assert_eq!(
            strip_quoted("Evet\n-----Original Message-----\nFrom: x"),
            "Evet"
        );
        assert_eq!(strip_quoted("Cevap\n> a\n> b\n"), "Cevap");
        assert_eq!(strip_quoted("> alıntı\nyanıtım"), "> alıntı\nyanıtım");
        assert_eq!(text_to_markdown("a\nb\n\nc"), "a  \nb\n\nc");
    }

    #[test]
    fn reply_tokens_and_addresses() {
        let t = new_reply_token();
        assert_eq!(t.len(), 40);
        assert_ne!(t, new_reply_token());
        let a = reply_address("Destek <destek+eski@example.net>", &t).unwrap();
        assert_eq!(a, format!("destek+t{t}@example.net"));
        assert_eq!(reply_token(&a).as_deref(), Some(t.as_str()));
        assert_eq!(reply_token(&a.to_uppercase()).as_deref(), Some(t.as_str()));
        assert_eq!(reply_token("destek+tabc@example.net"), None);
        assert_eq!(reply_token("destek@example.net"), None);
        assert_eq!(without_detail(&a).as_deref(), Some("destek@example.net"));
        assert_eq!(bare_address("kötü adres"), None);
        assert_eq!(ticket_number_in("Re: XTCK-123456"), None);
        assert_eq!(
            ticket_number_in("Re: tck-654321 ok").as_deref(),
            Some("TCK-654321")
        );
        assert_eq!(ticket_number_in("TCK-12345"), None);
    }

    #[test]
    fn mailgun_signature() {
        let key = "key-0123456789abcdef";
        let ts = "1700000000";
        let token = "e0b5477167110d68991efc6b9f89f0a11066af27834600e123";
        let sig =
            hos_core::crypto::hmac_sha256_hex(key.as_bytes(), format!("{ts}{token}").as_bytes());
        assert_eq!(verify_mailgun(key, ts, token, &sig, 1_700_000_100), Ok(()));
        assert_eq!(
            verify_mailgun(
                key,
                ts,
                token,
                &sig,
                1_700_000_000 + MAILGUN_WINDOW_SECS + 1
            ),
            Err("stale timestamp")
        );
        assert_eq!(
            verify_mailgun("other", ts, token, &sig, 1_700_000_100),
            Err("bad signature")
        );
        assert_eq!(
            verify_mailgun(key, ts, "x", &sig, 1_700_000_100),
            Err("bad token")
        );
        let mut tampered = sig.clone();
        tampered.replace_range(0..1, if sig.starts_with('0') { "1" } else { "0" });
        assert_eq!(
            verify_mailgun(key, ts, token, &tampered, 1_700_000_100),
            Err("bad signature")
        );
        assert_eq!(
            verify_mailgun(key, "abc", token, &sig, 1_700_000_100),
            Err("bad timestamp")
        );
    }
}
