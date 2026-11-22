//! Steps after the entities: the dry-run conclusions, and for an import the
//! invoice sequence, the cut-over grace markers and the audit entry.

use std::collections::BTreeMap;

use chrono::Days;
use hos_db::enums::ActorKind;
use hos_db::repo::audit::{self, AuditEntry};
use hos_db::repo::import as repo;
use hos_db::repo::saas;
use serde_json::{Value as Json, json};

use super::Engine;
use super::analysis::{Analysis, Conflict, LimitCheck};
use crate::config::ServerTarget;
use crate::error::ImportResult;
use crate::source::Source;

impl Engine<'_> {
    pub(super) async fn finish_analysis(
        &mut self,
        _source: &mut dyn Source,
    ) -> ImportResult<Analysis> {
        let mut a = std::mem::take(&mut self.analysis);
        for (module, used) in &a.gateways {
            if !self.maps.gateways.contains_key(module) {
                a.conflicts.push(Conflict {
                    entity: "gateways".into(),
                    code: "gateway_unmapped".into(),
                    message: format!(
                        "payment gateway {module:?} ({} rows) is not mapped: records keep the module name but no integration",
                        used.count
                    ),
                });
            }
        }
        for (module, used) in &a.registrars {
            if !self.maps.registrars.contains_key(module) {
                a.conflicts.push(Conflict {
                    entity: "registrars".into(),
                    code: "registrar_unmapped".into(),
                    message: format!("registrar {module:?} ({} rows) is not mapped: domains get no registrar integration", used.count),
                });
            }
        }
        for s in &a.servers {
            if self.maps.server(s.id, s.kind.as_deref()) == ServerTarget::Skip {
                a.conflicts.push(Conflict {
                    entity: "servers".into(),
                    code: "server_type_unknown".into(),
                    message: format!(
                        "server {:?} (module {:?}) has no HostingOS provider and is skipped",
                        s.name, s.module
                    ),
                });
            }
        }
        for c in &a.currencies {
            if c.len() != 3 || !c.bytes().all(|b| b.is_ascii_uppercase()) {
                a.conflicts.push(Conflict {
                    entity: "currencies".into(),
                    code: "currency_invalid".into(),
                    message: format!("currency code {c:?} is not ISO 4217; rows in it fall back to the default currency"),
                });
            }
        }
        let unmapped_products = a
            .products
            .iter()
            .filter(|p| self.maps.product(p.id).is_none())
            .count();
        if unmapped_products > 0 {
            a.conflicts.push(Conflict {
                entity: "products".into(),
                code: "products_created".into(),
                message: format!("{unmapped_products} products are not mapped to existing HostingOS products and will be created"),
            });
        }
        for key in ["max_clients", "max_services", "max_staff", "integrations"] {
            let s = saas::plan_limit(self.pool, self.tenant, key).await?;
            let check = LimitCheck {
                key: key.into(),
                used: s.used,
                max: s.max,
                incoming: a.incoming.get(key).copied().unwrap_or(0),
            };
            if !check.fits() {
                a.conflicts.push(Conflict {
                    entity: key.into(),
                    code: "plan_limit".into(),
                    message: format!(
                        "plan limit {key}: {} in use + {} incoming > {}; rows beyond the limit are skipped",
                        check.used,
                        check.incoming,
                        check.max.unwrap_or_default()
                    ),
                });
            }
            a.limits.push(check);
        }
        for c in a.conflicts.clone() {
            self.note(&c.entity, "conflict", None, &c.code, c.message);
        }
        Ok(a)
    }

    pub(super) async fn post_import(&mut self) -> ImportResult<BTreeMap<String, Json>> {
        let mut post = BTreeMap::new();
        // Invoice numbers continue after the highest imported one.
        let max = match self.max_invoice_seq {
            Some(m) => Some(m),
            None => repo::max_invoice_seq(self.pool, self.tenant).await?,
        };
        if let Some(m) = max {
            let next = repo::bump_invoice_sequence(self.pool, self.tenant, m).await?;
            post.insert("invoice_sequence_next".into(), json!(next));
        }
        // Grace: reminders / domain notices that would fall into the grace
        // window are marked as already sent.
        if self.opts.grace_days > 0 && !self.progress.steps.contains("cutover") {
            let settings =
                hos_billing::settings::BillingSettings::load(self.pool, self.tenant).await?;
            let mut keys: Vec<String> = Vec::new();
            for (id, due) in
                repo::imported_open_invoices(self.pool, self.tenant, &self.source_key).await?
            {
                for d in 0..=u64::from(self.opts.grace_days) {
                    let day = self.today + Days::new(d);
                    if let Some(kind) = hos_billing::automation::reminder_for(due, day, &settings) {
                        keys.push(match kind {
                            hos_billing::automation::ReminderKind::BeforeDue { days } => {
                                format!("reminder:{id}:before:{days}")
                            }
                            hos_billing::automation::ReminderKind::Overdue { notice, .. } => {
                                format!("reminder:{id}:overdue:{notice}")
                            }
                        });
                    }
                }
            }
            for (id, expires) in
                repo::imported_domain_expiries(self.pool, self.tenant, &self.source_key).await?
            {
                let exp = expires.date_naive();
                for d in 0..=u64::from(self.opts.grace_days) {
                    let day = self.today + Days::new(d);
                    if let Some(left) = hos_billing::automation::expiry_notice(
                        exp,
                        day,
                        &settings.domain_expiry_notice_days,
                    ) {
                        keys.push(format!("domain_notice:{id}:{exp}:{left}"));
                    }
                }
            }
            let mut added = 0;
            for chunk in keys.chunks(1000) {
                added += repo::add_automation_marks(self.pool, self.tenant, chunk).await?;
            }
            post.insert("automation_marks".into(), json!(added));
            self.progress.steps.insert("cutover".into());
        }
        post.insert(
            "automation_paused".into(),
            json!(self.opts.pause_automation),
        );
        post.insert("grace_days".into(), json!(self.opts.grace_days));
        let entry = AuditEntry::new(Some(self.tenant), ActorKind::System, None, "import.completed")
            .metadata(json!({
                "run_id": self.run_id,
                "source": self.source_key,
                "started_by": self.started_by,
                "entities": self.progress.entities.iter()
                    .map(|(k, e)| (k.clone(), json!({"created": e.created, "updated": e.updated, "adopted": e.adopted, "skipped": e.skipped, "errors": e.errors})))
                    .collect::<serde_json::Map<String, Json>>(),
            }));
        audit::write(self.pool, &entry).await?;
        Ok(post)
    }
}
