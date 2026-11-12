use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

use crate::parse::{Arg, Error, parse_args};

pub(crate) struct MainArgs {
    name: Option<Literal>,
    signals: Option<Ident>,
    fatal_signals: Option<Ident>,
    ping_interval: Option<Literal>,
    timer_resolution: Option<Literal>,
    panic_handling: Option<Ident>,
    task_callbacks: Option<TokenStream>,
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

        let sys_fatal_signals = self
            .fatal_signals
            .map(|fatal| quote!(.fatal_signals(#fatal)))
            .unwrap_or_default();

        let sys_panics = self
            .panic_handling
            .map(|panics| quote!(.panic_handling(#panics)))
            .unwrap_or_default();

        let sys_task_callbacks = gen_task_callbacks(self.task_callbacks);

        quote! {
            #sys_name
            #sys_ping_interval
            #sys_timer_resolution
            #sys_signals
            #sys_fatal_signals
            #sys_panics
            #sys_task_callbacks
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
            fatal_signals: None,
            panic_handling: None,
            ping_interval: None,
            timer_resolution: None,
            task_callbacks: None,
        };

        for param in parse_args(input)? {
            let name = param.name.to_string();
            let duplicate = match name.as_str() {
                "name" => args.name.is_some(),
                "signals" => args.signals.is_some(),
                "fatal_signals" => args.fatal_signals.is_some(),
                "panic_handling" => args.panic_handling.is_some(),
                "ping_interval" => args.ping_interval.is_some(),
                "timer_resolution" => args.timer_resolution.is_some(),
                "task_callbacks" => args.task_callbacks.is_some(),
                "rt" => args.rt.is_some(),
                _ => {
                    return Err(Error::new_spanned(
                        &param.name,
                        "unknown argument, expected `name`, `signals`, `fatal_signals`, `panic_handling`, `ping_interval`, `timer_resolution`, `task_callbacks` or `rt`",
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
                "fatal_signals" => {
                    args.fatal_signals = Some(param.lit_bool().ok_or_else(|| {
                        param.value_error("`fatal_signals` value must be an bool literal")
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
                "task_callbacks" => {
                    args.task_callbacks = Some(parse_task_callbacks(&param)?);
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

/// Builder call for the `task_callbacks` argument, the path names a factory
/// function returning `TaskCallbacks`
fn gen_task_callbacks(path: Option<TokenStream>) -> TokenStream {
    path.map(|path| quote!(.task_callbacks(#path)))
        .unwrap_or_default()
}

fn parse_task_callbacks(param: &Arg) -> Result<TokenStream, Error> {
    param
        .path()
        .ok_or_else(|| param.value_error("`task_callbacks` value must be a path"))
}

/// Arguments of the `test` macro
pub(crate) struct TestArgs {
    timer_resolution: Option<Literal>,
    task_callbacks: Option<TokenStream>,
}

impl TestArgs {
    pub(crate) fn gen_sys_config(self) -> TokenStream {
        let timer_resolution = self
            .timer_resolution
            .map(|millis| quote!(.timer_resolution(#millis)))
            .unwrap_or_default();
        let task_callbacks = gen_task_callbacks(self.task_callbacks);
        quote!(#timer_resolution #task_callbacks)
    }

    pub(crate) fn parse(input: TokenStream) -> Result<Self, Error> {
        let mut args = TestArgs {
            timer_resolution: None,
            task_callbacks: None,
        };

        for param in parse_args(input)? {
            let name = param.name.to_string();
            let duplicate = match name.as_str() {
                "timer_resolution" => args.timer_resolution.is_some(),
                "task_callbacks" => args.task_callbacks.is_some(),
                _ => {
                    return Err(Error::new_spanned(
                        &param.name,
                        "unknown argument, expected `timer_resolution` or `task_callbacks`",
                    ));
                }
            };
            if duplicate {
                return Err(Error::new_spanned(
                    &param.name,
                    format!("duplicate `{name}` argument"),
                ));
            }

            if name == "timer_resolution" {
                args.timer_resolution = Some(param.lit_int().ok_or_else(|| {
                    param.value_error("`timer_resolution` value must be an integer literal")
                })?);
            } else {
                args.task_callbacks = Some(parse_task_callbacks(&param)?);
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

    /// STATEMENT: Unknown `main` arguments are rejected with the list of supported arguments.
    ///
    /// HOW:
    /// - Parse `foo = 1` and compare the error with the expected argument list.
    /// - Parse known boolean arguments and confirm they are accepted.
    #[test]
    fn unknown_arg() {
        assert_eq!(
            err("foo = 1"),
            "unknown argument, expected `name`, `signals`, `fatal_signals`, `panic_handling`, `ping_interval`, `timer_resolution`, `task_callbacks` or `rt`"
        );
        assert!(parse("panic_handling = true, signals = true, fatal_signals = true").is_ok());
    }

    /// STATEMENT: `main` arguments generate builder calls and invalid values report errors.
    ///
    /// HOW:
    /// - Parse all value arguments and compare the generated runtime and
    ///   builder calls, including `.timer_resolution(2)` and a
    ///   `.task_callbacks(..)` call passing the given path.
    /// - Parse wrongly typed and duplicate arguments and compare their errors.
    #[test]
    fn args() {
        let mut args = parse(
            r#"name = "srv", signals = true, fatal_signals = true, ping_interval = 250, timer_resolution = 2, task_callbacks = my::cbs, rt = my::Rt,"#,
        )
        .ok()
        .unwrap();
        assert_eq!(args.gen_sys_rt().to_string(), "my :: Rt");
        assert_eq!(
            args.gen_sys_config(&Ident::new("main", proc_macro2::Span::call_site()))
                .to_string(),
            r#". name ("srv") . ping_interval (250) . timer_resolution (2) . signals (true) . fatal_signals (true) . task_callbacks (my :: cbs)"#
        );

        assert_eq!(err("name = 1"), "`name` value must be an string literal");
        assert_eq!(
            err("signals = 1"),
            "`signals` value must be an bool literal"
        );
        assert_eq!(
            err("fatal_signals = 1"),
            "`fatal_signals` value must be an bool literal"
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
        assert_eq!(
            err("task_callbacks = 1"),
            "`task_callbacks` value must be a path"
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

    /// STATEMENT: The `test` macro accepts only `timer_resolution` and
    /// `task_callbacks` and turns them into builder calls.
    ///
    /// HOW:
    /// - Parse empty arguments, `timer_resolution = 1` and `task_callbacks`
    ///   and compare the generated builder calls.
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
        assert_eq!(
            cfg("task_callbacks = cbs, timer_resolution = 1"),
            ". timer_resolution (1) . task_callbacks (cbs)"
        );

        let unknown = "unknown argument, expected `timer_resolution` or `task_callbacks`";
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
        assert_eq!(
            test_err("task_callbacks = \"cbs\""),
            "`task_callbacks` value must be a path"
        );
        assert_eq!(
            test_err("task_callbacks = a, task_callbacks = b"),
            "duplicate `task_callbacks` argument"
        );
    }
}
