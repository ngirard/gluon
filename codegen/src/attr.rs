use proc_macro2::{Group, Span, TokenStream, TokenTree};

use syn::{
    self,
    parse::{self, Parse},
    Meta::*,
    Path,
};

fn get_gluon_meta_items(attr: &syn::Attribute) -> Option<Vec<syn::NestedMeta>> {
    if attr.path.segments.len() == 1
        && (attr.path.segments[0].ident == "gluon"
            || attr.path.segments[0].ident == "gluon_trace"
            || attr.path.segments[0].ident == "gluon_userdata")
    {
        match attr.parse_meta() {
            Ok(List(ref meta)) => Some(meta.nested.iter().cloned().collect()),
            _ => None,
        }
    } else {
        None
    }
}

pub enum CrateName {
    Some(syn::Path),
    GluonVm,
    None,
}

pub struct Container {
    pub crate_name: CrateName,
    pub vm_type: Option<String>,
    pub newtype: bool,
    pub skip: bool,
    pub clone: bool,
}

impl Container {
    pub fn from_ast(item: &syn::DeriveInput) -> Container {
        use syn::NestedMeta::*;

        let mut crate_name = CrateName::None;
        let mut vm_type = None;
        let mut newtype = false;
        let mut skip = false;
        let mut clone = false;

        for meta_items in item.attrs.iter().filter_map(get_gluon_meta_items) {
            for meta_item in meta_items {
                match meta_item {
                    // Parse `#[gluon(crate_name = "foo")]`
                    Meta(NameValue(ref m)) if m.path.is_ident("crate_name") => {
                        if let Ok(path) = parse_lit_into_path(&m.path, &m.lit) {
                            crate_name = CrateName::Some(path);
                        }
                    }

                    // Parse `#[gluon(gluon_vm)]`
                    Meta(Path(ref w)) if w.is_ident("gluon_vm") => {
                        crate_name = CrateName::GluonVm;
                    }

                    Meta(Path(ref w)) if w.is_ident("newtype") => {
                        newtype = true;
                    }

                    Meta(NameValue(ref m)) if m.path.is_ident("vm_type") => {
                        vm_type = Some(get_lit_str(&m.path, &m.path, &m.lit).unwrap().value())
                    }

                    Meta(Path(ref w)) if w.is_ident("skip") => {
                        skip = true;
                    }

                    Meta(Path(ref w)) if w.is_ident("clone") => {
                        clone = true;
                    }

                    _ => panic!("unexpected gluon container attribute: {:?}", meta_item),
                }
            }
        }

        Container {
            crate_name,
            vm_type,
            newtype,
            skip,
            clone,
        }
    }
}

fn get_lit_str<'a>(
    attr_name: &Path,
    _meta_item_name: &Path,
    lit: &'a syn::Lit,
) -> Result<&'a syn::LitStr, ()> {
    if let syn::Lit::Str(ref lit) = *lit {
        Ok(lit)
    } else {
        panic!("Expected attribute `{:?}` to be a string", attr_name)
    }
}

fn parse_lit_into_path(attr_name: &Path, lit: &syn::Lit) -> Result<syn::Path, ()> {
    let string = get_lit_str(attr_name, attr_name, lit)?;
    parse_lit_str(string).map_err(|_| panic!("failed to parse path: {:?}", string.value()))
}

fn parse_lit_str<T>(s: &syn::LitStr) -> Result<T, parse::Error>
where
    T: Parse,
{
    let tokens = spanned_tokens(s)?;
    syn::parse2(tokens)
}

fn spanned_tokens(s: &syn::LitStr) -> Result<TokenStream, parse::Error> {
    let stream = syn::parse_str(&s.value())?;
    Ok(respan_token_stream(stream, s.span()))
}

fn respan_token_stream(stream: TokenStream, span: Span) -> TokenStream {
    stream
        .into_iter()
        .map(|token| respan_token_tree(token, span))
        .collect()
}

fn respan_token_tree(mut token: TokenTree, span: Span) -> TokenTree {
    if let TokenTree::Group(ref mut g) = token {
        *g = Group::new(g.delimiter(), respan_token_stream(g.stream().clone(), span));
    }
    token.set_span(span);
    token
}
