//! Network status notices: e-mail and in-app notification to the clients
//! whose services run on an integration bound to a component of an incident
//! or maintenance window.

use hos_core::CoreError;
use hos_core::domain::Locale;
use hos_core::ids::{ClientId, TenantId};
use hos_db::repo::status::{
    self as srepo, IncidentId, IncidentImpact, IncidentKind, IncidentStatus,
};
use hos_db::repo::{clients, platform};
use serde_json::json;

use crate::{Billing, BillingResult};

/// Label of an incident status in `locale`.
pub fn status_label(s: IncidentStatus, locale: Locale) -> &'static str {
    match (s, locale) {
        (IncidentStatus::Investigating, Locale::Tr) => "İnceleniyor",
        (IncidentStatus::Identified, Locale::Tr) => "Sorun tespit edildi",
        (IncidentStatus::Monitoring, Locale::Tr) => "İzleniyor",
        (IncidentStatus::Resolved, Locale::Tr) => "Çözüldü",
        (IncidentStatus::Scheduled, Locale::Tr) => "Planlandı",
        (IncidentStatus::InProgress, Locale::Tr) => "Sürüyor",
        (IncidentStatus::Completed, Locale::Tr) => "Tamamlandı",
        (IncidentStatus::Investigating, Locale::En) => "Investigating",
        (IncidentStatus::Identified, Locale::En) => "Identified",
        (IncidentStatus::Monitoring, Locale::En) => "Monitoring",
        (IncidentStatus::Resolved, Locale::En) => "Resolved",
        (IncidentStatus::Scheduled, Locale::En) => "Scheduled",
        (IncidentStatus::InProgress, Locale::En) => "In progress",
        (IncidentStatus::Completed, Locale::En) => "Completed",
    }
}

/// Label of an impact in `locale`.
pub fn impact_label(i: IncidentImpact, locale: Locale) -> &'static str {
    match (i, locale) {
        (IncidentImpact::None, Locale::Tr) => "Etki yok",
        (IncidentImpact::Minor, Locale::Tr) => "Küçük",
        (IncidentImpact::Major, Locale::Tr) => "Büyük",
        (IncidentImpact::Critical, Locale::Tr) => "Kritik",
        (IncidentImpact::None, Locale::En) => "None",
        (IncidentImpact::Minor, Locale::En) => "Minor",
        (IncidentImpact::Major, Locale::En) => "Major",
        (IncidentImpact::Critical, Locale::En) => "Critical",
    }
}

impl Billing {
    /// Notifies every affected client of an incident once (later calls do
    /// nothing): in-app notification + `network_notice` e-mail with the
    /// latest update. One e-mail per address (case-insensitive: accounts that
    /// share an address get one message listing all their services). Returns
    /// the number of clients notified.
    pub async fn notify_incident(
        &self,
        tenant_id: TenantId,
        id: IncidentId,
    ) -> BillingResult<usize> {
        let mut tx = self.pool().begin().await?;
        let incident = srepo::get_incident(&mut *tx, tenant_id, id)
            .await?
            .ok_or_else(|| CoreError::not_found("incident", id))?;
        if !incident.is_public
            || !srepo::mark_notified(&mut *tx, tenant_id, id, self.clock().now()).await?
        {
            return Ok(0);
        }
        let message = srepo::incident_updates(&mut *tx, tenant_id, id)
            .await?
            .first()
            .map(|u| u.body.chars().take(2000).collect::<String>())
            .unwrap_or_default();
        let tenant = platform::tenant_context(&mut *tx, tenant_id)
            .await?
            .ok_or_else(|| CoreError::not_found("tenant", tenant_id))?;
        let base = self.config().tenant_url(Some(&tenant));
        let recipients = srepo::affected_clients(&mut *tx, tenant_id, id).await?;
        let starts = match (incident.kind, incident.scheduled_start) {
            (IncidentKind::Maintenance, Some(t)) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
            _ => String::new(),
        };
        // The e-mail list as a set of addresses (first account wins, the
        // services of every account with that address are listed).
        let mut by_address: Vec<(String, ClientId, Vec<String>)> = Vec::new();
        for (client_id, labels) in &recipients {
            let Some(client) = clients::get_client(&mut *tx, tenant_id, *client_id).await? else {
                continue;
            };
            let key = client.email.trim().to_lowercase();
            match by_address.iter_mut().find(|(k, _, _)| *k == key) {
                Some((_, _, all)) => all.extend(labels.iter().cloned()),
                None => by_address.push((key, *client_id, labels.clone())),
            }
            self.notify_client(
                &mut tx,
                tenant_id,
                *client_id,
                "network.notice",
                &incident.title,
                Some(&labels.join(", ")),
                Some("/client"),
            )
            .await?;
        }
        for (_, client_id, mut labels) in by_address {
            let Some(client) = clients::get_client(&mut *tx, tenant_id, client_id).await? else {
                continue;
            };
            labels.sort();
            labels.dedup();
            let locale = client.locale;
            let services = labels.join(", ");
            self.queue_client_email(
                &mut tx,
                tenant_id,
                client_id,
                hos_mail::defaults::keys::NETWORK_NOTICE,
                json!({
                    "incident": {
                        "title": incident.title,
                        "kind": incident.kind.as_str(),
                        "status": status_label(incident.status, locale),
                        "impact": impact_label(incident.impact, locale),
                        "starts": starts,
                        "message": message,
                        "services": services,
                    },
                    "action_url": format!("{}/status/incidents/{}", base.trim_end_matches('/'), id),
                }),
                Some(("incident", *id.as_uuid())),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(recipients.len())
    }
}
