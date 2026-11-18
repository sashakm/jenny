//! Turning one name a user typed into every spelling the templates need.

use anyhow::{Result, bail};

/// The spellings of a resource name.
///
/// From `note` (or `Note`, or `order-line`) come the module name, the type name, the
/// URN segment and the URL path. Deriving them from one input is what keeps a generated
/// resource internally consistent — the alternative is four flags nobody wants to pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceNames {
    /// `note` — module and file name.
    pub snake: String,
    /// `Note` — type name.
    pub pascal: String,
    /// `notes` — URN segment and URL path.
    pub plural: String,
    /// `NOTE` — for constants.
    pub screaming: String,
}

impl ResourceNames {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("a resource name is required");
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("`{trimmed}` is not a usable resource name; use letters, digits, `-` and `_`");
        }
        if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            bail!("`{trimmed}` cannot start with a digit; it becomes a Rust identifier");
        }

        let snake = stringcase::snake_case(trimmed);
        Ok(Self {
            plural: pluralize(&snake),
            pascal: stringcase::pascal_case(trimmed),
            screaming: snake.to_uppercase(),
            snake,
        })
    }
}

/// The spellings of a project name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectNames {
    /// `my-notes` — crate name and directory.
    pub kebab: String,
    /// `my_notes` — the crate as an identifier.
    pub snake: String,
}

impl ProjectNames {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim().trim_end_matches('/');
        // A path is accepted; its last segment is the name.
        let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
        if name.is_empty() {
            bail!("a project name is required");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("`{name}` is not a usable crate name; use letters, digits, `-` and `_`");
        }
        if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            bail!("`{name}` cannot start with a digit");
        }

        Ok(Self {
            kebab: stringcase::kebab_case(name).to_lowercase(),
            snake: stringcase::snake_case(name),
        })
    }
}

/// Naive English pluralisation — enough for a scaffolded route path, and always
/// overridable by naming the resource whatever you want.
fn pluralize(word: &str) -> String {
    if word.ends_with('s') || word.ends_with('x') || word.ends_with("ch") || word.ends_with("sh") {
        format!("{word}es")
    } else if let Some(stem) = word.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        format!("{stem}ies")
    } else {
        format!("{word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_resource_name_yields_every_spelling() {
        let names = ResourceNames::parse("note").unwrap();
        assert_eq!(names.snake, "note");
        assert_eq!(names.pascal, "Note");
        assert_eq!(names.plural, "notes");
        assert_eq!(names.screaming, "NOTE");
    }

    #[test]
    fn the_input_casing_does_not_matter() {
        for input in ["order_line", "OrderLine", "order-line"] {
            let names = ResourceNames::parse(input).unwrap();
            assert_eq!(names.snake, "order_line", "for {input}");
            assert_eq!(names.pascal, "OrderLine", "for {input}");
            assert_eq!(names.plural, "order_lines", "for {input}");
        }
    }

    #[test]
    fn pluralisation_covers_the_ordinary_cases() {
        for (singular, plural) in [
            ("note", "notes"),
            ("box", "boxes"),
            ("batch", "batches"),
            ("dish", "dishes"),
            ("address", "addresses"),
            ("category", "categories"),
            ("day", "days"),
        ] {
            assert_eq!(pluralize(singular), plural, "for {singular}");
        }
    }

    #[test]
    fn unusable_resource_names_are_rejected() {
        for bad in ["", "   ", "my note", "note!", "note/v1", "1note"] {
            assert!(
                ResourceNames::parse(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }

    #[test]
    fn a_project_name_can_be_given_as_a_path() {
        let names = ProjectNames::parse("./services/my-notes").unwrap();
        assert_eq!(names.kebab, "my-notes");
        assert_eq!(names.snake, "my_notes");
    }

    #[test]
    fn project_names_normalise_casing() {
        let names = ProjectNames::parse("MyNotes").unwrap();
        assert_eq!(names.kebab, "my-notes");
        assert_eq!(names.snake, "my_notes");
    }

    #[test]
    fn unusable_project_names_are_rejected() {
        for bad in ["", "my notes", "notes!", "9lives"] {
            assert!(
                ProjectNames::parse(bad).is_err(),
                "`{bad}` should be rejected"
            );
        }
    }
}
