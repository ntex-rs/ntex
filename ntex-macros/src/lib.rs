//! Procedural macros for ntex.
//!
//! You don't need to depend on this crate directly, `ntex` re-exports every
//! macro:
//!
//! - `#[ntex::main]` runs an async function on the ntex runtime, see
//!   [`rt_main`].
//! - `#[ntex::test]` does the same for a test, see [`rt_test`].
//! - `#[ntex::web::get]`, `#[ntex::web::post]` and friends turn an async
//!   function into a web handler with a path and a method guard, see
//!   [`web_get`].
//!
//! ## Route macros
//!
//! | `ntex::web` | Method    | This crate        |
//! |-------------|-----------|-------------------|
//! | `get`       | `GET`     | [`web_get`]       |
//! | `post`      | `POST`    | [`web_post`]      |
//! | `put`       | `PUT`     | [`web_put`]       |
//! | `delete`    | `DELETE`  | [`web_delete`]    |
//! | `head`      | `HEAD`    | [`web_head`]      |
//! | `connect`   | `CONNECT` | [`web_connect`]   |
//! | `options`   | `OPTIONS` | [`web_options`]   |
//! | `trace`     | `TRACE`   | [`web_trace`]     |
//! | `patch`     | `PATCH`   | [`web_patch`]     |
//! | `query`     | `QUERY`   | [`web_query`]     |
//!
//! All of them take the same arguments, they are described on [`web_get`].
//!
//! ```rust
//! use ntex::web::{App, HttpResponse, get, types::Path};
//!
//! #[get("/users/{id}")]
//! async fn user(id: Path<u32>) -> HttpResponse {
//!     HttpResponse::Ok().body(format!("user {}", id.into_inner()))
//! }
//!
//! // `user` is now a service, register it on an application
//! let app = App::<()>::new().service(user);
//! ```

use proc_macro::TokenStream;
use quote::quote;

mod route;
mod sys;

/// Creates a route handler with a `GET` method guard.
///
/// Re-exported as `ntex::web::get`.
///
/// Syntax: `#[get("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// The macro takes a handler function and replaces it with a unit struct of
/// the same name. The struct is a web service, you register it with
/// `.service()` on an `App` or a `scope`. The function itself ends up inside
/// the generated code, so you can't call it directly anymore. The struct is
/// always `pub`, whatever the visibility of the function.
///
/// The function is any handler `Route::to()` accepts: an `async fn` with up
/// to 16 extractors, or a plain `fn` that returns a future. The function name
/// is also used as the resource name, so it works with `url_for()`.
///
/// ## Arguments
///
/// - `"path"` - path of the resource, the same syntax as for
///   `Resource::new()`, for example `"/users/{id}"`. Required, must come
///   first.
/// - `guard = "fn_name"` - adds a guard built with
///   `ntex::web::guard::fn_guard()`. The value is the name of a function
///   `fn(&RequestHead) -> bool` that is in scope where the macro is used. It
///   must be a plain name, paths like `"guards::is_json"` are not supported,
///   import the function instead. Can be given more than once, all guards must
///   pass.
/// - `state = Type` - type of the application state, written as a type path
///   without quotes. The handler can then only be registered on an
///   `App<Type>`, and its errors use the error type of `Type`. It does not
///   give the handler access to the state. Defaults to `()`, the state of
///   `App::new()`.
///
/// ## Examples
///
/// ```rust
/// use ntex::http::RequestHead;
/// use ntex::web::{App, HttpResponse, get, post};
///
/// #[get("/")]
/// async fn index() -> HttpResponse {
///     HttpResponse::Ok().body("hello")
/// }
///
/// fn is_json(req: &RequestHead) -> bool {
///     req.headers()
///         .get("content-type")
///         .is_some_and(|v| v == "application/json")
/// }
///
/// // only matches POST requests with a json content type
/// #[post("/items", guard = "is_json")]
/// async fn create_item(body: String) -> HttpResponse {
///     HttpResponse::Created().body(body)
/// }
///
/// let app = App::<()>::new().service((index, create_item));
/// ```
///
/// With a custom application state:
///
/// ```rust
/// use ntex::web::{self, App, HttpResponse, get};
///
/// #[derive(Clone)]
/// struct MyState;
///
/// impl web::State for MyState {
///     type Error = web::DefaultError;
/// }
///
/// #[get("/", state = MyState)]
/// async fn index() -> HttpResponse {
///     HttpResponse::Ok().build()
/// }
///
/// let app = App::<MyState>::new().service(index);
/// ```
#[proc_macro_attribute]
pub fn web_get(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Get) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `POST` method guard.
///
/// Re-exported as `ntex::web::post`.
///
/// Syntax: `#[post("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_post(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Post) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `PUT` method guard.
///
/// Re-exported as `ntex::web::put`.
///
/// Syntax: `#[put("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_put(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Put) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `DELETE` method guard.
///
/// Re-exported as `ntex::web::delete`.
///
/// Syntax: `#[delete("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_delete(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Delete) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `HEAD` method guard.
///
/// Re-exported as `ntex::web::head`.
///
/// Syntax: `#[head("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_head(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Head) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `CONNECT` method guard.
///
/// Re-exported as `ntex::web::connect`.
///
/// Syntax: `#[connect("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_connect(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Connect) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `OPTIONS` method guard.
///
/// Re-exported as `ntex::web::options`.
///
/// Syntax: `#[options("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_options(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Options) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `TRACE` method guard.
///
/// Re-exported as `ntex::web::trace`.
///
/// Syntax: `#[trace("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_trace(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Trace) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `PATCH` method guard.
///
/// Re-exported as `ntex::web::patch`.
///
/// Syntax: `#[patch("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_patch(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Patch) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Creates a route handler with a `QUERY` method guard.
///
/// Re-exported as `ntex::web::query`.
///
/// Syntax: `#[query("path"[, guard = "fn_name"]*[, state = Type])]`
///
/// Works the same way and takes the same arguments as [`web_get`].
#[proc_macro_attribute]
pub fn web_query(args: TokenStream, input: TokenStream) -> TokenStream {
    let gen_code = match route::Route::new(args, input, route::MethodType::Query) {
        Ok(gen_code) => gen_code,
        Err(err) => return err.to_compile_error().into(),
    };
    gen_code.generate()
}

/// Runs an async function on the ntex runtime.
///
/// Re-exported as `ntex::main`.
///
/// The function becomes a normal blocking function. When called, it builds a
/// `System`, runs the body to completion and returns its result, so the
/// function can return a value, for example `std::io::Result<()>`. It does
/// not have to be `main`.
///
/// ```rust
/// #[ntex::main]
/// async fn main() -> std::io::Result<()> {
///     println!("Hello world");
///     Ok(())
/// }
/// ```
///
/// ## Arguments
///
/// - `name = "..."` - name of the system. Defaults to the function name.
/// - `signals = true/false` - handle process signals. Off by default.
/// - `panic_handling = true/false` - report application panics as
///   `Signal::Panic`. Only useful together with `signals = true`. Off by
///   default.
/// - `ping_interval = N` - how often, in milliseconds, the system pings its
///   arbiters to spot busy ones. Defaults to 2000, zero turns pings off.
/// - `rt = path` - the runtime to run on, a value that implements
///   `ntex::rt::Runner`. Defaults to `ntex::rt::DefaultRuntime`, the runtime
///   picked by ntex features.
///
/// ```rust
/// #[ntex::main(name = "server", signals = true, ping_interval = 250)]
/// async fn main() {
///     println!("Hello world");
/// }
/// ```
#[proc_macro_attribute]
pub fn rt_main(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut args = syn::parse_macro_input!(args as sys::MainArgs);
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

    let runner = args.gen_sys_rt();
    let config = args.gen_sys_config(name);

    (quote! {
        #(#attrs)*
        #vis #sig {
            ntex::rt::System::build()
                #config
                .build( #runner )
                .block_on(async move { #body })
        }
    })
    .into()
}

/// Runs an async test on the ntex runtime.
///
/// Re-exported as `ntex::test`.
///
/// The macro adds `#[test]`, unless the function already has it, and runs the
/// body on a new `System` named after the test. The system is built in
/// testing mode, without signal and panic handling, and always uses
/// `ntex::rt::DefaultRuntime`. The test can return a `Result`, like a normal
/// test. The macro takes no arguments.
///
/// It also turns on `env_logger` at `trace` level, unless `RUST_LOG` is set.
/// Set `NTEX_NO_TEST_LOG` or enable the `no-test-logging` feature of ntex to
/// turn logging off.
///
/// ```no_run
/// #[ntex::test]
/// async fn my_test() {
///     assert!(true);
/// }
///
/// #[ntex::test]
/// async fn my_fallible_test() -> std::io::Result<()> {
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn rt_test(_: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let fut = boxed_future(ret, body);
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
                    .block_on(#fut)
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
                    .block_on(#fut)
            }
        }
    };

    result.into()
}

/// Same as [`rt_test`] for crates that depend on `ntex-rt` directly. Doesn't
/// enable test logging.
#[doc(hidden)]
#[proc_macro_attribute]
pub fn rt_test2(_: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let fut = boxed_future(ret, body);
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
                    .block_on(#fut)
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
                    .block_on(#fut)
            }
        }
    };

    result.into()
}

/// Same as [`rt_test`] for tests inside the `ntex` crate itself, it refers to
/// `crate::` paths.
#[doc(hidden)]
#[proc_macro_attribute]
pub fn rt_test_internal(_: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let fut = boxed_future(ret, body);
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
                    .block_on(#fut)
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
                    .block_on(#fut)
            }
        }
    };

    result.into()
}

/// Box the test body so the runtime's `block_on` is generated once per
/// return type instead of once per test function
fn boxed_future(ret: &syn::ReturnType, body: &syn::Block) -> proc_macro2::TokenStream {
    let output = match ret {
        syn::ReturnType::Default => quote! { () },
        syn::ReturnType::Type(_, ty) => quote! { #ty },
    };
    quote! {
        ::std::boxed::Box::pin(async { #body })
            as ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = #output>>>
    }
}
