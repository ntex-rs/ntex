use crate::{IntoPattern, Resource, ResourceDef, ResourcePath};

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ResourceId(pub u16);

/// Information about current resource
#[derive(Clone, Debug)]
pub struct ResourceInfo {
    resource: ResourceId,
}

/// Resource router.
pub struct Router<T, U = ()>(Vec<(ResourceDef, T, Option<U>)>);

impl<T, U> Router<T, U> {
    pub fn build() -> RouterBuilder<T, U> {
        RouterBuilder {
            resources: Vec::new(),
        }
    }

    pub fn recognize<R, P>(&self, resource: &mut R) -> Option<(&T, ResourceId)>
    where
        R: Resource<P>,
        P: ResourcePath,
    {
        for item in self.0.iter() {
            if item.0.match_path(resource.resource_path()) {
                return Some((&item.1, ResourceId(item.0.id())));
            }
        }
        None
    }

    pub fn recognize_mut<R, P>(&mut self, resource: &mut R) -> Option<(&mut T, ResourceId)>
    where
        R: Resource<P>,
        P: ResourcePath,
    {
        for item in self.0.iter_mut() {
            if item.0.match_path(resource.resource_path()) {
                return Some((&mut item.1, ResourceId(item.0.id())));
            }
        }
        None
    }

    pub fn recognize_mut_checked<R, P, F>(
        &mut self,
        resource: &mut R,
        check: F,
    ) -> Option<(&mut T, ResourceId)>
    where
        F: Fn(&R, &Option<U>) -> bool,
        R: Resource<P>,
        P: ResourcePath,
    {
        for item in self.0.iter_mut() {
            if item.0.match_path_checked(resource, &check, &item.2) {
                return Some((&mut item.1, ResourceId(item.0.id())));
            }
        }
        None
    }
}

pub struct RouterBuilder<T, U = ()> {
    resources: Vec<(ResourceDef, T, Option<U>)>,
}

impl<T, U> RouterBuilder<T, U> {
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
    pub fn prefix(&mut self, prefix: &str, resource: T) -> &mut (ResourceDef, T, Option<U>) {
        self.resources
            .push((ResourceDef::prefix(prefix), resource, None));
        self.resources.last_mut().unwrap()
    }

    /// Register resource for ResourceDef
    pub fn rdef(&mut self, rdef: ResourceDef, resource: T) -> &mut (ResourceDef, T, Option<U>) {
        self.resources.push((rdef, resource, None));
        self.resources.last_mut().unwrap()
    }

    /// Finish configuration and create router instance.
    pub fn finish(self) -> Router<T, U> {
        Router(self.resources)
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
        router.path("/v/{tail:.*}", 15).0.set_id(5);
        router.path("/test2/{test}.html", 16).0.set_id(6);
        router.path("/{test}/index.html", 17).0.set_id(7);
        let mut router = router.finish();

        let mut path = Path::new("/unknown");
        assert!(router.recognize_mut(&mut path).is_none());

        let mut path = Path::new("/name");
        let (h, info) = router.recognize_mut(&mut path).unwrap();
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
    fn test_recognizer_with_prefix() {
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
        path.skip(6);
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
}
