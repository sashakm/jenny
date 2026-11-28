//! Driving messaging adapter: the subjects a service consumes from JetStream, and the
//! handlers it consumes them with.
//!
//! [`MessagingAdapter`] is the messaging counterpart to `jenny-rest`'s `RestAdapter`: a
//! named driving adapter. [`on`](MessagingAdapter::on) binds a subject to a
//! [`Handler`](crate::inbound::Handler); registered with the runtime, the adapter gets a
//! durable pull consumer on its [`stream`](MessagingAdapter::stream), and every message
//! is turned into a [`Context`] and a call of the handler, then acked, nak'd or
//! terminated ([`consumer`](crate::consumer)). [`subscribe`](MessagingAdapter::subscribe)
//! only declares a subject, for `/health/status`.
//!
//! [`Context`]: jenny_core::port::Context

use jenny_core::domain::Urn;

use crate::inbound::{Dispatch, IntoHandler};

/// One subject this service consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectBinding {
    pub subject: String,
    pub description: Option<String>,
}

impl SubjectBinding {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A named messaging driving adapter, rooted at a subject prefix.
///
/// Like `RestAdapter`, it owns its own identity: the `name` roots its [`Urn`] and keys
/// its `applications.<name>` configuration section, and the prefix scopes every
/// subject it declares.
///
/// ```
/// use jenny_core::domain::DomainError;
/// use jenny_core::port::Context;
/// use jenny_messaging::{MessagingAdapter, SubjectBinding};
///
/// #[derive(serde::Deserialize)]
/// struct OrderCreated { id: u32 }
///
/// let adapter = MessagingAdapter::new("my-shop", "shop")?
///     .describe("order lifecycle events")
///     .stream("SHOP")
///     .on(
///         SubjectBinding::new("orders.created").with_description("a new order"),
///         |order: OrderCreated, cx: Context| async move {
///             tracing::info!(order = order.id, request_id = %cx.request_id(), "order created");
///             Ok::<(), DomainError>(())
///         },
///     )
///     .subscribe(SubjectBinding::new("orders.cancelled")); // declared, not consumed
///
/// assert_eq!(adapter.urn().to_string(), "urn:my-shop");
/// assert_eq!(adapter.subjects()[0].subject, "shop.orders.created");
/// assert_eq!(adapter.handlers().len(), 1);
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct MessagingAdapter {
    urn: Urn,
    prefix: String,
    description: Option<String>,
    stream: Option<String>,
    subjects: Vec<(SubjectBinding, Option<Dispatch>)>,
}

impl MessagingAdapter {
    /// Fails when `name` is not a valid URN segment.
    pub fn new(name: impl Into<String>, subject_prefix: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            urn: Urn::root(name)?,
            prefix: subject_prefix.into(),
            description: None,
            stream: None,
            subjects: Vec::new(),
        })
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The JetStream stream the adapter consumes from. It must exist: jenny creates the
    /// consumer on it, never the stream.
    pub fn stream(mut self, name: impl Into<String>) -> Self {
        self.stream = Some(name.into());
        self
    }

    /// Declare a subject without consuming it, prefixed with this adapter's subject
    /// prefix. It is listed in `/health/status`.
    pub fn subscribe(mut self, binding: SubjectBinding) -> Self {
        self.subjects.push((binding, None));
        self
    }

    /// Consume a subject (prefixed, and NATS wildcards allowed) with `handler`: a
    /// [`Handler`](crate::inbound::Handler) or a closure `Fn(M, Context) -> impl Future`.
    /// The payload is decoded as JSON into `M`.
    pub fn on<M, K>(mut self, binding: SubjectBinding, handler: impl IntoHandler<M, K>) -> Self {
        self.subjects.push((binding, Some(Dispatch::new(handler))));
        self
    }

    pub fn name(&self) -> &str {
        self.urn.name()
    }

    pub fn urn(&self) -> &Urn {
        &self.urn
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The subject prefix every declared subject is scoped under.
    pub fn subject_prefix(&self) -> &str {
        &self.prefix
    }

    /// The stream given to [`stream`](Self::stream).
    pub fn stream_name(&self) -> Option<&str> {
        self.stream.as_deref()
    }

    /// Every declared or handled subject, fully prefixed.
    pub fn subjects(&self) -> Vec<SubjectBinding> {
        self.subjects
            .iter()
            .map(|(binding, _)| SubjectBinding {
                subject: self.full(&binding.subject),
                description: binding.description.clone(),
            })
            .collect()
    }

    /// Every handled subject, fully prefixed, with its handler.
    pub fn handlers(&self) -> Vec<(String, Dispatch)> {
        self.subjects
            .iter()
            .filter_map(|(binding, dispatch)| {
                dispatch
                    .clone()
                    .map(|dispatch| (self.full(&binding.subject), dispatch))
            })
            .collect()
    }

    fn full(&self, subject: &str) -> String {
        if self.prefix.is_empty() {
            subject.to_string()
        } else {
            format!("{}.{subject}", self.prefix)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_subjects_are_scoped_by_the_prefix() {
        let adapter = MessagingAdapter::new("my-shop", "shop")
            .unwrap()
            .subscribe(SubjectBinding::new("orders.created").with_description("a new order"))
            .subscribe(SubjectBinding::new("orders.cancelled"));

        assert_eq!(adapter.name(), "my-shop");
        assert_eq!(adapter.subject_prefix(), "shop");
        assert_eq!(adapter.urn().to_string(), "urn:my-shop");

        let subjects = adapter.subjects();
        assert_eq!(subjects.len(), 2);
        assert_eq!(subjects[0].subject, "shop.orders.created");
        assert_eq!(subjects[0].description.as_deref(), Some("a new order"));
        assert_eq!(subjects[1].subject, "shop.orders.cancelled");
        assert!(subjects[1].description.is_none());
    }

    #[test]
    fn the_adapter_name_must_be_a_valid_urn_segment() {
        assert!(MessagingAdapter::new("My-Shop", "shop").is_ok());
        assert!(MessagingAdapter::new("my shop/v1", "shop").is_err());
    }
}
