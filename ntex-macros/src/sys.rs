use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

use crate::parse::{Error, parse_args};

pub(crate) struct MainArgs {
    name: Option<Literal>,
    signals: Option<Ident>,
    ping_interval: Option<Literal>,
    panic_handling: Option<Ident>,
    rt: Option<TokenStream>,
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
            .unwrap_or_else(|| quote!(ntex::rt::DefaultRuntime))
    }

    pub(crate) fn parse(input: TokenStream) -> Result<Self, Error> {
        let mut args = MainArgs {
            rt: None,
            name: None,
            signals: None,
            panic_handling: None,
            ping_interval: None,
        };

        for param in parse_args(input)? {
            let name = param.name.to_string();
            let duplicate = match name.as_str() {
                "name" => args.name.is_some(),
                "signals" => args.signals.is_some(),
                "panic_handling" => args.panic_handling.is_some(),
                "ping_interval" => args.ping_interval.is_some(),
                "rt" => args.rt.is_some(),
                _ => {
                    return Err(Error::new_spanned(
                        &param.name,
                        "unknown argument, expected `name`, `signals`, `panic_handling`, `ping_interval` or `rt`",
                    ));
                }
            };
            if duplicate {
                return Err(Error::new_spanned(
                    &param.name,
                    format!("duplicate `{name}` argument"),
                ));
            }

            match name.as_str() {
                "name" => {
                    args.name = Some(param.lit_str().ok_or_else(|| {
                        param.value_error("`name` value must be an string literal")
                    })?);
                }
                "signals" => {
                    args.signals = Some(param.lit_bool().ok_or_else(|| {
                        param.value_error("`signals` value must be an bool literal")
                    })?);
                }
                "panic_handling" => {
                    args.panic_handling = Some(param.lit_bool().ok_or_else(|| {
                        param.value_error("`panic_handling` value must be an bool literal")
                    })?);
                }
                "ping_interval" => {
                    args.ping_interval = Some(param.lit_int().ok_or_else(|| {
                        param.value_error("`ping_interval` value must be an integer literal")
                    })?);
                }
                _ => {
                    args.rt = Some(
                        param
                            .path()
                            .ok_or_else(|| param.value_error("`rt` value must be a type"))?,
                    );
                }
            }
        }

        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<MainArgs, Error> {
        MainArgs::parse(s.parse().unwrap())
    }

    fn err(s: &str) -> String {
        parse(s).err().unwrap().to_string()
    }

    #[test]
    fn unknown_arg() {
        assert_eq!(
            err("foo = 1"),
            "unknown argument, expected `name`, `signals`, `panic_handling`, `ping_interval` or `rt`"
        );
        assert!(parse("panic_handling = true, signals = true").is_ok());
    }

    #[test]
    fn args() {
        let mut args = parse(r#"name = "srv", signals = true, ping_interval = 250, rt = my::Rt,"#)
            .ok()
            .unwrap();
        assert_eq!(args.gen_sys_rt().to_string(), "my :: Rt");
        assert_eq!(
            args.gen_sys_config(&Ident::new("main", proc_macro2::Span::call_site()))
                .to_string(),
            r#". name ("srv") . ping_interval (250) . signals (true)"#
        );

        assert_eq!(err("name = 1"), "`name` value must be an string literal");
        assert_eq!(
            err("signals = 1"),
            "`signals` value must be an bool literal"
        );
        assert_eq!(
            err("panic_handling = \"x\""),
            "`panic_handling` value must be an bool literal"
        );
        assert_eq!(
            err("ping_interval = 1.5"),
            "`ping_interval` value must be an integer literal"
        );
        assert_eq!(err("rt = \"x\""), "`rt` value must be a type");
        assert_eq!(err("rt = a, rt = b"), "duplicate `rt` argument");
    }
}
