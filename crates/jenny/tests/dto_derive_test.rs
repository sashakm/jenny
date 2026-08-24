//! `#[derive(Dto)]` end to end: one set of attributes, a `Validate` impl and an
//! OpenAPI schema that agree with each other.

use jenny::{Dto, Validate};
use serde::Deserialize;

fn not_shouting(value: &str) -> Result<(), String> {
    if value.chars().any(|c| c.is_ascii_uppercase()) {
        Err("must be lowercase".to_string())
    } else {
        Ok(())
    }
}

/// A note to create.
#[derive(Debug, Deserialize, Dto)]
struct NoteRequest {
    #[dto(not_empty, max_len = 8, example = "Shopping")]
    title: String,

    #[dto(min_len = 2, max_len = 100, description = "Free text")]
    #[serde(default)]
    body: String,

    #[dto(min = 1, max = 5)]
    priority: u8,

    /// Only checked when present.
    #[dto(not_empty, max_len = 4)]
    tag: Option<String>,

    #[dto(custom = "not_shouting")]
    slug: String,
}

fn valid() -> NoteRequest {
    NoteRequest {
        title: "Shopping".into(),
        body: "milk".into(),
        priority: 3,
        tag: None,
        slug: "shopping".into(),
    }
}

fn issues(request: &NoteRequest) -> Vec<String> {
    request
        .validate()
        .unwrap_err()
        .issues()
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_valid_dto_passes() {
    assert!(valid().validate().is_ok());
}

#[test]
fn string_constraints_are_enforced() {
    let mut request = valid();
    request.title = "   ".into();
    assert!(issues(&request).contains(&"title: must not be empty".to_string()));

    request.title = "way too long a title".into();
    assert!(
        issues(&request)
            .iter()
            .any(|i| i.contains("at most 8 characters"))
    );

    let mut request = valid();
    request.body = "x".into();
    assert!(
        issues(&request)
            .iter()
            .any(|i| i.starts_with("body:") && i.contains("at least 2 characters"))
    );
}

#[test]
fn numeric_bounds_are_enforced_on_both_sides() {
    let mut request = valid();
    request.priority = 0;
    assert!(
        issues(&request)
            .iter()
            .any(|i| i == "priority: must be at least 1")
    );

    request.priority = 9;
    assert!(
        issues(&request)
            .iter()
            .any(|i| i == "priority: must be at most 5")
    );
}

#[test]
fn constraints_on_an_option_apply_only_when_present() {
    let mut request = valid();

    request.tag = None;
    assert!(request.validate().is_ok(), "absence is not a violation");

    request.tag = Some("  ".into());
    assert!(issues(&request).contains(&"tag: must not be empty".to_string()));

    request.tag = Some("far too long".into());
    assert!(
        issues(&request)
            .iter()
            .any(|i| i.starts_with("tag:") && i.contains("at most 4"))
    );

    request.tag = Some("home".into());
    assert!(request.validate().is_ok());
}

#[test]
fn a_custom_check_runs() {
    let mut request = valid();
    request.slug = "Shopping".into();
    assert!(issues(&request).contains(&"slug: must be lowercase".to_string()));
}

#[test]
fn every_violation_is_collected_not_just_the_first() {
    let request = NoteRequest {
        title: String::new(),
        body: "x".into(),
        priority: 99,
        tag: Some(String::new()),
        slug: "NOPE".into(),
    };
    let found = issues(&request);
    assert_eq!(
        found.len(),
        5,
        "expected one issue per broken rule, got {found:?}"
    );
}

#[cfg(feature = "openapi")]
mod openapi {
    use super::*;
    use utoipa::{PartialSchema, ToSchema};

    fn schema() -> serde_json::Value {
        serde_json::to_value(NoteRequest::schema()).unwrap()
    }

    #[test]
    fn the_schema_carries_the_same_constraints_as_the_validation() {
        let schema = schema();
        let properties = &schema["properties"];

        assert_eq!(properties["title"]["maxLength"], 8);
        assert_eq!(
            properties["title"]["minLength"], 1,
            "`not_empty` should surface as minLength 1"
        );
        assert_eq!(properties["title"]["examples"][0], "Shopping");

        assert_eq!(properties["body"]["minLength"], 2);
        assert_eq!(properties["body"]["maxLength"], 100);
        assert_eq!(properties["body"]["description"], "Free text");

        assert_eq!(properties["priority"]["minimum"], 1);
        assert_eq!(properties["priority"]["maximum"], 5);
    }

    #[test]
    fn the_title_and_description_come_from_the_type() {
        let schema = schema();
        assert_eq!(schema["title"], "NoteRequest");
        assert_eq!(
            schema["description"], "A note to create.",
            "the doc comment should become the description"
        );
        assert_eq!(NoteRequest::name(), "NoteRequest");
    }

    #[test]
    fn only_non_optional_fields_are_required() {
        let schema = schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"title"));
        assert!(required.contains(&"priority"));
        assert!(
            !required.contains(&"tag"),
            "an Option field must not be required"
        );
    }

    #[test]
    fn an_optional_field_is_described_by_its_inner_type() {
        let schema = schema();
        // `Option<String>` should describe the String, with the constraints applied.
        assert_eq!(schema["properties"]["tag"]["maxLength"], 4);
    }
}
