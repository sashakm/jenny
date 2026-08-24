//! Naming and categorising the things a service exposes.

use std::{fmt::Display, str::FromStr};

use serde::{Deserialize, Serialize};

/// A free-form `key: value` tag on a resource. Both halves are lowercased.
#[derive(Clone, Debug, Deserialize, Serialize, Hash, Eq, PartialEq)]
pub struct Label(String, String);

impl Label {
    pub fn new<S: Into<String>>(key: S, value: S) -> Self {
        Self(key.into().to_lowercase(), value.into().to_lowercase())
    }

    pub fn key(&self) -> &str {
        &self.0
    }

    pub fn value(&self) -> &str {
        &self.1
    }
}

impl From<(&str, &str)> for Label {
    fn from((key, value): (&str, &str)) -> Self {
        Self::new(key, value)
    }
}

impl Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key(), self.value())
    }
}

/// A stable, protocol-independent name for something the service exposes.
///
/// The root segment is the adapter's name; further segments narrow it down to a
/// resource. The same URN identifies that resource no matter which transport a call
/// arrives on, which is what makes it usable for authorization rules and telemetry
/// that must hold across REST, messaging and gRPC alike.
///
/// Segments are lowercased and limited to alphanumerics, dashes and underscores.
///
/// ### Example
/// ```
/// use jenny_core::domain::Urn;
///
/// let urn = Urn::root("my-shop").unwrap().child("orders").unwrap();
/// assert_eq!(urn.to_string(), "urn:my-shop:orders");
/// assert_eq!(urn.name(), "my-shop");
///
/// let parsed: Urn = "urn:my-shop:orders".parse().unwrap();
/// assert_eq!(parsed, urn);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Urn {
    segments: Vec<String>,
}

impl Urn {
    /// Start a URN at `name` — normally the name of a driving adapter.
    pub fn root<S: Into<String>>(name: S) -> anyhow::Result<Self> {
        Ok(Self {
            segments: vec![Self::sanitize(name.into())?],
        })
    }

    /// The root segment.
    pub fn name(&self) -> &str {
        &self.segments[0]
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Append one segment.
    pub fn child<S: Into<String>>(mut self, segment: S) -> anyhow::Result<Self> {
        self.segments.push(Self::sanitize(segment.into())?);
        Ok(self)
    }

    /// Append several segments, left to right.
    pub fn children<I, S>(mut self, segments: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for segment in segments {
            self.segments.push(Self::sanitize(segment.into())?);
        }
        Ok(self)
    }

    fn sanitize(s: String) -> anyhow::Result<String> {
        if s.is_empty() {
            anyhow::bail!("urn segment cannot be empty");
        }
        s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            .then(|| s.to_lowercase())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "urn segments may only contain alphanumeric characters, dashes and underscores; found `{s}`"
                )
            })
    }
}

impl Display for Urn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "urn:{}", self.segments.join(":"))
    }
}

impl FromStr for Urn {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut segments = value.split(':');
        if segments.next().is_none_or(|scheme| scheme != "urn") {
            anyhow::bail!("not a urn: expected the `urn:` prefix");
        }
        let Some(root) = segments.next() else {
            anyhow::bail!("urn is missing its root segment");
        };
        Self::root(root)?.children(segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_lowercase_both_halves() {
        let label = Label::new("Tag", "Foo");
        assert_eq!(label.to_string(), "tag: foo");
        assert_eq!(label.key(), "tag");
        assert_eq!(label.value(), "foo");
        assert_eq!(Label::from(("env", "prod")).value(), "prod");
    }

    #[test]
    fn urns_compose_from_root_and_children() {
        let urn = Urn::root("My-Shop").unwrap().child("Orders").unwrap();
        assert_eq!(urn.to_string(), "urn:my-shop:orders");
        assert_eq!(urn.name(), "my-shop");
        assert_eq!(urn.segments().len(), 2);

        let many = Urn::root("shop")
            .unwrap()
            .children(["orders", "items"])
            .unwrap();
        assert_eq!(many.to_string(), "urn:shop:orders:items");
    }

    #[test]
    fn invalid_segments_are_rejected() {
        assert!(Urn::root("invalid/root").is_err());
        assert!(Urn::root("shop").unwrap().child("").is_err());
        assert!(Urn::root("shop").unwrap().child("bad/segment").is_err());
    }

    #[test]
    fn urns_round_trip_through_strings() {
        let urn: Urn = "urn:my-shop:orders".parse().unwrap();
        assert_eq!(urn.to_string(), "urn:my-shop:orders");
        assert!("my-shop:orders".parse::<Urn>().is_err());
        assert!("urn:".parse::<Urn>().is_err());
        assert!("urn:invalid/root:orders".parse::<Urn>().is_err());
    }
}
