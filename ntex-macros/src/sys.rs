use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, Ident, Lit, LitBool, LitInt, LitStr, MetaNameValue, Token};

pub(crate) struct MainArgs {
    name: Option<LitStr>,
    signals: Option<LitBool>,
    ping_interval: Option<LitInt>,
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

        quote! {
            #sys_name
            #sys_ping_interval
            #sys_signals
        }
    }
}

impl Parse for MainArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut args = MainArgs {
            name: None,
            signals: None,
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
            } else {
                return Err(syn::Error::new_spanned(
                    param.path,
                    "unknown argument, expected `name or ping_interval`",
                ));
            }
        }

        Ok(args)
    }
}
