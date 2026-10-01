use super::tree::Tree;
use super::{IntoPattern, Resource, ResourceDef, ResourcePath};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Id of a matched resource.
///
/// It is the id set with [`ResourceDef::set_id()`], resources without an
/// explicitly set id all have id `0`.
pub struct ResourceId(u16);

impl ResourceId {
    /// Numeric value of the id.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Resource router.
///
/// Maps paths to values of type `T`. Each resource can also have an optional
/// value of type `U` used by the `recognize_*checked` methods. Resources are
/// matched in registration order, the first matching resource wins.
#[derive(Debug, Clone)]
pub struct Router<T, U = ()> {
    tree: Tree,
    resources: Vec<RouterEntry<T, U>>,
    insensitive: bool,
}

impl<T, U> Router<T, U> {
    /// Creates a router builder.
    pub fn builder() -> RouterBuilder<T, U> {
        RouterBuilder {
            resources: Vec::new(),
            insensitive: false,
        }
    }

    /// Finds the first resource that matches the path.
    ///
    /// On a match, the values of dynamic segments are stored in `resource`,
    /// and a prefix match skips the matched part of the path.
    pub fn recognize<R, P>(&self, resource: &mut R) -> Option<(&T, ResourceId)>
    where
        R: Resource<P>,
        P: ResourcePath,
    {
        if let Some(idx) = if self.insensitive {
            self.tree.find_insensitive(resource)
        } else {
            self.tree.find(resource)
        } {
            let item = &self.resources[idx];
            Some((&item.value, ResourceId(item.rdef.id())))
        } else {
            None
        }
    }

    /// Same as [`recognize()`](Self::recognize), returns a mutable reference.
    pub fn recognize_mut<R, P>(&mut self, resource: &mut R) -> Option<(&mut T, ResourceId)>
    where
        R: Resource<P>,
        P: ResourcePath,
    {
        if let Some(idx) = if self.insensitive {
            self.tree.find_insensitive(resource)
        } else {
            self.tree.find(resource)
        } {
            let item = &mut self.resources[idx];
            Some((&mut item.value, ResourceId(item.rdef.id())))
        } else {
            None
        }
    }

    /// Finds the first resource that matches the path and passes `check`.
    ///
    /// `check` is called for each matching resource with the resource and the
    /// resource's optional value of type `U`.
    pub fn recognize_checked<R, P, F>(&self, resource: &mut R, check: F) -> Option<(&T, ResourceId)>
    where
        F: Fn(&R, Option<&U>) -> bool,
        R: Resource<P>,
        P: ResourcePath,
    {
        if let Some(idx) = if self.insensitive {
            self.tree.find_checked_insensitive(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.check.as_ref())
            })
        } else {
            self.tree.find_checked(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.check.as_ref())
            })
        } {
            let item = &self.resources[idx];
            Some((&item.value, ResourceId(item.rdef.id())))
        } else {
            None
        }
    }

    /// Same as [`recognize_checked()`](Self::recognize_checked), returns a
    /// mutable reference.
    pub fn recognize_checked_mut<R, P, F>(
        &mut self,
        resource: &mut R,
        check: F,
    ) -> Option<(&mut T, ResourceId)>
    where
        F: Fn(&R, Option<&U>) -> bool,
        R: Resource<P>,
        P: ResourcePath,
    {
        if let Some(idx) = if self.insensitive {
            self.tree.find_checked_insensitive(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.check.as_ref())
            })
        } else {
            self.tree.find_checked(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.check.as_ref())
            })
        } {
            let item = &mut self.resources[idx];
            Some((&mut item.value, ResourceId(item.rdef.id())))
        } else {
            None
        }
    }
}

/// Registered resource, see [`RouterBuilder`].
///
/// Holds the resource definition, the value and the optional value passed to
/// the `check` function of the `recognize_*checked` methods.
#[derive(Debug, Clone)]
pub struct RouterEntry<T, U = ()> {
    rdef: ResourceDef,
    value: T,
    check: Option<U>,
}

impl<T, U> RouterEntry<T, U> {
    /// Resource definition
    pub fn resource(&self) -> &ResourceDef {
        &self.rdef
    }

    /// Mutable reference to the resource definition
    pub fn resource_mut(&mut self) -> &mut ResourceDef {
        &mut self.rdef
    }

    /// Value returned for matched resource
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Mutable reference to the value returned for matched resource
    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Value passed to the `check` function of the `recognize_*checked`
    /// methods
    pub fn check_value(&self) -> Option<&U> {
        self.check.as_ref()
    }

    /// Set value passed to the `check` function of the `recognize_*checked`
    /// methods
    pub fn set_check_value<V: Into<Option<U>>>(&mut self, value: V) -> &mut Self {
        self.check = value.into();
        self
    }

    /// Set resource id, see [`ResourceDef::set_id()`]
    pub fn set_id(&mut self, id: u16) -> &mut Self {
        self.rdef.set_id(id);
        self
    }

    /// Set resource name, see [`ResourceDef::set_name()`]
    pub fn set_name<N: Into<String>>(&mut self, name: N) -> &mut Self {
        self.rdef.set_name(name);
        self
    }
}

#[derive(Debug)]
/// Router builder, see [`Router::builder()`].
///
/// The registration methods return the registered [`RouterEntry`], it can be
/// used to set the resource id or name, or the value used by the
/// `recognize_*checked` methods.
pub struct RouterBuilder<T, U = ()> {
    insensitive: bool,
    resources: Vec<RouterEntry<T, U>>,
}

impl<T, U> RouterBuilder<T, U> {
    /// Make router case insensitive. Only static segments
    /// could be case insensitive.
    ///
    /// By default router is case sensitive.
    pub fn case_insensitive(&mut self) {
        self.insensitive = true;
    }

    /// Register resource for specified path patterns, see
    /// [`ResourceDef::new()`].
    pub fn path<P: IntoPattern>(&mut self, path: P, resource: T) -> &mut RouterEntry<T, U> {
        self.resource(ResourceDef::new(path), resource)
    }

    /// Register resource for specified path prefix patterns, see
    /// [`ResourceDef::prefix()`].
    pub fn prefix<P: IntoPattern>(&mut self, prefix: P, resource: T) -> &mut RouterEntry<T, U> {
        self.resource(ResourceDef::prefix(prefix), resource)
    }

    /// Register resource for `ResourceDef`
    pub fn resource(&mut self, rdef: ResourceDef, resource: T) -> &mut RouterEntry<T, U> {
        self.resources.push(RouterEntry {
            rdef,
            value: resource,
            check: None,
        });
        self.resources.last_mut().unwrap()
    }

    /// Finish configuration and create router instance.
    pub fn build(self) -> Router<T, U> {
        let tree = if self.resources.is_empty() {
            Tree::default()
        } else {
            let mut tree = Tree::new(&self.resources[0].rdef, 0);
            for (idx, r) in self.resources[1..].iter().enumerate() {
                tree.insert(&r.rdef, idx + 1);
            }
            tree
        };

        Router {
            tree,
            resources: self.resources,
            insensitive: self.insensitive,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::path::Path;
    use crate::router::{ResourceId, Router};

    #[test]
    fn test_recognizer_1() {
        let mut router = Router::<usize>::builder();
        router.path("/name", 10).set_id(0);
        router.path("/name/{val}", 11).set_id(1);
        router.path("/name/{val}/index.html", 12).set_id(2);
        router.path("/file/{file}.{ext}", 13).set_id(3);
        router.path("/v{val}/{val2}/index.html", 14).set_id(4);
        router.path("/v/{tail}*", 15).set_id(5);
        router.path("/test2/{test}.html", 16).set_id(6);
        router.path("/{test}/index.html", 17).set_id(7);
        router.path("/v2/{custom:.*}/test.html", 18).set_id(8);
        let mut router = router.build();

        let mut path = Path::new("/unknown");
        assert!(router.recognize_mut(&mut path).is_none());

        let mut path = Path::new("/unknown");
        assert!(router.recognize(&mut path).is_none());

        let mut path = Path::new("/name");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);
        assert_eq!(info, ResourceId(0));
        assert!(path.is_empty());

        let mut path = Path::new("/name");
        let (h, info) = router.recognize(&mut path).unwrap();
        assert_eq!(*h, 10);
        assert_eq!(info, ResourceId(0));
        assert!(path.is_empty());

        let mut path = Path::new("/name/value");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 11);
        assert_eq!(info, ResourceId(1));
        assert_eq!(path.get("val").unwrap(), "value");
        assert_eq!(&path["val"], "value");

        let mut path = Path::new("/name/value2/index.html");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 12);
        assert_eq!(info, ResourceId(2));
        assert_eq!(path.get("val").unwrap(), "value2");

        let mut path = Path::new("/file/file.gz");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 13);
        assert_eq!(info, ResourceId(3));
        assert_eq!(path.get("file").unwrap(), "file");
        assert_eq!(path.get("ext").unwrap(), "gz");

        let mut path = Path::new("/vtest/ttt/index.html");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 14);
        assert_eq!(info, ResourceId(4));
        assert_eq!(path.get("val").unwrap(), "test");
        assert_eq!(path.get("val2").unwrap(), "ttt");

        let mut path = Path::new("/v/blah-blah/index.html");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 15);
        assert_eq!(info, ResourceId(5));
        assert_eq!(path.get("tail").unwrap(), "blah-blah/index.html");

        let mut path = Path::new("/test2/index.html");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 16);
        assert_eq!(info, ResourceId(6));
        assert_eq!(path.get("test").unwrap(), "index");

        let mut path = Path::new("/bbb/index.html");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 17);
        assert_eq!(info, ResourceId(7));
        assert_eq!(path.get("test").unwrap(), "bbb");

        let mut path = Path::new("/v2/blah-blah/test.html");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 18);
        assert_eq!(info, ResourceId(8));
        assert_eq!(path.get("custom").unwrap(), "blah-blah");
    }

    #[test]
    fn test_recognizer_2() {
        let mut router = Router::<usize>::builder();
        router.path("/index.json", 10);
        router.path("/{source}.json", 11);
        let mut router = router.build();

        let mut path = Path::new("/index.json");
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);

        let mut path = Path::new("/test.json");
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 11);
    }

    #[test]
    fn test_recognizer_3() {
        let mut router = Router::<usize>::builder();
        router.path("/index.json", 10);
        router.path("/{source}.json", 11);
        router.case_insensitive();
        let mut router = router.build();

        let mut path = Path::new("/index.json");
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);

        let mut path = Path::new("/indeX.json");
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);

        let mut path = Path::new("/test.jsoN");
        assert!(router.recognize_mut(&mut path).is_none());
    }

    #[test]
    fn test_recognizer_long_path() {
        let mut router = Router::<usize>::builder();
        router.path("/{name}/{id}/{tail}*", 10);
        let router = router.build();

        let name = "a".repeat(u16::MAX as usize + 10);
        let mut path = Path::new(format!("/{name}/1/test/tail"));
        assert_eq!(router.recognize(&mut path), Some((&10, ResourceId(0))));
        assert_eq!(path.get("name"), Some(name.as_str()));
        assert_eq!(path.get("id"), Some("1"));
        assert_eq!(path.get("tail"), Some("test/tail"));

        let mut router = Router::<usize>::builder();
        router.prefix("/prefix", 10);
        let router = router.build();

        let mut path = Path::new(format!("/prefix/{name}"));
        path.skip(u32::from(u16::MAX) + 1);
        assert_eq!(router.recognize(&mut path), None);

        let mut path = Path::new(format!("/{name}/prefix/test"));
        path.skip(name.len() as u32 + 1);
        assert_eq!(router.recognize(&mut path), Some((&10, ResourceId(0))));
        assert_eq!(path.path(), "/test");
    }

    #[test]
    fn test_recognizer_with_path_skip() {
        let mut router = Router::<usize>::builder();
        router.path("/name", 10).set_id(0);
        router.path("/name/{val}", 11).set_id(1);
        let mut router = router.build();

        let mut path = Path::new("/name");
        path.skip(5);
        assert!(router.recognize_mut(&mut path).is_none());

        let mut path = Path::new("/test/name");
        path.skip(5);
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);

        let mut path = Path::new("/test/name/value");
        path.skip(5);
        let (h, id) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 11);
        assert_eq!(id, ResourceId(1));
        assert_eq!(path.get("val").unwrap(), "value");
        assert_eq!(&path["val"], "value");

        // same patterns
        let mut router = Router::<usize>::builder();
        router.path("/name", 10);
        router.path("/name/{val}", 11);
        let mut router = router.build();

        let mut path = Path::new("/name");
        path.skip(5);
        assert!(router.recognize_mut(&mut path).is_none());

        let mut path = Path::new("/test2/name");
        path.skip(6);
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);

        let mut path = Path::new("/test2/name-test");
        path.skip(6);
        assert!(router.recognize_mut(&mut path).is_none());

        let mut path = Path::new("/test2/name/ttt");
        path.skip(6);
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 11);
        assert_eq!(&path["val"], "ttt");
    }

    #[test]
    fn test_recognizer_checked() {
        let mut router = Router::<usize, usize>::builder();
        router.path("/name", 10).set_check_value(0);
        router.path("/name", 11).set_check_value(1);
        router.path("/name", 12).set_check_value(2);
        let mut router = router.build();

        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked(&mut p, |_, v| v == Some(&0))
                .unwrap()
                .0,
            10
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked(&mut p, |_, v| v == Some(&1))
                .unwrap()
                .0,
            11
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked(&mut p, |_, v| v == Some(&2))
                .unwrap()
                .0,
            12
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked_mut(&mut p, |_, v| v == Some(&0))
                .unwrap()
                .0,
            10
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked_mut(&mut p, |_, v| v == Some(&1))
                .unwrap()
                .0,
            11
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked_mut(&mut p, |_, v| v == Some(&2))
                .unwrap()
                .0,
            12
        );
    }

    #[test]
    fn test_recognizer_checked_insensitive() {
        let mut router = Router::<usize, usize>::builder();
        router.case_insensitive();
        router.path("/name", 10).set_check_value(0);
        router.path("/name", 11).set_check_value(1);
        router.path("/name", 12).set_check_value(2);
        let mut router = router.build();

        let mut p = Path::new("/Name");
        assert_eq!(
            *router
                .recognize_checked(&mut p, |_, v| v == Some(&0))
                .unwrap()
                .0,
            10
        );
        let mut p = Path::new("/Name");
        assert_eq!(
            *router
                .recognize_checked(&mut p, |_, v| v == Some(&1))
                .unwrap()
                .0,
            11
        );
        let mut p = Path::new("/Name");
        assert_eq!(
            *router
                .recognize_checked_mut(&mut p, |_, v| v == Some(&0))
                .unwrap()
                .0,
            10
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_checked_mut(&mut p, |_, v| v == Some(&1))
                .unwrap()
                .0,
            11
        );
    }

    #[test]
    fn test_prefix_exact_match_tail() {
        for (prefix, p, tail) in [
            ("/app", "/app", ""),
            ("/app", "/app/", "/"),
            ("/app", "/app/test", "/test"),
            ("/{name}", "/x", ""),
            ("/a/{name}", "/a/x", ""),
        ] {
            let mut router = Router::<usize>::builder();
            router.prefix(prefix, 1);
            let router = router.build();

            let mut path = Path::new(p);
            assert!(router.recognize(&mut path).is_some());
            assert_eq!(path.path(), tail);
            assert_eq!(path.path(), tail);
            assert_eq!(path.get("tail"), Some(tail));
            assert_eq!(&path["tail"], tail);
        }

        let mut path = Path::new("/app");
        path.skip(10);
        assert_eq!(path.path(), "");
        assert_eq!(path.get("tail"), Some(""));
    }
}
