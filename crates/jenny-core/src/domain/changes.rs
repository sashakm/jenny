//! What changed between two states, as an RFC 6902 JSON Patch.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The difference between two JSON documents, as the [RFC 6902] operations that turn
/// the first into the second.
///
/// [`Store::change`](crate::port::StoreExt::change) hands one back for every change it
/// saves, computed over the aggregate's stored form, so an update event can say exactly
/// what changed without the use case comparing fields by hand:
///
/// ```
/// use jenny_core::domain::Changes;
/// use serde_json::json;
///
/// let changes = Changes::between(
///     &json!({ "title": "Shopping", "body": "milk" }),
///     &json!({ "title": "Groceries", "body": "milk" }),
/// );
/// assert_eq!(changes.paths().collect::<Vec<_>>(), ["/title"]);
/// assert_eq!(
///     serde_json::to_value(&changes).unwrap(),
///     json!([{ "op": "replace", "path": "/title", "value": "Groceries" }]),
/// );
/// ```
///
/// It serialises as the bare patch array, so it can travel in an event payload and be
/// applied by any RFC 6902 implementation on the other end.
///
/// [RFC 6902]: https://www.rfc-editor.org/rfc/rfc6902
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Changes(json_patch::Patch);

impl Changes {
    /// The operations that turn `before` into `after`. Empty when they are equal.
    pub fn between(before: &Value, after: &Value) -> Self {
        Self(json_patch::diff(before, after))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The JSON Pointer each operation targets, in order.
    pub fn paths(&self) -> impl Iterator<Item = String> + '_ {
        self.0.iter().map(|operation| operation.path().to_string())
    }

    /// Apply these changes to `document`. Either every operation applies, or the
    /// document is left as it was.
    pub fn apply_to(&self, document: &mut Value) -> Result<(), json_patch::PatchError> {
        json_patch::patch(document, &self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn equal_documents_have_no_changes() {
        let doc = json!({ "a": 1, "b": [1, 2] });
        assert!(Changes::between(&doc, &doc).is_empty());
    }

    #[test]
    fn nested_changes_name_the_innermost_path() {
        let changes = Changes::between(
            &json!({ "author": { "name": "Ada", "age": 36 } }),
            &json!({ "author": { "name": "Ada", "age": 37 } }),
        );
        assert_eq!(changes.paths().collect::<Vec<_>>(), ["/author/age"]);
    }

    #[test]
    fn applying_the_changes_reproduces_the_second_document() {
        let before = json!({ "title": "a", "tags": ["x"], "gone": true });
        let after = json!({ "title": "b", "tags": ["x", "y"] });
        let changes = Changes::between(&before, &after);

        let mut document = before.clone();
        changes.apply_to(&mut document).unwrap();
        assert_eq!(document, after);
    }

    #[test]
    fn changes_round_trip_as_a_bare_patch_array() {
        let changes = Changes::between(&json!({ "a": 1 }), &json!({ "a": 2 }));
        let wire = serde_json::to_value(&changes).unwrap();
        assert!(wire.is_array());
        assert_eq!(serde_json::from_value::<Changes>(wire).unwrap(), changes);
    }
}
