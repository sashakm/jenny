//! Who is calling, and the two ways that can fail.
//!
//! [`Principal`] is the identity a driving adapter attaches to an invocation and the
//! domain reads back out of the [`Context`](crate::port::Context) when a rule depends
//! on who is asking. It is deliberately a bag of claims rather than a fixed struct:
//! the framework cannot know whether your identities carry tenants, roles, scopes, or
//! employee numbers.
//!
//! The two failure types are kept apart because they mean genuinely different things
//! and map to different statuses:
//!
//! - [`AuthenticationError`] — "I could not work out who you are" (401).
//! - [`AuthorizationError`] — "I know who you are, and you may not do this" (403).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fmt::Display};
use thiserror::Error;

/// The caller could not be identified.
#[derive(Debug, Error)]
#[error("{}", _0)]
pub struct AuthenticationError(pub String);

/// The caller is identified but not permitted.
#[derive(Debug, Error)]
#[error("{}", _0)]
pub struct AuthorizationError(pub String);

/// Raw credentials lifted off an inbound request, before they mean anything.
///
/// Extracting these is a per-protocol job (an HTTP `Authorization` header, a message
/// header, a CLI flag); verifying them is the job of an
/// [`Authenticator`](crate::port::outbound::Authenticator) implementation in a driven
/// adapter. This type is the handover point between the two.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum Credentials {
    JsonWebToken(String),
    ApiKey(String),
}

impl Credentials {
    /// The credential material itself, without the scheme.
    pub fn secret(&self) -> &str {
        match self {
            Credentials::ApiKey(value) | Credentials::JsonWebToken(value) => value,
        }
    }
}

impl Display for Credentials {
    /// Renders the *scheme* only. The secret is never part of the display output, so
    /// that logging a `Credentials` cannot leak it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = match self {
            Credentials::ApiKey(_) => "ApiKey",
            Credentials::JsonWebToken(_) => "JsonWebToken",
        };
        write!(f, "{scheme}")
    }
}

/// Which kind of identity a [`Principal`] is.
///
/// The kind is not part of the id, because an id is a string any credential can carry:
/// a JWT whose `sub` is `System[my-notes/tasks]` is still a user. Policy engines see the
/// kind as a separate entity type (`User`, `System` in Cedar), which no token can forge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    /// Someone or something that authenticated: a token's subject, an API key's owner,
    /// or a caller restored from an event. Every authenticator builds these.
    #[default]
    User,
    /// The service itself, acting on its own: a scheduled task, an inbound event
    /// handler. Only [`Principal::system`] builds one.
    System,
}

/// An identity interacting with the service.
///
/// Carries a stable `id` plus whatever else the authenticating adapter knew — JWT
/// claims, API-key metadata, tenant assignments. Domain code reads attributes by name
/// and type; nothing forces a shape on them.
///
/// It also has a [`kind`](Self::kind): a [`User`](PrincipalKind::User), as every
/// authenticator and [`PrincipalBuilder`] builds, or the service itself
/// ([`Principal::system`]). The kind is never serialised or deserialised, so a stored
/// API-key record or a JWT always yields a user.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Principal {
    #[serde(alias = "sub")]
    #[serde(alias = "user_id")]
    #[serde(alias = "userId")]
    id: String,
    #[serde(skip)]
    kind: PrincipalKind,
    #[serde(default, flatten)]
    attributes: HashMap<String, Value>,
}

impl Principal {
    /// The service acting on its own, as `component`: `"tasks"` for scheduled tasks,
    /// `"events"` for inbound event handlers.
    ///
    /// Its [`id`](Self::id) is `<service>/<component>` and it displays as
    /// `System[<service>/<component>]`, which is also what an event it raises carries as
    /// `caller-subject`. Cedar addresses it as `System::"<service>/<component>"`.
    ///
    /// ```
    /// use jenny_core::domain::{Principal, PrincipalKind};
    ///
    /// let tasks = Principal::system("my-notes", "tasks");
    /// assert_eq!(tasks.kind(), PrincipalKind::System);
    /// assert_eq!(tasks.id(), "my-notes/tasks");
    /// assert_eq!(tasks.to_string(), "System[my-notes/tasks]");
    /// ```
    pub fn system(service: impl AsRef<str>, component: impl AsRef<str>) -> Self {
        Self {
            id: format!("{}/{}", service.as_ref(), component.as_ref()),
            kind: PrincipalKind::System,
            attributes: HashMap::new(),
        }
    }

    /// Replace the principal's [`roles`](Self::roles). The runtime gives system
    /// principals the roles configured under `system.roles`.
    pub fn with_roles(mut self, roles: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let roles: Vec<String> = roles.into_iter().map(Into::into).collect();
        self.attributes.remove("groups");
        self.attributes
            .insert("roles".to_string(), Value::from(roles));
        self
    }

    /// Whether this is a user or the service itself.
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    /// `true` for the service acting on its own ([`Principal::system`]).
    pub fn is_system(&self) -> bool {
        self.kind == PrincipalKind::System
    }

    /// How the principal is named as the caller of something, such as in an event's
    /// `caller-subject` header: the [`id`](Self::id) of a user, `System[<id>]` for the
    /// service itself.
    pub fn subject(&self) -> String {
        match self.kind {
            PrincipalKind::User => self.id.clone(),
            PrincipalKind::System => format!("System[{}]", self.id),
        }
    }

    /// The (ideally unique) identifier of this identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Read an attribute, deserialised into `T`. `None` when absent or the wrong shape.
    pub fn attribute<T>(&self, key: &str) -> Option<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.attributes
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Read an attribute without deserialising it.
    pub fn raw_attribute(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }

    pub fn has_attribute(&self, key: &str) -> bool {
        self.attributes.contains_key(key)
    }

    pub fn attributes(&self) -> &HashMap<String, Value> {
        &self.attributes
    }

    /// The identity's role names.
    ///
    /// Reads `roles` first, then `groups`, so that principals minted from OIDC tokens
    /// (which conventionally carry `roles`) and from directory-backed stores (which
    /// conventionally carry `groups`) both work without the caller having to know
    /// which one it got. Returns an empty slice's worth of roles when neither is
    /// present.
    pub fn roles(&self) -> Vec<String> {
        self.attribute("roles")
            .or_else(|| self.attribute("groups"))
            .unwrap_or_default()
    }

    /// `true` when [`roles`](Self::roles) contains `role`.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles().iter().any(|held| held == role)
    }

    /// Start building a principal. Same as [`PrincipalBuilder::new`].
    pub fn builder(id: impl Into<String>) -> PrincipalBuilder {
        PrincipalBuilder::new(id)
    }
}

impl Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_system() {
            return write!(f, "System[{}]", self.id);
        }
        let name: String = self
            .attribute("name")
            .or_else(|| self.attribute("preferred_username"))
            .unwrap_or_else(|| self.id.clone());
        write!(f, "Principal({name})")
    }
}

#[derive(Debug, Clone)]
pub struct PrincipalBuilder {
    id: String,
    attributes: HashMap<String, Value>,
}

impl PrincipalBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            attributes: HashMap::default(),
        }
    }

    /// Insert or replace a single attribute. Values that cannot be serialised are
    /// skipped rather than failing the build.
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        if let Ok(json) = serde_json::to_value(value) {
            self.attributes.insert(key.into(), json);
        }
        self
    }

    /// Insert or replace several attributes at once.
    pub fn with_attributes(
        mut self,
        attrs: impl IntoIterator<Item = (impl Into<String>, impl Serialize)>,
    ) -> Self {
        self.attributes.extend(
            attrs
                .into_iter()
                .filter_map(|(k, v)| serde_json::to_value(v).ok().map(|v| (k.into(), v))),
        );
        self
    }

    /// Always a [`User`](PrincipalKind::User): only [`Principal::system`] makes the
    /// service itself.
    pub fn build(self) -> Principal {
        Principal {
            id: self.id,
            kind: PrincipalKind::User,
            attributes: self.attributes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn display_prefers_name_then_username_then_id() {
        let id = Uuid::new_v4().to_string();
        let bare = PrincipalBuilder::new(id.clone());
        assert_eq!(bare.clone().build().id(), &id);
        assert_eq!(bare.clone().build().to_string(), format!("Principal({id})"));

        let with_username = bare.with_attribute("preferred_username", "SArcher");
        assert_eq!(
            with_username.clone().build().to_string(),
            "Principal(SArcher)"
        );

        let with_name = with_username.with_attributes(vec![("name", "Sterling Archer")]);
        assert_eq!(with_name.build().to_string(), "Principal(Sterling Archer)");
    }

    #[test]
    fn roles_falls_back_from_roles_to_groups() {
        let from_roles = Principal::builder("a")
            .with_attribute("roles", vec!["Admin"])
            .build();
        assert_eq!(from_roles.roles(), vec!["Admin".to_string()]);
        assert!(from_roles.has_role("Admin"));

        let from_groups = Principal::builder("b")
            .with_attribute("groups", vec!["Dispatch"])
            .build();
        assert_eq!(from_groups.roles(), vec!["Dispatch".to_string()]);

        // `roles` wins when both are present.
        let both = Principal::builder("c")
            .with_attribute("roles", vec!["Admin"])
            .with_attribute("groups", vec!["Dispatch"])
            .build();
        assert_eq!(both.roles(), vec!["Admin".to_string()]);

        assert!(Principal::builder("d").build().roles().is_empty());
    }

    #[test]
    fn a_system_principal_is_its_own_kind() {
        let system = Principal::system("my-notes", "tasks");
        assert!(system.is_system());
        assert_eq!(system.id(), "my-notes/tasks");
        assert_eq!(system.subject(), "System[my-notes/tasks]");
        assert_eq!(system.to_string(), "System[my-notes/tasks]");
        assert!(system.roles().is_empty());

        let with_roles = system.with_roles(["Maintenance"]);
        assert!(with_roles.has_role("Maintenance"));
        assert!(with_roles.is_system());

        // The same id, built as anything else, is a user.
        let forged = Principal::builder("my-notes/tasks").build();
        assert_eq!(forged.kind(), PrincipalKind::User);
        assert_ne!(forged, Principal::system("my-notes", "tasks"));
        assert_eq!(forged.subject(), "my-notes/tasks");
    }

    #[test]
    fn deserialising_always_yields_a_user() {
        // Whatever a token or a stored record claims.
        let principal: Principal = serde_json::from_value(serde_json::json!({
            "sub": "System[my-notes/tasks]",
            "kind": "System",
        }))
        .unwrap();
        assert_eq!(principal.kind(), PrincipalKind::User);
        assert_eq!(principal.subject(), "System[my-notes/tasks]");
        assert_eq!(
            principal.raw_attribute("kind"),
            Some(&serde_json::json!("System"))
        );

        // And a system principal serialises without its kind.
        let json = serde_json::to_value(Principal::system("my-notes", "tasks")).unwrap();
        assert_eq!(json, serde_json::json!({ "id": "my-notes/tasks" }));
    }

    #[test]
    fn credentials_display_hides_the_secret() {
        let key = Credentials::ApiKey("super-secret".to_string());
        assert_eq!(key.to_string(), "ApiKey");
        assert!(!key.to_string().contains("super-secret"));
        assert_eq!(key.secret(), "super-secret");
        assert_eq!(
            Credentials::JsonWebToken("eyJ".into()).to_string(),
            "JsonWebToken"
        );
    }
}
