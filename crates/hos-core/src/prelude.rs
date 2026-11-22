//! Glob-importable re-exports: `use hos_core::prelude::*;`
//!
//! Contains the types nearly every crate touches. Request/response types of a
//! specific integration category are imported from their module
//! (`hos_core::integrations::virt::*`, ...).

pub use crate::async_trait;
pub use crate::config::AppConfig;
pub use crate::crypto::SecretBox;
pub use crate::domain::{
    DomainStatus, IntegrationCategory, InvoiceStatus, Locale, OrderStatus, ProductType,
    ServiceStatus, TenantStatus, TicketPriority, TicketStatus, UserKind,
};
pub use crate::error::{CoreError, CoreResult, IntegrationError, IntegrationResult};
pub use crate::ids::*;
pub use crate::integrations::{
    AiProvider, AnyIntegration, ConnectionInfo, DomainRegistrar, FieldKind, FieldReader, FieldSpec,
    HostingPanel, IntegrationDescriptor, Mailer, PaymentGateway, SelectOption, VirtProvider, kinds,
};
pub use crate::money::{BillingCycle, Currency, Money, MoneyError};
pub use crate::secret::{Secret, SecretString};
pub use crate::tenant::{Branding, HexColor, TenantContext};
