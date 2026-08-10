use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, Ident, Lit, LitBool, LitInt, LitStr, MetaNameValue, Token};

pub(crate) struct MainArgs {
    name: Option<LitStr>,
    signals: Option<LitBool>,
    ping_interval: Option<LitInt>,
    panic_handling: Option<LitBool>,
    rt: Option<syn::Path>,
}

impl MainArgs {
    pub(crate) fn gen_sys_config(self, name: &Ident) -> TokenStream {
        let sys_name = self
            .name
            .map(|n| quote!(.name(#n)))
            .unwrap_or_else(|| quote!(.name(stringify!(#name))));

        let sys_ping_interval = self
            .ping_interval
            .map(|interval| quote!(.ping_interval(#interval)))
            .unwrap_or_default();

        let sys_signals = self
            .signals
            .map(|signals| quote!(.signals(#signals)))
            .unwrap_or_default();

        let sys_panics = self
            .panic_handling
            .map(|panics| quote!(.panic_handling(#panics)))
            .unwrap_or_default();

        quote! {
            #sys_name
            #sys_ping_interval
            #sys_signals
            #sys_panics
        }
    }

    pub(crate) fn gen_sys_rt(&mut self) -> TokenStream {
        self.rt
            .take()
            .map(|runner| quote!(#runner))
            .unwrap_or_else(|| quote!(ntex::rt::DefaultRuntime))
    }
}

impl Parse for MainArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = MainArgs {
            rt: None,
            name: None,
            signals: None,
            panic_handling: None,
            ping_interval: None,
        };
        let params = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;

        for param in params {
            if param.path.is_ident("name") {
                if args.name.is_some() {
                    return Err(syn::Error::new_spanned(
                        param.path,
                        "duplicate `name` argument",
                    ));
                }

                match param.value {
                    Expr::Lit(ExprLit {
                        lit: Lit::Str(lit), ..
                    }) => {
                        args.name = Some(lit);
                    }
                    value => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "`name` value must be an string literal",
                        ));
                    }
                }
            } else if param.path.is_ident("signals") {
                if args.signals.is_some() {
                    return Err(syn::Error::new_spanned(
                        param.path,
                        "duplicate `signals` argument",
                    ));
                }

                match param.value {
                    Expr::Lit(ExprLit {
                        lit: Lit::Bool(lit),
                        ..
                    }) => {
                        args.signals = Some(lit);
                    }
                    value => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "`signals` value must be an bool literal",
                        ));
                    }
                }
            } else if param.path.is_ident("panic_handling") {
                if args.panic_handling.is_some() {
                    return Err(syn::Error::new_spanned(
                        param.path,
                        "duplicate `panic_handling` argument",
                    ));
                }

                match param.value {
                    Expr::Lit(ExprLit {
                        lit: Lit::Bool(lit),
                        ..
                    }) => {
                        args.panic_handling = Some(lit);
                    }
                    value => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "`panic_handling` value must be an bool literal",
                        ));
                    }
                }
            } else if param.path.is_ident("ping_interval") {
                if args.ping_interval.is_some() {
                    return Err(syn::Error::new_spanned(
                        param.path,
                        "duplicate `ping_interval` argument",
                    ));
                }

                match param.value {
                    Expr::Lit(ExprLit {
                        lit: Lit::Int(lit), ..
                    }) => {
                        args.ping_interval = Some(lit);
                    }
                    value => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "`ping_interval` value must be an integer literal",
                        ));
                    }
                }
            } else if param.path.is_ident("rt") {
                if args.rt.is_some() {
                    return Err(syn::Error::new_spanned(
                        param.path,
                        "duplicate `rt` argument",
                    ));
                }

                match param.value {
                    Expr::Path(syn::ExprPath { path, .. }) => {
                        args.rt = Some(path);
                    }
                    value => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "`rt` value must be a type",
                        ));
                    }
                }
            } else {
                return Err(syn::Error::new_spanned(
                    param.path,
                    "unknown argument, expected `name, ping_interval, signals or rt`",
                ));
            }
        }

        Ok(args)
    }
}
