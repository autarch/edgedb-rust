extern crate proc_macro;

use proc_macro::TokenStream;
use syn::{self, parse_macro_input};

mod attrib;
mod json;
mod shape;


/// Derive macro to allow structs and enums be queried from the database
///
/// This derive can be used on structures with named fields (which correspond
/// to "shapes" in EdgeDB).
///
/// ```rust
/// #[derive(edgedb_client::Queryable)]
/// struct User {
///     first_name: String,
///     age: i32,
/// }
/// ```
///
/// # Field attributes
///
/// ## JSON
///
/// The `#[edgedb(json)]` decodes a field using `serde_json` instead of EdgeDB
/// binary protocol. Useful if some data is stored in the database as JSON, but
/// you need to process it.  The underlying type must implement
/// `serde::Deserialize`.
///
/// ```rust
/// # use std::collections::HashMap;
///
/// #[derive(edgedb_client::Queryable)]
/// struct User {
///     #[edgedb(json)]
///     user_notes: HashMap<String, String>,
/// }
/// ```
///
/// # Container attributes
///
/// ## JSON
///
/// The `#[edgedb(json)]` can be used to unpack the structure from the JSON.
/// The underlying type must implement `serde::Deserialize`
///
/// ```rust
/// #[derive(edgedb_client::Queryable, serde::Deserialize)]
/// #[edgedb(json)]
/// struct JsonData {
///     field1: String,
///     field2: u32,
/// }
/// ```
#[proc_macro_derive(Queryable, attributes(edgedb))]
pub fn edgedb_queryable(input: TokenStream) -> TokenStream {
    let s = parse_macro_input!(input as syn::Item);
    match derive(&s) {
        Ok(stream) => stream.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn derive(item: &syn::Item) -> syn::Result<proc_macro2::TokenStream> {
    let attrs = match item {
        syn::Item::Struct(s) => &s.attrs,
        syn::Item::Enum(e) => &e.attrs,
        _ => {
            return Err(syn::Error::new_spanned(item,
                "can only derive Queryable for structs and enums"
            ));
        }
    };
    let attrs = attrib::ContainerAttrs::from_syn(&attrs)?;
    if attrs.json {
        json::derive(item)
    } else {
        match item {
            syn::Item::Struct(s) => shape::derive_struct(s),
            _ => {
                return Err(syn::Error::new_spanned(item,
                    "can only derive Queryable for a struct in non-JSON mode"
                ));
            }
        }
    }
}
