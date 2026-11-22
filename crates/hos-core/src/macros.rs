//! Declarative macros shared across the workspace.
//!
//! * [`define_id!`](crate::define_id) — strongly typed UUID v7 newtype ids.
//! * [`string_enum!`](crate::string_enum) — fieldless enums stored as `text` in
//!   Postgres (`as_str`/`FromStr`/serde/sqlx all derived from one table).
//!
//! Both macros are exported so other crates (e.g. `hos-db`, `hos-auth`) can define
//! their own ids/enums with identical behaviour. The generated sqlx impls are
//! selected by **hos-core's** `sqlx` feature, so a downstream crate needs no
//! direct sqlx dependency for them to work.

/// Defines a UUID-v7 backed identifier newtype.
///
/// Generated API (for `define_id!(pub struct FooId;)`):
/// * `FooId::new()` — fresh time-ordered UUID v7,
/// * `FooId::from_uuid(Uuid)`, `as_uuid()`, `into_uuid()`, `created_at()`,
/// * `Display` / `FromStr` (hyphenated UUID, error [`ParseIdError`](crate::ids::ParseIdError)),
/// * serde: transparent (a plain UUID string),
/// * with hos-core's `sqlx` feature: Postgres `Type`/`Encode`/`Decode`/`PgHasArrayType`
///   as `uuid` (so `Vec<FooId>` binds to `uuid[]`).
///
/// ```
/// hos_core::define_id! {
///     /// Identifier of a widget.
///     pub struct WidgetId;
/// }
/// let id = WidgetId::new();
/// assert_eq!(id, id.to_string().parse().unwrap());
/// ```
#[macro_export]
macro_rules! define_id {
    ($(#[$meta:meta])* $vis:vis struct $name:ident;) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(transparent)]
        $vis struct $name($crate::__private::uuid::Uuid);

        #[allow(clippy::new_without_default, dead_code)]
        impl $name {
            /// Generates a new, time-ordered (UUID v7) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self($crate::__private::uuid::Uuid::now_v7())
            }

            /// Wraps an existing UUID (e.g. read from the database).
            #[must_use]
            pub const fn from_uuid(uuid: $crate::__private::uuid::Uuid) -> Self {
                Self(uuid)
            }

            /// Borrows the underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> &$crate::__private::uuid::Uuid {
                &self.0
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn into_uuid(self) -> $crate::__private::uuid::Uuid {
                self.0
            }

            /// Creation time embedded in a UUID v7 (`None` for other versions).
            #[must_use]
            pub fn created_at(
                &self,
            ) -> ::core::option::Option<$crate::__private::chrono::DateTime<$crate::__private::chrono::Utc>>
            {
                let (secs, nanos) = self.0.get_timestamp()?.to_unix();
                let secs = i64::try_from(secs).ok()?;
                $crate::__private::chrono::DateTime::from_timestamp(secs, nanos)
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::ids::ParseIdError;

            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                $crate::__private::uuid::Uuid::parse_str(s.trim())
                    .map(Self)
                    .map_err(|source| $crate::ids::ParseIdError::new(stringify!($name), source))
            }
        }

        impl ::core::convert::From<$crate::__private::uuid::Uuid> for $name {
            fn from(uuid: $crate::__private::uuid::Uuid) -> Self {
                Self(uuid)
            }
        }

        impl ::core::convert::From<$name> for $crate::__private::uuid::Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl ::core::convert::AsRef<$crate::__private::uuid::Uuid> for $name {
            fn as_ref(&self) -> &$crate::__private::uuid::Uuid {
                &self.0
            }
        }

        impl $crate::__private::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::core::result::Result<S::Ok, S::Error>
            where
                S: $crate::__private::serde::Serializer,
            {
                $crate::__private::serde::Serialize::serialize(&self.0, serializer)
            }
        }

        impl<'de> $crate::__private::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: $crate::__private::serde::Deserializer<'de>,
            {
                <$crate::__private::uuid::Uuid as $crate::__private::serde::Deserialize<'de>>::deserialize(
                    deserializer,
                )
                .map(Self)
            }
        }

        $crate::__sqlx_uuid_newtype!($name);
    };
}

/// Defines a fieldless enum whose canonical representation is a fixed string
/// (stored in Postgres as `text` with a `CHECK` constraint, never a PG enum).
///
/// Generated API: `ALL`, `as_str()`, `Display`, `FromStr` (error
/// [`ParseEnumError`](crate::error::ParseEnumError)), `TryFrom<&str>`, serde as the
/// string, and (with hos-core's `sqlx` feature) Postgres `text` `Type`/`Encode`/
/// `Decode`/`PgHasArrayType`. Derives `Debug, Clone, Copy, PartialEq, Eq, Hash,
/// PartialOrd, Ord`.
///
/// ```
/// hos_core::string_enum! {
///     /// Colour of a widget.
///     pub enum Colour {
///         /// Red.
///         Red => "red",
///         /// Dark blue.
///         DarkBlue => "dark_blue",
///     }
/// }
/// assert_eq!(Colour::DarkBlue.as_str(), "dark_blue");
/// assert_eq!("red".parse::<Colour>().unwrap(), Colour::Red);
/// ```
#[macro_export]
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident => $value:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant,
            )+
        }

        #[allow(dead_code)]
        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// Canonical string form (the value stored in the database / sent over JSON).
            #[must_use]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $value,)+
                }
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::error::ParseEnumError;

            fn from_str(s: &str) -> ::core::result::Result<Self, $crate::error::ParseEnumError> {
                match s {
                    $($value => ::core::result::Result::Ok($name::$variant),)+
                    other => ::core::result::Result::Err(
                        $crate::error::ParseEnumError::new(stringify!($name), other),
                    ),
                }
            }
        }

        impl ::core::convert::TryFrom<&str> for $name {
            type Error = $crate::error::ParseEnumError;

            fn try_from(s: &str) -> ::core::result::Result<Self, $crate::error::ParseEnumError> {
                s.parse()
            }
        }

        impl $crate::__private::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::core::result::Result<S::Ok, S::Error>
            where
                S: $crate::__private::serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> $crate::__private::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: $crate::__private::serde::Deserializer<'de>,
            {
                let s = <::std::borrow::Cow<'de, str> as $crate::__private::serde::Deserialize<'de>>::deserialize(
                    deserializer,
                )?;
                s.parse().map_err(<D::Error as $crate::__private::serde::de::Error>::custom)
            }
        }

        $crate::__sqlx_text_newtype!($name);
    };
}

/// Internal: Postgres `uuid` impls for id newtypes (no-op without hos-core's `sqlx` feature).
#[cfg(feature = "sqlx")]
#[doc(hidden)]
#[macro_export]
macro_rules! __sqlx_uuid_newtype {
    ($name:ident) => {
        impl $crate::__private::sqlx::Type<$crate::__private::sqlx::Postgres> for $name {
            fn type_info() -> $crate::__private::sqlx::postgres::PgTypeInfo {
                <$crate::__private::uuid::Uuid as $crate::__private::sqlx::Type<
                    $crate::__private::sqlx::Postgres,
                >>::type_info()
            }

            fn compatible(ty: &$crate::__private::sqlx::postgres::PgTypeInfo) -> bool {
                <$crate::__private::uuid::Uuid as $crate::__private::sqlx::Type<
                    $crate::__private::sqlx::Postgres,
                >>::compatible(ty)
            }
        }

        impl<'q> $crate::__private::sqlx::Encode<'q, $crate::__private::sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut $crate::__private::sqlx::postgres::PgArgumentBuffer,
            ) -> ::core::result::Result<
                $crate::__private::sqlx::encode::IsNull,
                $crate::__private::sqlx::error::BoxDynError,
            > {
                <$crate::__private::uuid::Uuid as $crate::__private::sqlx::Encode<
                    'q,
                    $crate::__private::sqlx::Postgres,
                >>::encode_by_ref(&self.0, buf)
            }
        }

        impl<'r> $crate::__private::sqlx::Decode<'r, $crate::__private::sqlx::Postgres> for $name {
            fn decode(
                value: $crate::__private::sqlx::postgres::PgValueRef<'r>,
            ) -> ::core::result::Result<Self, $crate::__private::sqlx::error::BoxDynError> {
                <$crate::__private::uuid::Uuid as $crate::__private::sqlx::Decode<
                    'r,
                    $crate::__private::sqlx::Postgres,
                >>::decode(value)
                .map(Self)
            }
        }

        impl $crate::__private::sqlx::postgres::PgHasArrayType for $name {
            fn array_type_info() -> $crate::__private::sqlx::postgres::PgTypeInfo {
                <$crate::__private::uuid::Uuid as $crate::__private::sqlx::postgres::PgHasArrayType>::array_type_info()
            }
        }
    };
}

/// Internal: no-op variant used when hos-core is built without `sqlx`.
#[cfg(not(feature = "sqlx"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __sqlx_uuid_newtype {
    ($name:ident) => {};
}

/// Internal: Postgres `text` impls for string-backed types exposing `as_str()` + `FromStr`.
#[cfg(feature = "sqlx")]
#[doc(hidden)]
#[macro_export]
macro_rules! __sqlx_text_newtype {
    ($name:ident) => {
        impl $crate::__private::sqlx::Type<$crate::__private::sqlx::Postgres> for $name {
            fn type_info() -> $crate::__private::sqlx::postgres::PgTypeInfo {
                <&str as $crate::__private::sqlx::Type<$crate::__private::sqlx::Postgres>>::type_info()
            }

            fn compatible(ty: &$crate::__private::sqlx::postgres::PgTypeInfo) -> bool {
                <&str as $crate::__private::sqlx::Type<$crate::__private::sqlx::Postgres>>::compatible(ty)
            }
        }

        impl<'q> $crate::__private::sqlx::Encode<'q, $crate::__private::sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut $crate::__private::sqlx::postgres::PgArgumentBuffer,
            ) -> ::core::result::Result<
                $crate::__private::sqlx::encode::IsNull,
                $crate::__private::sqlx::error::BoxDynError,
            > {
                <&str as $crate::__private::sqlx::Encode<'q, $crate::__private::sqlx::Postgres>>::encode_by_ref(
                    &self.as_str(),
                    buf,
                )
            }
        }

        impl<'r> $crate::__private::sqlx::Decode<'r, $crate::__private::sqlx::Postgres> for $name {
            fn decode(
                value: $crate::__private::sqlx::postgres::PgValueRef<'r>,
            ) -> ::core::result::Result<Self, $crate::__private::sqlx::error::BoxDynError> {
                let s = <&str as $crate::__private::sqlx::Decode<'r, $crate::__private::sqlx::Postgres>>::decode(
                    value,
                )?;
                ::core::result::Result::Ok(s.parse::<$name>()?)
            }
        }

        impl $crate::__private::sqlx::postgres::PgHasArrayType for $name {
            fn array_type_info() -> $crate::__private::sqlx::postgres::PgTypeInfo {
                <&str as $crate::__private::sqlx::postgres::PgHasArrayType>::array_type_info()
            }
        }
    };
}

/// Internal: no-op variant used when hos-core is built without `sqlx`.
#[cfg(not(feature = "sqlx"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __sqlx_text_newtype {
    ($name:ident) => {};
}
