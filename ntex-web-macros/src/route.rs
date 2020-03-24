use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{quote, ToTokens, TokenStreamExt};
use syn::{AttributeArgs, Ident, NestedMeta};

#[derive(PartialEq)]
pub enum GuardType {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Connect,
    Options,
    Trace,
    Patch,
}

impl GuardType {
    fn as_str(&self) -> &'static str {
        match self {
            GuardType::Get => "Get",
            GuardType::Post => "Post",
            GuardType::Put => "Put",
            GuardType::Delete => "Delete",
            GuardType::Head => "Head",
            GuardType::Connect => "Connect",
            GuardType::Options => "Options",
            GuardType::Trace => "Trace",
            GuardType::Patch => "Patch",
        }
    }
}

impl ToTokens for GuardType {
    fn to_tokens(&self, stream: &mut TokenStream2) {
        let ident = self.as_str();
        let ident = Ident::new(ident, Span::call_site());
        stream.append(ident);
    }
}

struct Args {
    path: syn::LitStr,
    guards: Vec<Ident>,
}

impl Args {
    fn new(args: AttributeArgs) -> syn::Result<Self> {
        let mut path = None;
        let mut guards = Vec::new();
        for arg in args {
            match arg {
                NestedMeta::Lit(syn::Lit::Str(lit)) => match path {
                    None => {
                        path = Some(lit);
                    }
                    _ => {
                        return Err(syn::Error::new_spanned(
                            lit,
                            "Multiple paths specified! Should be only one!",
                        ));
                    }
                },
                NestedMeta::Meta(syn::Meta::NameValue(nv)) => {
                    if nv.path.is_ident("guard") {
                        if let syn::Lit::Str(lit) = nv.lit {
                            guards.push(Ident::new(&lit.value(), Span::call_site()));
                        } else {
                            return Err(syn::Error::new_spanned(
                                nv.lit,
                                "Attribute guard expects literal string!",
                            ));
                        }
                    } else {
                        return Err(syn::Error::new_spanned(
                            nv.path,
                            "Unknown attribute key is specified. Allowed: guard",
                        ));
                    }
                }
                arg => {
                    return Err(syn::Error::new_spanned(arg, "Unknown attribute"));
                }
            }
        }
        Ok(Args {
            path: path.unwrap(),
            guards,
        })
    }
}

pub struct Route {
    name: syn::Ident,
    args: Args,
    ast: syn::ItemFn,
    guard: GuardType,
}

impl Route {
    pub fn new(
        args: AttributeArgs,
        input: TokenStream,
        guard: GuardType,
    ) -> syn::Result<Self> {
        if args.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                format!(
                    r#"invalid server definition, expected #[{}("<some path>")]"#,
                    guard.as_str().to_ascii_lowercase()
                ),
            ));
        }
        let ast: syn::ItemFn = syn::parse(input)?;
        let name = ast.sig.ident.clone();
        let args = Args::new(args)?;

        Ok(Self {
            name,
            args,
            ast,
            guard,
        })
    }

    pub fn generate(&self) -> TokenStream {
        let name = &self.name;
        let resource_name = name.to_string();
        let guard = &self.guard;
        let ast = &self.ast;
        let path = &self.args.path;
        let extra_guards = &self.args.guards;

        let args: Vec<_> = ast
            .sig
            .inputs
            .iter()
            .filter(|item| match item {
                syn::FnArg::Receiver(_) => false,
                syn::FnArg::Typed(_) => true,
            })
            .map(|item| match item {
                syn::FnArg::Receiver(_) => panic!(),
                syn::FnArg::Typed(ref pt) => pt.ty.clone(),
            })
            .collect();

        let assert_responder: syn::Path = syn::parse_str(
            format!(
                "ntex::web::dev::__assert_handler{}",
                if args.is_empty() {
                    "".to_string()
                } else {
                    format!("{}", args.len())
                }
            )
            .as_str(),
        )
        .unwrap();

        let stream = quote! {
            // #[allow(non_camel_case_types)]
            pub struct #name;

            impl<__E: 'static> ntex::web::dev::HttpServiceFactory<__E> for #name
            where __E: ntex::web::error::ErrorRenderer
            {
                fn register(self, __config: &mut ntex::web::dev::AppService<__E>) {
                    #(ntex::web::dev::__assert_extractor::<__E, #args>();)*

                    #ast

                    let __resource = ntex::web::Resource::new(#path)
                        .name(#resource_name)
                        .guard(ntex::web::guard::#guard())
                        #(.guard(ntex::web::guard::fn_guard(#extra_guards)))*
                        .to(#assert_responder(#name));

                    ntex::web::dev::HttpServiceFactory::register(__resource, __config)
                }
            }
        };
        stream.into()
    }
}
