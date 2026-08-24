//! Driving messaging adapter — **declaration only**.
//!
//! [`MessagingAdapter`] is the messaging counterpart to `jenny-rest`'s `RestAdapter`:
//! a named driving adapter that says which subjects this service consumes. Registering
//! one with the runtime records and logs those subjects.
//!
//! The dispatch path — consume a message, build a [`Context`], convert the payload
//! into a domain command, invoke a use case, ack or nack — is not implemented yet.
//! When it is, it lands here without the shape below changing: the declaration is
//! already the right thing to hang a handler off.
//!
//! [`Context`]: jenny_core::port::Context

use jenny_core::domain::Urn;

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
/// use jenny_messaging::{MessagingAdapter, SubjectBinding};
///
/// let adapter = MessagingAdapter::new("my-shop", "shop")?
///     .describe("order lifecycle events")
///     .subscribe(SubjectBinding::new("orders.created").with_description("a new order"))
///     .subscribe(SubjectBinding::new("orders.cancelled"));
///
/// assert_eq!(adapter.urn().to_string(), "urn:my-shop");
/// assert_eq!(adapter.subjects()[0].subject, "shop.orders.created");
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct MessagingAdapter {
    urn: Urn,
    prefix: String,
    description: Option<String>,
    subjects: Vec<SubjectBinding>,
}

impl MessagingAdapter {
    /// Fails when `name` is not a valid URN segment.
    pub fn new(name: impl Into<String>, subject_prefix: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            urn: Urn::root(name)?,
            prefix: subject_prefix.into(),
            description: None,
            subjects: Vec::new(),
        })
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Declare a subject, which will be prefixed with this adapter's subject prefix.
    pub fn subscribe(mut self, binding: SubjectBinding) -> Self {
        self.subjects.push(binding);
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

    /// Every declared subject, fully prefixed.
    pub fn subjects(&self) -> Vec<SubjectBinding> {
        self.subjects
            .iter()
            .map(|binding| SubjectBinding {
                subject: format!("{}.{}", self.prefix, binding.subject),
                description: binding.description.clone(),
            })
            .collect()
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
