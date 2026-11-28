//! What an event carries besides its payload: headers, and the enrichers that fill them
//! from the [`Context`] (ADR-032).
//!
//! An enricher is registered on [`Events`](super::Events) in the composition root and
//! runs whenever an event is encoded: in [`Events::publish`](super::Events::publish), in
//! `change(…).raising(…)` and in `add(…).raising(…)`, before the store's write, so an
//! outbox stores the enriched envelope and a replay carries the same headers.
//!
//! ```
//! use std::sync::Arc;
//! use jenny_core::port::{Caller, Context, Events, Headers};
//! use jenny_core::testing::RecordedEvents;
//!
//! # struct TenantId(String);
//! let recorded = Arc::new(RecordedEvents::default());
//! let events = Events::new(recorded.clone())
//!     .enriched_with(Caller)
//!     .enriched_with(|cx: &Context, headers: &mut Headers| {
//!         if let Some(tenant) = cx.get::<TenantId>() {
//!             headers.insert("tenant", tenant.0.as_str());
//!         }
//!     });
//! ```

use std::collections::BTreeMap;

use super::context::Context;
use crate::domain::Principal;

/// Who an event was raised for: [`Principal::subject`], the id of a user or
/// `System[<service>/<component>]` for the service itself. Written by [`Caller`].
pub const CALLER_SUBJECT: &str = "caller-subject";
/// Who actually acted, when that was someone else on the subject's behalf: the `sub` of
/// the principal's RFC 8693 `act` claim. Written by [`Caller`].
pub const CALLER_ACTOR: &str = "caller-actor";
/// The name of the service that raised the event. Written by [`Source`].
pub const SOURCE_SERVICE: &str = "source-service";
/// The version of the service that raised the event. Written by [`Source`].
pub const SOURCE_VERSION: &str = "source-version";

/// An event's headers, as an enricher sees and changes them.
///
/// Only the headers: an enricher never sees the event's id, name, timestamp,
/// correlation id or payload. The reserved names the bus writes (`event-id`,
/// `event-name`, `event-occurred-at`, `correlation-id`) are overwritten on the way out
/// whatever an enricher puts here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers(BTreeMap<String, String>);

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a header, returning its previous value.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) -> Option<String> {
        self.0.insert(name.into(), value.into())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn remove(&mut self, name: &str) -> Option<String> {
        self.0.remove(name)
    }

    /// Every header, ordered by name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<BTreeMap<String, String>> for Headers {
    fn from(map: BTreeMap<String, String>) -> Self {
        Self(map)
    }
}

impl From<Headers> for BTreeMap<String, String> {
    fn from(headers: Headers) -> Self {
        headers.0
    }
}

/// Adds headers to every event an [`Events`](super::Events) encodes, from the
/// [`Context`] it was raised in.
///
/// Synchronous and infallible: it runs once per event inside the write. A requirement on
/// the context ("a tenant must be known") belongs in the use case, before anything is
/// written, and anything that needs I/O in the adapter that built the context.
///
/// Implemented for closures `Fn(&Context, &mut Headers)`; annotate both parameter types.
pub trait Enrich: Send + Sync + 'static {
    fn enrich(&self, cx: &Context, headers: &mut Headers);
}

impl<F> Enrich for F
where
    F: Fn(&Context, &mut Headers) + Send + Sync + 'static,
{
    fn enrich(&self, cx: &Context, headers: &mut Headers) {
        self(cx, headers)
    }
}

/// Writes who raised the event, as RFC 8693 names them: [`CALLER_SUBJECT`], and
/// [`CALLER_ACTOR`] when someone acted on the subject's behalf.
///
/// - `caller-subject` is [`Principal::subject`]: a user's id, or `System[…]` for the
///   service itself.
/// - `caller-actor` is `act.sub` from the principal's `act` attribute (a token-exchange
///   token's `act` claim), only when `act` is an object with a string `sub`. Only the
///   outermost `act` is read: RFC 8693 says nested ones are informational and must not
///   be used for access control.
///
/// Without a principal it writes nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller;

impl Caller {
    /// The actor of `principal`: `act.sub`, the outermost only.
    pub fn actor(principal: &Principal) -> Option<&str> {
        principal
            .raw_attribute("act")?
            .as_object()?
            .get("sub")?
            .as_str()
    }
}

impl Enrich for Caller {
    fn enrich(&self, cx: &Context, headers: &mut Headers) {
        let Some(principal) = cx.principal() else {
            return;
        };
        headers.insert(CALLER_SUBJECT, principal.subject());
        if let Some(actor) = Self::actor(principal) {
            headers.insert(CALLER_ACTOR, actor);
        }
    }
}

/// Writes which service raised the event: [`SOURCE_SERVICE`] and [`SOURCE_VERSION`].
///
/// Both are read when it is built. A value that is empty, meaning unknown, is left out
/// rather than written blank.
#[derive(Clone, Debug)]
pub struct Source {
    service: String,
    version: String,
}

impl Source {
    pub fn new(service: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            version: version.into(),
        }
    }
}

impl Enrich for Source {
    fn enrich(&self, _: &Context, headers: &mut Headers) {
        for (name, value) in [
            (SOURCE_SERVICE, &self.service),
            (SOURCE_VERSION, &self.version),
        ] {
            if !value.trim().is_empty() {
                headers.insert(name, value.as_str());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn enriched(enricher: &impl Enrich, cx: &Context) -> Headers {
        let mut headers = Headers::new();
        enricher.enrich(cx, &mut headers);
        headers
    }

    #[test]
    fn headers_behave_like_a_map() {
        let mut headers = Headers::new();
        assert!(headers.is_empty());
        assert_eq!(headers.insert("b", "2"), None);
        assert_eq!(headers.insert("a", "1"), None);
        assert_eq!(headers.insert("a", "one"), Some("1".to_string()));
        assert_eq!(headers.get("a"), Some("one"));
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            [("a", "one"), ("b", "2")]
        );
        assert_eq!(headers.remove("b"), Some("2".to_string()));
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn caller_writes_nothing_without_a_principal() {
        assert!(enriched(&Caller, &Context::new()).is_empty());
    }

    #[test]
    fn caller_writes_a_users_id_and_a_system_principals_form() {
        let user = Context::new().with_principal(Principal::builder("u-7").build());
        let headers = enriched(&Caller, &user);
        assert_eq!(headers.get(CALLER_SUBJECT), Some("u-7"));
        assert_eq!(headers.get(CALLER_ACTOR), None);

        let system = Context::new().with_principal(Principal::system("my-notes", "tasks"));
        assert_eq!(
            enriched(&Caller, &system).get(CALLER_SUBJECT),
            Some("System[my-notes/tasks]")
        );
    }

    #[test]
    fn caller_reads_the_outermost_act_sub_only() {
        let with_act = |act: serde_json::Value| {
            Context::new()
                .with_principal(Principal::builder("u-7").with_attribute("act", act).build())
        };
        let delegated = enriched(&Caller, &with_act(json!({ "sub": "bff" })));
        assert_eq!(delegated.get(CALLER_SUBJECT), Some("u-7"));
        assert_eq!(delegated.get(CALLER_ACTOR), Some("bff"));

        let nested = enriched(
            &Caller,
            &with_act(json!({ "sub": "service-a", "act": { "sub": "bff" } })),
        );
        assert_eq!(nested.get(CALLER_ACTOR), Some("service-a"));

        for no_actor in [
            json!({ "iss": "x" }),
            json!({ "sub": 7 }),
            json!("bff"),
            json!(null),
        ] {
            let headers = enriched(&Caller, &with_act(no_actor.clone()));
            assert_eq!(headers.get(CALLER_ACTOR), None, "{no_actor}");
            assert_eq!(headers.get(CALLER_SUBJECT), Some("u-7"));
        }
    }

    #[test]
    fn source_leaves_out_what_is_unknown() {
        let headers = enriched(&Source::new("my-notes", "1.2.3"), &Context::new());
        assert_eq!(headers.get(SOURCE_SERVICE), Some("my-notes"));
        assert_eq!(headers.get(SOURCE_VERSION), Some("1.2.3"));

        let headers = enriched(&Source::new("my-notes", ""), &Context::new());
        assert_eq!(headers.get(SOURCE_SERVICE), Some("my-notes"));
        assert_eq!(headers.get(SOURCE_VERSION), None);
    }

    #[test]
    fn a_closure_is_an_enricher() {
        let tenant = |_: &Context, headers: &mut Headers| {
            headers.insert("tenant", "acme");
        };
        assert_eq!(
            enriched(&tenant, &Context::new()).get("tenant"),
            Some("acme")
        );
    }
}
