//! What `#[serde(...)]` says about a DTO's wire format, so that a derived schema and
//! validation messages name fields the way clients send them.
//!
//! Only the deserialising side matters here: a DTO is what arrives.

use syn::meta::ParseNestedMeta;
use syn::{Attribute, LitStr, Token};

/// Container-level serde options.
#[derive(Default)]
pub(crate) struct SerdeContainer {
    pub(crate) rename_all: Option<String>,
    /// `#[serde(default)]` on the struct: every field may be absent.
    pub(crate) default: bool,
}

/// Field-level serde options.
#[derive(Default)]
pub(crate) struct SerdeField {
    pub(crate) rename: Option<String>,
    /// The field may be absent from the input.
    pub(crate) default: bool,
    /// The field is never read from the input.
    pub(crate) skip: bool,
}

pub(crate) fn container(attrs: &[Attribute]) -> syn::Result<SerdeContainer> {
    let mut options = SerdeContainer::default();
    for attr in serde_attrs(attrs) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                options.rename_all = deserialize_name(&meta)?.or(options.rename_all.take());
            } else if meta.path.is_ident("default") {
                options.default = true;
                skip(&meta)?;
            } else {
                skip(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(options)
}

pub(crate) fn field(attrs: &[Attribute]) -> syn::Result<SerdeField> {
    let mut options = SerdeField::default();
    for attr in serde_attrs(attrs) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                options.rename = deserialize_name(&meta)?.or(options.rename.take());
            } else if meta.path.is_ident("default") {
                options.default = true;
                skip(&meta)?;
            } else if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing") {
                options.skip = true;
            } else {
                skip(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(options)
}

fn serde_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|attr| attr.path().is_ident("serde"))
}

/// `name = "x"`, or `name(deserialize = "x", serialize = "y")`, as the name used when
/// deserialising. `None` for a form that only sets the serialising name.
fn deserialize_name(meta: &ParseNestedMeta) -> syn::Result<Option<String>> {
    if meta.input.peek(Token![=]) {
        return Ok(Some(meta.value()?.parse::<LitStr>()?.value()));
    }
    let mut name = None;
    meta.parse_nested_meta(|inner| {
        if inner.path.is_ident("deserialize") {
            name = Some(inner.value()?.parse::<LitStr>()?.value());
        } else {
            skip(&inner)?;
        }
        Ok(())
    })?;
    Ok(name)
}

/// Consume an option this derive has no use for, whatever its shape.
fn skip(meta: &ParseNestedMeta) -> syn::Result<()> {
    if meta.input.peek(Token![=]) {
        meta.value()?.parse::<syn::Expr>()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(|inner| skip(&inner))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn renames_prefer_the_deserialising_name() {
        let parsed =
            field(&[parse_quote!(#[serde(rename(serialize = "out", deserialize = "in"))])])
                .unwrap();
        assert_eq!(parsed.rename.as_deref(), Some("in"));

        let parsed = field(&[parse_quote!(#[serde(rename = "both")])]).unwrap();
        assert_eq!(parsed.rename.as_deref(), Some("both"));

        let parsed = field(&[parse_quote!(#[serde(rename(serialize = "out"))])]).unwrap();
        assert_eq!(parsed.rename, None);
    }

    #[test]
    fn unrelated_options_of_every_shape_are_skipped() {
        let parsed = field(&[parse_quote!(
            #[serde(alias = "a", default = "fallback", with = "module", flatten, bound(deserialize = "T: X"))]
        )])
        .unwrap();
        assert!(parsed.default);
        assert!(!parsed.skip);
        assert_eq!(parsed.rename, None);

        let parsed_container = container(&[
            parse_quote!(#[serde(deny_unknown_fields, rename_all = "camelCase")]),
            parse_quote!(#[serde(default)]),
        ])
        .unwrap();
        assert_eq!(parsed_container.rename_all.as_deref(), Some("camelCase"));
        assert!(parsed_container.default);
    }

    #[test]
    fn skipped_fields_are_recognised() {
        assert!(field(&[parse_quote!(#[serde(skip)])]).unwrap().skip);
        assert!(
            field(&[parse_quote!(#[serde(skip_deserializing)])])
                .unwrap()
                .skip
        );
        assert!(
            !field(&[parse_quote!(#[serde(skip_serializing)])])
                .unwrap()
                .skip
        );
    }
}
