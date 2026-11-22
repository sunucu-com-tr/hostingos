//! schema.org JSON-LD builders.
//!
//! Typed builders ([`Organization`], [`WebSite`], [`Product`] + [`Offer`] /
//! [`AggregateOffer`], [`Service`], [`FaqPage`], [`BreadcrumbList`],
//! [`Article`], [`HowTo`], [`SoftwareApplication`], [`ItemList`],
//! [`AggregateRating`], [`Review`]) implement [`Schema`]: `to_value()` gives a
//! `serde_json::Value` node, [`Schema::to_script`] a
//! `<script type="application/ld+json">` tag that is safe against `</script>`
//! break-out, and [`Schema::validate`] checks Google rich-result requirements.
//!
//! [`validate_value`] runs the same rules over *any* JSON-LD value (e.g. one
//! extracted from a page by the auditor), recursing into `@graph` and nested
//! objects.
//!
//! Ratings and reviews are emitted only from data the caller passes in — never
//! synthesise them (Google's review-snippet policy; also self-serving
//! `Organization` reviews are not eligible for stars).
//!
//! Notes on current Google behaviour: FAQ rich results are shown only for
//! well-known government / health sites (since 2023) and HowTo rich results
//! were retired (2023), and the sitelinks search box is gone (2024) — the
//! markup is still valid schema.org and is read by AI answer engines, so the
//! builders keep it.

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use hos_core::integrations::PostalAddress;
use hos_core::money::{BillingCycle, Money};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::escape;

/// The JSON-LD `@context`.
pub const CONTEXT: &str = "https://schema.org";

// ---------------------------------------------------------------------------
// Issues
// ---------------------------------------------------------------------------

/// Severity of a validation [`Issue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    /// Missing recommended property (still eligible, less rich).
    Warning,
    /// Missing required property / invalid value (not eligible for the rich result).
    Error,
}

/// One validation finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Severity.
    pub severity: IssueSeverity,
    /// JSON path, e.g. `Product.offers[0].priceCurrency`.
    pub path: String,
    /// Stable machine code, e.g. `missing_required`.
    pub code: String,
    /// English developer message.
    pub message: String,
}

impl Issue {
    fn error(path: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            severity: IssueSeverity::Error,
            path: path.to_owned(),
            code: code.to_owned(),
            message: message.into(),
        }
    }

    fn warning(path: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            severity: IssueSeverity::Warning,
            path: path.to_owned(),
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Schema trait / rendering
// ---------------------------------------------------------------------------

/// A schema.org node.
pub trait Schema {
    /// The node as JSON (without `@context`).
    fn to_value(&self) -> Value;

    /// The node as a standalone JSON-LD document (with `@context`).
    fn to_document(&self) -> Value {
        with_context(self.to_value())
    }

    /// `<script type="application/ld+json">…</script>` for the document.
    fn to_script(&self) -> String {
        script_tag(&self.to_document())
    }

    /// Google rich-result checks (see [`validate_value`]).
    fn validate(&self) -> Vec<Issue> {
        validate_value(&self.to_value())
    }
}

/// Adds `"@context": "https://schema.org"` to an object node.
pub fn with_context(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len() + 1);
            out.insert("@context".into(), Value::String(CONTEXT.into()));
            for (k, v) in map {
                if k != "@context" {
                    out.insert(k, v);
                }
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// Wraps a JSON value in a script tag. The JSON is escaped with
/// [`escape::json_for_script`] so no field content can terminate the tag.
pub fn script_tag(value: &Value) -> String {
    format!(
        "<script type=\"application/ld+json\">{}</script>",
        escape::json_for_script(value)
    )
}

/// Several nodes rendered as one `{"@context", "@graph": [...]}` document.
/// Give nodes `@id`s (e.g. `https://acme.example/#organization`) to link them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Graph {
    nodes: Vec<Value>,
}

impl Graph {
    /// Empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a node.
    #[must_use]
    pub fn with(mut self, node: &dyn Schema) -> Self {
        self.nodes.push(node.to_value());
        self
    }

    /// Adds a node in place.
    pub fn push(&mut self, node: &dyn Schema) {
        self.nodes.push(node.to_value());
    }

    /// Adds a raw JSON node.
    pub fn push_value(&mut self, node: Value) {
        self.nodes.push(node);
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The `@graph` document.
    pub fn to_document(&self) -> Value {
        json!({"@context": CONTEXT, "@graph": self.nodes})
    }

    /// Script tag of the document.
    pub fn to_script(&self) -> String {
        script_tag(&self.to_document())
    }

    /// Validates every node.
    pub fn validate(&self) -> Vec<Issue> {
        validate_document(&self.to_document())
    }
}

// ---------------------------------------------------------------------------
// Object builder helper
// ---------------------------------------------------------------------------

struct Obj(Map<String, Value>);

impl Obj {
    fn new(ty: &str) -> Self {
        let mut m = Map::new();
        m.insert("@type".into(), Value::String(ty.into()));
        Self(m)
    }

    fn id(self, id: Option<&str>) -> Self {
        self.s("@id", id)
    }

    fn s(mut self, key: &str, v: Option<&str>) -> Self {
        if let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) {
            self.0.insert(key.into(), Value::String(v.into()));
        }
        self
    }

    fn v(mut self, key: &str, v: Option<Value>) -> Self {
        if let Some(v) = v {
            self.0.insert(key.into(), v);
        }
        self
    }

    fn arr(mut self, key: &str, items: Vec<Value>) -> Self {
        if !items.is_empty() {
            self.0.insert(key.into(), Value::Array(items));
        }
        self
    }

    fn strs(self, key: &str, items: &[String]) -> Self {
        let vals = items
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.into()))
            .collect();
        self.arr(key, vals)
    }

    fn build(self) -> Value {
        Value::Object(self.0)
    }
}

fn date_time(t: &DateTime<Utc>) -> Value {
    Value::String(t.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn date(d: &NaiveDate) -> Value {
    Value::String(d.format("%Y-%m-%d").to_string())
}

fn number(d: Decimal) -> Value {
    let n = d.normalize();
    if n.scale() == 0
        && let Some(i) = n.to_i64()
    {
        return Value::from(i);
    }
    n.to_f64()
        .and_then(serde_json::Number::from_f64)
        .map_or_else(|| Value::String(n.to_string()), Value::Number)
}

fn quantity_months(months: u32) -> Value {
    json!({"@type": "QuantitativeValue", "value": months, "unitCode": "MON"})
}

// ---------------------------------------------------------------------------
// Shared building blocks
// ---------------------------------------------------------------------------

/// Person or organisation reference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// `Person`.
    #[default]
    Person,
    /// `Organization`.
    Organization,
}

/// A `Person` / `Organization` reference (author, publisher, provider, seller).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entity {
    /// Kind.
    pub kind: EntityKind,
    /// Name.
    pub name: String,
    /// Profile / home URL.
    pub url: Option<String>,
    /// `@id` reference (e.g. `https://acme.example/#organization`).
    pub id: Option<String>,
    /// Logo URL (organisations).
    pub logo: Option<String>,
}

impl Entity {
    /// A person.
    pub fn person(name: impl Into<String>) -> Self {
        Self {
            kind: EntityKind::Person,
            name: name.into(),
            ..Self::default()
        }
    }

    /// An organisation.
    pub fn organization(name: impl Into<String>) -> Self {
        Self {
            kind: EntityKind::Organization,
            name: name.into(),
            ..Self::default()
        }
    }

    /// Sets the URL.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Sets the `@id`.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    fn to_value(&self) -> Value {
        let ty = match self.kind {
            EntityKind::Person => "Person",
            EntityKind::Organization => "Organization",
        };
        let logo = self
            .logo
            .as_deref()
            .map(|l| json!({"@type": "ImageObject", "url": l}));
        Obj::new(ty)
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .s("url", self.url.as_deref())
            .v("logo", logo)
            .build()
    }
}

fn address_value(a: &PostalAddress) -> Value {
    let street = match a.line2.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(l2) => format!("{} {}", a.line1.trim(), l2),
        None => a.line1.trim().to_owned(),
    };
    Obj::new("PostalAddress")
        .s("streetAddress", Some(&street))
        .s("addressLocality", Some(&a.city))
        .s("addressRegion", a.state.as_deref())
        .s("postalCode", a.postal_code.as_deref())
        .s("addressCountry", Some(&a.country_code))
        .build()
}

/// `additionalProperty` name/value pair (package specs: "Disk" → "20 GB NVMe").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyValue {
    /// Property name.
    pub name: String,
    /// Value.
    pub value: String,
}

impl PropertyValue {
    /// Creates a pair.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    fn to_value(&self) -> Value {
        json!({"@type": "PropertyValue", "name": self.name.trim(), "value": self.value.trim()})
    }
}

// ---------------------------------------------------------------------------
// Organization / WebSite
// ---------------------------------------------------------------------------

/// Organisation subtype.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationKind {
    /// `Organization`.
    #[default]
    Organization,
    /// `OnlineBusiness` (recommended by Google for online-only companies).
    OnlineBusiness,
    /// `Corporation`.
    Corporation,
    /// `LocalBusiness`.
    LocalBusiness,
}

impl OrganizationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "Organization",
            Self::OnlineBusiness => "OnlineBusiness",
            Self::Corporation => "Corporation",
            Self::LocalBusiness => "LocalBusiness",
        }
    }
}

/// `ContactPoint`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactPoint {
    /// `"customer support"`, `"technical support"`, `"sales"`, `"billing support"`.
    pub contact_type: String,
    /// Phone in international format.
    pub telephone: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Contact page URL.
    pub url: Option<String>,
    /// ISO country codes served.
    pub area_served: Vec<String>,
    /// Languages (`"Turkish"`, `"en"`).
    pub available_language: Vec<String>,
}

impl ContactPoint {
    fn to_value(&self) -> Value {
        Obj::new("ContactPoint")
            .s("contactType", Some(&self.contact_type))
            .s("telephone", self.telephone.as_deref())
            .s("email", self.email.as_deref())
            .s("url", self.url.as_deref())
            .strs("areaServed", &self.area_served)
            .strs("availableLanguage", &self.available_language)
            .build()
    }
}

/// `Organization` (the tenant / hosting company).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Organization {
    /// Subtype.
    pub kind: OrganizationKind,
    /// `@id`, conventionally `{home}/#organization`.
    pub id: Option<String>,
    /// Brand name.
    pub name: String,
    /// Registered legal name.
    pub legal_name: Option<String>,
    /// Home page.
    pub url: Option<String>,
    /// Logo URL (≥ 112×112 px).
    pub logo: Option<String>,
    /// Short description.
    pub description: Option<String>,
    /// Public email.
    pub email: Option<String>,
    /// Public phone.
    pub telephone: Option<String>,
    /// Postal address.
    pub address: Option<PostalAddress>,
    /// Social / directory profiles (entity disambiguation for AI engines).
    pub same_as: Vec<String>,
    /// Contact points.
    pub contact_points: Vec<ContactPoint>,
    /// Founding date.
    pub founding_date: Option<NaiveDate>,
    /// VAT id.
    pub vat_id: Option<String>,
    /// Tax id (Türkiye: VKN).
    pub tax_id: Option<String>,
    /// Countries / regions served.
    pub area_served: Vec<String>,
}

impl Organization {
    /// Organisation with a name and URL.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: Some(url.into()),
            ..Self::default()
        }
    }
}

impl Schema for Organization {
    fn to_value(&self) -> Value {
        Obj::new(self.kind.as_str())
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .s("legalName", self.legal_name.as_deref())
            .s("url", self.url.as_deref())
            .s("logo", self.logo.as_deref())
            .s("description", self.description.as_deref())
            .s("email", self.email.as_deref())
            .s("telephone", self.telephone.as_deref())
            .v("address", self.address.as_ref().map(address_value))
            .strs("sameAs", &self.same_as)
            .arr(
                "contactPoint",
                self.contact_points
                    .iter()
                    .map(ContactPoint::to_value)
                    .collect(),
            )
            .v("foundingDate", self.founding_date.as_ref().map(date))
            .s("vatID", self.vat_id.as_deref())
            .s("taxID", self.tax_id.as_deref())
            .strs("areaServed", &self.area_served)
            .build()
    }
}

/// `WebSite` (+ optional `SearchAction`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSite {
    /// `@id`, conventionally `{home}/#website`.
    pub id: Option<String>,
    /// Site name (Google "site name" feature).
    pub name: String,
    /// Alternate names.
    pub alternate_names: Vec<String>,
    /// Home URL.
    pub url: String,
    /// Description.
    pub description: Option<String>,
    /// Content languages.
    pub in_language: Vec<String>,
    /// `@id` of the publishing [`Organization`].
    pub publisher_id: Option<String>,
    /// Search URL with `{search_term_string}`, e.g. `https://acme.example/search?q={search_term_string}`.
    pub search_url_template: Option<String>,
}

impl Schema for WebSite {
    fn to_value(&self) -> Value {
        let action = self.search_url_template.as_deref().map(|t| {
            json!({
                "@type": "SearchAction",
                "target": {"@type": "EntryPoint", "urlTemplate": t},
                "query-input": "required name=search_term_string"
            })
        });
        let in_language = match self.in_language.as_slice() {
            [] => None,
            [one] => Some(Value::String(one.clone())),
            many => Some(json!(many)),
        };
        Obj::new("WebSite")
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .strs("alternateName", &self.alternate_names)
            .s("url", Some(&self.url))
            .s("description", self.description.as_deref())
            .v("inLanguage", in_language)
            .v(
                "publisher",
                self.publisher_id.as_deref().map(|id| json!({"@id": id})),
            )
            .v("potentialAction", action)
            .build()
    }
}

// ---------------------------------------------------------------------------
// Offers / Product / Service / SoftwareApplication
// ---------------------------------------------------------------------------

/// `availability`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// In stock / orderable.
    #[default]
    InStock,
    /// Sold out.
    OutOfStock,
    /// Pre-order.
    PreOrder,
    /// Discontinued.
    Discontinued,
    /// Limited availability.
    LimitedAvailability,
    /// Online only.
    OnlineOnly,
}

impl Availability {
    /// Full schema.org URL.
    pub fn url(self) -> &'static str {
        match self {
            Self::InStock => "https://schema.org/InStock",
            Self::OutOfStock => "https://schema.org/OutOfStock",
            Self::PreOrder => "https://schema.org/PreOrder",
            Self::Discontinued => "https://schema.org/Discontinued",
            Self::LimitedAvailability => "https://schema.org/LimitedAvailability",
            Self::OnlineOnly => "https://schema.org/OnlineOnly",
        }
    }
}

/// One price of a hosting package (one currency × billing cycle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// Price per billing period.
    pub price: Money,
    /// Billing cycle; recurring cycles add a `UnitPriceSpecification` with
    /// `referenceQuantity` / `billingDuration` in months.
    #[serde(default)]
    pub billing_cycle: Option<BillingCycle>,
    /// One-time setup fee (`priceComponentType: ActivationFee`).
    #[serde(default)]
    pub setup_fee: Option<Money>,
    /// Availability.
    #[serde(default)]
    pub availability: Option<Availability>,
    /// Order / product URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Date the price is guaranteed until.
    #[serde(default)]
    pub price_valid_until: Option<NaiveDate>,
    /// Offer name (e.g. "Yıllık").
    #[serde(default)]
    pub name: Option<String>,
    /// SKU of this variant.
    #[serde(default)]
    pub sku: Option<String>,
    /// Seller.
    #[serde(default)]
    pub seller: Option<Entity>,
}

impl Offer {
    /// An in-stock offer for a price.
    pub fn new(price: Money) -> Self {
        Self {
            price,
            billing_cycle: None,
            setup_fee: None,
            availability: Some(Availability::InStock),
            url: None,
            price_valid_until: None,
            name: None,
            sku: None,
            seller: None,
        }
    }

    /// Sets the billing cycle.
    #[must_use]
    pub fn with_cycle(mut self, cycle: BillingCycle) -> Self {
        self.billing_cycle = Some(cycle);
        self
    }

    fn price_specs(&self) -> Vec<Value> {
        let mut specs = Vec::new();
        if let Some(months) = self.billing_cycle.and_then(|c| c.months()) {
            specs.push(json!({
                "@type": "UnitPriceSpecification",
                "price": self.price.to_decimal_string(),
                "priceCurrency": self.price.currency.as_str(),
                "priceComponentType": "https://schema.org/Subscription",
                "referenceQuantity": quantity_months(months),
                "billingDuration": quantity_months(months),
            }));
        }
        if let Some(fee) = self.setup_fee.as_ref().filter(|f| f.is_positive()) {
            specs.push(json!({
                "@type": "UnitPriceSpecification",
                "price": fee.to_decimal_string(),
                "priceCurrency": fee.currency.as_str(),
                "priceComponentType": "https://schema.org/ActivationFee",
            }));
        }
        specs
    }

    fn to_value(&self) -> Value {
        Obj::new("Offer")
            .s("name", self.name.as_deref())
            .s("price", Some(&self.price.to_decimal_string()))
            .s("priceCurrency", Some(self.price.currency.as_str()))
            .s("availability", self.availability.map(Availability::url))
            .s("url", self.url.as_deref())
            .v("priceValidUntil", self.price_valid_until.as_ref().map(date))
            .s("sku", self.sku.as_deref())
            .arr("priceSpecification", self.price_specs())
            .v("seller", self.seller.as_ref().map(Entity::to_value))
            .build()
    }
}

/// `AggregateOffer` (price range across cycles / variants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateOffer {
    /// Lowest price.
    pub low_price: Money,
    /// Highest price.
    #[serde(default)]
    pub high_price: Option<Money>,
    /// Number of offers.
    #[serde(default)]
    pub offer_count: Option<u32>,
    /// The individual offers (optional).
    #[serde(default)]
    pub offers: Vec<Offer>,
}

impl AggregateOffer {
    /// Builds the range from offers in one currency (offers in other
    /// currencies than the first are ignored). `None` when empty.
    pub fn from_offers(offers: Vec<Offer>) -> Option<Self> {
        let currency = offers.first()?.price.currency;
        let same: Vec<Offer> = offers
            .into_iter()
            .filter(|o| o.price.currency == currency)
            .collect();
        let low = same.iter().map(|o| o.price.amount).min()?;
        let high = same.iter().map(|o| o.price.amount).max()?;
        Some(Self {
            low_price: Money::new(low, currency),
            high_price: Some(Money::new(high, currency)),
            offer_count: u32::try_from(same.len()).ok(),
            offers: same,
        })
    }

    fn to_value(&self) -> Value {
        Obj::new("AggregateOffer")
            .s("lowPrice", Some(&self.low_price.to_decimal_string()))
            .s(
                "highPrice",
                self.high_price
                    .as_ref()
                    .map(Money::to_decimal_string)
                    .as_deref(),
            )
            .s("priceCurrency", Some(self.low_price.currency.as_str()))
            .v("offerCount", self.offer_count.map(Value::from))
            .arr("offers", self.offers.iter().map(Offer::to_value).collect())
            .build()
    }
}

/// The `offers` of a product / service / application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Offers {
    /// One offer.
    Single(Offer),
    /// Several offers (e.g. one per billing cycle).
    Multiple(Vec<Offer>),
    /// A price range.
    Aggregate(AggregateOffer),
}

impl Offers {
    fn to_value(&self) -> Value {
        match self {
            Self::Single(o) => o.to_value(),
            Self::Multiple(list) => Value::Array(list.iter().map(Offer::to_value).collect()),
            Self::Aggregate(a) => a.to_value(),
        }
    }
}

/// `AggregateRating` — only from real review data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateRating {
    /// Average rating.
    pub rating_value: Decimal,
    /// Number of ratings.
    #[serde(default)]
    pub rating_count: Option<u32>,
    /// Number of written reviews.
    #[serde(default)]
    pub review_count: Option<u32>,
    /// Best possible rating (default 5).
    #[serde(default)]
    pub best_rating: Option<Decimal>,
    /// Worst possible rating (default 1).
    #[serde(default)]
    pub worst_rating: Option<Decimal>,
}

impl AggregateRating {
    fn node(&self) -> Value {
        Obj::new("AggregateRating")
            .v("ratingValue", Some(number(self.rating_value)))
            .v("ratingCount", self.rating_count.map(Value::from))
            .v("reviewCount", self.review_count.map(Value::from))
            .v("bestRating", self.best_rating.map(number))
            .v("worstRating", self.worst_rating.map(number))
            .build()
    }
}

impl Schema for AggregateRating {
    fn to_value(&self) -> Value {
        self.node()
    }
}

/// A single `Review` — only from real customer reviews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// Reviewer.
    pub author: Entity,
    /// Rating given.
    #[serde(default)]
    pub rating_value: Option<Decimal>,
    /// Best possible rating (default 5).
    #[serde(default)]
    pub best_rating: Option<Decimal>,
    /// Review text.
    #[serde(default)]
    pub body: Option<String>,
    /// Review title.
    #[serde(default)]
    pub name: Option<String>,
    /// Publication date.
    #[serde(default)]
    pub date_published: Option<NaiveDate>,
}

impl Review {
    fn node(&self) -> Value {
        let rating = self.rating_value.map(|v| {
            Obj::new("Rating")
                .v("ratingValue", Some(number(v)))
                .v("bestRating", self.best_rating.map(number))
                .build()
        });
        Obj::new("Review")
            .v("author", Some(self.author.to_value()))
            .v("reviewRating", rating)
            .s("reviewBody", self.body.as_deref())
            .s("name", self.name.as_deref())
            .v("datePublished", self.date_published.as_ref().map(date))
            .build()
    }
}

impl Schema for Review {
    fn to_value(&self) -> Value {
        self.node()
    }
}

/// `Product` (a hosting package).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Product {
    /// `@id`.
    pub id: Option<String>,
    /// Package name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Product page URL.
    pub url: Option<String>,
    /// SKU.
    pub sku: Option<String>,
    /// Brand name (the tenant).
    pub brand: Option<String>,
    /// Images.
    pub images: Vec<String>,
    /// Category (`"Web Hosting"`).
    pub category: Option<String>,
    /// Offers.
    pub offers: Option<Offers>,
    /// Aggregate rating (real data only).
    pub aggregate_rating: Option<AggregateRating>,
    /// Reviews (real data only).
    pub reviews: Vec<Review>,
    /// Specs (`additionalProperty`).
    pub properties: Vec<PropertyValue>,
}

impl Schema for Product {
    fn to_value(&self) -> Value {
        Obj::new("Product")
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .s("description", self.description.as_deref())
            .s("url", self.url.as_deref())
            .s("sku", self.sku.as_deref())
            .v(
                "brand",
                self.brand
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(|b| json!({"@type": "Brand", "name": b})),
            )
            .strs("image", &self.images)
            .s("category", self.category.as_deref())
            .v("offers", self.offers.as_ref().map(Offers::to_value))
            .v(
                "aggregateRating",
                self.aggregate_rating.as_ref().map(AggregateRating::node),
            )
            .arr("review", self.reviews.iter().map(Review::node).collect())
            .arr(
                "additionalProperty",
                self.properties
                    .iter()
                    .map(PropertyValue::to_value)
                    .collect(),
            )
            .build()
    }
}

/// `Service` (managed services, migrations, support plans).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Service {
    /// `@id`.
    pub id: Option<String>,
    /// Name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// URL.
    pub url: Option<String>,
    /// `serviceType` (`"Web hosting"`).
    pub service_type: Option<String>,
    /// Provider (usually an `@id` reference to the organisation).
    pub provider: Option<Entity>,
    /// Areas served.
    pub area_served: Vec<String>,
    /// Offers.
    pub offers: Option<Offers>,
}

impl Schema for Service {
    fn to_value(&self) -> Value {
        Obj::new("Service")
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .s("description", self.description.as_deref())
            .s("url", self.url.as_deref())
            .s("serviceType", self.service_type.as_deref())
            .v("provider", self.provider.as_ref().map(Entity::to_value))
            .strs("areaServed", &self.area_served)
            .v("offers", self.offers.as_ref().map(Offers::to_value))
            .build()
    }
}

/// `SoftwareApplication` (control panels, one-click apps, SaaS products).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoftwareApplication {
    /// `@id`.
    pub id: Option<String>,
    /// Name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// URL.
    pub url: Option<String>,
    /// `applicationCategory` (`"BusinessApplication"`, `"DeveloperApplication"`).
    pub application_category: Option<String>,
    /// `operatingSystem` (`"Web"`, `"Linux"`).
    pub operating_system: Option<String>,
    /// Version.
    pub software_version: Option<String>,
    /// Offers (Google requires a price).
    pub offers: Option<Offers>,
    /// Aggregate rating (real data only).
    pub aggregate_rating: Option<AggregateRating>,
    /// Reviews (real data only).
    pub reviews: Vec<Review>,
}

impl Schema for SoftwareApplication {
    fn to_value(&self) -> Value {
        Obj::new("SoftwareApplication")
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .s("description", self.description.as_deref())
            .s("url", self.url.as_deref())
            .s("applicationCategory", self.application_category.as_deref())
            .s("operatingSystem", self.operating_system.as_deref())
            .s("softwareVersion", self.software_version.as_deref())
            .v("offers", self.offers.as_ref().map(Offers::to_value))
            .v(
                "aggregateRating",
                self.aggregate_rating.as_ref().map(AggregateRating::node),
            )
            .arr("review", self.reviews.iter().map(Review::node).collect())
            .build()
    }
}

// ---------------------------------------------------------------------------
// FAQ / Breadcrumbs / ItemList
// ---------------------------------------------------------------------------

/// One FAQ entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaqEntry {
    /// Question.
    pub question: String,
    /// Answer (plain text or simple HTML: `p`, `a`, `ul`, `ol`, `li`, `b`, `strong`, `i`, `em`, `br`).
    pub answer: String,
}

impl FaqEntry {
    /// Creates an entry.
    pub fn new(question: impl Into<String>, answer: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            answer: answer.into(),
        }
    }
}

/// `FAQPage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FaqPage {
    /// `@id`.
    pub id: Option<String>,
    /// Page URL.
    pub url: Option<String>,
    /// Questions.
    pub entries: Vec<FaqEntry>,
}

impl Schema for FaqPage {
    fn to_value(&self) -> Value {
        let items = self
            .entries
            .iter()
            .map(|e| {
                json!({
                    "@type": "Question",
                    "name": e.question.trim(),
                    "acceptedAnswer": {"@type": "Answer", "text": e.answer.trim()}
                })
            })
            .collect::<Vec<_>>();
        let mut obj = Obj::new("FAQPage")
            .id(self.id.as_deref())
            .s("url", self.url.as_deref());
        obj.0.insert("mainEntity".into(), Value::Array(items));
        obj.build()
    }
}

/// One breadcrumb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crumb {
    /// Label.
    pub name: String,
    /// URL (may be omitted for the last crumb).
    pub url: Option<String>,
}

impl Crumb {
    /// A linked crumb.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: Some(url.into()),
        }
    }
}

/// `BreadcrumbList`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BreadcrumbList {
    /// Crumbs from the home page down to the current page.
    pub items: Vec<Crumb>,
}

impl Schema for BreadcrumbList {
    fn to_value(&self) -> Value {
        let items = self
            .items
            .iter()
            .enumerate()
            .map(|(i, c)| {
                Obj::new("ListItem")
                    .v("position", Some(Value::from(i + 1)))
                    .s("name", Some(&c.name))
                    .s("item", c.url.as_deref())
                    .build()
            })
            .collect::<Vec<_>>();
        let mut obj = Obj::new("BreadcrumbList");
        obj.0.insert("itemListElement".into(), Value::Array(items));
        obj.build()
    }
}

/// An entry of an [`ItemList`] (summary-page carousel pattern).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    /// URL of the item's page.
    pub url: String,
    /// Optional name.
    #[serde(default)]
    pub name: Option<String>,
}

/// `ItemList`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemList {
    /// List name.
    pub name: Option<String>,
    /// Entries in display order.
    pub items: Vec<ListEntry>,
}

impl Schema for ItemList {
    fn to_value(&self) -> Value {
        let items = self
            .items
            .iter()
            .enumerate()
            .map(|(i, e)| {
                Obj::new("ListItem")
                    .v("position", Some(Value::from(i + 1)))
                    .s("url", Some(&e.url))
                    .s("name", e.name.as_deref())
                    .build()
            })
            .collect::<Vec<_>>();
        let mut obj = Obj::new("ItemList")
            .s("name", self.name.as_deref())
            .v("numberOfItems", Some(Value::from(self.items.len())));
        obj.0.insert("itemListElement".into(), Value::Array(items));
        obj.build()
    }
}

// ---------------------------------------------------------------------------
// Article / HowTo
// ---------------------------------------------------------------------------

/// Article subtype.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticleKind {
    /// `Article`.
    #[default]
    Article,
    /// `BlogPosting`.
    BlogPosting,
    /// `NewsArticle` (announcements).
    NewsArticle,
    /// `TechArticle` (knowledge base).
    TechArticle,
}

impl ArticleKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Article => "Article",
            Self::BlogPosting => "BlogPosting",
            Self::NewsArticle => "NewsArticle",
            Self::TechArticle => "TechArticle",
        }
    }
}

/// `Article` / `BlogPosting` / `NewsArticle` / `TechArticle`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Article {
    /// Subtype.
    pub kind: ArticleKind,
    /// `@id`.
    pub id: Option<String>,
    /// Headline (≤ 110 characters).
    pub headline: String,
    /// Description / excerpt.
    pub description: Option<String>,
    /// Canonical URL (`mainEntityOfPage`).
    pub url: Option<String>,
    /// Images (16:9, 4:3 and 1:1 recommended).
    pub images: Vec<String>,
    /// First publication.
    pub date_published: Option<DateTime<Utc>>,
    /// Last modification.
    pub date_modified: Option<DateTime<Utc>>,
    /// Authors.
    pub authors: Vec<Entity>,
    /// Publisher (organisation).
    pub publisher: Option<Entity>,
    /// Language.
    pub in_language: Option<String>,
    /// Keywords.
    pub keywords: Vec<String>,
    /// Section / category.
    pub section: Option<String>,
    /// Word count.
    pub word_count: Option<u32>,
}

impl Schema for Article {
    fn to_value(&self) -> Value {
        let keywords = (!self.keywords.is_empty()).then(|| Value::String(self.keywords.join(", ")));
        Obj::new(self.kind.as_str())
            .id(self.id.as_deref())
            .s("headline", Some(&self.headline))
            .s("description", self.description.as_deref())
            .v(
                "mainEntityOfPage",
                self.url
                    .as_deref()
                    .map(|u| json!({"@type": "WebPage", "@id": u})),
            )
            .s("url", self.url.as_deref())
            .strs("image", &self.images)
            .v("datePublished", self.date_published.as_ref().map(date_time))
            .v("dateModified", self.date_modified.as_ref().map(date_time))
            .arr(
                "author",
                self.authors.iter().map(Entity::to_value).collect(),
            )
            .v("publisher", self.publisher.as_ref().map(Entity::to_value))
            .s("inLanguage", self.in_language.as_deref())
            .v("keywords", keywords)
            .s("articleSection", self.section.as_deref())
            .v("wordCount", self.word_count.map(Value::from))
            .build()
    }
}

/// One `HowToStep`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HowToStep {
    /// Short step name.
    pub name: Option<String>,
    /// Instruction text.
    pub text: String,
    /// Anchor URL of the step (`…#step-2`).
    pub url: Option<String>,
    /// Image URL.
    pub image: Option<String>,
}

/// `HowTo` (step-by-step knowledge-base articles).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HowTo {
    /// `@id`.
    pub id: Option<String>,
    /// Title.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// ISO 8601 duration (`PT10M`).
    pub total_time: Option<String>,
    /// Supplies.
    pub supplies: Vec<String>,
    /// Tools.
    pub tools: Vec<String>,
    /// Steps.
    pub steps: Vec<HowToStep>,
    /// Image.
    pub image: Option<String>,
}

impl Schema for HowTo {
    fn to_value(&self) -> Value {
        let steps = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                Obj::new("HowToStep")
                    .v("position", Some(Value::from(i + 1)))
                    .s("name", s.name.as_deref())
                    .s("text", Some(&s.text))
                    .s("url", s.url.as_deref())
                    .s("image", s.image.as_deref())
                    .build()
            })
            .collect::<Vec<_>>();
        let named = |ty: &str, items: &[String]| -> Vec<Value> {
            items
                .iter()
                .map(|n| json!({"@type": ty, "name": n.trim()}))
                .collect()
        };
        let mut obj = Obj::new("HowTo")
            .id(self.id.as_deref())
            .s("name", Some(&self.name))
            .s("description", self.description.as_deref())
            .s("totalTime", self.total_time.as_deref())
            .s("image", self.image.as_deref())
            .arr("supply", named("HowToSupply", &self.supplies))
            .arr("tool", named("HowToTool", &self.tools));
        obj.0.insert("step".into(), Value::Array(steps));
        obj.build()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates a complete JSON-LD document: `@context` must reference
/// schema.org, then every node is checked with [`validate_value`].
pub fn validate_document(doc: &Value) -> Vec<Issue> {
    let mut issues = Vec::new();
    let docs: Vec<&Value> = match doc {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for d in docs {
        let ctx_ok = d
            .get("@context")
            .map(|c| c.to_string().contains("schema.org"))
            .unwrap_or(false);
        if !ctx_ok {
            issues.push(Issue::error(
                "@context",
                "missing_context",
                "@context must be https://schema.org",
            ));
        }
        issues.extend(validate_value(d));
    }
    issues
}

/// Validates every typed node in `value` against Google rich-result rules
/// (required → [`IssueSeverity::Error`], recommended → [`IssueSeverity::Warning`]).
pub fn validate_value(value: &Value) -> Vec<Issue> {
    let mut issues = Vec::new();
    walk(value, "", &mut issues);
    issues
}

fn types_of(v: &Value) -> Vec<&str> {
    match v.get("@type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn walk(value: &Value, path: &str, issues: &mut Vec<Issue>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{i}]"), issues);
            }
        }
        Value::Object(map) => {
            let types = types_of(value);
            let here = match (path.is_empty(), types.first()) {
                (true, Some(t)) => (*t).to_owned(),
                (true, None) => String::new(),
                (false, _) => path.to_owned(),
            };
            for t in &types {
                check_node(t, value, &here, issues);
            }
            for (k, v) in map {
                if k.starts_with('@') && k != "@graph" {
                    continue;
                }
                if v.is_object() || v.is_array() {
                    let child = if here.is_empty() {
                        k.clone()
                    } else {
                        format!("{here}.{k}")
                    };
                    walk(v, &child, issues);
                }
            }
        }
        _ => {}
    }
}

fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match v.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.as_str()),
        _ => None,
    }
}

fn has(v: &Value, key: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    }
}

fn as_decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::Number(n) => n.to_string().parse().ok(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn first<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v.get(key)? {
        Value::Array(a) => a.first(),
        other => Some(other),
    }
}

fn require(v: &Value, path: &str, key: &str, issues: &mut Vec<Issue>) -> bool {
    if has(v, key) {
        true
    } else {
        issues.push(Issue::error(
            &format!("{path}.{key}"),
            "missing_required",
            format!("{key} is required"),
        ));
        false
    }
}

fn recommend(v: &Value, path: &str, key: &str, issues: &mut Vec<Issue>) {
    if !has(v, key) {
        issues.push(Issue::warning(
            &format!("{path}.{key}"),
            "missing_recommended",
            format!("{key} is recommended"),
        ));
    }
}

fn check_url(v: &Value, path: &str, key: &str, issues: &mut Vec<Issue>) {
    if let Some(s) = get_str(v, key)
        && escape::safe_http_url(s).is_none()
    {
        issues.push(Issue::warning(
            &format!("{path}.{key}"),
            "invalid_url",
            format!("{key} should be an absolute http(s) URL"),
        ));
    }
}

fn check_node(ty: &str, v: &Value, path: &str, issues: &mut Vec<Issue>) {
    check_url(v, path, "url", issues);
    match ty {
        "Product" => {
            require(v, path, "name", issues);
            if !(has(v, "offers") || has(v, "review") || has(v, "aggregateRating")) {
                issues.push(Issue::error(
                    path,
                    "product_needs_offer_review_or_rating",
                    "Product needs offers, review or aggregateRating",
                ));
            }
            recommend(v, path, "image", issues);
            recommend(v, path, "description", issues);
        }
        "Offer" => {
            let price = v
                .get("price")
                .or_else(|| first(v, "priceSpecification").and_then(|s| s.get("price")));
            match price {
                None => issues.push(Issue::error(
                    &format!("{path}.price"),
                    "missing_required",
                    "price (or priceSpecification.price) is required",
                )),
                Some(p) if as_decimal(p).is_none() => issues.push(Issue::error(
                    &format!("{path}.price"),
                    "invalid_price",
                    "price must be a number with '.' as decimal separator and no currency symbol",
                )),
                Some(p) if as_decimal(p).is_some_and(|d| d.is_sign_negative()) => {
                    issues.push(Issue::error(
                        &format!("{path}.price"),
                        "invalid_price",
                        "price must not be negative",
                    ));
                }
                Some(_) => {}
            }
            let currency = get_str(v, "priceCurrency").or_else(|| {
                first(v, "priceSpecification").and_then(|s| get_str(s, "priceCurrency"))
            });
            match currency {
                None => issues.push(Issue::error(
                    &format!("{path}.priceCurrency"),
                    "missing_required",
                    "priceCurrency is required",
                )),
                Some(c) if !(c.len() == 3 && c.chars().all(|ch| ch.is_ascii_uppercase())) => {
                    issues.push(Issue::error(
                        &format!("{path}.priceCurrency"),
                        "invalid_currency",
                        "priceCurrency must be an ISO 4217 code",
                    ));
                }
                Some(_) => {}
            }
            recommend(v, path, "availability", issues);
            if let Some(d) = get_str(v, "priceValidUntil")
                && NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err()
            {
                issues.push(Issue::error(
                    &format!("{path}.priceValidUntil"),
                    "invalid_date",
                    "priceValidUntil must be an ISO 8601 date",
                ));
            }
        }
        "AggregateOffer" => {
            if require(v, path, "lowPrice", issues)
                && v.get("lowPrice").and_then(as_decimal).is_none()
            {
                issues.push(Issue::error(
                    &format!("{path}.lowPrice"),
                    "invalid_price",
                    "lowPrice must be a number",
                ));
            }
            require(v, path, "priceCurrency", issues);
            recommend(v, path, "highPrice", issues);
            recommend(v, path, "offerCount", issues);
        }
        "AggregateRating" => {
            let value = v.get("ratingValue").and_then(as_decimal);
            if require(v, path, "ratingValue", issues) && value.is_none() {
                issues.push(Issue::error(
                    &format!("{path}.ratingValue"),
                    "invalid_rating",
                    "ratingValue must be a number",
                ));
            }
            let count = v
                .get("ratingCount")
                .and_then(as_decimal)
                .or_else(|| v.get("reviewCount").and_then(as_decimal));
            match count {
                None => issues.push(Issue::error(
                    &format!("{path}.ratingCount"),
                    "missing_required",
                    "ratingCount or reviewCount is required",
                )),
                Some(c) if c <= Decimal::ZERO => issues.push(Issue::error(
                    &format!("{path}.ratingCount"),
                    "invalid_rating",
                    "ratingCount / reviewCount must be positive",
                )),
                Some(_) => {}
            }
            check_rating_range(v, path, issues);
        }
        "Review" => {
            match v.get("author") {
                Some(a) if first_name(a).is_some() => {}
                _ => issues.push(Issue::error(
                    &format!("{path}.author"),
                    "missing_required",
                    "author with a name is required",
                )),
            }
            match v.get("reviewRating") {
                Some(r) if r.get("ratingValue").and_then(as_decimal).is_some() => {
                    check_rating_range(r, &format!("{path}.reviewRating"), issues);
                }
                _ => issues.push(Issue::error(
                    &format!("{path}.reviewRating.ratingValue"),
                    "missing_required",
                    "reviewRating.ratingValue is required",
                )),
            }
            recommend(v, path, "datePublished", issues);
        }
        "FAQPage" => match v.get("mainEntity") {
            Some(Value::Array(a)) if !a.is_empty() => {}
            Some(Value::Object(_)) => {}
            _ => issues.push(Issue::error(
                &format!("{path}.mainEntity"),
                "missing_required",
                "FAQPage needs at least one Question in mainEntity",
            )),
        },
        "Question" => {
            require(v, path, "name", issues);
            let answered = match v.get("acceptedAnswer") {
                Some(Value::Array(a)) => a.iter().any(|x| get_str(x, "text").is_some()),
                Some(a @ Value::Object(_)) => get_str(a, "text").is_some(),
                _ => false,
            };
            if !answered {
                issues.push(Issue::error(
                    &format!("{path}.acceptedAnswer.text"),
                    "missing_required",
                    "Question needs acceptedAnswer.text",
                ));
            }
        }
        "BreadcrumbList" => match v.get("itemListElement") {
            Some(Value::Array(items)) if !items.is_empty() => {
                if items.len() < 2 {
                    issues.push(Issue::warning(
                        &format!("{path}.itemListElement"),
                        "too_few_items",
                        "a breadcrumb trail should have at least two items",
                    ));
                }
                for (i, item) in items.iter().enumerate() {
                    let p = format!("{path}.itemListElement[{i}]");
                    require(item, &p, "position", issues);
                    if !has(item, "name") && !item.get("item").is_some_and(|it| has(it, "name")) {
                        issues.push(Issue::error(
                            &format!("{p}.name"),
                            "missing_required",
                            "name is required",
                        ));
                    }
                    if i + 1 < items.len() && !has(item, "item") {
                        issues.push(Issue::error(
                            &format!("{p}.item"),
                            "missing_required",
                            "item (URL) is required for all but the last crumb",
                        ));
                    }
                    check_url(item, &p, "item", issues);
                    if item
                        .get("position")
                        .and_then(Value::as_u64)
                        .is_some_and(|pos| pos != (i as u64) + 1)
                    {
                        issues.push(Issue::warning(
                            &format!("{p}.position"),
                            "position_order",
                            "positions should be 1, 2, 3, …",
                        ));
                    }
                }
            }
            _ => issues.push(Issue::error(
                &format!("{path}.itemListElement"),
                "missing_required",
                "itemListElement is required",
            )),
        },
        "ItemList" => match v.get("itemListElement") {
            Some(Value::Array(items)) if !items.is_empty() => {
                for (i, item) in items.iter().enumerate() {
                    let p = format!("{path}.itemListElement[{i}]");
                    if item.is_object() && types_of(item).contains(&"ListItem") {
                        require(item, &p, "position", issues);
                        if !has(item, "url") && !has(item, "item") {
                            issues.push(Issue::error(
                                &format!("{p}.url"),
                                "missing_required",
                                "ListItem needs url or item",
                            ));
                        }
                    }
                }
            }
            _ => issues.push(Issue::error(
                &format!("{path}.itemListElement"),
                "missing_required",
                "itemListElement is required",
            )),
        },
        "Article" | "BlogPosting" | "NewsArticle" | "TechArticle" | "Report" => {
            match get_str(v, "headline") {
                Some(h) if h.chars().count() > 110 => issues.push(Issue::warning(
                    &format!("{path}.headline"),
                    "headline_too_long",
                    "headline should be at most 110 characters",
                )),
                Some(_) => {}
                None => recommend(v, path, "headline", issues),
            }
            recommend(v, path, "image", issues);
            recommend(v, path, "datePublished", issues);
            recommend(v, path, "dateModified", issues);
            match v.get("author") {
                None => recommend(v, path, "author", issues),
                Some(a) if first_name(a).is_none() => issues.push(Issue::error(
                    &format!("{path}.author.name"),
                    "missing_required",
                    "every author needs a name",
                )),
                Some(_) => {}
            }
            for key in ["datePublished", "dateModified"] {
                if let Some(d) = get_str(v, key)
                    && parse_date_any(d).is_none()
                {
                    issues.push(Issue::error(
                        &format!("{path}.{key}"),
                        "invalid_date",
                        format!("{key} must be ISO 8601"),
                    ));
                }
            }
            if let (Some(p), Some(m)) = (
                get_str(v, "datePublished").and_then(parse_date_any),
                get_str(v, "dateModified").and_then(parse_date_any),
            ) && m < p
            {
                issues.push(Issue::warning(
                    &format!("{path}.dateModified"),
                    "modified_before_published",
                    "dateModified is earlier than datePublished",
                ));
            }
        }
        "Organization" | "OnlineBusiness" | "Corporation" | "LocalBusiness" | "OnlineStore" => {
            require(v, path, "name", issues);
            // Nested references (author / publisher) only need a name.
            if path.is_empty() || !path.contains('.') {
                recommend(v, path, "url", issues);
                recommend(v, path, "logo", issues);
            }
            check_url(v, path, "logo", issues);
        }
        "WebSite" => {
            require(v, path, "name", issues);
            require(v, path, "url", issues);
            if let Some(action) = v.get("potentialAction") {
                let template = action
                    .get("target")
                    .and_then(|t| get_str(t, "urlTemplate").or_else(|| t.as_str()));
                let query = get_str(action, "query-input");
                let ok = match (template, query) {
                    (Some(t), Some(q)) => q
                        .split_whitespace()
                        .find_map(|p| p.strip_prefix("name="))
                        .is_some_and(|name| t.contains(&format!("{{{name}}}"))),
                    _ => false,
                };
                if !ok {
                    issues.push(Issue::error(
                        &format!("{path}.potentialAction"),
                        "invalid_search_action",
                        "SearchAction urlTemplate must contain the placeholder named in query-input",
                    ));
                }
            }
        }
        "HowTo" => {
            require(v, path, "name", issues);
            match v.get("step") {
                Some(Value::Array(steps)) if !steps.is_empty() => {
                    for (i, s) in steps.iter().enumerate() {
                        if s.is_object() && !has(s, "text") && !has(s, "itemListElement") {
                            issues.push(Issue::error(
                                &format!("{path}.step[{i}].text"),
                                "missing_required",
                                "HowToStep needs text",
                            ));
                        }
                    }
                }
                _ => issues.push(Issue::error(
                    &format!("{path}.step"),
                    "missing_required",
                    "HowTo needs at least one step",
                )),
            }
        }
        "SoftwareApplication" | "WebApplication" | "MobileApplication" => {
            require(v, path, "name", issues);
            let price_ok = match v.get("offers") {
                Some(Value::Array(a)) => a.iter().any(|o| has(o, "price") || has(o, "lowPrice")),
                Some(o) => has(o, "price") || has(o, "lowPrice"),
                None => false,
            };
            if !price_ok {
                issues.push(Issue::error(
                    &format!("{path}.offers.price"),
                    "missing_required",
                    "offers.price is required (use 0 for free apps)",
                ));
            }
            if !(has(v, "aggregateRating") || has(v, "review")) {
                issues.push(Issue::error(
                    path,
                    "app_needs_rating_or_review",
                    "aggregateRating or review is required for software app rich results",
                ));
            }
            recommend(v, path, "applicationCategory", issues);
            recommend(v, path, "operatingSystem", issues);
        }
        "Service" => {
            require(v, path, "name", issues);
            recommend(v, path, "provider", issues);
        }
        _ => {}
    }
}

fn first_name(author: &Value) -> Option<&str> {
    match author {
        Value::Array(a) => a.iter().find_map(first_name),
        Value::Object(_) => get_str(author, "name"),
        Value::String(s) if !s.trim().is_empty() => Some(s),
        _ => None,
    }
}

fn check_rating_range(v: &Value, path: &str, issues: &mut Vec<Issue>) {
    let best = v
        .get("bestRating")
        .and_then(as_decimal)
        .unwrap_or(Decimal::from(5));
    let worst = v
        .get("worstRating")
        .and_then(as_decimal)
        .unwrap_or(Decimal::ONE);
    if let Some(r) = v.get("ratingValue").and_then(as_decimal)
        && (r < worst || r > best)
    {
        issues.push(Issue::error(
            &format!("{path}.ratingValue"),
            "rating_out_of_range",
            format!("ratingValue {r} is outside {worst}..{best}"),
        ));
    }
}

fn parse_date_any(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::money::Currency;
    use rust_decimal_macros::dec;

    fn try_price(amount: Decimal) -> Money {
        Money::new(amount, Currency::TRY)
    }

    fn package() -> Product {
        Product {
            id: Some("https://acme.example/hosting/pro#product".into()),
            name: "Pro Hosting".into(),
            description: Some("20 GB NVMe, ücretsiz SSL".into()),
            url: Some("https://acme.example/hosting/pro".into()),
            brand: Some("Acme".into()),
            images: vec!["https://acme.example/pro.png".into()],
            offers: Some(Offers::Multiple(vec![
                Offer {
                    price_valid_until: NaiveDate::from_ymd_opt(2026, 12, 31),
                    setup_fee: Some(try_price(dec!(25))),
                    ..Offer::new(try_price(dec!(49.9))).with_cycle(BillingCycle::Monthly)
                },
                Offer::new(try_price(dec!(499))).with_cycle(BillingCycle::Annual),
            ])),
            properties: vec![PropertyValue::new("Disk", "20 GB NVMe")],
            ..Product::default()
        }
    }

    #[test]
    fn product_with_billing_specs() {
        let v = package().to_document();
        assert_eq!(v["@context"], CONTEXT);
        assert_eq!(v["@type"], "Product");
        let monthly = &v["offers"][0];
        assert_eq!(monthly["price"], "49.90");
        assert_eq!(monthly["priceCurrency"], "TRY");
        assert_eq!(monthly["priceValidUntil"], "2026-12-31");
        assert_eq!(monthly["availability"], "https://schema.org/InStock");
        let spec = &monthly["priceSpecification"][0];
        assert_eq!(spec["@type"], "UnitPriceSpecification");
        assert_eq!(spec["billingDuration"]["value"], 1);
        assert_eq!(spec["billingDuration"]["unitCode"], "MON");
        assert_eq!(
            monthly["priceSpecification"][1]["priceComponentType"],
            "https://schema.org/ActivationFee"
        );
        assert_eq!(
            v["offers"][1]["priceSpecification"][0]["referenceQuantity"]["value"],
            12
        );
        assert!(
            package()
                .validate()
                .iter()
                .all(|i| i.severity != IssueSeverity::Error)
        );
    }

    #[test]
    fn aggregate_offer_from_offers() {
        let agg = AggregateOffer::from_offers(vec![
            Offer::new(try_price(dec!(99))),
            Offer::new(try_price(dec!(19.5))),
            Offer::new(Money::new(dec!(1), Currency::USD)),
        ])
        .unwrap();
        let v = Offers::Aggregate(agg).to_value();
        assert_eq!(v["lowPrice"], "19.50");
        assert_eq!(v["highPrice"], "99.00");
        assert_eq!(v["offerCount"], 2);
    }

    #[test]
    fn script_tag_cannot_be_broken_out_of() {
        let faq = FaqPage {
            entries: vec![FaqEntry::new(
                "</script><script>alert('x')</script>",
                "Answer with \"quotes\" & <b>bold</b> <!-- comment",
            )],
            ..FaqPage::default()
        };
        let tag = faq.to_script();
        assert!(tag.starts_with("<script type=\"application/ld+json\">"));
        assert!(tag.ends_with("</script>"));
        let inner =
            &tag["<script type=\"application/ld+json\">".len()..tag.len() - "</script>".len()];
        assert!(!inner.contains("</"));
        assert!(!inner.contains("<!--"));
        let parsed: Value = serde_json::from_str(inner).unwrap();
        assert_eq!(
            parsed["mainEntity"][0]["name"],
            "</script><script>alert('x')</script>"
        );
        assert_eq!(
            parsed["mainEntity"][0]["acceptedAnswer"]["text"],
            "Answer with \"quotes\" & <b>bold</b> <!-- comment"
        );
    }

    #[test]
    fn validation_catches_missing_required() {
        let bare = Product {
            name: "X".into(),
            ..Product::default()
        };
        let issues = bare.validate();
        assert!(
            issues
                .iter()
                .any(|i| i.code == "product_needs_offer_review_or_rating")
        );

        let bad = json!({
            "@context": "https://schema.org",
            "@graph": [
                {"@type": "Product", "name": "P", "offers": {"@type": "Offer", "price": "₺49,90"}},
                {"@type": "AggregateRating", "ratingValue": 7},
                {"@type": "FAQPage", "mainEntity": [{"@type": "Question", "name": "Q?"}]},
                {"@type": "BreadcrumbList", "itemListElement": [
                    {"@type": "ListItem", "position": 1, "name": "Home"},
                    {"@type": "ListItem", "position": 2, "name": "VPS"}
                ]},
                {"@type": "WebSite", "name": "Acme", "url": "https://a.example",
                 "potentialAction": {"@type": "SearchAction",
                    "target": {"@type": "EntryPoint", "urlTemplate": "https://a.example/s?q={q}"},
                    "query-input": "required name=search_term_string"}},
                {"@type": "SoftwareApplication", "name": "Panel"}
            ]
        });
        let issues = validate_document(&bad);
        let codes: Vec<(&str, &str)> = issues
            .iter()
            .filter(|i| i.severity == IssueSeverity::Error)
            .map(|i| (i.path.as_str(), i.code.as_str()))
            .collect();
        assert!(
            codes.contains(&("@graph[0].offers.price", "invalid_price")),
            "{codes:?}"
        );
        assert!(codes.contains(&("@graph[0].offers.priceCurrency", "missing_required")));
        assert!(codes.contains(&("@graph[1].ratingCount", "missing_required")));
        assert!(codes.contains(&("@graph[1].ratingValue", "rating_out_of_range")));
        assert!(codes.contains(&(
            "@graph[2].mainEntity[0].acceptedAnswer.text",
            "missing_required"
        )));
        assert!(codes.contains(&("@graph[3].itemListElement[0].item", "missing_required")));
        assert!(codes.contains(&("@graph[4].potentialAction", "invalid_search_action")));
        assert!(codes.contains(&("@graph[5].offers.price", "missing_required")));
        assert!(codes.contains(&("@graph[5]", "app_needs_rating_or_review")));
        assert!(
            validate_document(&json!({"@type": "Thing"}))
                .iter()
                .any(|i| i.code == "missing_context")
        );
    }

    #[test]
    fn builders_produce_valid_markup() {
        let org = Organization {
            id: Some("https://acme.example/#organization".into()),
            logo: Some("https://acme.example/logo.png".into()),
            same_as: vec![
                "https://x.com/acme".into(),
                "https://www.linkedin.com/company/acme".into(),
            ],
            contact_points: vec![ContactPoint {
                contact_type: "customer support".into(),
                email: Some("destek@acme.example".into()),
                available_language: vec!["tr".into(), "en".into()],
                ..ContactPoint::default()
            }],
            address: Some(PostalAddress {
                line1: "Levent Mah.".into(),
                city: "İstanbul".into(),
                country_code: "TR".into(),
                ..PostalAddress::default()
            }),
            ..Organization::new("Acme Hosting", "https://acme.example/")
        };
        let site = WebSite {
            name: "Acme".into(),
            url: "https://acme.example/".into(),
            publisher_id: org.id.clone(),
            search_url_template: Some("https://acme.example/ara?q={search_term_string}".into()),
            in_language: vec!["tr".into()],
            ..WebSite::default()
        };
        let crumbs = BreadcrumbList {
            items: vec![
                Crumb::new("Ana sayfa", "https://acme.example/"),
                Crumb {
                    name: "VPS".into(),
                    url: None,
                },
            ],
        };
        let article = Article {
            kind: ArticleKind::BlogPosting,
            headline: "VPS nedir?".into(),
            url: Some("https://acme.example/blog/vps-nedir".into()),
            images: vec!["https://acme.example/vps.png".into()],
            date_published: DateTime::parse_from_rfc3339("2025-03-01T10:00:00Z")
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            date_modified: DateTime::parse_from_rfc3339("2025-04-01T10:00:00Z")
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            authors: vec![
                Entity::person("Ayşe Yılmaz").with_url("https://acme.example/yazar/ayse"),
            ],
            publisher: Some(
                Entity::organization("Acme").with_id("https://acme.example/#organization"),
            ),
            ..Article::default()
        };
        let howto = HowTo {
            name: "cPanel'de e-posta hesabı oluşturma".into(),
            steps: vec![
                HowToStep {
                    text: "cPanel'e giriş yapın.".into(),
                    ..HowToStep::default()
                },
                HowToStep {
                    text: "E-posta Hesapları'na tıklayın.".into(),
                    ..HowToStep::default()
                },
            ],
            ..HowTo::default()
        };
        let app = SoftwareApplication {
            name: "Acme Panel".into(),
            application_category: Some("BusinessApplication".into()),
            operating_system: Some("Web".into()),
            offers: Some(Offers::Single(Offer::new(try_price(dec!(0))))),
            aggregate_rating: Some(AggregateRating {
                rating_value: dec!(4.7),
                rating_count: Some(120),
                review_count: None,
                best_rating: None,
                worst_rating: None,
            }),
            ..SoftwareApplication::default()
        };
        let list = ItemList {
            name: Some("Paketler".into()),
            items: vec![ListEntry {
                url: "https://acme.example/a".into(),
                name: None,
            }],
        };
        let graph = Graph::new()
            .with(&org)
            .with(&site)
            .with(&crumbs)
            .with(&article)
            .with(&howto)
            .with(&app)
            .with(&list)
            .with(&package());
        let errors: Vec<Issue> = graph
            .validate()
            .into_iter()
            .filter(|i| i.severity == IssueSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let doc = graph.to_document();
        assert_eq!(doc["@graph"][0]["address"]["addressCountry"], "TR");
        assert_eq!(
            doc["@graph"][1]["publisher"]["@id"],
            "https://acme.example/#organization"
        );
        assert_eq!(doc["@graph"][2]["itemListElement"][1]["position"], 2);
        assert!(doc["@graph"][2]["itemListElement"][1].get("item").is_none());
        assert_eq!(doc["@graph"][5]["aggregateRating"]["ratingValue"], 4.7);
        assert_eq!(doc["@graph"][5]["offers"]["price"], "0.00");
    }

    #[test]
    fn reviews_need_author_and_rating() {
        let r = Review {
            author: Entity::person(""),
            rating_value: None,
            best_rating: None,
            body: Some("Harika".into()),
            name: None,
            date_published: None,
        };
        let codes: Vec<String> = r.validate().into_iter().map(|i| i.path).collect();
        assert!(codes.contains(&"Review.author".to_owned()));
        assert!(codes.contains(&"Review.reviewRating.ratingValue".to_owned()));
    }
}
