//! Small parsers for the macro inputs, built on `unsynn`
// `unsynn::Error` is large, the lint fires inside `unsynn!` expansions
#![allow(clippy::result_large_err)]
use std::fmt;

use proc_macro2::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};
use quote::quote;
use unsynn::{
    BraceGroup, BracketGroup, Comma, Cons, EndOfStream, Except, Lt, ParenthesisGroup, Parse,
    PathSep, Pound, RArrow, Result as PResult, ToTokens, TokenIter, unsynn,
};

/// Macro error, rendered as `compile_error!` over the `start..end` span
pub(crate) struct Error {
    start: Span,
    end: Span,
    msg: String,
}

impl Error {
    pub(crate) fn new(span: Span, msg: impl Into<String>) -> Self {
        Self {
            start: span,
            end: span,
            msg: msg.into(),
        }
    }

    /// Error that covers `tokens`
    pub(crate) fn new_spanned(tokens: &(impl ToTokens + ?Sized), msg: impl Into<String>) -> Self {
        let mut iter = tokens.to_token_stream().into_iter();
        let start = iter.next().map_or_else(Span::call_site, |t| t.span());
        let end = iter.last().map_or(start, |t| t.span());
        Self {
            start,
            end,
            msg: msg.into(),
        }
    }

    /// Error at the token where `unsynn` failed
    fn parse(err: &unsynn::Error, msg: impl Into<String>) -> Self {
        let span = err.failed_at().map_or_else(Span::call_site, |t| t.span());
        Self::new(span, msg)
    }

    pub(crate) fn to_compile_error(&self) -> TokenStream {
        let (start, end) = (self.start, self.end);
        let mut msg = Literal::string(&self.msg);
        msg.set_span(end);
        let mut group = Group::new(Delimiter::Brace, TokenTree::Literal(msg).into());
        group.set_span(end);

        let sep = || {
            let mut p1 = Punct::new(':', Spacing::Joint);
            let mut p2 = Punct::new(':', Spacing::Alone);
            p1.set_span(start);
            p2.set_span(start);
            [TokenTree::Punct(p1), TokenTree::Punct(p2)]
        };
        let mut bang = Punct::new('!', Spacing::Alone);
        bang.set_span(start);

        let mut out = TokenStream::new();
        out.extend(sep());
        out.extend([TokenTree::Ident(Ident::new("core", start))]);
        out.extend(sep());
        out.extend([
            TokenTree::Ident(Ident::new("compile_error", start)),
            TokenTree::Punct(bang),
            TokenTree::Group(group),
        ]);
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl From<Error> for proc_macro::TokenStream {
    fn from(err: Error) -> Self {
        err.to_compile_error().into()
    }
}

unsynn! {
    keyword KwAsync = "async";
    keyword KwConst = "const";
    keyword KwExtern = "extern";
    keyword KwFn = "fn";
    keyword KwPub = "pub";
    keyword KwUnsafe = "unsafe";
    keyword KwWhere = "where";

    /// Outer attribute, `#[...]`
    pub(crate) struct Attribute {
        pound: Pound,
        body: BracketGroup,
    }

    /// `pub`, `pub(crate)`, ...
    struct Visibility {
        kw: KwPub,
        restriction: Option<ParenthesisGroup>,
    }

    /// Function qualifier, `const`, `async`, `unsafe` or `extern "abi"`
    enum Qualifier {
        Const(KwConst),
        Async(KwAsync),
        Unsafe(KwUnsafe),
        Extern(Cons<KwExtern, Option<Literal>>),
    }

    /// Everything before the function name
    struct FnPrefix {
        attrs: Vec<Attribute>,
        vis: Option<Visibility>,
        qualifiers: Vec<Qualifier>,
        fn_kw: KwFn,
    }

    /// Function body, the last token of the item
    struct Body {
        group: BraceGroup,
        eos: EndOfStream,
    }

    struct ReturnType {
        arrow: RArrow,
        ty: Vec<Cons<Except<KwWhere>, Except<Body>, TokenTree>>,
    }

    struct WhereClause {
        kw: KwWhere,
        predicates: Vec<Cons<Except<Body>, TokenTree>>,
    }

    struct FnItem {
        attrs: Vec<Attribute>,
        vis: Option<Visibility>,
        qualifiers: Vec<Qualifier>,
        fn_kw: KwFn,
        ident: Ident,
        generics: Option<AngleBracketed>,
        params: ParenthesisGroup,
        output: Option<ReturnType>,
        where_clause: Option<WhereClause>,
        body: Body,
    }

    struct PathSegment {
        ident: Ident,
        args: Option<Cons<Option<PathSep>, AngleBracketed>>,
    }

    /// Type or expression path, `a::b::C<T>`
    struct TypePath {
        leading: Option<PathSep>,
        first: PathSegment,
        rest: Vec<Cons<PathSep, PathSegment>>,
    }

    /// Tokens up to the next top-level comma
    struct Value(Vec<Cons<Except<Comma>, TokenTree>>);

    /// `name = value`
    struct NameValue {
        name: Ident,
        eq: unsynn::Assign,
        value: Value,
    }
}

impl Attribute {
    /// Attribute path is the single identifier `name`
    fn is_ident(&self, name: &str) -> bool {
        let mut iter = self.body.0.stream().into_iter();
        matches!(iter.next(), Some(TokenTree::Ident(id)) if id == name)
            && !matches!(iter.next(), Some(TokenTree::Punct(p)) if p.as_char() == ':')
    }
}

/// Balanced `<...>`, generic parameters or arguments
#[derive(Debug)]
struct AngleBracketed(TokenStream);

impl unsynn::Parser for AngleBracketed {
    fn parser(tokens: &mut TokenIter) -> PResult<Self> {
        let lt = Lt::parser(tokens)?;
        let mut out = lt.to_token_stream();
        let mut depth = 1usize;
        let mut arrow = false;
        while depth > 0 {
            let tt = TokenTree::parser(tokens)?;
            if let TokenTree::Punct(p) = &tt {
                match p.as_char() {
                    '<' => depth += 1,
                    // skip `->` in `Fn() -> T` bounds
                    '>' if !arrow => depth -= 1,
                    _ => {}
                }
                arrow = p.as_char() == '-' && p.spacing() == Spacing::Joint;
            } else {
                arrow = false;
            }
            out.extend([tt]);
        }
        Ok(Self(out))
    }
}

impl ToTokens for AngleBracketed {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(self.0.clone());
    }
}

/// Parsed `fn` item
pub(crate) struct ItemFn(FnItem);

impl ItemFn {
    pub(crate) fn parse(input: TokenStream) -> Result<Self, Error> {
        let input = flatten(input);
        FnItem::parse_all(&mut TokenIter::new(input.clone()))
            .map(Self)
            .map_err(|e| match FnPrefix::parse(&mut TokenIter::new(input)) {
                Ok(_) => Error::parse(&e, "invalid `fn` item"),
                Err(e) => Error::parse(&e, "expected `fn`"),
            })
    }

    pub(crate) fn ident(&self) -> &Ident {
        &self.0.ident
    }

    pub(crate) fn attrs(&self) -> Vec<TokenStream> {
        self.0.attrs.iter().map(ToTokens::to_token_stream).collect()
    }

    pub(crate) fn has_attr(&self, name: &str) -> bool {
        self.0.attrs.iter().any(|attr| attr.is_ident(name))
    }

    pub(crate) fn vis(&self) -> TokenStream {
        self.0.vis.to_token_stream()
    }

    pub(crate) fn is_async(&self) -> bool {
        self.0
            .qualifiers
            .iter()
            .any(|q| matches!(q, Qualifier::Async(_)))
    }

    /// Error spanned on the `fn` keyword
    pub(crate) fn fn_error(&self, msg: impl Into<String>) -> Error {
        Error::new_spanned(&self.0.fn_kw, msg)
    }

    /// Function signature without `async`
    pub(crate) fn sig_without_async(&self) -> TokenStream {
        let f = &self.0;
        let mut out = TokenStream::new();
        for q in &f.qualifiers {
            if !matches!(q, Qualifier::Async(_)) {
                q.to_tokens(&mut out);
            }
        }
        f.fn_kw.to_tokens(&mut out);
        f.ident.to_tokens(&mut out);
        f.generics.to_tokens(&mut out);
        f.params.to_tokens(&mut out);
        f.output.to_tokens(&mut out);
        f.where_clause.to_tokens(&mut out);
        out
    }

    /// `-> T`, or nothing
    pub(crate) fn output(&self) -> TokenStream {
        self.0.output.to_token_stream()
    }

    /// Return type, `()` if there is none
    pub(crate) fn output_type(&self) -> TokenStream {
        match &self.0.output {
            Some(ret) => ret.ty.to_token_stream(),
            None => quote!(()),
        }
    }

    /// Function body, `{ ... }`
    pub(crate) fn body(&self) -> TokenStream {
        self.0.body.group.to_token_stream()
    }
}

/// Inline top-level `None`-delimited groups, `macro_rules!` fragments
fn flatten(input: TokenStream) -> TokenStream {
    input
        .into_iter()
        .flat_map(|tt| match tt {
            TokenTree::Group(g) if g.delimiter() == Delimiter::None => flatten(g.stream()),
            tt => tt.into(),
        })
        .collect()
}

/// Argument of a `name = value` list
pub(crate) struct Arg {
    pub(crate) name: Ident,
    eq_span: Span,
    value: Vec<TokenTree>,
}

impl Arg {
    /// Error spanned on the value
    pub(crate) fn value_error(&self, msg: impl Into<String>) -> Error {
        if self.value.is_empty() {
            Error::new(self.eq_span, msg)
        } else {
            Error::new_spanned(&self.value, msg)
        }
    }

    fn single(&self) -> Option<&TokenTree> {
        match self.value.as_slice() {
            [tt] => Some(tt),
            _ => None,
        }
    }

    /// String literal value, `"..."` or `r"..."`
    pub(crate) fn lit_str(&self) -> Option<Literal> {
        match self.single() {
            Some(TokenTree::Literal(lit)) if str_value(lit).is_some() => Some(lit.clone()),
            _ => None,
        }
    }

    /// `true` or `false`
    pub(crate) fn lit_bool(&self) -> Option<Ident> {
        match self.single() {
            Some(TokenTree::Ident(id)) if id == "true" || id == "false" => Some(id.clone()),
            _ => None,
        }
    }

    /// Integer literal value
    pub(crate) fn lit_int(&self) -> Option<Literal> {
        match self.single() {
            Some(TokenTree::Literal(lit)) if is_int(&lit.to_string()) => Some(lit.clone()),
            _ => None,
        }
    }

    /// Path value, `a::b::C`
    pub(crate) fn path(&self) -> Option<TokenStream> {
        let tokens: TokenStream = self.value.iter().cloned().collect();
        TypePath::parse_all(&mut TokenIter::new(tokens.clone()))
            .ok()
            .map(|_| tokens)
    }
}

/// Parse a comma separated `name = value` list, trailing comma is allowed
pub(crate) fn parse_args(input: TokenStream) -> Result<Vec<Arg>, Error> {
    unsynn::CommaDelimitedVec::<NameValue>::parse_all(&mut TokenIter::new(input))
        .map(|args| {
            args.into_iter()
                .map(|arg| {
                    let arg = arg.value;
                    let eq_span = arg.eq.to_token_stream().into_iter().next();
                    Arg {
                        name: arg.name,
                        eq_span: eq_span.map_or_else(Span::call_site, |t| t.span()),
                        value: arg.value.0.into_iter().map(|c| c.second).collect(),
                    }
                })
                .collect()
        })
        .map_err(|e| Error::parse(&e, "expected `name = value` argument"))
}

/// Unquoted value of a string literal, `None` for other literals
pub(crate) fn str_value(lit: &Literal) -> Option<String> {
    let s = lit.to_string();
    if let Some(raw) = s.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let inner = raw[hashes..].strip_prefix('"')?;
        let end = inner.rfind('"')?;
        return (inner[end + 1..].len() >= hashes).then(|| inner[..end].to_string());
    }
    let inner = s.strip_prefix('"')?;
    Some(inner[..inner.rfind('"')?].to_string())
}

fn is_int(s: &str) -> bool {
    if !s.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    if s.starts_with("0x") || s.starts_with("0o") || s.starts_with("0b") {
        return true;
    }
    !s.contains(['.', 'e', 'E']) && !s.ends_with("f32") && !s.ends_with("f64")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(s: &str) -> ItemFn {
        ItemFn::parse(s.parse().unwrap()).ok().unwrap()
    }

    #[test]
    fn item_fn() {
        let f = item(
            "#[test] #[cfg(unix)] pub(crate) async unsafe fn foo<F: Fn(u8) -> u8, T: Into<Vec<u8>>>(f: F) \
             -> Result<(), Box<dyn Error>> where T: 'static { body }",
        );
        assert_eq!(f.ident().to_string(), "foo");
        assert!(f.is_async());
        assert!(f.has_attr("test"));
        assert!(f.has_attr("cfg"));
        assert!(!f.has_attr("ignore"));
        assert_eq!(f.attrs().len(), 2);
        assert_eq!(f.vis().to_string(), "pub (crate)");
        assert_eq!(
            f.sig_without_async().to_string(),
            "unsafe fn foo < F : Fn (u8) -> u8 , T : Into < Vec < u8 >>> (f : F) \
             -> Result < () , Box < dyn Error >> where T : 'static"
        );
        assert_eq!(
            f.output_type().to_string(),
            "Result < () , Box < dyn Error >>"
        );
        assert_eq!(f.body().to_string(), "{ body }");

        let f = item("fn bar() {}");
        assert!(!f.is_async());
        assert!(f.vis().is_empty());
        assert!(f.output().is_empty());
        assert_eq!(f.output_type().to_string(), "()");

        let f = item("#[tokio::test] async fn baz() -> impl Trait<{ N }> {}");
        assert!(!f.has_attr("test"));
        assert_eq!(f.output_type().to_string(), "impl Trait < { N } >");

        let f = item(r#"pub const async extern "C" fn q() {}"#);
        assert!(f.is_async());
        assert_eq!(
            f.sig_without_async().to_string(),
            r#"const extern "C" fn q ()"#
        );

        let err = |s: &str| ItemFn::parse(s.parse().unwrap()).err().unwrap().to_string();
        assert_eq!(err("struct S;"), "expected `fn`");
        assert_eq!(err("async struct S;"), "expected `fn`");
        assert_eq!(err("fn f() {} fn g() {}"), "invalid `fn` item");
        assert_eq!(err("fn f();"), "invalid `fn` item");
    }

    #[test]
    fn args() {
        let args = parse_args(
            r#"a = "s", b = r"raw", c = true, d = 0x1F, e = ::a::B<T>::C,"#
                .parse()
                .unwrap(),
        )
        .ok()
        .unwrap();
        assert_eq!(args.len(), 5);
        assert_eq!(str_value(&args[0].lit_str().unwrap()).unwrap(), "s");
        assert_eq!(str_value(&args[1].lit_str().unwrap()).unwrap(), "raw");
        assert!(args[2].lit_bool().is_some());
        assert!(args[3].lit_int().is_some());
        assert!(args[4].path().is_some());
        assert!(args[0].lit_bool().is_none());
        assert!(args[2].lit_str().is_none());
        assert!(args[4].lit_int().is_none());

        let args = parse_args("a = 1.0, b = 1 + 2, c = \"x\"".parse().unwrap())
            .ok()
            .unwrap();
        assert!(args[0].lit_int().is_none());
        assert!(args[1].lit_int().is_none());
        assert!(args[1].path().is_none());
        assert!(args[2].path().is_none());

        assert!(parse_args("a".parse().unwrap()).is_err());
        assert!(parse_args("".parse().unwrap()).ok().unwrap().is_empty());
        assert_eq!(str_value(&Literal::string("a\"b")), Some("a\\\"b".into()));
        assert_eq!(
            str_value(&"r#\"a\"b\"#".parse().unwrap()),
            Some("a\"b".into())
        );
    }
}
