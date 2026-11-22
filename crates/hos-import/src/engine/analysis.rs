//! Dry-run analysis: what the wizard's mapping step needs (gateways,
//! registrars, servers, products, staff roles found at the source) and the
//! conflicts the import would run into.

use std::collections::{BTreeMap, HashMap};

use hos_core::ids::TenantId;
use hos_db::repo::integrations;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::entity::Entity;
use crate::error::ImportResult;
use crate::map;
use crate::row::Row;

/// A payment gateway module seen at the source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleUse {
    /// Rows using it (clients, invoices, transactions, orders, services / domains).
    pub count: u64,
    /// HostingOS provider kind it corresponds to.
    pub kind: Option<String>,
    /// Existing integration of that kind (wizard default).
    pub suggested: Option<Uuid>,
}

/// A WHMCS server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub module: String,
    /// HostingOS provider kind (`None`: no counterpart → skipped by default).
    pub kind: Option<String>,
}

/// A WHMCS product.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductInfo {
    pub id: i64,
    pub name: String,
    pub group_id: i64,
    pub product_type: String,
    pub module: String,
}

/// A conflict found by the dry run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub entity: String,
    pub code: String,
    pub message: String,
}

/// A plan-limit check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitCheck {
    pub key: String,
    pub used: i64,
    pub max: Option<i64>,
    /// Source rows that would count against the limit.
    pub incoming: i64,
}

impl LimitCheck {
    /// Whether everything fits.
    pub fn fits(&self) -> bool {
        self.max.is_none_or(|m| self.used + self.incoming <= m)
    }
}

/// Dry-run findings (stored in `import_runs.preview.analysis`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Analysis {
    pub gateways: BTreeMap<String, ModuleUse>,
    pub registrars: BTreeMap<String, ModuleUse>,
    pub servers: Vec<ServerInfo>,
    pub products: Vec<ProductInfo>,
    /// WHMCS admin role id → name.
    pub roles: BTreeMap<String, String>,
    pub currencies: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub limits: Vec<LimitCheck>,
    /// HostingOS integrations the mapping step can choose from: `(id, category, kind, name)`.
    pub integrations: Vec<(Uuid, String, String, String)>,
    #[serde(skip)]
    pub(crate) emails: HashMap<String, (String, i64)>,
    #[serde(skip)]
    pub(crate) incoming: HashMap<&'static str, i64>,
}

impl Analysis {
    /// Loads the tenant's integrations (suggestions).
    pub(crate) async fn begin(&mut self, pool: &PgPool, tenant: TenantId) -> ImportResult<()> {
        self.integrations = integrations::list_integrations(pool, tenant, None)
            .await?
            .into_iter()
            .map(|i| {
                (
                    i.id.into_uuid(),
                    i.category.as_str().to_owned(),
                    i.kind,
                    i.name,
                )
            })
            .collect();
        Ok(())
    }

    fn suggest(&self, category: &str, kind: &str) -> Option<Uuid> {
        self.integrations
            .iter()
            .find(|(_, c, k, _)| c == category && k == kind)
            .map(|(id, _, _, _)| *id)
    }

    fn gateway(&mut self, module: &str) {
        let module = module.trim();
        if module.is_empty() {
            return;
        }
        let kind = map::gateway_kind(module);
        let suggested = self.suggest("gateway", &kind);
        let e = self
            .gateways
            .entry(module.to_owned())
            .or_insert_with(|| ModuleUse {
                count: 0,
                kind: Some(kind),
                suggested,
            });
        e.count += 1;
    }

    fn registrar(&mut self, module: &str) {
        let module = module.trim();
        if module.is_empty() {
            return;
        }
        let kind = map::registrar_kind(module).map(str::to_owned);
        let suggested = kind.as_deref().and_then(|k| self.suggest("registrar", k));
        let e = self
            .registrars
            .entry(module.to_owned())
            .or_insert_with(|| ModuleUse {
                count: 0,
                kind,
                suggested,
            });
        e.count += 1;
    }

    fn email(&mut self, entity: &str, row: &Row) {
        if let Some(e) = map::email(&row.text("email")) {
            if let Some((other_entity, other_id)) = self.emails.get(&e) {
                self.conflicts.push(Conflict {
                    entity: entity.to_owned(),
                    code: "duplicate_email".into(),
                    message: format!(
                        "{e}: {entity} #{} and {other_entity} #{other_id} share the address (one login is created)",
                        row.id()
                    ),
                });
            } else {
                self.emails.insert(e, (entity.to_owned(), row.id()));
            }
        }
    }

    fn incoming(&mut self, key: &'static str) {
        *self.incoming.entry(key).or_default() += 1;
    }

    /// Looks at a source row.
    pub(crate) fn observe(&mut self, entity: Entity, row: &Row) {
        match entity {
            Entity::Currencies => {
                if let Some(c) = row.str("code") {
                    self.currencies.push(c.to_ascii_uppercase());
                }
            }
            Entity::Servers => {
                let module = row.text("type");
                self.servers.push(ServerInfo {
                    id: row.id(),
                    name: crate::sanitize::clean_line(&row.text("name"), 120),
                    host: crate::sanitize::clean_line(
                        &row.str("hostname").unwrap_or_else(|| row.text("ipaddress")),
                        253,
                    ),
                    kind: map::server_kind(&module).map(|(_, k)| k.to_owned()),
                    module,
                });
                self.incoming("integrations");
            }
            Entity::Staff => {
                if let (Some(id), Some(name)) = (row.int("roleid"), row.str("role_name")) {
                    self.roles
                        .insert(id.to_string(), crate::sanitize::clean_line(&name, 120));
                } else if let Some(id) = row.int("roleid") {
                    self.roles
                        .entry(id.to_string())
                        .or_insert_with(|| format!("Role {id}"));
                }
                if !row.flag("disabled") {
                    self.incoming("max_staff");
                }
                self.email("staff", row);
            }
            Entity::Clients => {
                self.gateway(&row.text("defaultgateway"));
                if map::client_status(&row.text("status")) != "closed" {
                    self.incoming("max_clients");
                }
            }
            Entity::Users => self.email("users", row),
            Entity::Products => self.products.push(ProductInfo {
                id: row.id(),
                name: crate::sanitize::clean_line(&row.text("name"), 120),
                group_id: row.int("gid").unwrap_or(0),
                product_type: row.text("type"),
                module: row.text("servertype"),
            }),
            Entity::Tlds => self.registrar(&row.text("autoreg")),
            Entity::Invoices | Entity::Orders => self.gateway(&row.text("paymentmethod")),
            Entity::Services => {
                self.gateway(&row.text("paymentmethod"));
                if matches!(
                    map::service_status(&row.text("domainstatus")),
                    Some("pending" | "active" | "suspended")
                ) {
                    self.incoming("max_services");
                }
            }
            Entity::Transactions => self.gateway(&row.text("gateway")),
            Entity::Domains => {
                self.gateway(&row.text("paymentmethod"));
                self.registrar(&row.text("registrar"));
            }
            _ => {}
        }
    }
}
