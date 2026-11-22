//! Encryption conventions (AES-256-GCM `SecretBox`, key `HOS_SECRET_KEY`) for
//! the secrets the billing engine stores outside `integrations`.
//!
//! Every ciphertext is bound to its row with an AAD; callers that write these
//! columns themselves (e.g. `hos-web` storing an EPP code in `cart_items`) must
//! use the same helper:
//!
//! | Column | AAD |
//! |---|---|
//! | `services.password_enc` | [`service_password_aad`] |
//! | `cart_items / order_items / domains.epp_code_enc` | [`epp_code_aad`] (by domain name, so it survives the copies) |
//! | `payment_methods.token_enc` | [`payment_method_aad`] |
//! | `webhook_endpoints.secret_enc` | [`webhook_endpoint_aad`] |
//! | `email_log.sealed_body` | [`email_body_aad`] |

use hos_core::CoreError;
use hos_core::crypto::SecretBox;
use hos_core::ids::{ServiceId, TenantId};
use hos_core::secret::SecretString;
use uuid::Uuid;

// The `hf:` AAD labels below predate the HostingOS name and are kept on purpose:
// stored ciphertext only decrypts with the exact AAD it was sealed with.
fn aad(label: &[u8], tenant_id: TenantId, tail: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(label.len() + 16 + tail.len());
    out.extend_from_slice(label);
    out.extend_from_slice(tenant_id.as_uuid().as_bytes());
    out.extend_from_slice(tail);
    out
}

/// AAD of `services.password_enc`.
pub fn service_password_aad(tenant_id: TenantId, service_id: ServiceId) -> Vec<u8> {
    aad(
        b"hf:service-password:",
        tenant_id,
        service_id.as_uuid().as_bytes(),
    )
}

/// AAD of an EPP / transfer auth code (`cart_items`, `order_items`, `domains`).
/// Keyed by the lower-cased domain name.
pub fn epp_code_aad(tenant_id: TenantId, domain_name: &str) -> Vec<u8> {
    aad(
        b"hf:epp:",
        tenant_id,
        domain_name.trim().to_ascii_lowercase().as_bytes(),
    )
}

/// AAD of `payment_methods.token_enc`.
pub fn payment_method_aad(tenant_id: TenantId, payment_method_id: Uuid) -> Vec<u8> {
    aad(
        b"hf:payment-method:",
        tenant_id,
        payment_method_id.as_bytes(),
    )
}

/// AAD of `webhook_endpoints.secret_enc`.
pub fn webhook_endpoint_aad(tenant_id: TenantId) -> Vec<u8> {
    aad(b"hf:webhook-endpoint:", tenant_id, &[])
}

/// AAD of `mail_inbound_settings.mailgun_key_enc` (Mailgun webhook signing
/// key of the inbound e-mail route).
pub fn mail_inbound_aad(tenant_id: TenantId) -> Vec<u8> {
    aad(b"hf:mail-inbound-mailgun:", tenant_id, &[])
}

/// AAD of `email_log.sealed_body` (unredacted message of an e-mail with
/// one-time links).
pub fn email_body_aad(tenant_id: TenantId, email_log_id: Uuid) -> Vec<u8> {
    aad(b"hf:email-body:", tenant_id, email_log_id.as_bytes())
}

/// Encrypts a UTF-8 secret.
pub fn seal(secret_box: &SecretBox, value: &str, aad: &[u8]) -> Result<Vec<u8>, CoreError> {
    Ok(secret_box.encrypt(value.as_bytes(), aad)?)
}

/// Decrypts a secret produced by [`seal`].
pub fn open(secret_box: &SecretBox, sealed: &[u8], aad: &[u8]) -> Result<SecretString, CoreError> {
    let plain = secret_box.decrypt(sealed, aad)?;
    String::from_utf8(plain)
        .map(SecretString::new)
        .map_err(|_| CoreError::Internal("decrypted secret is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_binding() {
        let b = SecretBox::new(&[7u8; 32]);
        let t = TenantId::new();
        let s = ServiceId::new();
        let sealed = seal(&b, "hunter22", &service_password_aad(t, s)).unwrap();
        assert_eq!(
            open(&b, &sealed, &service_password_aad(t, s))
                .unwrap()
                .expose_str(),
            "hunter22"
        );
        assert!(open(&b, &sealed, &service_password_aad(t, ServiceId::new())).is_err());
        assert_eq!(
            epp_code_aad(t, "Example.COM "),
            epp_code_aad(t, "example.com")
        );
    }
}
