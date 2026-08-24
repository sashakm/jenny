//! A very small template renderer.
//!
//! Two constructs, and no more:
//!
//! - `{{name}}` — substitute a variable.
//! - `{{#if flag}}` … `{{else}}` … `{{/if}}` — include a block, on its own lines.
//!
//! Deliberately not a template engine. Scaffolding needs substitution and a handful of
//! feature-conditional blocks, and a real engine would bring an expression language,
//! its own error surface and a dependency, to render files that ship inside this
//! binary. What it does have is strictness: an unknown variable or an unbalanced block
//! is an error, so a broken template fails here rather than emitting broken Rust.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

/// The values a template is rendered against.
#[derive(Debug, Default)]
pub struct Bindings {
    values: BTreeMap<String, String>,
    flags: BTreeSet<String>,
}

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.values.insert(key.to_string(), value.into());
        self
    }

    pub fn flag(mut self, name: &str, enabled: bool) -> Self {
        if enabled {
            self.flags.insert(name.to_string());
        }
        self
    }

    pub fn has_flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

/// Render `source`, failing on anything the bindings cannot satisfy.
pub fn render(source: &str, bindings: &Bindings) -> Result<String> {
    let included = apply_conditionals(source, bindings)?;
    substitute(&included, bindings)
}

/// Keep only the lines whose enclosing `{{#if}}` blocks are satisfied.
fn apply_conditionals(source: &str, bindings: &Bindings) -> Result<String> {
    // Each frame is (this block is taken, an earlier branch already matched).
    let mut stack: Vec<(bool, bool)> = Vec::new();
    let mut out = String::with_capacity(source.len());

    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let emitting = stack.iter().all(|(taken, _)| *taken);

        if let Some(flag) = directive(trimmed, "#if") {
            let taken = emitting && bindings.has_flag(flag);
            stack.push((taken, taken));
            continue;
        }
        if trimmed == "{{else}}" {
            let Some((taken, matched)) = stack.pop() else {
                bail!(
                    "line {}: `{{{{else}}}}` outside any `{{{{#if}}}}`",
                    number + 1
                );
            };
            let parents_emitting = stack.iter().all(|(taken, _)| *taken);
            stack.push((parents_emitting && !matched, matched || taken));
            continue;
        }
        if trimmed == "{{/if}}" {
            if stack.pop().is_none() {
                bail!(
                    "line {}: `{{{{/if}}}}` without a matching `{{{{#if}}}}`",
                    number + 1
                );
            }
            continue;
        }

        if emitting {
            out.push_str(line);
            out.push('\n');
        }
    }

    if !stack.is_empty() {
        bail!("template ended with {} unclosed `{{{{#if}}}}`", stack.len());
    }
    Ok(out)
}

/// `{{#if flag}}` on a line of its own -> `Some("flag")`.
fn directive<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let inner = line.strip_prefix("{{")?.strip_suffix("}}")?.trim();
    let rest = inner.strip_prefix(keyword)?;
    let name = rest.trim();
    (!name.is_empty()).then_some(name)
}

fn substitute(source: &str, bindings: &Bindings) -> Result<String> {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .with_context(|| format!("unterminated `{{{{` near: {}", preview(after)))?;
        let key = after[..end].trim();

        // `{{{name}}}` is ambiguous: a literal brace before a placeholder, or a
        // placeholder whose name starts with one. Rather than guess, say so — the
        // template should split the line instead.
        if key.starts_with('{') || key.ends_with('}') {
            bail!(
                "ambiguous `{{{{{{` near `{key}`: put the literal brace on its own line, \
                 or split the statement"
            );
        }

        let value = bindings
            .get(key)
            .with_context(|| format!("template refers to unknown variable `{key}`"))?;
        out.push_str(value);
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

fn preview(text: &str) -> String {
    text.chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings() -> Bindings {
        Bindings::new()
            .set("name", "my-notes")
            .set("Resource", "Note")
            .flag("persistence", true)
            .flag("messaging", false)
    }

    #[test]
    fn variables_are_substituted() {
        // Rendering normalises to a trailing newline: these become files.
        assert_eq!(
            render("service {{name}} has {{Resource}}s", &bindings()).unwrap(),
            "service my-notes has Notes\n"
        );
    }

    #[test]
    fn the_same_variable_can_appear_more_than_once() {
        assert_eq!(
            render("{{name}}/{{name}}", &bindings()).unwrap(),
            "my-notes/my-notes\n"
        );
    }

    #[test]
    fn an_ambiguous_triple_brace_is_reported_rather_than_guessed() {
        let err = render("use a::{{{name}}b};", &bindings()).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn an_unknown_variable_is_an_error_rather_than_an_empty_string() {
        let err = render("{{nope}}", &bindings()).unwrap_err();
        assert!(err.to_string().contains("unknown variable `nope`"), "{err}");
    }

    #[test]
    fn an_unterminated_variable_is_an_error() {
        assert!(render("{{name", &bindings()).is_err());
    }

    #[test]
    fn enabled_blocks_are_kept_and_disabled_ones_dropped() {
        let source = "\
always
{{#if persistence}}
sqlx = \"0.9\"
{{/if}}
{{#if messaging}}
nats = \"1\"
{{/if}}
end";
        assert_eq!(
            render(source, &bindings()).unwrap(),
            "always\nsqlx = \"0.9\"\nend\n"
        );
    }

    #[test]
    fn else_branches_work() {
        let source = "\
{{#if messaging}}
bus
{{else}}
no bus
{{/if}}";
        assert_eq!(render(source, &bindings()).unwrap(), "no bus\n");

        let source = "\
{{#if persistence}}
db
{{else}}
no db
{{/if}}";
        assert_eq!(render(source, &bindings()).unwrap(), "db\n");
    }

    #[test]
    fn blocks_nest() {
        let source = "\
{{#if persistence}}
outer
{{#if messaging}}
inner
{{/if}}
still outer
{{/if}}";
        assert_eq!(render(source, &bindings()).unwrap(), "outer\nstill outer\n");
    }

    #[test]
    fn a_disabled_outer_block_hides_an_enabled_inner_one() {
        let source = "\
{{#if messaging}}
outer
{{#if persistence}}
inner
{{/if}}
{{/if}}
after";
        assert_eq!(render(source, &bindings()).unwrap(), "after\n");
    }

    #[test]
    fn unbalanced_blocks_are_errors() {
        assert!(render("{{#if persistence}}\nx", &bindings()).is_err());
        assert!(render("x\n{{/if}}", &bindings()).is_err());
        assert!(render("{{else}}", &bindings()).is_err());
    }

    #[test]
    fn variables_inside_kept_blocks_are_still_substituted() {
        let source = "\
{{#if persistence}}
name = \"{{name}}\"
{{/if}}";
        assert_eq!(
            render(source, &bindings()).unwrap(),
            "name = \"my-notes\"\n"
        );
    }
}
