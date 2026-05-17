//! ntex macros module
//!
//! Generators for routes
//!
//! ## Route
//!
//! Macros:
//!
//! - [get](attr.web_get.html)
//! - [post](attr.web_post.html)
//! - [put](attr.web_put.html)
//! - [delete](attr.web_delete.html)
//! - [head](attr.web_head.html)
//! - [connect](attr.web_connect.html)
//! - [options](attr.web_options.html)
//! - [trace](attr.web_trace.html)
//! - [patch](attr.web_patch.html)
//!
//! ### Attributes:
//!
//! - `"path"` - Raw literal string with path for which to register handle. Mandatory.
//! - `guard = "function_name"` - Registers function as guard using `ntex::web::guard::fn_guard`
//! - `error = "ErrorRenderer"` - Register handler for specified error renderer
//!
//! ## Notes
//!
//! Function name can be specified as any expression that is going to be accessible to the generate
//! code (e.g `my_guard` or `my_module::my_guard`)
//!
//! ## Example:
//!
//! ```rust
//! use ntex::web::{get, Error, HttpResponse};
//! use futures::{future, Future};
//!
//! #[get("/test")]
//! async fn async_test() -> Result<HttpResponse, Error> {
//!     Ok(HttpResponse::Ok().finish())
//! }
//! ```

extern crate proc_macro;

mod route;

use proc_macro::TokenStream;
use quote::quote;

/// Creates route handler with `GET` method guard.
///
/// Syntax: `#[get("path"[, attributes])]`
///
/// ## Attributes:
///
/// - `"path"` - Raw literal string with path for which to register handler. Mandatory.
/// - `guard = "function_name"` - Registers function as guard using `ntex::web::guard::fn_guard`
/// - `error = "ErrorRenderer"` - Register handler for different error renderer
#[proc_macro_attribute]
pub fn web_get(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Get) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `POST` method guard.
///
/// Syntax: `#[post("path"[, attributes])]`
///
/// Attributes are the same as in [get](attr.get.html)
#[proc_macro_attribute]
pub fn web_post(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Post) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `PUT` method guard.
///
/// Syntax: `#[put("path"[, attributes])]`
///
/// Attributes are the same as in [get](attr.get.html)
#[proc_macro_attribute]
pub fn web_put(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Put) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `DELETE` method guard.
///
/// Syntax: `#[delete("path"[, attributes])]`
///
/// Attributes are the same as in [get](attr.get.html)
#[proc_macro_attribute]
pub fn web_delete(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Delete) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `HEAD` method guard.
///
/// Syntax: `#[head("path"[, attributes])]`
///
/// Attributes are the same as in [head](attr.head.html)
#[proc_macro_attribute]
pub fn web_head(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Head) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `CONNECT` method guard.
///
/// Syntax: `#[connect("path"[, attributes])]`
///
/// Attributes are the same as in [connect](attr.connect.html)
#[proc_macro_attribute]
pub fn web_connect(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Connect) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `OPTIONS` method guard.
///
/// Syntax: `#[options("path"[, attributes])]`
///
/// Attributes are the same as in [options](attr.options.html)
#[proc_macro_attribute]
pub fn web_options(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Options) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `TRACE` method guard.
///
/// Syntax: `#[trace("path"[, attributes])]`
///
/// Attributes are the same as in [trace](attr.trace.html)
#[proc_macro_attribute]
pub fn web_trace(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Trace) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates route handler with `PATCH` method guard.
///
/// Syntax: `#[patch("path"[, attributes])]`
///
/// Attributes are the same as in [patch](attr.patch.html)
#[proc_macro_attribute]
pub fn web_patch(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Patch) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Marks async function to be executed by ntex system.
///
/// ## Usage
///
/// ```rust
/// #[ntex::main]
/// async fn main() {
///     println!("Hello world");
/// }
/// ```
#[proc_macro_attribute]
pub fn rt_main(_: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = syn::parse_macro_input!(item as syn::ItemFn);
    let attrs = &input.attrs;
    let vis = &input.vis;
    let sig = &mut input.sig;
    let body = &input.block;
    let name = &sig.ident;

    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(sig.fn_token, "only async fn is supported")
            .to_compile_error()
            .into();
    }

    sig.asyncness = None;

    (quote! {
        #(#attrs)*
        #vis #sig {
            ntex::rt::System::build()
                .name(stringify!(#name))
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move { #body })
        }
    })
    .into()
}

/// Marks async test function to be executed by ntex runtime.
///
/// ## Usage
///
/// ```no_run
/// #[ntex::test]
/// async fn my_test() {
///     assert!(true);
/// }
/// ```
#[proc_macro_attribute]
pub fn rt_test(_: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let mut has_test_attr = false;

    for attr in attrs {
        if attr.path().is_ident("test") {
            has_test_attr = true;
        }
    }

    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input.sig.fn_token,
            format!("only async fn is supported, {}", input.sig.ident),
        )
        .to_compile_error()
        .into();
    }

    let result = if has_test_attr {
        quote! {
            #(#attrs)*
            fn #name() #ret {
                ntex::util::enable_test_logging();
                ntex::rt::System::build()
                    .name(stringify!(#name))
                    .testing()
                    .build(ntex::rt::DefaultRuntime)
                    .block_on(async { #body })
            }
        }
    } else {
        quote! {
            #[test]
            #(#attrs)*
            fn #name() #ret {
                ntex::util::enable_test_logging();
                ntex::rt::System::build()
                    .name(stringify!(#name))
                    .testing()
                    .build(ntex::rt::DefaultRuntime)
                    .block_on(async { #body })
            }
        }
    };

    result.into()
}

/// Marks async test function to be executed by ntex runtime.
///
/// ## Usage
///
/// ```no_run
/// #[ntex::test]
/// async fn my_test() {
///     assert!(true);
/// }
/// ```
#[doc(hidden)]
#[proc_macro_attribute]
pub fn rt_test2(_: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let mut has_test_attr = false;

    for attr in attrs {
        if attr.path().is_ident("test") {
            has_test_attr = true;
        }
    }

    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input.sig.fn_token,
            format!("only async fn is supported, {}", input.sig.ident),
        )
        .to_compile_error()
        .into();
    }

    let result = if has_test_attr {
        quote! {
            #(#attrs)*
            fn #name() #ret {
                ntex_rt::System::build()
                    .name(stringify!(#name))
                    .testing()
                    .build(ntex::rt::DefaultRuntime)
                    .block_on(async { #body })
            }
        }
    } else {
        quote! {
            #[test]
            #(#attrs)*
            fn #name() #ret {
                ntex_rt::System::build()
                    .name(stringify!(#name))
                    .testing()
                    .build(ntex::rt::DefaultRuntime)
                    .block_on(async { #body })
            }
        }
    };

    result.into()
}

/// Marks async test function to be executed by ntex runtime.
///
/// ## Usage
///
/// ```no_run
/// #[ntex::test]
/// async fn my_test() {
///     assert!(true);
/// }
/// ```
#[doc(hidden)]
#[proc_macro_attribute]
pub fn rt_test_internal(_: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let mut has_test_attr = false;

    for attr in attrs {
        if attr.path().is_ident("test") {
            has_test_attr = true;
        }
    }

    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input.sig.fn_token,
            format!("only async fn is supported, {}", input.sig.ident),
        )
        .to_compile_error()
        .into();
    }

    let result = if has_test_attr {
        quote! {
            #(#attrs)*
            fn #name() #ret {
                crate::util::enable_test_logging();
                ntex_rt::System::build()
                    .name(stringify!(#name))
                    .testing()
                    .build(crate::rt::DefaultRuntime)
                    .block_on(async { #body })
            }
        }
    } else {
        quote! {
            #[test]
            #(#attrs)*
            fn #name() #ret {
                crate::util::enable_test_logging();
                ntex_rt::System::build()
                    .name(stringify!(#name))
                    .testing()
                    .build(crate::rt::DefaultRuntime)
                    .block_on(async { #body })
            }
        }
    };

    result.into()
}
