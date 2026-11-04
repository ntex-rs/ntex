use proc_macro::TokenStream;
use proc_macro2::{Ident, Literal, Span, TokenStream as TokenStream2, TokenTree};
use quote::{ToTokens, TokenStreamExt, quote};

use crate::parse::{Error, ItemFn, parse_args, str_value};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MethodType {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Connect,
    Options,
    Trace,
    Patch,
    Query,
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
            MethodType::Query => "Query",
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
    path: Literal,
    guards: Vec<Ident>,
    state: TokenStream2,
}

impl Args {
    fn parse(input: TokenStream2) -> Result<Self, Error> {
        let mut iter = input.into_iter();
        let path = match iter.next() {
            Some(TokenTree::Literal(lit)) if str_value(&lit).is_some() => lit,
            Some(tt) => return Err(Error::new_spanned(&tt, "expected string literal")),
            None => return Err(Error::new(Span::call_site(), "expected string literal")),
        };
        match iter.next() {
            Some(TokenTree::Punct(p)) if p.as_char() == ',' => {}
            Some(tt) => return Err(Error::new_spanned(&tt, "expected `,`")),
            None => {}
        }

        let mut guards = Vec::new();
        let mut state = None;
        for arg in parse_args(iter.collect())? {
            if arg.name == "guard" {
                let lit = arg
                    .lit_str()
                    .and_then(|lit| str_value(&lit))
                    .ok_or_else(|| arg.value_error("expected string literal"))?;
                guards.push(Ident::new(&lit, Span::call_site()));
            } else if arg.name == "state" {
                state = Some(
                    arg.path()
                        .ok_or_else(|| arg.value_error("expected identifier"))?,
                );
            } else {
                return Err(Error::new_spanned(
                    &arg.name,
                    "unknown argument, expected `guard` or `state`",
                ));
            }
        }

        Ok(Args {
            path,
            guards,
            state: state.unwrap_or_else(|| quote!(ntex::web::dev::DefaultState)),
        })
    }
}

fn missing_path(method: MethodType) -> Error {
    Error::new(
        Span::call_site(),
        format!(
            r#"missing path, expected #[{}("<path>")]"#,
            method.as_str().to_ascii_lowercase()
        ),
    )
}

/// Maximum number of web handler arguments
const MAX_HANDLER_ARGS: usize = 16;

pub(crate) struct Route {
    name: Ident,
    args: Args,
    ast: TokenStream2,
    method: MethodType,
    params: Option<usize>,
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Route({:?})", self.name)
    }
}

impl Route {
    pub(crate) fn new(
        args: TokenStream,
        input: TokenStream,
        method: MethodType,
    ) -> Result<Self, Error> {
        if args.is_empty() {
            return Err(missing_path(method));
        }
        let ast = TokenStream2::from(input);
        let item = ItemFn::parse(ast.clone())?;
        let params = item.param_count();
        if params.is_some_and(|count| count > MAX_HANDLER_ARGS) {
            return Err(item.fn_error(format!(
                "web handlers accept at most {MAX_HANDLER_ARGS} arguments"
            )));
        }
        let name = item.ident().clone();
        let args = Args::parse(args.into())?;

        Ok(Self {
            name,
            args,
            ast,
            method,
            params,
        })
    }

    pub(crate) fn generate(&self) -> TokenStream {
        let name = &self.name;
        let resource_name = name.to_string();
        let ast = &self.ast;
        let path = &self.args.path;
        let extra_guards = &self.args.guards;
        let state = &self.args.state;
        let method = &self.method;
        let (check, handler) = if let Some(params) = self.params {
            // Check each argument and the return type separately so errors
            // name the failing bound instead of `Handler`.
            let args: Vec<_> = (0..params)
                .map(|i| Ident::new(&format!("__A{i}"), Span::call_site()))
                .collect();
            let check = quote! {
                fn __check_handler<__Fn, __Res, #(#args),*>(
                    f: __Fn,
                ) -> impl ntex::web::Handler<#state, (#(#args,)*), Output = __Res>
                where
                    __Fn: ::core::ops::AsyncFn(#(#args),*) -> __Res + 'static,
                    __Res: ntex::web::Responder<#state> + 'static,
                    #(#args: ntex::web::FromRequest<#state>,)*
                {
                    f
                }
            };
            (check, quote!(__check_handler(#name)))
        } else {
            // Parameter conditions expand after this macro; let `Handler`
            // infer the remaining arguments.
            (TokenStream2::new(), quote!(#name))
        };

        let stream = quote! {
            #[allow(non_camel_case_types)]
            pub struct #name;

            impl<St: 'static> ntex::web::dev::WebServiceFactory<#state, St> for #name {
                fn register(self, __config: &mut ntex::web::dev::WebServiceConfig<#state, St>) {
                    #ast
                    #check

                    let __resource = ntex::web::Resource::<_, St>::new(#path)
                        .name(#resource_name)
                        #(.guard(ntex::web::guard::fn_guard(#extra_guards)))*
                        .guard(ntex::web::guard::#method())
                        .to(#handler);

                    ntex::web::dev::WebServiceFactory::<_, St>::register(__resource, __config)
                }
            }
        };
        stream.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &str) -> Result<Args, Error> {
        Args::parse(args.parse().unwrap())
    }

    fn err(args: &str) -> String {
        parse(args).err().unwrap().to_string()
    }

    /// STATEMENT: Route diagnostics identify the handler without dumping route arguments or its body.
    ///
    /// HOW:
    /// - Construct a route with a named handler, guarded path and body.
    /// - Check its exact handler-only representation.
    #[test]
    fn debug_route() {
        let route = Route {
            name: Ident::new("handler", Span::call_site()),
            args: parse(r#""/private", guard = "check""#).ok().unwrap(),
            ast: quote!(
                async fn handler() {
                    unreachable!()
                }
            ),
            method: MethodType::Get,
            params: Some(0),
        };
        assert_eq!(format!("{route:?}"), format!("Route({:?})", route.name));
    }

    #[test]
    fn args() {
        let args = parse(r#""/a", guard = "g1", guard = "g2", state = St"#)
            .ok()
            .unwrap();
        assert_eq!(str_value(&args.path).unwrap(), "/a");
        assert_eq!(args.guards.len(), 2);
        assert_eq!(args.state.to_string(), "St");

        let args = parse(r#""/a","#).ok().unwrap();
        assert!(args.guards.is_empty());
        assert_eq!(args.state.to_string(), "ntex :: web :: dev :: DefaultState");

        assert_eq!(err("a"), "expected string literal");
        assert_eq!(err(r#""/a" b"#), "expected `,`");
        assert_eq!(err(r#""/a", guard = g"#), "expected string literal");
        assert_eq!(err(r#""/a", state = "St""#), "expected identifier");

        assert_eq!(
            err(r#""/a", error = "Foo""#),
            "unknown argument, expected `guard` or `state`"
        );
        assert_eq!(
            missing_path(MethodType::Get).to_string(),
            r#"missing path, expected #[get("<path>")]"#
        );
    }
}
