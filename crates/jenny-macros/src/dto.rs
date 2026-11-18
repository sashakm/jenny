//! `#[derive(Dto)]` — one set of constraints, two consumers.

use darling::FromAttributes;
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::ext::IdentExt;
use syn::{Data, DeriveInput, Fields, Lit, Type, parse_macro_input};

use crate::serde_attrs;
use crate::utils::apply_casing;

/// Container-level `#[dto(...)]` options.
#[derive(Default, FromAttributes)]
#[darling(attributes(dto), default)]
struct DtoContainer {
    /// Schema title. Defaults to the type name.
    title: Option<String>,
    /// Schema description. Defaults to the type's doc comment, if any.
    description: Option<String>,
    /// Where to find `Validate` and `ValidationErrors`.
    ///
    /// Defaults to `::jenny`, the facade almost every service depends on. Crates that
    /// depend on `jenny-core` directly — the framework's own, mostly — pass
    /// `#[dto(krate = ::jenny_core)]`. A proc macro cannot discover what its caller
    /// named its dependencies, so this has to be sayable.
    krate: Option<syn::Path>,
}

/// Field-level `#[dto(...)]` options.
#[derive(Default, FromAttributes)]
#[darling(attributes(dto), default)]
struct DtoField {
    /// String must not be empty once trimmed.
    not_empty: bool,
    /// Minimum length in characters.
    min_len: Option<usize>,
    /// Maximum length in characters.
    max_len: Option<usize>,
    /// Inclusive numeric lower bound.
    min: Option<Lit>,
    /// Inclusive numeric upper bound.
    max: Option<Lit>,
    /// Extra check: a path to `fn(&T) -> Result<(), String>`.
    custom: Option<syn::Path>,
    /// Example value for the schema.
    example: Option<Lit>,
    /// Field description for the schema.
    description: Option<String>,
}

pub(crate) fn derive_dto_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.write_errors().into(),
    }
}

fn expand(input: &DeriveInput) -> darling::Result<TokenStream> {
    let container = DtoContainer::from_attributes(&input.attrs)?;
    let serde_container = serde_attrs::container(&input.attrs)?;
    let ident = &input.ident;

    let Data::Struct(data) = &input.data else {
        return Err(darling::Error::custom(
            "Dto can only be derived for structs; enums have no field constraints to lift",
        )
        .with_span(ident));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(darling::Error::custom(
            "Dto needs named fields; a tuple struct has no field names to report in errors",
        )
        .with_span(ident));
    };

    let mut validations = Vec::new();
    let mut properties = Vec::new();
    let mut required = Vec::new();

    for field in &fields.named {
        let options = DtoField::from_attributes(&field.attrs)?;
        let serde_field = serde_attrs::field(&field.attrs)?;
        let name = field.ident.as_ref().expect("named field");
        // The name the field has on the wire, which is the one a client needs to see.
        let name_str = match (&serde_field.rename, &serde_container.rename_all) {
            (Some(rename), _) => rename.clone(),
            (None, Some(casing)) => {
                apply_casing(casing, &name.unraw().to_string()).map_err(|e| e.with_span(name))?
            }
            (None, None) => name.unraw().to_string(),
        };
        let ty = &field.ty;

        validations.extend(field_validation(name, &name_str, ty, &options));
        if serde_field.skip {
            continue;
        }
        properties.push(field_schema(&name_str, ty, &options));
        if !is_option(ty) && !serde_field.default && !serde_container.default {
            required.push(quote!(#name_str));
        }
    }

    let krate = container
        .krate
        .unwrap_or_else(|| syn::parse_quote!(::jenny));
    let title = container.title.unwrap_or_else(|| ident.to_string());
    let description = container
        .description
        .or_else(|| doc_comment(&input.attrs))
        .map(|d| quote!(Some(#d)))
        .unwrap_or_else(|| quote!(None::<&str>));

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let schema_name = ident.to_string();

    let validate_impl = quote! {
        #[automatically_derived]
        impl #impl_generics #krate::domain::Validate for #ident #ty_generics #where_clause {
            fn validate(
                &self,
            ) -> ::core::result::Result<(), #krate::domain::ValidationErrors> {
                let mut __errors = #krate::domain::ValidationErrors::default();
                #(#validations)*
                __errors.into_result()
            }
        }
    };

    // Gated on *this* crate's feature, decided when jenny-macros is compiled. Emitting
    // `#[cfg(feature = "openapi")]` into the caller instead would silently vanish in
    // any crate that happens not to define a feature by that name.
    if !cfg!(feature = "openapi") {
        return Ok(validate_impl);
    }

    Ok(quote! {
        #validate_impl

        #[automatically_derived]
        impl #impl_generics ::utoipa::PartialSchema for #ident #ty_generics #where_clause {
            fn schema() -> ::utoipa::openapi::RefOr<::utoipa::openapi::schema::Schema> {
                #[allow(unused_mut)]
                let mut __object = ::utoipa::openapi::ObjectBuilder::new()
                    .schema_type(::utoipa::openapi::schema::Type::Object)
                    .title(Some(#title))
                    .description(#description);
                #(#properties)*
                #( __object = __object.required(#required); )*
                ::utoipa::openapi::RefOr::T(::utoipa::openapi::schema::Schema::Object(
                    __object.build(),
                ))
            }
        }

        #[automatically_derived]
        impl #impl_generics ::utoipa::ToSchema for #ident #ty_generics #where_clause {
            fn name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(#schema_name)
            }
        }
    })
}

/// The validation statements for one field.
fn field_validation(
    name: &syn::Ident,
    name_str: &str,
    ty: &Type,
    options: &DtoField,
) -> Vec<TokenStream> {
    let mut checks = Vec::new();

    // Every check reads a `__value` binding rather than inlining `&self.field`.
    // Inlining would generate `&self.title.trim().is_empty()`, which parses as
    // `&(…)` — a borrow of the result, not a borrow of the field.
    if options.not_empty {
        checks.push(quote! {
            if __value.trim().is_empty() {
                __errors.push_field(#name_str, "must not be empty");
            }
        });
    }
    if let Some(min) = options.min_len {
        checks.push(quote! {
            if __value.chars().count() < #min {
                __errors.push_field(#name_str, format!("must be at least {} characters", #min));
            }
        });
    }
    if let Some(max) = options.max_len {
        checks.push(quote! {
            if __value.chars().count() > #max {
                __errors.push_field(#name_str, format!("must be at most {} characters", #max));
            }
        });
    }
    if let Some(min) = &options.min {
        checks.push(quote! {
            if *__value < (#min as _) {
                __errors.push_field(#name_str, format!("must be at least {}", #min));
            }
        });
    }
    if let Some(max) = &options.max {
        checks.push(quote! {
            if *__value > (#max as _) {
                __errors.push_field(#name_str, format!("must be at most {}", #max));
            }
        });
    }
    if let Some(custom) = &options.custom {
        checks.push(quote! {
            if let Err(__reason) = #custom(__value) {
                __errors.push_field(#name_str, __reason);
            }
        });
    }

    if checks.is_empty() {
        return Vec::new();
    }

    // `Option<T>` constraints apply to the value when present, not to its absence —
    // absence is expressed by the type, and checking for it too would contradict it.
    if is_option(ty) {
        vec![quote! {
            if let Some(__value) = self.#name.as_ref() {
                #(#checks)*
            }
        }]
    } else {
        vec![quote! {
            {
                let __value = &self.#name;
                #(#checks)*
            }
        }]
    }
}

/// The schema property for one field, with the same constraints applied.
fn field_schema(name_str: &str, ty: &Type, options: &DtoField) -> TokenStream {
    let inner = inner_option_type(ty).unwrap_or(ty);

    let mut applications = Vec::new();
    if let Some(min) = options.min_len.or(options.not_empty.then_some(1)) {
        applications.push(quote!(__field.min_length = Some(#min);));
    }
    if let Some(max) = options.max_len {
        applications.push(quote!(__field.max_length = Some(#max);));
    }
    if let Some(min) = &options.min {
        let number = number_literal(min);
        applications.push(quote!(__field.minimum = Some(#number);));
    }
    if let Some(max) = &options.max {
        let number = number_literal(max);
        applications.push(quote!(__field.maximum = Some(#number);));
    }
    if let Some(description) = &options.description {
        applications.push(quote!(__field.description = Some(#description.to_string());));
    }
    if let Some(example) = &options.example {
        // `.into()` rather than `serde_json::json!`, so the caller need not depend on
        // serde_json just to write an example.
        applications.push(quote!(__field.examples.push(#example.into());));
    }

    quote! {
        __object = __object.property(#name_str, {
            let mut __schema = <#inner as ::utoipa::PartialSchema>::schema();
            // Constraints only mean anything on a concrete object schema; a `$ref` to
            // a named type is left exactly as the referenced type defined it.
            if let ::utoipa::openapi::RefOr::T(::utoipa::openapi::schema::Schema::Object(
                ref mut __field,
            )) = __schema
            {
                #(#applications)*
            }
            __schema
        });
    }
}

/// Render a numeric literal as a `utoipa::Number`, keeping integers integral.
fn number_literal(lit: &Lit) -> TokenStream {
    match lit {
        Lit::Int(value) => {
            let raw = value.base10_digits();
            let parsed: i64 = raw.parse().unwrap_or_default();
            quote!(::utoipa::Number::Int(#parsed as isize))
        }
        Lit::Float(value) => {
            let raw = value.base10_digits();
            let parsed: f64 = raw.parse().unwrap_or_default();
            quote!(::utoipa::Number::Float(#parsed))
        }
        other => {
            let tokens = other.to_token_stream();
            quote!(::utoipa::Number::Float(#tokens as f64))
        }
    }
}

fn is_option(ty: &Type) -> bool {
    inner_option_type(ty).is_some()
}

/// `Option<T>` -> `T`.
fn inner_option_type(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != format_ident!("Option") {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

/// The type's doc comment, joined, so a description need not be written twice.
fn doc_comment(attrs: &[syn::Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(s), ..
                }) => Some(s.value().trim().to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();

    (!lines.is_empty()).then(|| lines.join(" ").trim().to_string())
}
