use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, TokenStreamExt, quote};
use syn::{AttributeArgs, Ident, NestedMeta, Path};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MethodType {
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

impl MethodType {
    fn as_str(&self) -> &'static str {
        match self {
            MethodType::Get => "Get",
            MethodType::Post => "Post",
            MethodType::Put => "Put",
            MethodType::Delete => "Delete",
            MethodType::Head => "Head",
            MethodType::Connect => "Connect",
            MethodType::Options => "Options",
            MethodType::Trace => "Trace",
            MethodType::Patch => "Patch",
        }
    }
}

impl ToTokens for MethodType {
    fn to_tokens(&self, stream: &mut TokenStream2) {
        let ident = self.as_str();
        let ident = Ident::new(ident, Span::call_site());
        stream.append(ident);
    }
}

struct Args {
    path: syn::LitStr,
    guards: Vec<Ident>,
    error: Path,
}

impl Args {
    fn new(args: AttributeArgs) -> syn::Result<Self> {
        let mut path = None;
        let mut guards = Vec::new();
        let mut error: Option<Path> = None;
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
                    } else if nv.path.is_ident("error") {
                        if let syn::Lit::Str(lit) = nv.lit {
                            error = Some(syn::parse_str(&lit.value())?);
                        } else {
                            return Err(syn::Error::new_spanned(
                                nv.lit,
                                "Attribute error expects type path!",
                            ));
                        }
                    } else {
                        return Err(syn::Error::new_spanned(
                            nv.path,
                            "Unknown attribute key is specified. Allowed: guard or error",
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
            error: error
                .unwrap_or_else(|| syn::parse_str("ntex::web::DefaultError").unwrap()),
        })
    }
}

pub struct Route {
    name: syn::Ident,
    args: Args,
    ast: syn::ItemFn,
    method: MethodType,
}

impl Route {
    pub fn new(
        args: AttributeArgs,
        input: TokenStream,
        method: MethodType,
    ) -> syn::Result<Self> {
        if args.is_empty() {
            return Err(syn::Error::new(
                Span::call_site(),
                format!(
                    r#"invalid server definition, expected #[{}("<some path>")]"#,
                    method.as_str().to_ascii_lowercase()
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
            method,
        })
    }

    pub fn generate(&self) -> TokenStream {
        let name = &self.name;
        let resource_name = name.to_string();
        let ast = &self.ast;
        let path = &self.args.path;
        let extra_guards = &self.args.guards;
        let error = &self.args.error;
        let method = &self.method;

        let stream = quote! {
            #[allow(non_camel_case_types)]
            pub struct #name;

            impl ntex::web::dev::WebServiceFactory<#error> for #name
            {
                fn register(self, __config: &mut ntex::web::dev::WebServiceConfig<#error>) {
                    #ast

                    let __resource = ntex::web::Resource::new(#path)
                        .name(#resource_name)
                        .guard(ntex::web::guard::#method())
                        #(.guard(ntex::web::guard::fn_guard(#extra_guards)))*
                        .to(#name);

                    ntex::web::dev::WebServiceFactory::register(__resource, __config)
                }
            }
        };
        stream.into()
    }
}
