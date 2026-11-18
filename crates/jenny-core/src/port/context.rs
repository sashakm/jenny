//! The ambient facts about an invocation, independent of how it arrived.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use uuid::Uuid;

use crate::domain::{DomainError, Label, Principal};
use crate::port::outbound::{AccessRequest, Authorizer};

/// Everything a use case may want to know about the call that is not part of the
/// call's input.
///
/// A driving adapter builds one of these per invocation and passes it by reference.
/// Keeping it separate from the input is what lets a use case take a plain domain
/// command as its argument — no envelope to unwrap, no framework type threaded
/// through the domain's own signatures:
///
/// ```
/// # use jenny_core::domain::DomainResult;
/// # use jenny_core::port::Context;
/// # use jenny_core::port::outbound::{AccessRequest, Resource};
/// # struct ArchiveNote(u32);
/// # struct Note { id: u32, owner: String }
/// # async fn load(id: u32) -> DomainResult<Note> { Ok(Note { id, owner: "u-7".into() }) }
/// async fn archive(cmd: ArchiveNote, cx: &Context) -> DomainResult<()> {
///     let note = load(cmd.0).await?;
///     // fine-grained authorization, the same on every transport
///     cx.authorize(&AccessRequest::new(
///         "archive",
///         Resource::new("Note", note.id).with_attribute("owner", &note.owner),
///     ))
///     .await?;
///     tracing::info!(request_id = %cx.request_id(), actor = %cx.require_principal()?, "archiving note");
///     Ok(())
/// }
/// ```
///
/// The same context type is produced by every driving adapter, so a use case written
/// against it works unchanged whether it was reached over HTTP, off a message queue,
/// or from a scheduled task.
#[derive(Default)]
pub struct Context {
    request_id: Uuid,
    principal: Option<Principal>,
    authorizer: Option<Arc<dyn Authorizer>>,
    labels: Vec<Label>,
    extensions: Extensions,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("request_id", &self.request_id)
            .field("principal", &self.principal)
            .field("authorizer", &self.authorizer.is_some())
            .field("labels", &self.labels)
            .field("extensions", &self.extensions)
            .finish()
    }
}

impl Context {
    /// A context for a brand-new invocation, with a freshly minted request id.
    pub fn new() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            principal: None,
            authorizer: None,
            labels: Vec::new(),
            extensions: Extensions::new(),
        }
    }

    /// Correlates this invocation across logs, traces and downstream calls.
    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    /// Who is calling, if the adapter authenticated them.
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    /// Who is calling, failing with [`DomainError::Unauthenticated`] if nobody is.
    ///
    /// The idiomatic first line of a use case that is only meaningful for a known
    /// caller.
    pub fn require_principal(&self) -> Result<&Principal, DomainError> {
        self.principal
            .as_ref()
            .ok_or_else(|| DomainError::unauthenticated("no authenticated principal"))
    }

    /// The policy engine this invocation is checked against, if the adapter attached one.
    pub fn authorizer(&self) -> Option<&dyn Authorizer> {
        self.authorizer.as_deref()
    }

    /// Ask the attached [`Authorizer`] whether the caller may do `request`.
    ///
    /// Fails with [`DomainError::Unauthenticated`] when nobody is calling,
    /// [`DomainError::Forbidden`] when the policies deny it, and
    /// [`DomainError::Internal`] when no authorizer was attached, because a use case that
    /// asks has to get an answer: silently allowing it would be the worst outcome.
    pub async fn authorize(&self, request: &AccessRequest) -> Result<(), DomainError> {
        let principal = self.require_principal()?;
        let authorizer = self
            .authorizer
            .as_deref()
            .ok_or_else(|| DomainError::internal("no authorizer is attached to this context"))?;
        Ok(authorizer.authorize(principal, request).await?)
    }

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    /// Read a value some earlier layer attached. See [`Extensions`].
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get()
    }

    /// Attach a value, replacing any previous value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.extensions.insert(value)
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    pub fn with_request_id(mut self, id: Uuid) -> Self {
        self.request_id = id;
        self
    }

    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = Some(principal);
        self
    }

    pub fn with_authorizer(mut self, authorizer: Arc<dyn Authorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    pub fn with_labels(mut self, labels: impl IntoIterator<Item = Label>) -> Self {
        self.labels.extend(labels);
        self
    }

    /// Builder form of [`insert`](Self::insert).
    pub fn with<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
        self
    }
}

type AnyMap = HashMap<TypeId, Box<dyn Any + Send + Sync>>;

/// A map keyed by type rather than by name.
///
/// Used for the things a middleware or the composition root wants to hand further in
/// without every intermediate signature having to mention them — a resolved tenant, a
/// trace parent, a connection pool. Each type may appear once; inserting the same type
/// again replaces it.
///
/// ```
/// use jenny_core::port::Extensions;
///
/// #[derive(Debug, PartialEq)]
/// struct TenantId(String);
///
/// let mut ext = Extensions::new();
/// ext.insert(TenantId("acme".into()));
/// assert_eq!(ext.get::<TenantId>(), Some(&TenantId("acme".into())));
/// ```
#[derive(Default)]
pub struct Extensions {
    map: AnyMap,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value, returning the previous one of the same type if there was one.
    pub fn insert<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|prev| prev.downcast::<T>().ok().map(|b| *b))
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }

    pub fn get_mut<T: Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut::<T>())
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast::<T>().ok().map(|b| *b))
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct A(u32);
    #[derive(Debug, PartialEq)]
    struct B(&'static str);

    #[test]
    fn a_fresh_context_has_an_id_and_nothing_else() {
        let cx = Context::new();
        assert_ne!(cx.request_id(), Uuid::nil());
        assert!(cx.principal().is_none());
        assert!(cx.labels().is_empty());
        assert!(cx.extensions().is_empty());
    }

    #[test]
    fn require_principal_reports_unauthenticated_when_absent() {
        let cx = Context::new();
        assert!(matches!(
            cx.require_principal().unwrap_err(),
            DomainError::Unauthenticated { .. }
        ));

        let cx = cx.with_principal(Principal::builder("u-1").build());
        assert_eq!(cx.require_principal().unwrap().id(), "u-1");
    }

    struct OwnerOnly;

    #[async_trait::async_trait]
    impl Authorizer for OwnerOnly {
        async fn authorize(
            &self,
            principal: &Principal,
            request: &AccessRequest,
        ) -> Result<(), crate::domain::AuthorizationError> {
            if request.resource().attributes().get("owner")
                == Some(&serde_json::json!(principal.id()))
            {
                Ok(())
            } else {
                Err(crate::domain::AuthorizationError("not the owner".into()))
            }
        }
    }

    fn archive_note_of(owner: &str) -> AccessRequest {
        AccessRequest::new(
            "archive",
            crate::port::outbound::Resource::new("Note", 1).with_attribute("owner", owner),
        )
    }

    #[tokio::test]
    async fn authorize_asks_the_attached_authorizer() {
        let cx = Context::new()
            .with_principal(Principal::builder("u-1").build())
            .with_authorizer(Arc::new(OwnerOnly));
        assert!(cx.authorize(&archive_note_of("u-1")).await.is_ok());
        assert!(matches!(
            cx.authorize(&archive_note_of("u-2")).await.unwrap_err(),
            DomainError::Forbidden { .. }
        ));
    }

    #[tokio::test]
    async fn authorize_fails_closed() {
        let anonymous = Context::new().with_authorizer(Arc::new(OwnerOnly));
        assert!(matches!(
            anonymous
                .authorize(&archive_note_of("u-1"))
                .await
                .unwrap_err(),
            DomainError::Unauthenticated { .. }
        ));

        let unwired = Context::new().with_principal(Principal::builder("u-1").build());
        assert!(matches!(
            unwired
                .authorize(&archive_note_of("u-1"))
                .await
                .unwrap_err(),
            DomainError::Internal { .. }
        ));
    }

    #[test]
    fn builders_compose() {
        let id = Uuid::new_v4();
        let cx = Context::new()
            .with_request_id(id)
            .with_labels([Label::new("source", "test")])
            .with(A(7));
        assert_eq!(cx.request_id(), id);
        assert_eq!(cx.labels().len(), 1);
        assert_eq!(cx.get::<A>(), Some(&A(7)));
    }

    #[test]
    fn extensions_are_keyed_by_type() {
        let mut ext = Extensions::new();
        assert!(ext.is_empty());
        ext.insert(A(7));
        ext.insert(B("hello"));
        assert_eq!(ext.len(), 2);
        assert_eq!(ext.get::<A>(), Some(&A(7)));
        assert_eq!(ext.get::<B>(), Some(&B("hello")));
        assert!(ext.contains::<A>());
    }

    #[test]
    fn inserting_the_same_type_replaces_and_returns_the_old_value() {
        let mut ext = Extensions::new();
        assert_eq!(ext.insert(A(1)), None);
        assert_eq!(ext.insert(A(2)), Some(A(1)));
        assert_eq!(ext.get::<A>(), Some(&A(2)));
    }

    #[test]
    fn values_can_be_mutated_removed_and_cleared() {
        let mut ext = Extensions::new();
        ext.insert(A(1));
        if let Some(a) = ext.get_mut::<A>() {
            a.0 = 99;
        }
        assert_eq!(ext.get::<A>(), Some(&A(99)));
        assert_eq!(ext.remove::<A>(), Some(A(99)));
        assert!(!ext.contains::<A>());

        ext.insert(B("x"));
        ext.clear();
        assert!(ext.is_empty());
    }
}
