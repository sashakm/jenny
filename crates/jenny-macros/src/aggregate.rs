//! `#[derive(Aggregate)]` and `#[derive(DomainEvent)]` — the two one-method traits a
//! domain module implements for every aggregate and event.

use darling::util::Flag;
use darling::{FromDeriveInput, FromField};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse_macro_input};

#[derive(FromField)]
#[darling(attributes(aggregate))]
struct AggregateField {
    ident: Option<syn::Ident>,
    ty: syn::Type,
    /// This field is the identity.
    id: Flag,
}

#[derive(FromDeriveInput)]
#[darling(attributes(aggregate), supports(struct_named))]
struct AggregateInput {
    ident: syn::Ident,
    generics: syn::Generics,
    data: darling::ast::Data<(), AggregateField>,
    /// `Aggregate::KIND`. Defaults to the type name in snake case.
    #[darling(default)]
    kind: Option<String>,
    /// Path to the jenny facade.
    #[darling(default)]
    krate: Option<syn::Path>,
}

pub(crate) fn derive_aggregate_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match AggregateInput::from_derive_input(&input).and_then(|parsed| expand_aggregate(&parsed)) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.write_errors().into(),
    }
}

fn expand_aggregate(input: &AggregateInput) -> darling::Result<TokenStream> {
    let fields = input
        .data
        .as_ref()
        .take_struct()
        .expect("darling enforces struct_named")
        .fields;

    let marked: Vec<&AggregateField> = fields
        .iter()
        .copied()
        .filter(|f| f.id.is_present())
        .collect();
    let id = match marked.as_slice() {
        [one] => *one,
        [] => fields
            .iter()
            .copied()
            .find(|f| f.ident.as_ref().is_some_and(|ident| ident == "id"))
            .ok_or_else(|| {
                darling::Error::custom(
                    "no identity: name a field `id`, or mark one with #[aggregate(id)]",
                )
                .with_span(&input.ident)
            })?,
        [_, second, ..] => {
            return Err(darling::Error::custom("only one field can be the identity")
                .with_span(&second.ident));
        }
    };

    let ident = &input.ident;
    let id_field = id.ident.as_ref().expect("named fields have names");
    let id_type = &id.ty;
    let kind = input
        .kind
        .clone()
        .unwrap_or_else(|| stringcase::snake_case(&ident.to_string()).to_lowercase());
    let krate = krate(&input.krate);
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics #krate::domain::Aggregate for #ident #type_generics #where_clause {
            type Id = #id_type;
            const KIND: &'static str = #kind;

            fn id(&self) -> Self::Id {
                ::core::clone::Clone::clone(&self.#id_field)
            }
        }
    })
}

#[derive(FromDeriveInput)]
#[darling(attributes(event), supports(struct_any, enum_any))]
struct EventInput {
    ident: syn::Ident,
    generics: syn::Generics,
    /// `DomainEvent::name`. Defaults to the type name's words, dotted: `NoteCreated`
    /// becomes `note.created`.
    #[darling(default)]
    name: Option<String>,
    #[darling(default)]
    krate: Option<syn::Path>,
}

pub(crate) fn derive_domain_event_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match EventInput::from_derive_input(&input) {
        Ok(parsed) => expand_event(&parsed).into(),
        Err(e) => e.write_errors().into(),
    }
}

fn expand_event(input: &EventInput) -> TokenStream {
    let ident = &input.ident;
    let name = input
        .name
        .clone()
        .unwrap_or_else(|| event_name(&ident.to_string()));
    let krate = krate(&input.krate);
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    quote! {
        impl #impl_generics #krate::domain::DomainEvent for #ident #type_generics #where_clause {
            fn name(&self) -> &str {
                #name
            }
        }
    }
}

/// `NoteCreated` → `note.created`.
fn event_name(type_name: &str) -> String {
    stringcase::snake_case(type_name)
        .to_lowercase()
        .replace('_', ".")
}

fn krate(path: &Option<syn::Path>) -> syn::Path {
    path.clone().unwrap_or_else(|| syn::parse_quote!(::jenny))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_the_dotted_words_of_the_type() {
        assert_eq!(event_name("NoteCreated"), "note.created");
        assert_eq!(event_name("OrderLineAdded"), "order.line.added");
    }
}
