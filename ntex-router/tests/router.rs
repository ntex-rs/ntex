use std::cell::Cell;

use ntex_router::{Path, ResourceDef, ResourcePath, Router};

fn find(router: &Router<usize>, p: &str) -> Option<usize> {
    router.recognize(&mut Path::new(p)).map(|(v, _)| *v)
}

/// Checked match with a check that rejects every resource, returns number of checks
fn rejected(router: &Router<usize>, p: &str) -> usize {
    let calls = Cell::new(0);
    let res = router.recognize_checked(&mut Path::new(p), |_, _| {
        calls.set(calls.get() + 1);
        false
    });
    assert!(res.is_none(), "{p}");
    calls.get()
}

/// STATEMENT: An empty `Router` recognizes no paths and never invokes a resource check.
///
/// HOW:
/// - Build a `Router` without resources and confirm `/` and the empty path return `None`.
/// - Reject `/a` with `recognize_checked` and confirm the check is called zero times.
#[test]
fn empty_router() {
    let router = Router::<usize>::builder().build();
    assert_eq!(find(&router, "/"), None);
    assert_eq!(find(&router, ""), None);
    assert_eq!(rejected(&router, "/a"), 0);
}

/// STATEMENT: Named resources retain their names, assigned IDs, and captured parameters after
/// routing.
///
/// HOW:
/// - Name a `/user/{id}` `ResourceDef` as `user`, register it, and assign ID `7`.
/// - Check the name and ID through the builder entry, then recognize `/user/10`.
/// - Verify value `1`, the `ResourceId(7)` debug representation and numeric ID, and `id` equal
///   to `10`.
#[test]
fn rdef_and_names() {
    let mut rdef = ResourceDef::new("/user/{id}");
    rdef.set_name("user");
    assert_eq!(rdef.name(), "user");

    let mut builder = Router::<usize>::builder();
    let entry = builder.resource(rdef, 1);
    entry.set_id(7);
    assert_eq!(entry.resource().name(), "user");
    assert_eq!(entry.resource().id(), 7);
    let router = builder.build();

    let mut path = Path::new("/user/10");
    let (v, id) = router.recognize(&mut path).unwrap();
    assert_eq!(*v, 1);
    assert_eq!(format!("{id:?}"), "ResourceId(7)");
    assert_eq!(id.get(), 7);
    assert_eq!(&path["id"], "10");
}

/// STATEMENT: Resource definitions expose their assigned names and distinguish exact paths from
/// prefixes.
///
/// HOW:
/// - Name a `/user/{id}` definition as `user` and verify `name()` returns it.
/// - Confirm `new` produces a non-prefix definition while `prefix` and `root_prefix` produce
///   prefixes.
#[test]
fn resource_def_accessors() {
    let mut rdef = ResourceDef::new("/user/{id}");
    rdef.set_name("user");
    assert_eq!(rdef.name(), "user");
    assert!(!rdef.is_prefix());
    assert!(ResourceDef::prefix("/user").is_prefix());
    assert!(ResourceDef::root_prefix("user").is_prefix());
}

/// STATEMENT: Builder entry mutations determine the resource metadata and value used by the
/// built router.
///
/// HOW:
/// - Register `/a`, verify its initial value and absent check value, then mutate the value to
///   `2`.
/// - Set the name, ID, and check value, overwrite the name through `resource_mut`, and check
///   the results.
/// - Clear the check value and recognize `/a` to verify value `2` and ID `3`.
#[test]
fn router_entry() {
    let mut builder = Router::<usize, &str>::builder();
    let entry = builder.path("/a", 1);
    assert_eq!(*entry.value(), 1);
    assert_eq!(entry.check_value(), None);
    *entry.value_mut() = 2;
    entry
        .set_name("a")
        .set_id(3)
        .set_check_value("check")
        .resource_mut()
        .set_name("b");
    assert_eq!(entry.resource().name(), "b");
    assert_eq!(entry.check_value(), Some(&"check"));
    entry.set_check_value(None);
    assert_eq!(entry.check_value(), None);
    let router = builder.build();

    let (v, id) = router.recognize(&mut Path::new("/a")).unwrap();
    assert_eq!((*v, id.get()), (2, 3));
}

/// STATEMENT: Captured parameters iterate in pattern order without changing the original owned
/// path.
///
/// HOW:
/// - Recognize the owned path `/x/y` against `/{a}/{b}`.
/// - Iterate over `&Path` and verify the pairs are `("a", "x")` followed by `("b", "y")`.
/// - Consume the `Path` with `into_inner` and confirm it returns `/x/y`.
#[test]
fn path_into_inner_and_iter() {
    let mut builder = Router::<usize>::builder();
    builder.path("/{a}/{b}", 1);
    let router = builder.build();

    let mut path = Path::new("/x/y".to_string());
    router.recognize(&mut path).unwrap();
    let mut items = Vec::new();
    for (k, v) in &path {
        items.push((k, v));
    }
    assert_eq!(items, [("a", "x"), ("b", "y")]);
    assert_eq!(path.into_inner(), "/x/y");
}

/// STATEMENT: String and collection conversions preserve resource patterns and definition
/// equality.
///
/// HOW:
/// - Convert a borrowed `String`, a vector, and an array into `ResourceDef` and compare their
///   patterns.
/// - Convert an array of owned strings and confirm it equals `ResourceDef::new("/a")`.
#[test]
fn resource_def_from() {
    let path = "/a".to_string();
    assert_eq!(ResourceDef::from(&path).patterns(), ["/a"]);
    assert_eq!(ResourceDef::from(vec!["/a", "/b"]).patterns(), ["/a", "/b"]);
    assert_eq!(ResourceDef::from(["/a", "/b"]).patterns(), ["/a", "/b"]);
    assert_eq!(
        ResourceDef::from(["/a".to_string()]),
        ResourceDef::new("/a")
    );
}

/// STATEMENT: A builder prefix entry supports multiple patterns and leaves the unmatched suffix
/// available.
///
/// HOW:
/// - Register `[/a, /b]` with `prefix` and verify the entry is a prefix with both patterns.
/// - Recognize `/b/rest` and confirm value `1` and the remaining path `/rest`.
#[test]
fn builder_prefix_patterns() {
    let mut builder = Router::<usize>::builder();
    let entry = builder.prefix(["/a", "/b"], 1);
    assert!(entry.resource().is_prefix());
    assert_eq!(entry.resource().patterns(), ["/a", "/b"]);
    let router = builder.build();

    let mut path = Path::new("/b/rest");
    assert_eq!(*router.recognize(&mut path).unwrap().0, 1);
    assert_eq!(path.path(), "/rest");
}

/// STATEMENT: Checked recognition can reject every match across exact, dynamic, tail, and
/// prefix resources.
///
/// HOW:
/// - Register overlapping static, dynamic, regex, tail, prefix, and multi-pattern resources.
/// - For each listed matching path, use an always-false check and confirm no resource is
///   returned and at least one check runs.
/// - Recognize each path without a check and confirm a resource is found.
#[test]
fn checked_rejects_all() {
    let mut builder = Router::<usize>::builder();
    builder.path("/", 0);
    builder.path("", 1);
    builder.path("/name", 2);
    builder.path("/name/", 3);
    builder.path("/name/{val}", 4);
    builder.path("/tail/{tail}*", 5);
    builder.path("/static/*", 6);
    builder.prefix("/pre", 7);
    builder.prefix("/pre2/", 8);
    builder.path(["/multi/a", "/multi/b/"], 10);
    builder.resource(ResourceDef::prefix(["/mpre/a", "/mpre/b/"]), 11);
    builder.path("/re/{id:[0-9]+}", 12);
    builder.path("/{a}/{b}", 13);
    builder.prefix("", 9);
    let router = builder.build();

    for p in [
        "/",
        "",
        "/name",
        "/name/",
        "/name/1",
        "/tail/a/b",
        "/static/a/b",
        "/pre",
        "/pre/",
        "/pre/a",
        "/pre2/",
        "/pre2/a",
        "/multi/a",
        "/multi/b/",
        "/mpre/a/x",
        "/mpre/b/x",
        "/re/10",
        "/x/y",
    ] {
        assert!(rejected(&router, p) > 0, "{p}");
        assert!(find(&router, p).is_some(), "{p}");
    }
}

/// STATEMENT: Checks select duplicate resources before captures are stored and allow mutation
/// of the selected value.
///
/// HOW:
/// - Register three `/user/{id}` resources with different optional check values and select the
///   `user` entry.
/// - Verify value `2` and captured `id` equal to `1`, then mutate the unchecked entry's value
///   to `30`.
/// - Select that entry only when the check sees no captured `id`; verify value `30` and the
///   subsequent capture.
/// - Confirm an always-false mutable check returns `None`.
#[test]
fn checked_selects_resource() {
    let mut builder = Router::<usize, &str>::builder();
    builder.path("/user/{id}", 1).set_check_value("admin");
    builder.path("/user/{id}", 2).set_check_value(Some("user"));
    builder.path("/user/{id}", 3);
    let mut router = builder.build();

    let mut path = Path::new("/user/1");
    let (v, _) = router
        .recognize_checked(&mut path, |_, u| u == Some(&"user"))
        .unwrap();
    assert_eq!(*v, 2);
    assert_eq!(&path["id"], "1");

    let mut path = Path::new("/user/1");
    let (v, _) = router
        .recognize_checked_mut(&mut path, |_, u| u.is_none())
        .unwrap();
    *v = 30;
    // matched segments are stored after a successful check
    let mut path = Path::new("/user/1");
    let (v, _) = router
        .recognize_checked(&mut path, |res, u| u.is_none() && res.get("id").is_none())
        .unwrap();
    assert_eq!(*v, 30);
    assert_eq!(&path["id"], "1");

    let mut path = Path::new("/user/1");
    assert!(
        router
            .recognize_checked_mut(&mut path, |_, _| false)
            .is_none()
    );
}

/// STATEMENT: Case-insensitive routing preserves checked selection across duplicate, prefix,
/// and tail resources.
///
/// HOW:
/// - Register mixed-case duplicate `/Name/{val}` routes, a `/Pre` prefix, and a `/Tail/{tail}*`
///   route.
/// - Accept the first duplicate for `/name/x`, then accept the second check for `/NAME/x` and
///   verify value `2` and `val` equal to `x`.
/// - Confirm rejecting checks run for lowercase prefix and tail paths, while uppercase paths
///   recognize values `3` and `4`.
#[test]
fn checked_insensitive() {
    let mut builder = Router::<usize>::builder();
    builder.case_insensitive();
    builder.path("/Name/{val}", 1);
    builder.path("/Name/{val}", 2);
    builder.prefix("/Pre", 3);
    builder.path("/Tail/{tail}*", 4);
    let mut router = builder.build();

    let mut path = Path::new("/name/x");
    let (v, _) = router.recognize_checked(&mut path, |_, _| true).unwrap();
    assert_eq!(*v, 1);

    let calls = Cell::new(0);
    let mut path = Path::new("/NAME/x");
    let (v, _) = router
        .recognize_checked_mut(&mut path, |_, _| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        })
        .unwrap();
    assert_eq!(*v, 2);
    assert_eq!(&path["val"], "x");

    assert!(rejected(&router, "/pre/a") > 0);
    assert!(rejected(&router, "/tail/a/b") > 0);
    assert_eq!(find(&router, "/TAIL/a/b"), Some(4));
    assert_eq!(find(&router, "/PRE"), Some(3));
}

/// STATEMENT: Checked tail routes can fall through duplicate resources and capture both named
/// segments and tails.
///
/// HOW:
/// - Register duplicate `/v/{tail}*` routes and `/w/{name}/{tail}*`.
/// - Reject the first match for `/v/a/b` and verify value `2` with `tail` equal to `a/b`.
/// - Accept `/w/n/a/b` and verify value `3`, `name` equal to `n`, and `tail` equal to `a/b`.
#[test]
fn checked_tail() {
    let mut builder = Router::<usize>::builder();
    builder.path("/v/{tail}*", 1);
    builder.path("/v/{tail}*", 2);
    builder.path("/w/{name}/{tail}*", 3);
    let router = builder.build();

    let calls = Cell::new(0);
    let mut path = Path::new("/v/a/b");
    let (v, _) = router
        .recognize_checked(&mut path, |_, _| {
            calls.set(calls.get() + 1);
            calls.get() > 1
        })
        .unwrap();
    assert_eq!(*v, 2);
    assert_eq!(&path["tail"], "a/b");

    let mut path = Path::new("/w/n/a/b");
    let (v, _) = router.recognize_checked(&mut path, |_, _| true).unwrap();
    assert_eq!(*v, 3);
    assert_eq!(&path["name"], "n");
    assert_eq!(&path["tail"], "a/b");
}

/// STATEMENT: Multi-pattern prefixes match segment boundaries and preserve each pattern's
/// unmatched suffix.
///
/// HOW:
/// - Register a single prefix resource with `/a`, `/b/`, and `/c/{id}` patterns.
/// - Match each listed exact or extended path and verify value `1` and its expected remaining
///   path.
/// - Confirm `/ax` and `/d` do not match.
#[test]
fn multi_pattern_prefix() {
    let mut builder = Router::<usize>::builder();
    builder.resource(ResourceDef::prefix(["/a", "/b/", "/c/{id}"]), 1);
    let router = builder.build();

    for (p, rest) in [
        ("/a", ""),
        ("/a/x", "/x"),
        ("/b/", "/"),
        ("/b/x", "/x"),
        ("/c/1", ""),
        ("/c/1/x", "/x"),
    ] {
        let mut path = Path::new(p);
        assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1), "{p}");
        assert_eq!(path.path(), rest, "{p}");
    }
    assert_eq!(find(&router, "/ax"), None);
    assert_eq!(find(&router, "/d"), None);
}

/// STATEMENT: Owned strings, borrowed strings, and byte strings support routing and parameter
/// extraction.
///
/// HOW:
/// - Recognize `/user/1`, `/user/2`, and `/user/3` against `/user/{id}` using `String`,
///   `&String`, and `ByteString` sources.
/// - Verify value `1` and the corresponding `id` for each source.
/// - For the byte string, also confirm parameter iteration and numeric indexing return `3`.
#[test]
fn string_sources() {
    let mut builder = Router::<usize>::builder();
    builder.path("/user/{id}", 1);
    let router = builder.build();

    let mut path = Path::new(String::from("/user/1"));
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(&path["id"], "1");

    let s = String::from("/user/2");
    let mut path = Path::new(&s);
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(&path["id"], "2");

    let mut path = Path::new(ntex_bytes::ByteString::from_static("/user/3"));
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(&path["id"], "3");
    assert_eq!(path.iter().collect::<Vec<_>>(), vec![("id", "3")]);
    assert_eq!(&path[0], "3");
}

/// STATEMENT: URL-backed paths expose decoded and plain captures consistently through all
/// parameter accessors.
///
/// HOW:
/// - Recognize `urly::Url` paths `/files/a%20b` and `/files/plain` against `/files/{name}`.
/// - Verify value `1` and captures `a b` and `plain` through named indexing, numeric indexing,
///   and iteration.
#[test]
fn url_source() {
    let mut builder = Router::<usize>::builder();
    builder.path("/files/{name}", 1);
    let router = builder.build();

    let uri: urly::Url = "/files/a%20b".parse().unwrap();
    let mut path = Path::new(uri);
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(&path["name"], "a b");
    assert_eq!(&path[0], "a b");
    assert_eq!(path.iter().next(), Some(("name", "a b")));

    let uri: urly::Url = "/files/plain".parse().unwrap();
    let mut path = Path::new(uri);
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(&path["name"], "plain");
    assert_eq!(&path[0], "plain");
    assert_eq!(path.iter().next(), Some(("name", "plain")));
}

/// STATEMENT: Empty prefixes match only empty or root paths, while root prefixes preserve
/// slash-led paths.
///
/// HOW:
/// - Register an empty prefix and verify matches for the empty path and `/`, misses for `/a`
///   and `/a/b`, and one rejecting check for each match.
/// - Register `/` as a prefix and confirm the empty path does not match.
/// - Recognize `/`, `/a`, and `/a/b`, verifying value `1` and an unchanged remaining path.
#[test]
fn empty_and_root_prefix() {
    let mut builder = Router::<usize>::builder();
    builder.prefix("", 1);
    let router = builder.build();
    assert_eq!(find(&router, ""), Some(1));
    assert_eq!(find(&router, "/"), Some(1));
    assert_eq!(find(&router, "/a"), None);
    assert_eq!(find(&router, "/a/b"), None);
    assert_eq!(rejected(&router, ""), 1);
    assert_eq!(rejected(&router, "/"), 1);

    let mut builder = Router::<usize>::builder();
    builder.prefix("/", 1);
    let router = builder.build();
    assert_eq!(find(&router, ""), None);
    for p in ["/", "/a", "/a/b"] {
        let mut path = Path::new(p);
        assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1), "{p}");
        assert_eq!(path.path(), p);
    }
}

/// STATEMENT: Overlapping slash prefixes and tail routes are checked in order, including an
/// empty tail.
///
/// HOW:
/// - Register `/p/` as a prefix, an exact path, and another prefix around `/p/{tail}*`.
/// - Verify unchecked recognition selects the first prefix for `/p/` and `/p/a`.
/// - Reject all matches and confirm four checks for `/p/` and three for `/p/a`.
/// - Skip one or two checks and verify the first accepted exact, tail, or later prefix resource
///   wins.
#[test]
fn checked_prefix_with_slash() {
    let mut builder = Router::<usize>::builder();
    builder.prefix("/p/", 1);
    builder.path("/p/", 2);
    builder.path("/p/{tail}*", 3);
    builder.prefix("/p/", 4);
    let router = builder.build();

    assert_eq!(find(&router, "/p/"), Some(1));
    assert_eq!(find(&router, "/p/a"), Some(1));
    // `{tail}*` matches an empty tail
    assert_eq!(rejected(&router, "/p/"), 4);
    assert_eq!(rejected(&router, "/p/a"), 3);

    // the first resource that passes the check wins
    for (p, skip, expected) in [("/p/", 1, 2), ("/p/a", 1, 3), ("/p/a", 2, 4)] {
        let calls = Cell::new(0);
        let res = router.recognize_checked(&mut Path::new(p), |_, _| {
            calls.set(calls.get() + 1);
            calls.get() > skip
        });
        assert_eq!(res.map(|v| *v.0), Some(expected), "{p}");
    }
}

/// STATEMENT: Each matching tail resource is checked once regardless of the number of trailing
/// segments.
///
/// HOW:
/// - Register `/p/{tail}*` and `/{tail}*` and reject paths with empty, single-segment, multi-
///   segment, and slash-ended tails.
/// - Verify two checks for each `/p/` path, unchecked selection of value `1`, and a capture
///   equal to the suffix after `/p/`.
/// - Reject `/a` and confirm only one tail resource is checked.
#[test]
fn tail_checked_once() {
    let mut builder = Router::<usize>::builder();
    builder.path("/p/{tail}*", 1);
    builder.path("/{tail}*", 2);
    let router = builder.build();

    for p in ["/p/", "/p/a", "/p/a/b", "/p/a/"] {
        assert_eq!(rejected(&router, p), 2, "{p}");
        let mut path = Path::new(p);
        assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1), "{p}");
        assert_eq!(path.get("tail"), Some(&p[3..]), "{p}");
    }
    assert_eq!(rejected(&router, "/a"), 1);
}

/// Recognizes `p` while rejecting every resource whose check value is `true`
fn accepted(router: &Router<usize, bool>, p: &str) -> Option<usize> {
    router
        .recognize_checked(&mut Path::new(p), |_, reject| reject != Some(&true))
        .map(|(v, _)| *v)
}

/// STATEMENT: When the check rejects a root `/` prefix, matching continues with the
/// root's other routes.
///
/// HOW:
/// - Register the prefix `/` as value `1` with a check value that rejects it, then
///   register `/x` as value `2`.
/// - Without a check, confirm `/x` and `/y` both match the prefix.
/// - With the rejecting check, confirm `/x` falls through to `2` and `/y` matches nothing.
#[test]
fn rejected_root_prefix_falls_through() {
    let mut builder = Router::<usize, bool>::builder();
    builder.prefix("/", 1).set_check_value(true);
    builder.path("/x", 2);
    let router = builder.build();

    assert_eq!(
        router.recognize(&mut Path::new("/x")).map(|v| *v.0),
        Some(1)
    );
    assert_eq!(
        router.recognize(&mut Path::new("/y")).map(|v| *v.0),
        Some(1)
    );
    assert_eq!(accepted(&router, "/x"), Some(2));
    assert_eq!(accepted(&router, "/y"), None);
}

/// STATEMENT: For the path `/`, a root path, a later `/` prefix and another root path
/// are tried in registration order, skipping rejected ones.
///
/// HOW:
/// - Register `/` as value `1` and the prefix `/` as value `2`, both rejected by the
///   check, then `/` again as value `3`.
/// - Without a check, confirm `/` matches `1`.
/// - With the rejecting check, confirm `/` matches `3`, and the empty path, which no
///   accepted resource matches, matches nothing.
#[test]
fn rejected_root_resources_tried_in_order() {
    let mut builder = Router::<usize, bool>::builder();
    builder.path("/", 1).set_check_value(true);
    builder.prefix("/", 2).set_check_value(true);
    builder.path("/", 3);
    let router = builder.build();

    assert_eq!(router.recognize(&mut Path::new("/")).map(|v| *v.0), Some(1));
    assert_eq!(accepted(&router, "/"), Some(3));
    assert_eq!(accepted(&router, ""), None);
}

/// STATEMENT: Every pattern of the first registered resource matches, including a later
/// pattern with a trailing slash.
///
/// HOW:
/// - Register only the resource with the patterns `/a` and `/b/` as value `1`.
/// - Confirm `/a` and `/b/` match it, while `/b` and `/a/` don't.
#[test]
fn first_resource_with_slash_pattern() {
    let mut builder = Router::<usize>::builder();
    builder.resource(ResourceDef::new(["/a", "/b/"]), 1);
    let router = builder.build();

    assert_eq!(find(&router, "/a"), Some(1));
    assert_eq!(find(&router, "/b/"), Some(1));
    assert_eq!(find(&router, "/b"), None);
    assert_eq!(find(&router, "/a/"), None);
}

/// A path source that keeps the default `unquote` and `needs_unquote`
struct RawPath(&'static str);

impl ResourcePath for RawPath {
    fn path(&self) -> &str {
        self.0
    }
}

/// STATEMENT: A `ResourcePath` that keeps the default `unquote` matches and captures
/// segments without decoding them.
///
/// HOW:
/// - Register `/files/{name}` as value `1` and `/a%20b` as value `2`.
/// - Recognize `RawPath` sources `/files/a%20b` and `/a%20b`.
/// - Confirm `/files/a%20b` matches `1` with `name` still `a%20b`, and `/a%20b` matches
///   the static route `2` by its encoded form.
#[test]
fn default_resource_path_does_not_decode() {
    let mut builder = Router::<usize>::builder();
    builder.path("/files/{name}", 1);
    builder.path("/a%20b", 2);
    let router = builder.build();

    let mut path = Path::new(RawPath("/files/a%20b"));
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(&path["name"], "a%20b");

    let mut path = Path::new(RawPath("/a%20b"));
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(2));
}

/// STATEMENT: A custom regex that matches without capturing every named parameter
/// does not match the resource.
///
/// HOW:
/// - Register `/{id:a)|(b}` as value `1`, whose regex `^(?P<id>a)|(b)$` matches `b`
///   without the `id` group.
/// - Confirm `/a` matches with `id` equal to `a`.
/// - Confirm `/b` matches nothing and leaves no `id` capture in the path.
/// - Register tail pattern `/{id:a)|(b}-{tail}*`, whose regex matches `b-x` without
///   the `id` group, and confirm `/b-x` matches nothing.
#[test]
fn missing_named_capture_is_not_a_match() {
    let mut builder = Router::<usize>::builder();
    builder.path("/{id:a)|(b}", 1);
    let router = builder.build();

    let mut path = Path::new("/a");
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), Some(1));
    assert_eq!(path.get("id"), Some("a"));

    let mut path = Path::new("/b");
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), None);
    assert_eq!(path.get("id"), None);

    let mut builder = Router::<usize>::builder();
    builder.path("/{id:a)|(b}-{tail}*", 1);
    let router = builder.build();

    let mut path = Path::new("/b-x");
    assert_eq!(router.recognize(&mut path).map(|v| *v.0), None);
}
