//! Policy-based authorization behind the [`Authorizer`] port, evaluated by
//! [Cedar](https://www.cedarpolicy.com).
//!
//! Policies are plain Cedar. What this module adds is the translation from jenny's
//! engine-neutral [`AccessRequest`] to a Cedar request:
//!
//! | Cedar | Built from |
//! |---|---|
//! | `principal` | `User::"<Principal::id>"`, a member of `Role::"<name>"` for each of [`Principal::roles`]. Its attributes are the principal's claims plus `id`. |
//! | `action` | `Action::"<AccessRequest::action>"`: the HTTP method for the REST gate, a verb such as `"archive"` for a use case |
//! | `resource` | `<Resource::kind>::"<Resource::id>"`, a member of each of its parents, with its attributes |
//! | `context` | [`AccessRequest::context`] |
//!
//! `jenny-rest`'s `Authorization` layer asks about `Route::"/api/orders/{id}"`, the
//! route template rather than the concrete path, inside `Resource::"urn:shop:orders"`
//! inside `Adapter::"urn:shop"`, with the concrete path in `context.path`. So a policy
//! can grant a single route, a whole resource or a whole adapter by exact match or `in`,
//! never by a string pattern that might cross a `/`:
//!
//! ```cedar
//! permit (principal in Role::"Admin", action, resource in Adapter::"urn:shop");
//!
//! permit (
//!   principal in Role::"Dispatch",
//!   action in [Action::"GET", Action::"PATCH"],
//!   resource == Route::"/api/orders/{id}/status"
//! );
//! ```
//!
//! ### With a schema
//!
//! [`with_schema`](CedarAuthorizer::with_schema) validates every policy against a Cedar
//! schema when the service starts, so a typo in an entity type or attribute name fails
//! the deployment instead of quietly never matching. From then on every request is
//! checked against the schema too, and the principal only carries the claims the schema
//! declares for `User`. Undeclared claims are dropped rather than failing every request.
//! `docs/data/cedar/` in the jenny repository has a schema for the HTTP gate to start from.
//!
//! ### Static entities
//!
//! Facts that do not come with a request, such as a role hierarchy, are loaded once with
//! [`with_entities`](CedarAuthorizer::with_entities) in Cedar's JSON entity format. An
//! entity loaded this way wins over one a request would contribute with the same uid, so
//! `Role::"Support"` declared as a member of `Role::"Dispatch"` keeps that membership
//! whoever holds it.
//!
//! ### Denials fail closed
//!
//! Anything that keeps a request from being evaluated (an id Cedar cannot represent, a
//! request the schema rejects) is a denial, logged at `error`. Cedar skips a policy whose
//! evaluation errors. For a `permit` that only withholds a grant, but a skipped `forbid`
//! would grant more than the policies say, so this adapter denies when a `forbid` errors.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_trait::async_trait;
use cedar_policy as cedar;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tracing::{debug, error, warn};

use jenny_core::domain::{AuthorizationError, Principal};
use jenny_core::port::outbound::{AccessRequest, Authorizer, Resource};

/// The entity type of the calling [`Principal`].
pub const PRINCIPAL_TYPE: &str = "User";
/// The entity type of each of the principal's roles.
pub const ROLE_TYPE: &str = "Role";
/// The entity type of actions. Cedar requires actions to be of a type named `Action`.
pub const ACTION_TYPE: &str = "Action";

/// The policies, schema or entities could not be loaded.
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("could not read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid Cedar policies in {origin}: {detail}")]
    Policies { origin: String, detail: String },
    #[error("invalid Cedar schema: {0}")]
    Schema(String),
    #[error("the policies do not validate against the schema:\n{0}")]
    Validation(String),
    #[error("invalid Cedar entities: {0}")]
    Entities(String),
}

/// An [`Authorizer`] that evaluates Cedar policies.
///
/// ```
/// use jenny_core::domain::Principal;
/// use jenny_core::port::outbound::{AccessRequest, Authorizer, Resource};
/// use jenny_security::CedarAuthorizer;
///
/// # #[tokio::main] async fn main() -> anyhow::Result<()> {
/// let authorizer = CedarAuthorizer::new(r#"
///     permit (principal in Role::"Dispatch", action == Action::"GET", resource is Route);
///     permit (principal, action == Action::"archive", resource is Note)
///       when { resource.owner == principal.id };
/// "#)?;
///
/// let dispatcher = Principal::builder("u-1").with_attribute("roles", ["Dispatch"]).build();
/// let route = Resource::new("Route", "/api/orders/{id}");
/// assert!(authorizer.authorize(&dispatcher, &AccessRequest::new("GET", route.clone())).await.is_ok());
/// assert!(authorizer.authorize(&dispatcher, &AccessRequest::new("DELETE", route)).await.is_err());
///
/// let own_note = Resource::new("Note", 7).with_attribute("owner", "u-1");
/// assert!(authorizer.authorize(&dispatcher, &AccessRequest::new("archive", own_note)).await.is_ok());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct CedarAuthorizer {
    policies: cedar::PolicySet,
    schema: Option<Schema>,
    /// Kept as JSON so that a schema added afterwards can re-parse it.
    entities_json: Option<Value>,
    entities: cedar::Entities,
    namespace: Option<String>,
    engine: cedar::Authorizer,
}

#[derive(Debug)]
struct Schema {
    schema: cedar::Schema,
    /// The attributes the schema declares for the principal type: the claims that are kept.
    principal_attributes: HashSet<String>,
}

impl CedarAuthorizer {
    /// Parse policies from Cedar source text.
    ///
    /// Each policy is identified by its `@id("…")` annotation in logs and diagnostics,
    /// falling back to its position (`policies#2`).
    pub fn new(policies: &str) -> Result<Self, PolicyError> {
        Ok(Self::from_policy_set(parse_policies(policies, "policies")?))
    }

    /// Load policies from a `.cedar` file, or from every `.cedar` file in a directory,
    /// in file-name order.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let path = path.as_ref();
        let files = if path.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(path)
                .map_err(|source| io_error(path, source))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|file| file.extension().is_some_and(|ext| ext == "cedar"))
                .collect();
            files.sort();
            files
        } else {
            vec![path.to_path_buf()]
        };

        let mut policies = cedar::PolicySet::new();
        for file in files {
            let source = read(&file)?;
            let origin = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string());
            policies
                .merge(&parse_policies(&source, &origin)?, false)
                .map_err(|e| PolicyError::Policies {
                    origin,
                    detail: e.to_string(),
                })?;
        }
        Ok(Self::from_policy_set(policies))
    }

    fn from_policy_set(policies: cedar::PolicySet) -> Self {
        Self {
            policies,
            schema: None,
            entities_json: None,
            entities: cedar::Entities::empty(),
            namespace: None,
            engine: cedar::Authorizer::new(),
        }
    }

    /// Put jenny's entity types (`User`, `Role`, `Action`, and resource kinds without a
    /// `::`) into a Cedar namespace, so that `User` becomes `Shop::User`.
    ///
    /// Call it before [`with_schema`](Self::with_schema), which reads the principal's
    /// declared attributes from that namespace.
    pub fn in_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Validate the policies against a schema, in Cedar's schema syntax or its JSON
    /// form, and check every request against it from now on.
    pub fn with_schema(mut self, schema: &str) -> Result<Self, PolicyError> {
        let fragment = if schema.trim_start().starts_with('{') {
            cedar::SchemaFragment::from_json_str(schema)
                .map_err(|e| PolicyError::Schema(e.to_string()))?
        } else {
            let (fragment, warnings) = cedar::SchemaFragment::from_cedarschema_str(schema)
                .map_err(|e| PolicyError::Schema(e.to_string()))?;
            for warning in warnings {
                warn!("cedar schema: {warning}");
            }
            fragment
        };
        let described = fragment
            .to_json_string()
            .ok()
            .and_then(|json| serde_json::from_str::<Value>(&json).ok())
            .unwrap_or_default();
        let principal_attributes = declared_attributes(
            &described,
            self.namespace.as_deref().unwrap_or_default(),
            PRINCIPAL_TYPE,
        );
        let schema = cedar::Schema::from_schema_fragments([fragment])
            .map_err(|e| PolicyError::Schema(e.to_string()))?;

        let result = cedar::Validator::new(schema.clone())
            .validate(&self.policies, cedar::ValidationMode::Strict);
        if !result.validation_passed() {
            let errors: Vec<String> = result
                .validation_errors()
                .map(|e| format!("  - {e}"))
                .collect();
            return Err(PolicyError::Validation(errors.join("\n")));
        }
        for warning in result.validation_warnings() {
            warn!("cedar policies: {warning}");
        }

        self.schema = Some(Schema {
            schema,
            principal_attributes,
        });
        self.rebuild_entities()?;
        Ok(self)
    }

    /// [`with_schema`](Self::with_schema), read from a file.
    pub fn with_schema_file(self, path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let schema = read(path.as_ref())?;
        self.with_schema(&schema)
    }

    /// Entities every request can see, in Cedar's JSON entity format: a role hierarchy,
    /// action groups, the adapters and resources a policy refers to.
    pub fn with_entities(mut self, json: &str) -> Result<Self, PolicyError> {
        self.entities_json =
            Some(serde_json::from_str(json).map_err(|e| PolicyError::Entities(e.to_string()))?);
        self.rebuild_entities()?;
        Ok(self)
    }

    /// [`with_entities`](Self::with_entities), read from a file.
    pub fn with_entities_file(self, path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let json = read(path.as_ref())?;
        self.with_entities(&json)
    }

    /// The policies in force, for inspection.
    pub fn policies(&self) -> &cedar::PolicySet {
        &self.policies
    }

    fn schema(&self) -> Option<&cedar::Schema> {
        self.schema.as_ref().map(|schema| &schema.schema)
    }

    /// The static entities, plus the action entities a schema declares (which is where
    /// action groups come from).
    fn rebuild_entities(&mut self) -> Result<(), PolicyError> {
        let invalid =
            |e: cedar::entities_errors::EntitiesError| PolicyError::Entities(e.to_string());
        let mut entities = match self.schema() {
            Some(schema) => schema.action_entities().map_err(invalid)?,
            None => cedar::Entities::empty(),
        };
        if let Some(json) = self.entities_json.clone() {
            let loaded = cedar::Entities::from_json_value(json, self.schema()).map_err(invalid)?;
            entities = entities
                .add_entities(loaded.iter().cloned(), self.schema())
                .map_err(invalid)?;
        }
        self.entities = entities;
        Ok(())
    }

    fn type_name(&self, kind: &str) -> Result<cedar::EntityTypeName, String> {
        let qualified = match &self.namespace {
            Some(namespace) if !kind.contains("::") => format!("{namespace}::{kind}"),
            _ => kind.to_string(),
        };
        cedar::EntityTypeName::from_str(&qualified)
            .map_err(|e| format!("`{qualified}` is not a Cedar entity type name: {e}"))
    }

    fn uid(&self, kind: &str, id: &str) -> Result<cedar::EntityUid, String> {
        Ok(cedar::EntityUid::from_type_name_and_id(
            self.type_name(kind)?,
            cedar::EntityId::new(id),
        ))
    }

    /// The Cedar request for `request`, and the entities it is evaluated against.
    fn query(
        &self,
        principal: &Principal,
        request: &AccessRequest,
    ) -> Result<(cedar::Request, cedar::Entities), String> {
        let schema = self.schema();

        let principal_uid = self.uid(PRINCIPAL_TYPE, principal.id())?;
        let roles = principal
            .roles()
            .iter()
            .map(|role| self.uid(ROLE_TYPE, role))
            .collect::<Result<Vec<_>, _>>()?;

        let mut attributes: Map<String, Value> = principal
            .attributes()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        attributes.insert("id".to_string(), Value::String(principal.id().to_string()));
        if let Some(schema) = &self.schema {
            attributes.retain(|key, _| schema.principal_attributes.contains(key));
        }

        let mut contributed = vec![self.entity(&principal_uid, attributes, &roles)?];
        contributed.extend(
            roles
                .iter()
                .map(|role| cedar::Entity::new_no_attrs(role.clone(), HashSet::new())),
        );
        let resource_uid = self.resource_entities(request.resource(), &mut contributed)?;

        // A static entity wins over a contributed one with the same uid, and a uid
        // contributed twice (a parent shared by two branches) is only added once.
        let mut seen = HashSet::new();
        let contributed: Vec<cedar::Entity> = contributed
            .into_iter()
            .filter(|entity| {
                self.entities.get(&entity.uid()).is_none() && seen.insert(entity.uid())
            })
            .collect();
        let entities = self
            .entities
            .clone()
            .add_entities(contributed, schema)
            .map_err(|e| e.to_string())?;

        let action_uid = self.uid(ACTION_TYPE, request.action())?;
        let context =
            match schema {
                Some(schema) => cedar::Context::from_json_value(
                    Value::Object(request.context().clone()),
                    Some((schema, &action_uid)),
                )
                .map_err(|e| e.to_string())?,
                None => cedar::Context::from_pairs(request.context().iter().filter_map(
                    |(key, value)| expression(value).map(|value| (key.clone(), value)),
                ))
                .map_err(|e| e.to_string())?,
            };

        let request = cedar::Request::new(principal_uid, action_uid, resource_uid, context, schema)
            .map_err(|e| e.to_string())?;
        Ok((request, entities))
    }

    /// Push `resource` and its ancestors onto `entities`, returning its uid.
    fn resource_entities(
        &self,
        resource: &Resource,
        entities: &mut Vec<cedar::Entity>,
    ) -> Result<cedar::EntityUid, String> {
        let uid = self.uid(resource.kind(), resource.id())?;
        let parents = resource
            .parents()
            .iter()
            .map(|parent| self.resource_entities(parent, entities))
            .collect::<Result<Vec<_>, _>>()?;
        entities.push(self.entity(&uid, resource.attributes().clone(), &parents)?);
        Ok(uid)
    }

    /// With a schema, the attributes are parsed as the schema declares them, which is
    /// what turns a string into a `datetime` or an `ip`. Without one they are taken
    /// literally: a JSON object is a record, never an entity reference.
    fn entity(
        &self,
        uid: &cedar::EntityUid,
        attributes: Map<String, Value>,
        parents: &[cedar::EntityUid],
    ) -> Result<cedar::Entity, String> {
        match self.schema() {
            Some(schema) => {
                let attributes: Map<String, Value> = attributes
                    .into_iter()
                    .filter(|(_, value)| !value.is_null())
                    .collect();
                let parents: Vec<Value> = parents
                    .iter()
                    .map(|parent| {
                        json!({ "type": parent.type_name().to_string(), "id": parent.id().unescaped() })
                    })
                    .collect();
                cedar::Entity::from_json_value(
                    json!({
                        "uid": { "type": uid.type_name().to_string(), "id": uid.id().unescaped() },
                        "attrs": attributes,
                        "parents": parents,
                    }),
                    Some(schema),
                )
                .map_err(|e| e.to_string())
            }
            None => {
                let attributes: HashMap<String, cedar::RestrictedExpression> = attributes
                    .iter()
                    .filter_map(|(key, value)| expression(value).map(|value| (key.clone(), value)))
                    .collect();
                cedar::Entity::new(uid.clone(), attributes, parents.iter().cloned().collect())
                    .map_err(|e| e.to_string())
            }
        }
    }
}

#[async_trait]
impl Authorizer for CedarAuthorizer {
    async fn authorize(
        &self,
        principal: &Principal,
        request: &AccessRequest,
    ) -> Result<(), AuthorizationError> {
        let denied = || {
            AuthorizationError(format!(
                "{principal} may not {} {}",
                request.action(),
                request.resource()
            ))
        };

        let (query, entities) = self.query(principal, request).map_err(|reason| {
            error!(
                principal = principal.id(),
                action = request.action(),
                resource = %request.resource(),
                "authorization request could not be evaluated, denying: {reason}"
            );
            denied()
        })?;

        let response = self.engine.is_authorized(&query, &self.policies, &entities);
        let mut forbid_failed = false;
        for failure in response.diagnostics().errors() {
            let cedar::AuthorizationError::PolicyEvaluationError(failure) = failure;
            let forbids = self
                .policies
                .policy(failure.policy_id())
                .is_some_and(|policy| policy.effect() == cedar::Effect::Forbid);
            forbid_failed |= forbids;
            warn!(policy = %failure.policy_id(), "skipped a policy that failed to evaluate: {failure}");
        }

        let reasons: Vec<String> = response
            .diagnostics()
            .reason()
            .map(ToString::to_string)
            .collect();
        match response.decision() {
            cedar::Decision::Allow if !forbid_failed => {
                debug!(principal = principal.id(), action = request.action(), resource = %request.resource(), policies = ?reasons, "authorized");
                Ok(())
            }
            _ => {
                debug!(principal = principal.id(), action = request.action(), resource = %request.resource(), forbidden_by = ?reasons, forbid_failed, "denied");
                Err(denied())
            }
        }
    }
}

/// Parse `source`, naming each policy by its `@id` annotation or its position.
fn parse_policies(source: &str, origin: &str) -> Result<cedar::PolicySet, PolicyError> {
    let invalid = |detail: String| PolicyError::Policies {
        origin: origin.to_string(),
        detail,
    };
    let parsed = cedar::PolicySet::from_str(source).map_err(|e| invalid(e.to_string()))?;
    if parsed.templates().next().is_some() {
        return Err(invalid(
            "policy templates need linking, which this adapter does not do; write static policies"
                .into(),
        ));
    }

    let mut policies = cedar::PolicySet::new();
    for (position, policy) in parsed.policies().enumerate() {
        let id = policy
            .annotation("id")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{origin}#{position}"));
        policies
            .add(policy.new_id(cedar::PolicyId::new(id)))
            .map_err(|e| invalid(e.to_string()))?;
    }
    Ok(policies)
}

/// The attributes `namespace::type_name` declares, read from a schema's JSON form.
fn declared_attributes(schema: &Value, namespace: &str, type_name: &str) -> HashSet<String> {
    let declarations = &schema[namespace];
    let mut shape = &declarations["entityTypes"][type_name]["shape"];
    // A shape may name a common type instead of spelling out a record.
    if shape.get("attributes").is_none()
        && let Some(name) = shape["name"].as_str().or_else(|| shape["type"].as_str())
    {
        shape = &declarations["commonTypes"][name];
    }
    shape["attributes"]
        .as_object()
        .map(|attributes| attributes.keys().cloned().collect())
        .unwrap_or_default()
}

/// A JSON value as a literal Cedar value. `null` and non-integer numbers have no Cedar
/// equivalent and are dropped.
fn expression(value: &Value) -> Option<cedar::RestrictedExpression> {
    match value {
        Value::Null => None,
        Value::Bool(value) => Some(cedar::RestrictedExpression::new_bool(*value)),
        Value::Number(value) => value.as_i64().map(cedar::RestrictedExpression::new_long),
        Value::String(value) => Some(cedar::RestrictedExpression::new_string(value.clone())),
        Value::Array(values) => Some(cedar::RestrictedExpression::new_set(
            values.iter().filter_map(expression),
        )),
        Value::Object(fields) => cedar::RestrictedExpression::new_record(
            fields
                .iter()
                .filter_map(|(key, value)| expression(value).map(|value| (key.clone(), value))),
        )
        .ok(),
    }
}

fn read(path: &Path) -> Result<String, PolicyError> {
    std::fs::read_to_string(path).map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> PolicyError {
    PolicyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(id: &str, roles: &[&str]) -> Principal {
        Principal::builder(id)
            .with_attribute("roles", roles)
            .build()
    }

    fn route(template: &str) -> Resource {
        Resource::new("Route", template).within(
            Resource::new("Resource", "urn:shop:orders")
                .within(Resource::new("Adapter", "urn:shop")),
        )
    }

    async fn allows(authorizer: &CedarAuthorizer, who: &Principal, request: AccessRequest) -> bool {
        authorizer.authorize(who, &request).await.is_ok()
    }

    const SCHEMA: &str = r#"
        entity Role in [Role];
        entity User in [Role] { id: String, tenant?: String };
        entity Adapter;
        entity Resource in [Adapter];
        entity Route in [Resource];
        entity Note { owner: String };
        type Http = { path: String };
        action "GET", "DELETE" appliesTo { principal: [User], resource: [Route], context: Http };
        action archive appliesTo { principal: [User], resource: [Note] };
    "#;

    #[tokio::test]
    async fn membership_grants_a_whole_resource_but_only_the_named_actions() {
        let authorizer = CedarAuthorizer::new(
            r#"permit (principal in Role::"Reader", action == Action::"GET", resource in Resource::"urn:shop:orders");"#,
        )
        .unwrap();
        let reader = principal("u-1", &["Reader"]);

        assert!(
            allows(
                &authorizer,
                &reader,
                AccessRequest::new("GET", route("/api/orders/{id}"))
            )
            .await
        );
        assert!(
            !allows(
                &authorizer,
                &reader,
                AccessRequest::new("DELETE", route("/api/orders/{id}"))
            )
            .await
        );
        assert!(
            !allows(
                &authorizer,
                &principal("u-2", &["Stranger"]),
                AccessRequest::new("GET", route("/api/orders"))
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_route_template_matches_exactly() {
        let authorizer = CedarAuthorizer::new(
            r#"permit (principal, action, resource == Route::"/api/orders/{id}");"#,
        )
        .unwrap();
        let anyone = principal("u-1", &[]);

        assert!(
            allows(
                &authorizer,
                &anyone,
                AccessRequest::new("GET", route("/api/orders/{id}"))
            )
            .await
        );
        // What the glob-based RBAC used to over-grant: neither a deeper route nor a
        // sibling with the same prefix is covered.
        assert!(
            !allows(
                &authorizer,
                &anyone,
                AccessRequest::new("GET", route("/api/orders/{id}/refunds"))
            )
            .await
        );
        assert!(
            !allows(
                &authorizer,
                &anyone,
                AccessRequest::new("GET", route("/api/orders-admin/{id}"))
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_forbid_overrides_a_permit() {
        let authorizer = CedarAuthorizer::new(
            r#"
            permit (principal, action, resource);
            forbid (principal in Role::"Suspended", action, resource);
            "#,
        )
        .unwrap();
        assert!(
            allows(
                &authorizer,
                &principal("u-1", &[]),
                AccessRequest::new("GET", route("/x"))
            )
            .await
        );
        assert!(
            !allows(
                &authorizer,
                &principal("u-2", &["Suspended"]),
                AccessRequest::new("GET", route("/x"))
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_forbid_that_fails_to_evaluate_denies() {
        // `resource.locked` does not exist on a route, so this forbid errors. Cedar would
        // skip it and let the permit through.
        let authorizer = CedarAuthorizer::new(
            r#"
            permit (principal, action, resource);
            forbid (principal, action, resource) when { resource.locked };
            "#,
        )
        .unwrap();
        assert!(
            !allows(
                &authorizer,
                &principal("u-1", &[]),
                AccessRequest::new("GET", route("/x"))
            )
            .await
        );
    }

    #[tokio::test]
    async fn domain_rules_read_resource_attributes_and_claims() {
        let authorizer = CedarAuthorizer::new(
            r#"
            permit (principal, action == Action::"archive", resource is Note)
              when { resource.owner == principal.id };
            permit (principal, action == Action::"audit", resource is Note)
              when { principal has tenant && resource.tenant == principal.tenant };
            "#,
        )
        .unwrap();
        let alice = Principal::builder("alice")
            .with_attribute("tenant", "acme")
            .build();
        let note = |owner: &str| {
            Resource::new("Note", 1)
                .with_attribute("owner", owner)
                .with_attribute("tenant", "acme")
        };

        assert!(
            allows(
                &authorizer,
                &alice,
                AccessRequest::new("archive", note("alice"))
            )
            .await
        );
        assert!(
            !allows(
                &authorizer,
                &alice,
                AccessRequest::new("archive", note("bob"))
            )
            .await
        );
        assert!(
            allows(
                &authorizer,
                &alice,
                AccessRequest::new("audit", note("bob"))
            )
            .await
        );
    }

    #[tokio::test]
    async fn the_context_is_visible_to_policies() {
        let authorizer = CedarAuthorizer::new(
            r#"permit (principal, action, resource) when { context.path == "/api/orders/7" };"#,
        )
        .unwrap();
        let anyone = principal("u-1", &[]);
        let request = |path: &str| {
            AccessRequest::new("GET", route("/api/orders/{id}")).with_context("path", path)
        };

        assert!(allows(&authorizer, &anyone, request("/api/orders/7")).await);
        assert!(!allows(&authorizer, &anyone, request("/api/orders/8")).await);
    }

    #[tokio::test]
    async fn static_entities_supply_a_role_hierarchy() {
        let authorizer = CedarAuthorizer::new(
            r#"permit (principal in Role::"Dispatch", action, resource);"#,
        )
        .unwrap()
        .with_entities(
            r#"[{ "uid": { "type": "Role", "id": "Support" }, "attrs": {}, "parents": [{ "type": "Role", "id": "Dispatch" }] }]"#,
        )
        .unwrap();

        assert!(
            allows(
                &authorizer,
                &principal("u-1", &["Support"]),
                AccessRequest::new("GET", route("/x"))
            )
            .await
        );
        assert!(
            !allows(
                &authorizer,
                &principal("u-2", &["Customer"]),
                AccessRequest::new("GET", route("/x"))
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_schema_catches_typos_at_startup() {
        let err = CedarAuthorizer::new(r#"permit (principal in Rol::"Admin", action, resource);"#)
            .unwrap()
            .with_schema(SCHEMA)
            .unwrap_err();
        assert!(matches!(err, PolicyError::Validation(_)), "{err}");

        let err = CedarAuthorizer::new(
            r#"permit (principal, action == Action::"archive", resource is Note) when { resource.ownr == principal.id };"#,
        )
        .unwrap()
        .with_schema(SCHEMA)
        .unwrap_err();
        assert!(matches!(err, PolicyError::Validation(_)), "{err}");
    }

    #[tokio::test]
    async fn with_a_schema_only_declared_claims_reach_the_principal() {
        let authorizer = CedarAuthorizer::new(
            r#"
            permit (principal, action == Action::"GET", resource)
              when { principal has tenant && principal.tenant == "acme" };
            "#,
        )
        .unwrap()
        .with_schema(SCHEMA)
        .unwrap();

        // `email` is not declared on `User`; without the filter the entity would fail
        // schema conformance and every request would be denied.
        let alice = Principal::builder("alice")
            .with_attribute("tenant", "acme")
            .with_attribute("email", "alice@example.com")
            .build();
        let request = AccessRequest::new("GET", route("/api/orders/{id}"))
            .with_context("path", "/api/orders/1");
        assert!(allows(&authorizer, &alice, request.clone()).await);

        let bob = Principal::builder("bob")
            .with_attribute("tenant", "other")
            .build();
        assert!(!allows(&authorizer, &bob, request).await);
    }

    #[tokio::test]
    async fn with_a_schema_a_request_that_does_not_conform_is_denied() {
        let authorizer = CedarAuthorizer::new("permit (principal, action, resource);")
            .unwrap()
            .with_schema(SCHEMA)
            .unwrap();
        let anyone = principal("u-1", &[]);

        // The schema requires `context.path` for HTTP actions...
        assert!(!allows(&authorizer, &anyone, AccessRequest::new("GET", route("/x"))).await);
        // ...does not know this action...
        assert!(
            !allows(
                &authorizer,
                &anyone,
                AccessRequest::new("PURGE", route("/x")).with_context("path", "/x")
            )
            .await
        );
        // ...and requires a note's owner.
        assert!(
            !allows(
                &authorizer,
                &anyone,
                AccessRequest::new("archive", Resource::new("Note", 1))
            )
            .await
        );
        assert!(
            allows(
                &authorizer,
                &anyone,
                AccessRequest::new(
                    "archive",
                    Resource::new("Note", 1).with_attribute("owner", "u-1")
                )
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_namespace_qualifies_every_jenny_type() {
        let authorizer = CedarAuthorizer::new(
            r#"permit (principal in Shop::Role::"Reader", action == Shop::Action::"GET", resource is Shop::Route);"#,
        )
        .unwrap()
        .in_namespace("Shop");
        assert!(
            allows(
                &authorizer,
                &principal("u-1", &["Reader"]),
                AccessRequest::new("GET", Resource::new("Route", "/x"))
            )
            .await
        );
    }

    #[test]
    fn policies_are_named_by_their_id_annotation_or_their_position() {
        let authorizer = CedarAuthorizer::new(
            r#"
            @id("admins")
            permit (principal in Role::"Admin", action, resource);
            permit (principal, action == Action::"GET", resource);
            "#,
        )
        .unwrap();
        let ids: Vec<String> = authorizer
            .policies()
            .policies()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["admins", "policies#1"]);
    }

    #[test]
    fn malformed_policies_and_templates_are_rejected() {
        assert!(matches!(
            CedarAuthorizer::new("permit (principal, action").unwrap_err(),
            PolicyError::Policies { .. }
        ));
        assert!(matches!(
            CedarAuthorizer::new("permit (principal == ?principal, action, resource);")
                .unwrap_err(),
            PolicyError::Policies { .. }
        ));
    }

    #[test]
    fn a_policy_directory_is_loaded_in_file_name_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("b.cedar"),
            "permit (principal, action, resource);",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.cedar"),
            "forbid (principal, action, resource);",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a policy").unwrap();

        let authorizer = CedarAuthorizer::from_path(dir.path()).unwrap();
        let ids: Vec<String> = authorizer
            .policies()
            .policies()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["a.cedar#0", "b.cedar#0"]);
    }

    #[test]
    fn declared_attributes_follow_common_types() {
        // Only the JSON form can give an entity a named type as its shape.
        let fragment = cedar::SchemaFragment::from_json_str(
            r#"{ "": {
                "commonTypes": { "Claims": { "type": "Record", "attributes": {
                    "tenant": { "type": "String" },
                    "email": { "type": "String", "required": false }
                } } },
                "entityTypes": {
                    "User": { "shape": { "type": "Claims" } },
                    "Plain": { "shape": { "type": "Record", "attributes": { "a": { "type": "Long" } } } }
                },
                "actions": {}
            } }"#,
        )
        .unwrap();
        let json: Value = serde_json::from_str(&fragment.to_json_string().unwrap()).unwrap();
        assert_eq!(
            declared_attributes(&json, "", "User"),
            HashSet::from(["tenant".to_string(), "email".to_string()])
        );
        assert_eq!(
            declared_attributes(&json, "", "Plain"),
            HashSet::from(["a".to_string()])
        );
        assert!(declared_attributes(&json, "", "Missing").is_empty());

        let (fragment, _) = cedar::SchemaFragment::from_cedarschema_str(
            "namespace Shop { entity User { tenant: String }; }",
        )
        .unwrap();
        let json: Value = serde_json::from_str(&fragment.to_json_string().unwrap()).unwrap();
        assert_eq!(
            declared_attributes(&json, "Shop", "User"),
            HashSet::from(["tenant".to_string()])
        );
    }
}
