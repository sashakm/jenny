//! `#[use_cases]` — one span and one measurement per use case, without writing either.

use darling::FromMeta;
use darling::ast::NestedMeta;
use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{FnArg, ImplItem, ImplItemFn, ItemImpl, Pat, ReturnType, Type, Visibility};

/// `#[use_cases(...)]` on the impl block.
#[derive(Default, FromMeta)]
#[darling(default)]
struct Container {
    /// Prefix of every span name. Defaults to the type's name in snake case.
    name: Option<String>,
    /// Path to the jenny facade. See `Dto`'s `krate`.
    krate: Option<syn::Path>,
}

/// `#[use_case(...)]` on one method.
#[derive(Default, FromMeta)]
#[darling(default)]
struct Method {
    /// This method's part of the span name. Defaults to the method name.
    name: Option<String>,
    /// Leave this method alone.
    skip: bool,
}

pub(crate) fn use_cases_impl(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    match expand(attr.into(), item.into()) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.write_errors().into(),
    }
}

fn expand(attr: TokenStream, item: TokenStream) -> darling::Result<TokenStream> {
    let container = Container::from_list(&NestedMeta::parse_meta_list(attr)?)?;
    let mut block: ItemImpl = syn::parse2(item)?;

    if let Some((path, _)) = &block.trait_ {
        return Err(darling::Error::custom(
            "#[use_cases] belongs on an inherent impl block; a trait impl's methods are \
             the trait's, not use cases",
        )
        .with_span(path));
    }

    let prefix = match container.name {
        Some(name) => name,
        None => default_prefix(&block.self_ty)?,
    };
    let krate = container
        .krate
        .unwrap_or_else(|| syn::parse_quote!(::jenny));

    let mut errors = darling::Error::accumulator();
    for item in &mut block.items {
        if let ImplItem::Fn(method) = item {
            errors.handle(wrap(method, &prefix, &krate));
        }
    }
    errors.finish()?;

    Ok(quote!(#block))
}

/// `Notes` → `notes`, `NoteService` → `note_service`.
fn default_prefix(self_ty: &Type) -> darling::Result<String> {
    match self_ty {
        Type::Path(path) => match path.path.segments.last() {
            Some(segment) => Ok(stringcase::snake_case(&segment.ident.to_string()).to_lowercase()),
            None => Err(darling::Error::custom("expected a type name").with_span(self_ty)),
        },
        _ => Err(darling::Error::custom(
            "cannot derive a span name from this type; give one with #[use_cases(name = \"...\")]",
        )
        .with_span(self_ty)),
    }
}

fn wrap(method: &mut ImplItemFn, prefix: &str, krate: &syn::Path) -> darling::Result<()> {
    let options = take_options(method)?;
    let explicit = options.is_some();
    let options = options.unwrap_or_default();

    if options.skip {
        return Ok(());
    }
    if method.sig.asyncness.is_none() {
        // Only an explicit `#[use_case]` on a synchronous method is a mistake; anything
        // else synchronous is a helper and stays as written.
        return if explicit {
            Err(darling::Error::custom("a use case is an async fn").with_span(&method.sig))
        } else {
            Ok(())
        };
    }
    if !explicit && !matches!(method.vis, Visibility::Public(_)) {
        return Ok(());
    }

    let ReturnType::Type(_, output) = &method.sig.output else {
        return Err(darling::Error::custom(
            "a use case returns a Result, so its outcome can be reported",
        )
        .with_span(&method.sig));
    };

    let name = format!(
        "{prefix}.{}",
        options.name.unwrap_or_else(|| method.sig.ident.to_string())
    );
    let request_id = match context_argument(method) {
        Some(cx) => quote!(::core::option::Option::Some(
            #krate::port::Context::request_id(#cx)
        )),
        None => quote!(::core::option::Option::None),
    };

    let body = &method.block;
    // The annotated binding pins the async block's output to the method's return type,
    // so `?` and early `return`s inside the body convert exactly as they did before.
    method.block = syn::parse_quote_spanned! {body.span()=>
        {
            #krate::port::use_case::__private::observe(#name, #request_id, async move {
                let __jenny_outcome: #output = #body;
                __jenny_outcome
            })
            .await
        }
    };
    Ok(())
}

/// Remove `#[use_case(...)]` from the method, returning its options if it had one.
fn take_options(method: &mut ImplItemFn) -> darling::Result<Option<Method>> {
    let mut found = None;
    let mut error = None;
    method.attrs.retain(|attr| {
        if !attr.path().is_ident("use_case") {
            return true;
        }
        let parsed = match &attr.meta {
            syn::Meta::Path(_) => Ok(Method::default()),
            meta => Method::from_meta(meta),
        };
        match parsed {
            Ok(options) => found = Some(options),
            Err(e) => error = Some(e),
        }
        false
    });
    match error {
        Some(e) => Err(e),
        None => Ok(found),
    }
}

/// The parameter bound to a `&Context`, so the span can carry its request id.
fn context_argument(method: &ImplItemFn) -> Option<&syn::Ident> {
    method.sig.inputs.iter().find_map(|input| {
        let FnArg::Typed(typed) = input else {
            return None;
        };
        let Type::Reference(reference) = &*typed.ty else {
            return None;
        };
        let Type::Path(path) = &*reference.elem else {
            return None;
        };
        if path.path.segments.last()?.ident != "Context" {
            return None;
        }
        match &*typed.pat {
            Pat::Ident(binding) => Some(&binding.ident),
            _ => None,
        }
    })
}
