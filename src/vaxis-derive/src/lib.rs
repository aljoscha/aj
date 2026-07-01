//! Proc-macro support for `vaxis`.
//!
//! Hosts `#[derive(TableRow)]`, the replacement for Zig's comptime struct
//! reflection in the `Table` widget: it derives the column headers from the
//! struct's field names and the per-cell formatting from the field types.

use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, Type, parse_macro_input};

/// Derives [`vaxis::widgets::table::TableRow`] for a struct with named fields.
///
/// The macro maps each non-skipped field to a table column, in declaration
/// order:
///
/// - `headers()` yields the field name, or the `#[table(rename = "...")]`
///   override.
/// - `column_count()` is the number of non-skipped fields.
/// - `cell(col)` dispatches by column index to
///   `TableCell::to_cell(&self.field)`. Fields whose type is syntactically
///   `Option<_>` are unwrapped in the generated code: `Some` formats the inner
///   value, `None` renders `-`. This mirrors upstream's per-field `.optional`
///   inspection and sidesteps the coherence conflict a blanket `TableCell for
///   Option<T>` would have with the `Display` blanket impl.
///
/// Field attributes:
///
/// - `#[table(skip)]` drops the field from the table.
/// - `#[table(rename = "Header")]` overrides the header text.
#[proc_macro_derive(TableRow, attributes(table))]
pub fn derive_table_row(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Per-column data gathered from one struct field.
struct Column {
    header: String,
    ident: syn::Ident,
    is_option: bool,
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new(
                    input.span(),
                    "TableRow can only be derived for structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "TableRow can only be derived for structs",
            ));
        }
    };

    let mut columns = Vec::new();
    for field in fields {
        let (skip, rename) = parse_field_attrs(field)?;
        if skip {
            continue;
        }
        // Named-field structs always have an ident, so this never fires.
        let ident = field
            .ident
            .clone()
            .ok_or_else(|| syn::Error::new(field.span(), "expected a named field"))?;
        let header = rename.unwrap_or_else(|| ident.to_string());
        columns.push(Column {
            header,
            is_option: is_option_type(&field.ty),
            ident,
        });
    }

    let count = columns.len();
    let headers = columns
        .iter()
        .map(|c| {
            let h = &c.header;
            quote! { ::std::borrow::Cow::Borrowed(#h) }
        })
        .collect::<Vec<_>>();

    let cell_arms = columns.iter().enumerate().map(|(idx, c)| {
        let idx = proc_macro2::Literal::usize_unsuffixed(idx);
        let ident = &c.ident;
        if c.is_option {
            quote! {
                #idx => match &self.#ident {
                    ::std::option::Option::Some(value) =>
                        ::vaxis::widgets::table::TableCell::to_cell(value),
                    ::std::option::Option::None => ::std::borrow::Cow::Borrowed("-"),
                },
            }
        } else {
            quote! {
                #idx => ::vaxis::widgets::table::TableCell::to_cell(&self.#ident),
            }
        }
    });

    Ok(quote! {
        impl #impl_generics ::vaxis::widgets::table::TableRow for #name #ty_generics #where_clause {
            fn headers() -> ::std::vec::Vec<::std::borrow::Cow<'static, str>> {
                ::std::vec![ #(#headers),* ]
            }

            fn column_count() -> usize {
                #count
            }

            fn cell(&self, col: usize) -> ::std::borrow::Cow<'_, str> {
                match col {
                    #(#cell_arms)*
                    // Out-of-range columns render empty rather than panic.
                    _ => ::std::borrow::Cow::Borrowed(""),
                }
            }
        }
    })
}

/// Parses the `#[table(...)]` attributes on a field, returning whether the
/// field is skipped and its optional header rename.
fn parse_field_attrs(field: &syn::Field) -> syn::Result<(bool, Option<String>)> {
    let mut skip = false;
    let mut rename = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("table") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                skip = true;
                return Ok(());
            }
            if meta.path.is_ident("rename") {
                let value: syn::LitStr = meta.value()?.parse()?;
                rename = Some(value.value());
                return Ok(());
            }
            Err(meta.error("unsupported table attribute, expected `skip` or `rename`"))
        })?;
    }
    Ok((skip, rename))
}

/// True when `ty` is written as `Option<_>` (any path ending in `Option` with
/// one generic argument). Type aliases are not seen through, matching the
/// limits of syntactic inspection.
fn is_option_type(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    if segment.ident != "Option" {
        return false;
    }
    matches!(&segment.arguments, syn::PathArguments::AngleBracketed(args) if args.args.len() == 1)
}
