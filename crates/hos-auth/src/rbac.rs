//! Role-based access control for tenant staff, plus the request [`Principal`].
//!
//! * [`Permission`] — the catalog of `area.action` permissions. Roles store
//!   **grants** (`role_permissions.permission` rows): an exact permission, an
//!   area wildcard (`billing.*`) or the global wildcard (`*`).
//! * [`PermissionSet::allows`] — `*` allows everything, `area.*` everything in
//!   the area, and `area.manage` implies `area.view`.
//! * [`default_roles`] — owner, admin, billing, support, sales.
//! * [`Principal`] — who is making the request; `hos-web` builds it from the
//!   session row (+ role grants) and puts it in request extensions.
//! * Impersonation — staff with `clients.manage` can act as a client
//!   ([`start_impersonation`]); the resulting principal remembers the staff
//!   user and must not change credentials ([`Principal::may_change_credentials`]).

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use hos_core::CoreError;
use hos_core::ids::{ClientId, PlatformAdminId, TenantId, UserId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A permission from the catalog (`area.action`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Permission(&'static str);

impl Permission {
    /// View clients.
    pub const CLIENTS_VIEW: Self = Self("clients.view");
    /// Create/edit/delete clients, impersonate client users.
    pub const CLIENTS_MANAGE: Self = Self("clients.manage");
    /// View invoices, transactions.
    pub const BILLING_VIEW: Self = Self("billing.view");
    /// Create/edit invoices, refunds, credits, gateways settings in billing.
    pub const BILLING_MANAGE: Self = Self("billing.manage");
    /// Manage products, pricing, coupons.
    pub const PRODUCTS_MANAGE: Self = Self("products.manage");
    /// View services.
    pub const SERVICES_VIEW: Self = Self("services.view");
    /// Provision/suspend/terminate services.
    pub const SERVICES_MANAGE: Self = Self("services.manage");
    /// Manage domains and DNS.
    pub const DOMAINS_MANAGE: Self = Self("domains.manage");
    /// View tickets.
    pub const SUPPORT_VIEW: Self = Self("support.view");
    /// Reply/assign/close tickets, manage KB and announcements.
    pub const SUPPORT_MANAGE: Self = Self("support.manage");
    /// Manage CMS pages and blog.
    pub const CONTENT_MANAGE: Self = Self("content.manage");
    /// Manage SEO settings, audits and AI content.
    pub const SEO_MANAGE: Self = Self("seo.manage");
    /// Configure integrations (virt, panels, registrars, gateways, mail, AI).
    pub const INTEGRATIONS_MANAGE: Self = Self("integrations.manage");
    /// Tenant settings (branding, taxes, email templates...).
    pub const SETTINGS_MANAGE: Self = Self("settings.manage");
    /// Server operations of a single-company installation: software
    /// updates, maintenance mode, server and job status (`/admin/server`).
    /// Only `*` (the owner role) holds it: it is not in [`Permission::ALL`],
    /// so it cannot be put into another role.
    pub const SYSTEM_MANAGE: Self = Self("system.manage");
    /// Invite/edit staff and roles.
    pub const STAFF_MANAGE: Self = Self("staff.manage");
    /// View reports.
    pub const REPORTS_VIEW: Self = Self("reports.view");
    /// Import data from another billing system (WHMCS): bulk-writes clients,
    /// services, invoices … and can roll an import back. Owner (`*`) only by
    /// default; not part of the built-in `admin` role.
    pub const IMPORT_MANAGE: Self = Self("import.manage");
    /// View quotes.
    pub const QUOTES_VIEW: Self = Self("quotes.view");
    /// Create, send, convert and cancel quotes.
    pub const QUOTES_MANAGE: Self = Self("quotes.manage");
    /// View affiliates, referrals, commissions and payouts.
    pub const AFFILIATES_VIEW: Self = Self("affiliates.view");
    /// Activate / suspend affiliates, commission rules, approve payouts.
    pub const AFFILIATES_MANAGE: Self = Self("affiliates.manage");
    /// Network status page: components, incidents, maintenance notices.
    pub const STATUS_MANAGE: Self = Self("status.manage");
    /// Staff to-do list (create, assign, complete tasks).
    pub const TASKS_MANAGE: Self = Self("tasks.manage");
    /// Order fraud rules, banned e-mails / domains / IPs, review queue.
    pub const FRAUD_MANAGE: Self = Self("fraud.manage");

    /// Every permission of the catalog.
    pub const ALL: &'static [Permission] = &[
        Self::CLIENTS_VIEW,
        Self::CLIENTS_MANAGE,
        Self::BILLING_VIEW,
        Self::BILLING_MANAGE,
        Self::PRODUCTS_MANAGE,
        Self::SERVICES_VIEW,
        Self::SERVICES_MANAGE,
        Self::DOMAINS_MANAGE,
        Self::SUPPORT_VIEW,
        Self::SUPPORT_MANAGE,
        Self::CONTENT_MANAGE,
        Self::SEO_MANAGE,
        Self::INTEGRATIONS_MANAGE,
        Self::SETTINGS_MANAGE,
        Self::STAFF_MANAGE,
        Self::REPORTS_VIEW,
        Self::IMPORT_MANAGE,
        Self::QUOTES_VIEW,
        Self::QUOTES_MANAGE,
        Self::AFFILIATES_VIEW,
        Self::AFFILIATES_MANAGE,
        Self::STATUS_MANAGE,
        Self::TASKS_MANAGE,
        Self::FRAUD_MANAGE,
    ];

    /// The `area.action` string.
    pub const fn as_str(&self) -> &'static str {
        self.0
    }

    /// The area (`clients` for `clients.view`).
    pub fn area(&self) -> &'static str {
        self.0.split_once('.').map_or(self.0, |(a, _)| a)
    }

    /// The action (`view` for `clients.view`).
    pub fn action(&self) -> &'static str {
        self.0.split_once('.').map_or("", |(_, b)| b)
    }

    /// Looks up a catalog permission by string.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.0 == s.trim())
    }

    /// All distinct areas of the catalog.
    pub fn areas() -> Vec<&'static str> {
        let mut areas: Vec<&'static str> = Self::ALL.iter().map(Permission::area).collect();
        areas.dedup();
        areas
    }
}

impl fmt::Debug for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Permission({})", self.0)
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Serialize for Permission {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

/// A grant string that is neither `*`, a known `area.*` nor a catalog permission.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission grant: {0:?}")]
pub struct InvalidGrant(pub String);

/// Validates and normalizes a grant (`*`, `area.*` or `area.action`).
pub fn parse_grant(grant: &str) -> Result<String, InvalidGrant> {
    let g = grant.trim();
    if g == "*" || Permission::parse(g).is_some() {
        return Ok(g.to_owned());
    }
    if let Some(area) = g.strip_suffix(".*")
        && Permission::areas().contains(&area)
    {
        return Ok(g.to_owned());
    }
    Err(InvalidGrant(g.chars().take(64).collect()))
}

/// The effective grants of a staff user (union of their roles' grants).
///
/// Serializes as a sorted list of grant strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSet {
    grants: BTreeSet<String>,
}

impl PermissionSet {
    /// Empty set (denies everything).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a set from stored grants, **dropping** unknown ones (deny by
    /// default; a warning is logged). Use [`PermissionSet::try_from_grants`] to
    /// validate admin input.
    pub fn from_grants<I, S>(grants: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = Self::default();
        for g in grants {
            match parse_grant(g.as_ref()) {
                Ok(valid) => {
                    set.grants.insert(valid);
                }
                Err(e) => tracing::warn!(error = %e, "ignoring unknown permission grant"),
            }
        }
        set
    }

    /// Like [`from_grants`](Self::from_grants) but fails on the first unknown grant.
    pub fn try_from_grants<I, S>(grants: I) -> Result<Self, InvalidGrant>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let grants = grants
            .into_iter()
            .map(|g| parse_grant(g.as_ref()))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self { grants })
    }

    /// Every permission allowed (`*`).
    pub fn all() -> Self {
        Self::from_grants(["*"])
    }

    /// Whether `permission` is allowed.
    pub fn allows(&self, permission: Permission) -> bool {
        let area = permission.area();
        let direct = |p: &str| {
            self.grants.contains("*")
                || self.grants.contains(p)
                || self.grants.contains(&format!("{area}.*"))
        };
        if direct(permission.as_str()) {
            return true;
        }
        // `area.manage` implies `area.view`.
        permission.action() == "view" && direct(&format!("{area}.manage"))
    }

    /// Whether all of `permissions` are allowed.
    pub fn allows_all(&self, permissions: &[Permission]) -> bool {
        permissions.iter().all(|p| self.allows(*p))
    }

    /// Whether any of `permissions` is allowed.
    pub fn allows_any(&self, permissions: &[Permission]) -> bool {
        permissions.iter().any(|p| self.allows(*p))
    }

    /// Union with another set (multiple roles).
    pub fn extend(&mut self, other: &PermissionSet) {
        self.grants.extend(other.grants.iter().cloned());
    }

    /// The normalized grants.
    pub fn grants(&self) -> impl Iterator<Item = &str> {
        self.grants.iter().map(String::as_str)
    }

    /// Catalog permissions this set allows (expanded wildcards), for UIs.
    pub fn effective(&self) -> Vec<Permission> {
        Permission::ALL
            .iter()
            .copied()
            .filter(|p| self.allows(*p))
            .collect()
    }

    /// `true` when nothing is granted.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

impl Serialize for PermissionSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.grants.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PermissionSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let grants = Vec::<String>::deserialize(deserializer)?;
        Ok(Self::from_grants(grants))
    }
}

// ---------------------------------------------------------------------------
// Default roles
// ---------------------------------------------------------------------------

/// A built-in role template (seeded per tenant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleTemplate {
    /// Stable key (`owner`, `admin`, ...).
    pub key: &'static str,
    /// Turkish display name.
    pub name_tr: &'static str,
    /// English display name.
    pub name_en: &'static str,
    /// Grants.
    pub grants: &'static [&'static str],
    /// The owner role: cannot be deleted/edited; at least one owner must remain.
    pub is_owner: bool,
}

impl RoleTemplate {
    /// Grants as a [`PermissionSet`].
    pub fn permissions(&self) -> PermissionSet {
        PermissionSet::from_grants(self.grants.iter().copied())
    }
}

/// Built-in roles. `owner` has `*` (including permissions added later);
/// `admin` gets every current permission explicitly, so new sensitive
/// permissions are opt-in for admins.
pub fn default_roles() -> Vec<RoleTemplate> {
    vec![
        RoleTemplate {
            key: "owner",
            name_tr: "Sahip",
            name_en: "Owner",
            grants: &["*"],
            is_owner: true,
        },
        RoleTemplate {
            key: "admin",
            name_tr: "Yönetici",
            name_en: "Administrator",
            grants: &[
                "clients.*",
                "billing.*",
                "products.manage",
                "services.*",
                "domains.manage",
                "support.*",
                "content.manage",
                "seo.manage",
                "integrations.manage",
                "settings.manage",
                "staff.manage",
                "reports.view",
                "quotes.*",
                "affiliates.*",
                "status.manage",
                "tasks.manage",
                "fraud.manage",
            ],
            is_owner: false,
        },
        RoleTemplate {
            key: "billing",
            name_tr: "Muhasebe",
            name_en: "Billing",
            grants: &[
                "clients.view",
                "billing.*",
                "services.view",
                "reports.view",
                "quotes.view",
                "affiliates.*",
                "fraud.manage",
                "tasks.manage",
            ],
            is_owner: false,
        },
        RoleTemplate {
            key: "support",
            name_tr: "Destek",
            name_en: "Support",
            grants: &[
                "clients.view",
                "services.view",
                "support.*",
                "billing.view",
                "domains.manage",
                "status.manage",
                "tasks.manage",
            ],
            is_owner: false,
        },
        RoleTemplate {
            key: "sales",
            name_tr: "Satış",
            name_en: "Sales",
            grants: &[
                "clients.manage",
                "billing.view",
                "services.view",
                "support.view",
                "reports.view",
                "quotes.*",
                "affiliates.view",
                "tasks.manage",
            ],
            is_owner: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------

/// A staff user acting as a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Impersonation {
    /// The staff user behind the session (audit log actor).
    pub staff_user_id: UserId,
    /// When impersonation started.
    pub started_at: DateTime<Utc>,
}

/// Who is making the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Principal {
    /// A platform operator (`/platform`). Has no tenant permissions.
    Platform {
        /// `platform_admins.id`.
        admin_id: PlatformAdminId,
    },
    /// Tenant staff (`/admin`).
    Staff {
        /// `users.id`.
        user_id: UserId,
        /// Tenant the user belongs to.
        tenant_id: TenantId,
        /// Effective permissions (union of roles).
        permissions: PermissionSet,
    },
    /// A client user (`/client`), possibly impersonated by staff.
    Client {
        /// `users.id`.
        user_id: UserId,
        /// The client (billing account) currently acted for.
        client_id: ClientId,
        /// Tenant.
        tenant_id: TenantId,
        /// Set when a staff member is impersonating this client.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        impersonator: Option<Impersonation>,
    },
    /// Not logged in.
    Anonymous,
}

impl Principal {
    /// Not [`Principal::Anonymous`].
    pub fn is_authenticated(&self) -> bool {
        !matches!(self, Self::Anonymous)
    }

    /// Tenant of a staff/client principal.
    pub fn tenant_id(&self) -> Option<TenantId> {
        match self {
            Self::Staff { tenant_id, .. } | Self::Client { tenant_id, .. } => Some(*tenant_id),
            Self::Platform { .. } | Self::Anonymous => None,
        }
    }

    /// `users.id` of a staff/client principal (the *client* user while impersonating).
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            Self::Staff { user_id, .. } | Self::Client { user_id, .. } => Some(*user_id),
            Self::Platform { .. } | Self::Anonymous => None,
        }
    }

    /// The user to record as actor in `audit_log` (the staff user while impersonating).
    pub fn audit_actor(&self) -> Option<UserId> {
        match self {
            Self::Client {
                impersonator: Some(imp),
                ..
            } => Some(imp.staff_user_id),
            other => other.user_id(),
        }
    }

    /// Staff permissions, if staff.
    pub fn permissions(&self) -> Option<&PermissionSet> {
        match self {
            Self::Staff { permissions, .. } => Some(permissions),
            _ => None,
        }
    }

    /// Whether this principal is staff and allowed `permission`.
    pub fn can(&self, permission: Permission) -> bool {
        self.permissions().is_some_and(|p| p.allows(permission))
    }

    /// `Ok` when allowed; `Unauthorized` for anonymous, `Forbidden` otherwise.
    pub fn require(&self, permission: Permission) -> Result<(), CoreError> {
        match self {
            Self::Anonymous => Err(CoreError::Unauthorized),
            _ if self.can(permission) => Ok(()),
            _ => Err(CoreError::Forbidden(format!(
                "missing permission {permission}"
            ))),
        }
    }

    /// Ensures the principal belongs to `tenant` (a session row from another
    /// tenant must never be honoured on this host).
    pub fn require_tenant(&self, tenant: TenantId) -> Result<(), CoreError> {
        match self.tenant_id() {
            Some(t) if t == tenant => Ok(()),
            _ if !self.is_authenticated() => Err(CoreError::Unauthorized),
            _ => Err(CoreError::Forbidden("wrong tenant".into())),
        }
    }

    /// Staff of `tenant`, else an error.
    pub fn require_staff(&self, tenant: TenantId) -> Result<UserId, CoreError> {
        self.require_tenant(tenant)?;
        match self {
            Self::Staff { user_id, .. } => Ok(*user_id),
            _ => Err(CoreError::Forbidden("staff only".into())),
        }
    }

    /// Client of `tenant`, else an error. Returns `(user_id, client_id)`.
    pub fn require_client(&self, tenant: TenantId) -> Result<(UserId, ClientId), CoreError> {
        self.require_tenant(tenant)?;
        match self {
            Self::Client {
                user_id, client_id, ..
            } => Ok((*user_id, *client_id)),
            _ => Err(CoreError::Forbidden("client area only".into())),
        }
    }

    /// Whether a client principal acts for `client_id`.
    pub fn acts_for_client(&self, client_id: ClientId) -> bool {
        matches!(self, Self::Client { client_id: c, .. } if *c == client_id)
    }

    /// Platform operator?
    pub fn is_platform_admin(&self) -> bool {
        matches!(self, Self::Platform { .. })
    }

    /// Whether a staff member is impersonating.
    pub fn is_impersonated(&self) -> bool {
        self.impersonator().is_some()
    }

    /// The impersonation record, if any.
    pub fn impersonator(&self) -> Option<&Impersonation> {
        match self {
            Self::Client { impersonator, .. } => impersonator.as_ref(),
            _ => None,
        }
    }

    /// Password, email, 2FA, OAuth links and API keys must not be changed
    /// while impersonating (the staff member is not the account holder).
    pub fn may_change_credentials(&self) -> bool {
        self.is_authenticated() && !self.is_impersonated() && !self.is_platform_admin()
    }
}

/// Starts impersonating client user `target_user` of client `target_client`.
///
/// Requires a staff principal of the same tenant with `clients.manage`.
/// `hos-web` then creates a **separate** session row (flagged with the staff
/// user) and writes an `audit_log` entry; ending impersonation deletes it and
/// returns to the staff session.
pub fn start_impersonation(
    staff: &Principal,
    tenant: TenantId,
    target_user: UserId,
    target_client: ClientId,
    now: DateTime<Utc>,
) -> Result<Principal, CoreError> {
    let staff_user_id = staff.require_staff(tenant)?;
    staff.require(Permission::CLIENTS_MANAGE)?;
    if staff_user_id == target_user {
        return Err(CoreError::Validation("cannot impersonate yourself".into()));
    }
    Ok(Principal::Client {
        user_id: target_user,
        client_id: target_client,
        tenant_id: tenant,
        impersonator: Some(Impersonation {
            staff_user_id,
            started_at: now,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog() {
        assert_eq!(Permission::ALL.len(), 24);
        assert_eq!(
            Permission::parse("billing.view"),
            Some(Permission::BILLING_VIEW)
        );
        assert_eq!(Permission::parse("billing.delete"), None);
        assert_eq!(Permission::CLIENTS_MANAGE.area(), "clients");
        assert_eq!(Permission::CLIENTS_MANAGE.action(), "manage");
        assert_eq!(Permission::areas().len(), 18);
        assert_eq!(
            serde_json::to_string(&Permission::SEO_MANAGE).unwrap(),
            "\"seo.manage\""
        );
    }

    #[test]
    fn grants_and_wildcards() {
        assert!(parse_grant("*").is_ok());
        assert!(parse_grant("billing.*").is_ok());
        assert!(parse_grant(" clients.view ").is_ok());
        assert!(parse_grant("bogus.*").is_err());
        assert!(parse_grant("billing.destroy").is_err());
        assert!(parse_grant("").is_err());

        let all = PermissionSet::all();
        assert!(Permission::ALL.iter().all(|p| all.allows(*p)));

        let billing = PermissionSet::from_grants(["billing.*", "clients.view", "nonsense"]);
        assert!(billing.allows(Permission::BILLING_MANAGE));
        assert!(billing.allows(Permission::BILLING_VIEW));
        assert!(billing.allows(Permission::CLIENTS_VIEW));
        assert!(!billing.allows(Permission::CLIENTS_MANAGE));
        assert_eq!(billing.grants().count(), 2);
        assert!(PermissionSet::try_from_grants(["billing.*", "nonsense"]).is_err());

        // manage implies view, not the other way round.
        let support = PermissionSet::from_grants(["support.manage"]);
        assert!(support.allows(Permission::SUPPORT_VIEW));
        let viewer = PermissionSet::from_grants(["support.view"]);
        assert!(!viewer.allows(Permission::SUPPORT_MANAGE));
        // services.manage does not leak into other areas.
        assert!(!PermissionSet::from_grants(["services.manage"]).allows(Permission::CLIENTS_VIEW));

        assert!(PermissionSet::empty().effective().is_empty());
        assert!(billing.allows_all(&[Permission::BILLING_VIEW, Permission::CLIENTS_VIEW]));
        assert!(!billing.allows_all(&[Permission::BILLING_VIEW, Permission::STAFF_MANAGE]));
        assert!(billing.allows_any(&[Permission::STAFF_MANAGE, Permission::BILLING_VIEW]));

        let mut merged = viewer.clone();
        merged.extend(&billing);
        assert!(
            merged.allows(Permission::SUPPORT_VIEW) && merged.allows(Permission::BILLING_MANAGE)
        );

        let json = serde_json::to_value(&billing).unwrap();
        assert_eq!(json, serde_json::json!(["billing.*", "clients.view"]));
        let back: PermissionSet = serde_json::from_value(json).unwrap();
        assert_eq!(back, billing);
    }

    #[test]
    fn default_roles_are_valid() {
        let roles = default_roles();
        let keys: Vec<_> = roles.iter().map(|r| r.key).collect();
        assert_eq!(keys, ["owner", "admin", "billing", "support", "sales"]);
        for role in &roles {
            assert!(
                PermissionSet::try_from_grants(role.grants.iter().copied()).is_ok(),
                "{}",
                role.key
            );
        }
        let admin = roles[1].permissions();
        // Everything except the WHMCS import (owner only).
        assert_eq!(admin.effective().len(), Permission::ALL.len() - 1);
        assert!(!admin.allows(Permission::IMPORT_MANAGE));
        assert!(roles[0].permissions().allows(Permission::IMPORT_MANAGE));
        let sales = roles[4].permissions();
        assert!(sales.allows(Permission::CLIENTS_VIEW));
        assert!(!sales.allows(Permission::BILLING_MANAGE));
        assert!(roles.iter().filter(|r| r.is_owner).count() == 1);
    }

    #[test]
    fn principal_checks() {
        let tenant = TenantId::new();
        let other_tenant = TenantId::new();
        let staff = Principal::Staff {
            user_id: UserId::new(),
            tenant_id: tenant,
            permissions: PermissionSet::from_grants(["clients.manage"]),
        };
        assert!(staff.require(Permission::CLIENTS_VIEW).is_ok());
        assert_eq!(
            staff
                .require(Permission::BILLING_VIEW)
                .unwrap_err()
                .http_status(),
            403
        );
        assert_eq!(
            Principal::Anonymous
                .require(Permission::CLIENTS_VIEW)
                .unwrap_err()
                .http_status(),
            401
        );
        assert!(staff.require_tenant(tenant).is_ok());
        assert_eq!(
            staff
                .require_tenant(other_tenant)
                .unwrap_err()
                .http_status(),
            403
        );
        assert_eq!(
            Principal::Anonymous
                .require_tenant(tenant)
                .unwrap_err()
                .http_status(),
            401
        );
        assert!(staff.require_client(tenant).is_err());

        let platform = Principal::Platform {
            admin_id: PlatformAdminId::new(),
        };
        assert!(!platform.can(Permission::CLIENTS_VIEW));
        assert!(platform.is_platform_admin());
        assert!(platform.tenant_id().is_none());

        let json = serde_json::to_value(&staff).unwrap();
        assert_eq!(json["type"], "staff");
        let back: Principal = serde_json::from_value(json).unwrap();
        assert_eq!(back, staff);
    }

    #[test]
    fn impersonation() {
        let tenant = TenantId::new();
        let staff_id = UserId::new();
        let staff = Principal::Staff {
            user_id: staff_id,
            tenant_id: tenant,
            permissions: PermissionSet::from_grants(["clients.manage"]),
        };
        let client_user = UserId::new();
        let client = ClientId::new();
        let now = Utc::now();
        let p = start_impersonation(&staff, tenant, client_user, client, now).unwrap();
        assert!(p.is_impersonated());
        assert_eq!(p.user_id(), Some(client_user));
        assert_eq!(p.audit_actor(), Some(staff_id));
        assert!(!p.may_change_credentials());
        assert!(p.acts_for_client(client));
        assert_eq!(p.require_client(tenant).unwrap(), (client_user, client));
        assert!(!p.can(Permission::CLIENTS_VIEW));

        // Not allowed: wrong tenant, missing permission, non-staff, self.
        assert!(start_impersonation(&staff, TenantId::new(), client_user, client, now).is_err());
        let viewer = Principal::Staff {
            user_id: staff_id,
            tenant_id: tenant,
            permissions: PermissionSet::from_grants(["clients.view"]),
        };
        assert_eq!(
            start_impersonation(&viewer, tenant, client_user, client, now)
                .unwrap_err()
                .http_status(),
            403
        );
        assert!(start_impersonation(&p, tenant, client_user, client, now).is_err());
        assert!(start_impersonation(&staff, tenant, staff_id, client, now).is_err());

        let normal_client = Principal::Client {
            user_id: client_user,
            client_id: client,
            tenant_id: tenant,
            impersonator: None,
        };
        assert!(normal_client.may_change_credentials());
        assert_eq!(normal_client.audit_actor(), Some(client_user));
        let json = serde_json::to_value(&normal_client).unwrap();
        assert!(json.get("impersonator").is_none());
    }
}
