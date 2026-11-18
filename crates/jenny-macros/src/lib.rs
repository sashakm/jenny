mod aggregate;
mod dto;
mod query_format;
mod serde_attrs;
mod use_cases;
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

/// `impl Aggregate` for a struct: the identity field, and a kind.
///
/// ```ignore
/// use jenny::Aggregate;
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize, Aggregate)]
/// pub struct Note {
///     id: NoteId,          // the field called `id`, or the one marked #[aggregate(id)]
///     title: NoteTitle,
/// }
/// // Note::KIND == "note"
/// ```
///
/// Options: `#[aggregate(kind = "...")]` on the struct overrides the kind, which
/// otherwise is the type name in snake case; `#[aggregate(id)]` on a field picks the
/// identity when it is not called `id`; `#[aggregate(krate = ...)]` is the path to the
/// jenny facade, as for `Dto`.
#[proc_macro_derive(Aggregate, attributes(aggregate))]
pub fn derive_aggregate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    crate::aggregate::derive_aggregate_impl(input)
}

/// `impl DomainEvent`: the event's name.
///
/// ```ignore
/// #[derive(Serialize, DomainEvent)]
/// pub struct NoteCreated { pub id: NoteId }            // "note.created"
///
/// #[derive(Serialize, DomainEvent)]
/// #[event(name = "note.archived.v2")]
/// pub struct NoteArchived { pub id: NoteId }
/// ```
///
/// The default is the type name's words, lower case and dotted, so `NoteCreated`
/// becomes `note.created`. `#[event(krate = ...)]` is the path to the jenny facade.
#[proc_macro_derive(DomainEvent, attributes(event))]
pub fn derive_domain_event(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    crate::aggregate::derive_domain_event_impl(input)
}

/// Observe every use case in an `impl` block: a span and a duration measurement each,
/// labelled with the outcome.
///
/// ```ignore
/// #[jenny::use_cases]
/// impl Notes {
///     pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
///         // runs inside a `use_case` span named `notes.archive`, carrying
///         // `cx.request_id()`, and reports `ok` or the `DomainError` kind
///     }
/// }
/// ```
///
/// Every `pub async fn` is wrapped; private and synchronous methods are helpers and
/// stay as written. A wrapped method must return a `Result` whose error implements
/// `UseCaseError`, which `DomainError` does.
///
/// The body is moved into an `async` block unchanged, so `?` and `return` behave as
/// before. Nothing else is generated: no trait, no struct, no registration. The
/// dependency graph is still the struct's fields, which is what ADR-001 requires of a
/// macro (ADR-012).
///
/// Options: `#[use_cases(name = "...")]` replaces the span prefix, which is otherwise
/// the type name in snake case; `#[use_cases(krate = ...)]` is the path to the jenny
/// facade. On a method, `#[use_case(name = "...")]` renames it in the span and
/// `#[use_case(skip)]` leaves it alone.
#[proc_macro_attribute]
pub fn use_cases(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    crate::use_cases::use_cases_impl(attr, item)
}
