use super::tree::Tree;
use super::{IntoPattern, Resource, ResourceDef, ResourcePath};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceId(u16);

/// Resource router.
#[derive(Debug, Clone)]
pub struct Router<T, U = ()> {
    tree: Tree,
    resources: Vec<(ResourceDef, T, Option<U>)>,
    insensitive: bool,
}

impl<T, U> Router<T, U> {
    pub fn build() -> RouterBuilder<T, U> {
        RouterBuilder {
            resources: Vec::new(),
            insensitive: false,
        }
    }

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
            Some((&item.1, ResourceId(item.0.id())))
        } else {
            None
        }
    }

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
            Some((&mut item.1, ResourceId(item.0.id())))
        } else {
            None
        }
    }

    pub fn recognize_checked<R, P, F>(
        &self,
        resource: &mut R,
        check: F,
    ) -> Option<(&T, ResourceId)>
    where
        F: Fn(&R, Option<&U>) -> bool,
        R: Resource<P>,
        P: ResourcePath,
    {
        if let Some(idx) = if self.insensitive {
            self.tree.find_checked_insensitive(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.2.as_ref())
            })
        } else {
            self.tree.find_checked(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.2.as_ref())
            })
        } {
            let item = &self.resources[idx];
            Some((&item.1, ResourceId(item.0.id())))
        } else {
            None
        }
    }

    pub fn recognize_mut_checked<R, P, F>(
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
                check(res, item.2.as_ref())
            })
        } else {
            self.tree.find_checked(resource, &|idx, res| {
                let item = &self.resources[idx];
                check(res, item.2.as_ref())
            })
        } {
            let item = &mut self.resources[idx];
            Some((&mut item.1, ResourceId(item.0.id())))
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct RouterBuilder<T, U = ()> {
    insensitive: bool,
    resources: Vec<(ResourceDef, T, Option<U>)>,
}

impl<T, U> RouterBuilder<T, U> {
    /// Make router case insensitive. Only static segments
    /// could be case insensitive.
    ///
    /// By default router is case sensitive.
    pub fn case_insensitive(&mut self) {
        self.insensitive = true;
    }

    /// Register resource for specified path.
    pub fn path<P: IntoPattern>(
        &mut self,
        path: P,
        resource: T,
    ) -> &mut (ResourceDef, T, Option<U>) {
        self.resources
            .push((ResourceDef::new(path), resource, None));
        self.resources.last_mut().unwrap()
    }

    /// Register resource for specified path prefix.
    pub fn prefix(
        &mut self,
        prefix: &str,
        resource: T,
    ) -> &mut (ResourceDef, T, Option<U>) {
        self.resources
            .push((ResourceDef::prefix(prefix), resource, None));
        self.resources.last_mut().unwrap()
    }

    /// Register resource for ResourceDef
    pub fn rdef(
        &mut self,
        rdef: ResourceDef,
        resource: T,
    ) -> &mut (ResourceDef, T, Option<U>) {
        self.resources.push((rdef, resource, None));
        self.resources.last_mut().unwrap()
    }

    /// Finish configuration and create router instance.
    pub fn finish(self) -> Router<T, U> {
        let tree = if self.resources.is_empty() {
            Tree::default()
        } else {
            let mut tree = Tree::new(&self.resources[0].0, 0);
            for (idx, r) in self.resources[1..].iter().enumerate() {
                tree.insert(&r.0, idx + 1)
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
        let mut router = Router::<usize>::build();
        router.path("/name", 10).0.set_id(0);
        router.path("/name/{val}", 11).0.set_id(1);
        router.path("/name/{val}/index.html", 12).0.set_id(2);
        router.path("/file/{file}.{ext}", 13).0.set_id(3);
        router.path("/v{val}/{val2}/index.html", 14).0.set_id(4);
        router.path("/v/{tail}*", 15).0.set_id(5);
        router.path("/test2/{test}.html", 16).0.set_id(6);
        router.path("/{test}/index.html", 17).0.set_id(7);
        router.path("/v2/{custom:.*}/test.html", 18).0.set_id(8);
        let mut router = router.finish();

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
        let mut router = Router::<usize>::build();
        router.path("/index.json", 10);
        router.path("/{source}.json", 11);
        let mut router = router.finish();

        let mut path = Path::new("/index.json");
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 10);

        let mut path = Path::new("/test.json");
        let (h, _) = router.recognize_mut(&mut path).unwrap();
        assert_eq!(*h, 11);
    }

    #[test]
    fn test_recognizer_3() {
        let mut router = Router::<usize>::build();
        router.path("/index.json", 10);
        router.path("/{source}.json", 11);
        router.case_insensitive();
        let mut router = router.finish();

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
    fn test_recognizer_with_path_skip() {
        let mut router = Router::<usize>::build();
        router.path("/name", 10).0.set_id(0);
        router.path("/name/{val}", 11).0.set_id(1);
        let mut router = router.finish();

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
        let mut router = Router::<usize>::build();
        router.path("/name", 10);
        router.path("/name/{val}", 11);
        let mut router = router.finish();

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
        let mut router = Router::<usize, usize>::build();
        router.path("/name", 10).2 = Some(0);
        router.path("/name", 11).2 = Some(1);
        router.path("/name", 12).2 = Some(2);
        let mut router = router.finish();

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
                .recognize_mut_checked(&mut p, |_, v| v == Some(&0))
                .unwrap()
                .0,
            10
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_mut_checked(&mut p, |_, v| v == Some(&1))
                .unwrap()
                .0,
            11
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_mut_checked(&mut p, |_, v| v == Some(&2))
                .unwrap()
                .0,
            12
        );
    }

    #[test]
    fn test_recognizer_checked_insensitive() {
        let mut router = Router::<usize, usize>::build();
        router.case_insensitive();
        router.path("/name", 10).2 = Some(0);
        router.path("/name", 11).2 = Some(1);
        router.path("/name", 12).2 = Some(2);
        let mut router = router.finish();

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
                .recognize_mut_checked(&mut p, |_, v| v == Some(&0))
                .unwrap()
                .0,
            10
        );
        let mut p = Path::new("/name");
        assert_eq!(
            *router
                .recognize_mut_checked(&mut p, |_, v| v == Some(&1))
                .unwrap()
                .0,
            11
        );
    }
}
