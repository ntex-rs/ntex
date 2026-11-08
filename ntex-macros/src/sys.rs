use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

use crate::parse::{Error, parse_args};

pub(crate) struct MainArgs {
    name: Option<Literal>,
    signals: Option<Ident>,
    ping_interval: Option<Literal>,
    timer_resolution: Option<Literal>,
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

        let sys_timer_resolution = self
            .timer_resolution
            .map(|millis| quote!(.timer_resolution(#millis)))
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
            #sys_timer_resolution
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
            timer_resolution: None,
        };

        for param in parse_args(input)? {
            let name = param.name.to_string();
            let duplicate = match name.as_str() {
                "name" => args.name.is_some(),
                "signals" => args.signals.is_some(),
                "panic_handling" => args.panic_handling.is_some(),
                "ping_interval" => args.ping_interval.is_some(),
                "timer_resolution" => args.timer_resolution.is_some(),
                "rt" => args.rt.is_some(),
                _ => {
                    return Err(Error::new_spanned(
                        &param.name,
                        "unknown argument, expected `name`, `signals`, `panic_handling`, `ping_interval`, `timer_resolution` or `rt`",
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
                "timer_resolution" => {
                    args.timer_resolution = Some(param.lit_int().ok_or_else(|| {
                        param.value_error("`timer_resolution` value must be an integer literal")
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

/// Arguments of the `test` macro
pub(crate) struct TestArgs {
    timer_resolution: Option<Literal>,
}

impl TestArgs {
    pub(crate) fn gen_sys_config(self) -> TokenStream {
        self.timer_resolution
            .map(|millis| quote!(.timer_resolution(#millis)))
            .unwrap_or_default()
    }

    pub(crate) fn parse(input: TokenStream) -> Result<Self, Error> {
        let mut args = TestArgs {
            timer_resolution: None,
        };

        for param in parse_args(input)? {
            if param.name != "timer_resolution" {
                return Err(Error::new_spanned(
                    &param.name,
                    "unknown argument, expected `timer_resolution`",
                ));
            }
            if args.timer_resolution.is_some() {
                return Err(Error::new_spanned(
                    &param.name,
                    "duplicate `timer_resolution` argument",
                ));
            }
            args.timer_resolution = Some(param.lit_int().ok_or_else(|| {
                param.value_error("`timer_resolution` value must be an integer literal")
            })?);
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

    /// STATEMENT: Unknown `main` arguments are rejected with the list of supported arguments.
    ///
    /// HOW:
    /// - Parse `foo = 1` and compare the error with the expected argument list.
    /// - Parse known boolean arguments and confirm they are accepted.
    #[test]
    fn unknown_arg() {
        assert_eq!(
            err("foo = 1"),
            "unknown argument, expected `name`, `signals`, `panic_handling`, `ping_interval`, `timer_resolution` or `rt`"
        );
        assert!(parse("panic_handling = true, signals = true").is_ok());
    }

    /// STATEMENT: `main` arguments generate builder calls and invalid values report errors.
    ///
    /// HOW:
    /// - Parse all value arguments and compare the generated runtime and
    ///   builder calls, including `.timer_resolution(2)`.
    /// - Parse wrongly typed and duplicate arguments and compare their errors.
    #[test]
    fn args() {
        let mut args = parse(
            r#"name = "srv", signals = true, ping_interval = 250, timer_resolution = 2, rt = my::Rt,"#,
        )
        .ok()
        .unwrap();
        assert_eq!(args.gen_sys_rt().to_string(), "my :: Rt");
        assert_eq!(
            args.gen_sys_config(&Ident::new("main", proc_macro2::Span::call_site()))
                .to_string(),
            r#". name ("srv") . ping_interval (250) . timer_resolution (2) . signals (true)"#
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
        assert_eq!(
            err("timer_resolution = \"2\""),
            "`timer_resolution` value must be an integer literal"
        );
        assert_eq!(
            err("timer_resolution = 2, timer_resolution = 4"),
            "duplicate `timer_resolution` argument"
        );
        assert_eq!(err("rt = \"x\""), "`rt` value must be a type");
        assert_eq!(err("rt = a, rt = b"), "duplicate `rt` argument");
    }

    fn test_err(s: &str) -> String {
        TestArgs::parse(s.parse().unwrap())
            .err()
            .unwrap()
            .to_string()
    }

    /// STATEMENT: The `test` macro accepts only `timer_resolution` and turns it into a builder call.
    ///
    /// HOW:
    /// - Parse empty arguments and `timer_resolution = 1` and compare the
    ///   generated builder calls.
    /// - Parse unknown, `main`-only, wrongly typed and duplicate arguments and
    ///   compare their errors.
    #[test]
    fn test_args() {
        let cfg = |s: &str| {
            TestArgs::parse(s.parse().unwrap())
                .ok()
                .unwrap()
                .gen_sys_config()
                .to_string()
        };
        assert_eq!(cfg(""), "");
        assert_eq!(cfg("timer_resolution = 1"), ". timer_resolution (1)");

        let unknown = "unknown argument, expected `timer_resolution`";
        assert_eq!(test_err("foo = 1"), unknown);
        assert_eq!(test_err("name = \"t\""), unknown);
        assert_eq!(
            test_err("timer_resolution = \"2\""),
            "`timer_resolution` value must be an integer literal"
        );
        assert_eq!(
            test_err("timer_resolution = 2, timer_resolution = 4"),
            "duplicate `timer_resolution` argument"
        );
    }
}
