mod dto;
mod query_format;
mod serde_attrs;
pub(crate) mod utils;

/// One set of field constraints; a `Validate` impl and an OpenAPI schema out of it.
///
/// For **inbound adapter DTOs**, where constraints are mechanical and would otherwise
/// be written twice — once for validation, once for the schema — and drift.
///
/// Not for domain types. The domain's answer to invalid values is a smart constructor
/// that refuses to build one, which is strictly stronger than a validation pass, and a
/// derive here would compete with the better pattern.
///
/// ```ignore
/// use jenny::Dto;
/// use serde::Deserialize;
///
/// /// A note to create.
/// #[derive(Deserialize, Dto)]
/// pub struct NoteRequest {
///     #[dto(not_empty, max_len = 120, example = "Shopping")]
///     title: String,
///
///     #[dto(max_len = 10_000)]
///     #[serde(default)]
///     body: String,
///
///     #[dto(range_note = "priority is 1..=5", min = 1, max = 5)]
///     priority: Option<u8>,
/// }
/// ```
///
/// ### Field options
///
/// | Option | Applies to | Effect |
/// |---|---|---|
/// | `not_empty` | strings | non-blank once trimmed; `minLength: 1` |
/// | `min_len` / `max_len` | strings | character bounds; `minLength` / `maxLength` |
/// | `min` / `max` | numbers | inclusive bounds; `minimum` / `maximum` |
/// | `custom = path` | anything | `fn(&T) -> Result<(), String>` |
/// | `example` | anything | schema example |
/// | `description` | anything | schema description |
///
/// Constraints on an `Option<T>` apply to the value when present: absence is already
/// expressed by the type, and checking for it as well would contradict it. Fields that
/// are not `Option` and have no `#[serde(default)]` are marked required in the schema.
///
/// Fields are named as serde names them on the way in: `#[serde(rename = …)]` and the
/// container's `#[serde(rename_all = …)]` apply to schema properties and to validation
/// messages alike, so a client sending `firstName` reads `firstName: must not be empty`.
/// `#[serde(skip)]` fields are left out of the schema.
///
/// Container options are `title`, `description` (defaults to the doc comment) and
/// `krate` — the path to the jenny facade, `::jenny` unless you say otherwise.
///
/// The OpenAPI half is emitted only when the `openapi` feature is on.
#[proc_macro_derive(Dto, attributes(dto))]
pub fn derive_dto(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    crate::dto::derive_dto_impl(input)
}

#[proc_macro_derive(QueryFormat, attributes(query))]
pub fn derive_entity(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    crate::query_format::derive_entity_impl(input)
}
