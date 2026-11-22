//! `#[derive(Stateful)]`: generates `sync` for a struct (every field in order) or a
//! fieldless enum (stored as its variant number). Mark fields that aren't part of the
//! machine's state (ROM data, host handles, callbacks) with `#[state(skip)]`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Index};

fn skipped(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("state") && {
            let mut skip = false;
            let _ = a.parse_nested_meta(|m| {
                if m.path.is_ident("skip") {
                    skip = true;
                }
                Ok(())
            });
            skip
        }
    })
}

#[proc_macro_derive(Stateful, attributes(state))]
pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_g, ty_g, where_c) = input.generics.split_for_impl();
    let body = match &input.data {
        Data::Struct(s) => {
            let steps = match &s.fields {
                Fields::Named(f) => f.named.iter().filter(|f| !skipped(&f.attrs)).map(|f| {
                    let id = f.ident.as_ref().unwrap();
                    quote! { ::state::Stateful::sync(&mut self.#id, s); }
                }).collect::<Vec<_>>(),
                Fields::Unnamed(f) => f.unnamed.iter().enumerate().filter(|(_, f)| !skipped(&f.attrs)).map(|(i, _)| {
                    let i = Index::from(i);
                    quote! { ::state::Stateful::sync(&mut self.#i, s); }
                }).collect::<Vec<_>>(),
                Fields::Unit => vec![],
            };
            quote! { #(#steps)* }
        }
        Data::Enum(e) => {
            if e.variants.iter().any(|v| !matches!(v.fields, Fields::Unit)) {
                return syn::Error::new_spanned(name, "Stateful enums must have no fields; write `sync` by hand for this one").to_compile_error().into();
            }
            let idents: Vec<_> = e.variants.iter().map(|v| &v.ident).collect();
            let idx: Vec<u8> = (0..idents.len() as u8).collect();
            quote! {
                let mut n: u8 = match self { #( Self::#idents => #idx, )* };
                ::state::Stateful::sync(&mut n, s);
                if s.is_load() {
                    *self = match n { #( #idx => Self::#idents, )* _ => { s.fail(); return; } };
                }
            }
        }
        Data::Union(_) => return syn::Error::new_spanned(name, "unions can't derive Stateful").to_compile_error().into(),
    };
    quote! {
        impl #impl_g ::state::Stateful for #name #ty_g #where_c {
            fn sync(&mut self, s: &mut ::state::State) {
                #body
            }
        }
    }
    .into()
}
