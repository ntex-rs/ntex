use std::cell::Cell;

use ntex_router::{Path, ResourceDef, Router};

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

#[test]
fn empty_router() {
    let router = Router::<usize>::builder().build();
    assert_eq!(find(&router, "/"), None);
    assert_eq!(find(&router, ""), None);
    assert_eq!(rejected(&router, "/a"), 0);
}

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

#[test]
fn resource_def_accessors() {
    let mut rdef = ResourceDef::new("/user/{id}");
    rdef.set_name("user");
    assert_eq!(rdef.name(), "user");
    assert!(!rdef.is_prefix());
    assert!(ResourceDef::prefix("/user").is_prefix());
    assert!(ResourceDef::root_prefix("user").is_prefix());
}

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
