use darling::{FromDeriveInput, FromField, FromMeta, util::Flag};
use proc_macro::TokenStream;
use syn::{DeriveInput, LitStr, PathArguments, Type, TypePath, parse_macro_input};

#[derive(FromMeta)]
struct QueryFormatContainerArgs {
    #[darling(default)]
    rename_all: Option<String>,
    #[darling(default)]
    table: Option<String>,
}

#[derive(FromMeta)]
struct ForeignKeyArg {
    table: String,
    column: String,
    #[darling(default)]
    alias: Option<String>,
}

#[derive(FromField)]
#[darling(attributes(query))]
struct QueryFormatFieldArgs {
    ident: Option<syn::Ident>,
    ty: syn::Type,
    exclude_insert: Flag,
    exclude: Flag,
    #[darling(default)]
    rename: Option<String>,
    #[darling(default)]
    foreign_key: Option<ForeignKeyArg>,
}

impl QueryFormatFieldArgs {
    fn field_type_is_option(&self) -> bool {
        if let Type::Path(TypePath { path, .. }) = &self.ty
            && let Some(seg) = path.segments.last()
            && seg.ident == "Option"
            && matches!(seg.arguments, PathArguments::AngleBracketed(_))
        {
            return true;
        }
        false
    }
}

#[derive(FromDeriveInput)]
#[darling(attributes(query), supports(struct_named))]
struct QueryFormatArgs {
    #[darling(flatten)]
    args: QueryFormatContainerArgs,
}

pub(crate) fn derive_entity_impl(input: TokenStream) -> TokenStream {
    let parsed = parse_macro_input!(input as DeriveInput);
    let mut errors = darling::Error::accumulator();

    let (impl_generics, type_generics, where_clause) = parsed.generics.split_for_impl();
    let ident = parsed.ident.clone();

    let struct_args = match QueryFormatArgs::from_derive_input(&parsed) {
        Ok(args) => args,
        Err(e) => {
            errors.push(e);
            return errors.finish().unwrap_err().write_errors().into();
        }
    };

    let mut foreign_keys = Vec::new();
    let mut insert_columns = Vec::new();
    let mut insert_binds = Vec::new();

    let columns = if let syn::Data::Struct(data_struct) = parsed.data {
        data_struct
            .fields
            .iter()
            .filter_map(|field| match QueryFormatFieldArgs::from_field(field) {
                Ok(fargs) => {
                    if fargs.exclude.is_present() {
                        return None;
                    }
                    if let Some(ident) = &fargs.ident {
                        let mut field_name = ident.to_string();
                        if let Some(rename) = &fargs.rename {
                            field_name = rename.to_string();
                        }
                        if let Some(ra) = &struct_args.args.rename_all {
                            match crate::utils::apply_casing(ra, &field_name) {
                                Ok(f) => field_name = f,
                                Err(e) => errors.push(e),
                            };
                        };
                        if let Some(fk) = &fargs.foreign_key {
                            let on = if let Some(table) = &struct_args.args.table {
                                format!("{table}.{field_name}")
                            } else {
                                field_name.clone()
                            };
                            let join_type = if fargs.field_type_is_option() {
                                "LEFT JOIN"
                            } else {
                                "JOIN"
                            };
                            let join_statement = if let Some(alias) = &fk.alias {
                                format!(
                                    "{join_type} {} {} ON {} = {}.{}",
                                    fk.table, alias, on, alias, fk.column
                                )
                            } else {
                                format!(
                                    "{join_type} {} ON {} = {}.{}",
                                    fk.table, on, fk.table, fk.column
                                )
                            };

                            foreign_keys.push(join_statement);
                        }
                        if !fargs.exclude_insert.is_present() {
                            insert_columns.push(field_name.clone());
                            insert_binds.push(format!("${}", insert_columns.len()));
                        }
                        Some(field_name)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    errors.push(e);
                    None
                }
            })
            .collect::<Vec<_>>()
    } else {
        errors.push(darling::Error::custom(
            "QueryFormat currently only supports named structs",
        ));
        return errors.finish().unwrap_err().write_errors().into();
    };

    if let Err(e) = errors.finish() {
        e.write_errors().into()
    } else {
        let table = struct_args.args.table.unwrap_or_default();
        let insert_statement = LitStr::new(
            &format!(
                "INSERT INTO {} ({}) VALUES ({}) RETURNING *",
                table,
                insert_columns.join(", "),
                insert_binds.join(", ")
            ),
            proc_macro2::Span::call_site(),
        );

        let table_expression = LitStr::new(
            &format!("FROM {} {}", table, foreign_keys.join(" ")),
            proc_macro2::Span::call_site(),
        );

        quote::quote! {
            #[automatically_derived]
            impl #impl_generics QueryFormat for #ident #type_generics #where_clause {
                const ENTITY_FIELDS: &'static [&'static str] = &[#(#columns),*];

                fn table_expression() -> &'static str {
                    #table_expression
                }

                #[doc = #insert_statement]
                fn insert_query() -> &'static str {
                    #insert_statement
                }
            }
        }
        .into()
    }
}
