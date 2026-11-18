//! Generic listing criteria: paging, sorting, filtering.
//!
//! Collection endpoints all want the same three things, and writing them per resource
//! gets old. This module is the transport-neutral vocabulary for them, so a REST
//! adapter and a future gRPC adapter produce the same [`Criteria`] and a repository
//! accepts it once.
//!
//! ### This vocabulary is stringly-typed on purpose, and that has a cost
//!
//! A [`Filter`] names its target as a string path, because the whole point is to accept
//! criteria a client composed. Nothing here can check that `"author.name"` is a real
//! attribute, and **an unchecked path that reaches a query language is an injection
//! vector**. Two things guard against that:
//!
//! 1. Parsing rejects anything that is not a dotted identifier path, so quotes,
//!    whitespace and punctuation never get past the front door.
//! 2. [`Criteria::ensure_allowed`] — and, in `jenny-persistence`, a mandatory allowlist
//!    argument — makes the *set of queryable attributes* an explicit decision at the
//!    place that knows the storage. There is deliberately no API that executes a
//!    filter without one.
//!
//! Where the set of useful queries is small and known, prefer a typed domain query
//! (`NoteQuery { archived: Option<bool> }`) over this. Generic criteria are for
//! genuinely open-ended list endpoints, and they trade type safety for that openness.

use std::fmt::Display;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::error::{DomainError, DomainResult};

/// Which way a [`Sort`] runs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Ordering {
    #[default]
    Asc,
    Desc,
}

impl Display for Ordering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Ordering::Asc => "asc",
            Ordering::Desc => "desc",
        })
    }
}

impl FromStr for Ordering {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "asc" => Ok(Ordering::Asc),
            "desc" => Ok(Ordering::Desc),
            other => Err(DomainError::invalid(format!(
                "unknown ordering `{other}`; expected one of: asc, desc"
            ))),
        }
    }
}

/// Order a listing by one attribute.
///
/// Parses from `path`, `path:asc`, `path:desc`, or the `-path` shorthand for
/// descending:
///
/// ```
/// use jenny_core::domain::query::{Ordering, Sort};
///
/// let s: Sort = "created_at:desc".parse()?;
/// assert_eq!(s.path(), "created_at");
/// assert_eq!(s.ordering(), Ordering::Desc);
///
/// assert_eq!("-created_at".parse::<Sort>()?, s);
/// assert_eq!("title".parse::<Sort>()?.ordering(), Ordering::Asc);
/// # Ok::<(), jenny_core::domain::DomainError>(())
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "SortFields")]
pub struct Sort {
    path: String,
    #[serde(default)]
    ordering: Ordering,
}

/// What a [`Sort`] deserialises from, so that the path is validated on that way in too.
#[derive(Deserialize)]
struct SortFields {
    path: String,
    #[serde(default)]
    ordering: Ordering,
}

impl TryFrom<SortFields> for Sort {
    type Error = DomainError;

    fn try_from(fields: SortFields) -> DomainResult<Self> {
        Sort::new(fields.path, fields.ordering)
    }
}

impl Sort {
    /// Fails when `path` is not a dotted identifier path.
    pub fn new(path: impl Into<String>, ordering: Ordering) -> DomainResult<Self> {
        Ok(Self {
            path: validate_path(path.into())?,
            ordering,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn ordering(&self) -> Ordering {
        self.ordering
    }
}

impl Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.path, self.ordering)
    }
}

impl FromStr for Sort {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix('-') {
            return Sort::new(path, Ordering::Desc);
        }
        match s.split_once(':') {
            Some((path, ordering)) => Sort::new(path, ordering.parse()?),
            None => Sort::new(s, Ordering::Asc),
        }
    }
}

/// How a [`Filter`] compares.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Check {
    /// Equal to the value.
    #[default]
    Eq,
    /// Starts with the value.
    Begins,
    /// Ends with the value.
    Ends,
    /// Contains the value.
    Contains,
    /// Greater than the value.
    Gt,
    /// Smaller than the value.
    St,
}

impl Check {
    /// The canonical keyword, as it appears in a query string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Check::Eq => "eq",
            Check::Begins => "begins",
            Check::Ends => "ends",
            Check::Contains => "contains",
            Check::Gt => "gt",
            Check::St => "st",
        }
    }

    /// Every keyword a client may write, for error messages.
    pub const KEYWORDS: &'static [&'static str] =
        &["eq", "begins", "ends", "contains", "gt", "st", "lt"];
}

impl Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Check {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "eq" => Ok(Check::Eq),
            "begins" => Ok(Check::Begins),
            "ends" => Ok(Check::Ends),
            "contains" => Ok(Check::Contains),
            "gt" => Ok(Check::Gt),
            // `st` is canonical; `lt` is accepted because it is what most people type.
            "st" | "lt" => Ok(Check::St),
            other => Err(DomainError::invalid(format!(
                "unknown check `{other}`; expected one of: {}",
                Check::KEYWORDS.join(", ")
            ))),
        }
    }
}

/// Narrow a listing to entries whose `path` satisfies `check` against `value`.
///
/// Parses from `path:check:value`, or `path:value` when the check is left out:
///
/// ```
/// use jenny_core::domain::query::{Check, Filter};
///
/// let f: Filter = "status:eq:active".parse()?;
/// assert_eq!((f.path(), f.check(), f.value()), ("status", Check::Eq, "active"));
///
/// // Two parts: the check defaults to `eq`.
/// assert_eq!("name:Bob".parse::<Filter>()?.check(), Check::Eq);
///
/// // The split is bounded, so a value may contain colons.
/// assert_eq!("at:eq:12:30".parse::<Filter>()?.value(), "12:30");
///
/// // A middle token that is not a check is an error, never a silent misparse.
/// assert!("a:nope:b".parse::<Filter>().is_err());
/// # Ok::<(), jenny_core::domain::DomainError>(())
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "FilterFields")]
pub struct Filter {
    path: String,
    #[serde(default)]
    check: Check,
    value: String,
}

/// What a [`Filter`] deserialises from, so that the path is validated on that way in too.
#[derive(Deserialize)]
struct FilterFields {
    path: String,
    #[serde(default)]
    check: Check,
    value: String,
}

impl TryFrom<FilterFields> for Filter {
    type Error = DomainError;

    fn try_from(fields: FilterFields) -> DomainResult<Self> {
        Filter::new(fields.path, fields.check, fields.value)
    }
}

impl Filter {
    /// Fails when `path` is not a dotted identifier path.
    pub fn new(
        path: impl Into<String>,
        check: Check,
        value: impl Into<String>,
    ) -> DomainResult<Self> {
        Ok(Self {
            path: validate_path(path.into())?,
            check,
            value: value.into(),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn check(&self) -> Check {
        self.check
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.path, self.check, self.value)
    }
}

impl FromStr for Filter {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // At most two splits, so the value keeps any colons of its own.
        let mut parts = s.splitn(3, ':');
        let path = parts.next().unwrap_or_default();
        let Some(second) = parts.next() else {
            return Err(DomainError::invalid(format!(
                "filter `{s}` is missing a value; expected `path:value` or `path:check:value`"
            )));
        };

        match parts.next() {
            // Three parts: the middle one must be a check. Guessing here would turn a
            // typo into a filter that silently matches the wrong thing.
            Some(value) => Filter::new(path, second.parse()?, value),
            None => Filter::new(path, Check::Eq, second),
        }
    }
}

/// How many entries a page holds when the client did not say.
pub const DEFAULT_PAGE_SIZE: u32 = 50;
/// The most entries a page may hold. A listing is never unbounded: a table that grows
/// must not turn one request into a full scan shipped over the wire.
pub const MAX_PAGE_SIZE: u32 = 1000;

/// Which slice of a listing to return. Pages are 1-based.
///
/// Every listing is paged: without a size it is [`DEFAULT_PAGE_SIZE`] entries, and no
/// size goes above [`MAX_PAGE_SIZE`].
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Page {
    #[serde(default)]
    number: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

impl Page {
    pub fn new(number: Option<u32>, size: Option<u32>) -> Self {
        Self { number, size }
    }

    /// How many entries to skip. Page `0` is treated as page `1` rather than
    /// underflowing.
    pub fn offset(&self) -> u32 {
        self.limit()
            .saturating_mul(self.number.unwrap_or(1).saturating_sub(1))
    }

    /// How many entries to return: the requested size, [`DEFAULT_PAGE_SIZE`] when none
    /// (or zero) was requested, and never more than [`MAX_PAGE_SIZE`].
    pub fn limit(&self) -> u32 {
        self.size
            .filter(|size| *size > 0)
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .min(MAX_PAGE_SIZE)
    }

    pub fn number(&self) -> Option<u32> {
        self.number
    }

    pub fn size(&self) -> Option<u32> {
        self.size
    }

    /// `true` when the client asked for neither a page nor a size.
    pub fn is_unset(&self) -> bool {
        self.number.is_none() && self.size.is_none()
    }
}

/// Everything a client asked for about a listing.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Criteria {
    #[serde(default)]
    pub page: Page,
    #[serde(default)]
    pub sort: Vec<Sort>,
    #[serde(default)]
    pub filter: Vec<Filter>,
}

impl Criteria {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_page(mut self, page: Page) -> Self {
        self.page = page;
        self
    }

    pub fn with_sort(mut self, sort: Sort) -> Self {
        self.sort.push(sort);
        self
    }

    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filter.push(filter);
        self
    }

    /// Every attribute path mentioned, sorted or filtered.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.sort
            .iter()
            .map(Sort::path)
            .chain(self.filter.iter().map(Filter::path))
    }

    /// Reject any path that is not in `allowed`.
    ///
    /// Call this — or let `jenny-persistence` do it, since its translator requires the
    /// allowlist as an argument — before a criteria set reaches storage. The error
    /// names the offending path and lists what *is* queryable, because a client that
    /// guessed wrong needs to know what to guess instead.
    ///
    /// ```
    /// use jenny_core::domain::query::Criteria;
    ///
    /// let criteria = Criteria::new().with_sort("title".parse()?);
    /// assert!(criteria.ensure_allowed(&["title", "created_at"]).is_ok());
    /// assert!(criteria.ensure_allowed(&["created_at"]).is_err());
    /// # Ok::<(), jenny_core::domain::DomainError>(())
    /// ```
    pub fn ensure_allowed(&self, allowed: &[&str]) -> DomainResult<()> {
        for path in self.paths() {
            if !allowed.contains(&path) {
                return Err(DomainError::invalid(format!(
                    "`{path}` is not queryable; try one of: {}",
                    allowed.join(", ")
                )));
            }
        }
        Ok(())
    }
}

/// A dotted identifier path — `title`, `author.name`.
///
/// The first line of defence. Rejecting quotes, whitespace, wildcards and punctuation
/// here means a path can never carry a fragment of a query language, whatever the
/// allowlist downstream does or does not do.
fn validate_path(path: String) -> DomainResult<String> {
    if path.is_empty() {
        return Err(DomainError::invalid("attribute path must not be empty"));
    }
    let valid = path.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    if valid {
        Ok(path)
    } else {
        Err(DomainError::invalid(format!(
            "`{path}` is not a valid attribute path; expected dotted identifiers like `author.name`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_parses_every_accepted_spelling() {
        assert_eq!(
            "created_at".parse::<Sort>().unwrap(),
            Sort::new("created_at", Ordering::Asc).unwrap()
        );
        assert_eq!(
            "created_at:asc".parse::<Sort>().unwrap().ordering(),
            Ordering::Asc
        );
        assert_eq!(
            "created_at:DESC".parse::<Sort>().unwrap().ordering(),
            Ordering::Desc
        );
        assert_eq!(
            "-created_at".parse::<Sort>().unwrap().ordering(),
            Ordering::Desc
        );
    }

    #[test]
    fn an_unknown_ordering_names_the_alternatives() {
        let err = "title:sideways".parse::<Sort>().unwrap_err();
        assert!(err.to_string().contains("asc, desc"), "{err}");
    }

    #[test]
    fn filters_default_to_equality() {
        let f = "name:Bob".parse::<Filter>().unwrap();
        assert_eq!(f.path(), "name");
        assert_eq!(f.check(), Check::Eq);
        assert_eq!(f.value(), "Bob");
    }

    #[test]
    fn every_check_keyword_parses() {
        for (input, expected) in [
            ("eq", Check::Eq),
            ("begins", Check::Begins),
            ("ends", Check::Ends),
            ("contains", Check::Contains),
            ("gt", Check::Gt),
            ("st", Check::St),
            ("lt", Check::St),
            ("CONTAINS", Check::Contains),
        ] {
            assert_eq!(input.parse::<Check>().unwrap(), expected, "parsing {input}");
        }
    }

    #[test]
    fn a_value_may_contain_colons() {
        let f = "at:eq:12:30:00".parse::<Filter>().unwrap();
        assert_eq!(f.value(), "12:30:00");
        assert_eq!(f.check(), Check::Eq);
    }

    #[test]
    fn an_unknown_check_is_an_error_not_a_guess() {
        let err = "a:nope:b".parse::<Filter>().unwrap_err();
        assert!(err.to_string().contains("unknown check"), "{err}");
        assert!(matches!(err, DomainError::Invalid { .. }));
    }

    #[test]
    fn a_filter_without_a_value_is_rejected() {
        let err = "justapath".parse::<Filter>().unwrap_err();
        assert!(err.to_string().contains("missing a value"), "{err}");
    }

    #[test]
    fn paths_that_could_carry_injection_are_rejected_at_parse_time() {
        for hostile in [
            "title; DROP TABLE notes",
            "title'",
            "title\"",
            "1=1",
            "ti tle",
            "title--",
            "*",
            "",
            "author..name",
            ".name",
        ] {
            assert!(
                Sort::new(hostile, Ordering::Asc).is_err(),
                "`{hostile}` should not parse as an attribute path"
            );
            assert!(
                Filter::new(hostile, Check::Eq, "x").is_err(),
                "`{hostile}` should not parse as an attribute path"
            );
        }
    }

    #[test]
    fn ordinary_paths_are_accepted() {
        for good in ["title", "created_at", "author.name", "_internal", "a1.b2"] {
            assert!(
                Sort::new(good, Ordering::Asc).is_ok(),
                "`{good}` should parse"
            );
        }
    }

    #[test]
    fn paging_is_one_based_and_does_not_underflow() {
        assert_eq!(Page::default().offset(), 0);
        assert!(Page::default().is_unset());

        assert_eq!(Page::new(Some(2), Some(40)).offset(), 40);
        assert_eq!(Page::new(Some(1), Some(40)).offset(), 0);
        assert_eq!(Page::new(Some(0), Some(40)).offset(), 0);
        assert_eq!(Page::new(Some(3), Some(40)).limit(), 40);
    }

    #[test]
    fn the_allowlist_rejects_anything_it_does_not_name() {
        let criteria = Criteria::new()
            .with_sort("title".parse().unwrap())
            .with_filter("author.name:contains:bo".parse().unwrap());

        assert!(criteria.ensure_allowed(&["title", "author.name"]).is_ok());

        let err = criteria.ensure_allowed(&["title"]).unwrap_err();
        assert!(err.to_string().contains("author.name"), "{err}");
        assert!(err.to_string().contains("not queryable"), "{err}");
    }

    #[test]
    fn criteria_report_every_path_they_mention() {
        let criteria = Criteria::new()
            .with_sort("title".parse().unwrap())
            .with_filter("status:eq:active".parse().unwrap());
        assert_eq!(criteria.paths().collect::<Vec<_>>(), ["title", "status"]);
    }

    #[test]
    fn sort_and_filter_round_trip_through_display() {
        for input in ["created_at:desc", "title:asc"] {
            let parsed: Sort = input.parse().unwrap();
            assert_eq!(parsed.to_string(), input);
            assert_eq!(parsed.to_string().parse::<Sort>().unwrap(), parsed);
        }
        for input in ["status:eq:active", "title:contains:milk"] {
            let parsed: Filter = input.parse().unwrap();
            assert_eq!(parsed.to_string(), input);
            assert_eq!(parsed.to_string().parse::<Filter>().unwrap(), parsed);
        }
    }

    #[test]
    fn deserialising_validates_paths_like_parsing_does() {
        let criteria: Criteria = serde_json::from_str(
            r#"{ "sort": [{ "path": "title", "ordering": "desc" }], "filter": [{ "path": "author.name", "value": "x" }] }"#,
        )
        .unwrap();
        assert_eq!(criteria.sort[0].path(), "title");
        assert_eq!(criteria.filter[0].check(), Check::Eq);

        for hostile in [
            r#"{ "sort": [{ "path": "title; DROP TABLE notes" }] }"#,
            r#"{ "filter": [{ "path": "a'--", "value": "x" }] }"#,
            r#"{ "filter": [{ "path": "", "value": "x" }] }"#,
        ] {
            assert!(
                serde_json::from_str::<Criteria>(hostile).is_err(),
                "{hostile} must not deserialise"
            );
        }
    }

    #[test]
    fn every_page_is_bounded() {
        assert_eq!(Page::default().limit(), DEFAULT_PAGE_SIZE);
        assert_eq!(Page::new(None, Some(0)).limit(), DEFAULT_PAGE_SIZE);
        assert_eq!(Page::new(None, Some(20)).limit(), 20);
        assert_eq!(Page::new(None, Some(u32::MAX)).limit(), MAX_PAGE_SIZE);
        assert_eq!(Page::new(Some(3), None).offset(), 2 * DEFAULT_PAGE_SIZE);
        assert_eq!(
            Page::new(Some(u32::MAX), Some(MAX_PAGE_SIZE)).offset(),
            u32::MAX
        );
    }
}
