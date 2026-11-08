use std::{cell::RefCell, rc::Rc};

use urly::{Url, quoting::Component, quoting::quote};

use crate::router::{BuildPathError, ResourceDef};
use crate::util::HashMap;
use crate::web::{error::UrlGenerationError, httprequest::HttpRequest};

/// Map of registered resources, used for URL generation.
///
/// The map is built from the application's resource definitions and is used
/// by [`HttpRequest::url_for()`](crate::web::HttpRequest::url_for) to resolve
/// named and external resources.
#[derive(Clone, Debug)]
pub struct ResourceMap {
    #[allow(dead_code)]
    root: ResourceDef,
    parent: RefCell<Option<Rc<ResourceMap>>>,
    named: HashMap<String, ResourceDef>,
    patterns: Vec<(ResourceDef, Option<Rc<ResourceMap>>)>,
}

impl ResourceMap {
    /// Create an empty resource map for the `root` resource.
    pub fn new(root: ResourceDef) -> Self {
        ResourceMap {
            root,
            parent: RefCell::new(None),
            named: HashMap::default(),
            patterns: Vec::new(),
        }
    }

    /// Add a resource pattern, optionally with the resource map of a nested
    /// scope.
    ///
    /// Assigns the pattern its id within this map. Named patterns can be used
    /// for URL generation.
    ///
    /// # Panics
    ///
    /// Panics if a pattern with the same name is already added to this map.
    pub fn add(&mut self, pattern: &mut ResourceDef, nested: Option<Rc<ResourceMap>>) {
        pattern.set_id(self.patterns.len() as u16);
        if !pattern.name().is_empty() {
            let prev = self
                .named
                .insert(pattern.name().to_string(), pattern.clone());
            assert!(
                prev.is_none(),
                "Resource name {:?} is already used",
                pattern.name()
            );
        }
        self.patterns.push((pattern.clone(), nested));
    }

    pub(crate) fn build(&self, current: &Rc<ResourceMap>) {
        for (_, nested) in &self.patterns {
            if let Some(nested) = nested {
                *nested.parent.borrow_mut() = Some(current.clone());
                nested.build(nested);
            }
        }
    }
}

impl ResourceMap {
    /// Generate url for named resource
    ///
    /// Check [`HttpRequest::url_for()`](crate::web::HttpRequest::url_for) for detailed information.
    pub fn url_for<U, I>(
        &self,
        req: &HttpRequest,
        name: &str,
        elements: U,
    ) -> Result<Url, UrlGenerationError>
    where
        U: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let mut path = String::new();
        let mut elements = elements.into_iter();

        if self.patterns_for(name, &mut path, &mut elements)?.is_some() {
            if elements.next().is_some() {
                return Err(UrlGenerationError::TooManyElements);
            }
            if path.starts_with('/') {
                let conn = req.connection_info();
                let (scheme, host) = (conn.scheme(), conn.host());
                let mut url = String::with_capacity(scheme.len() + host.len() + path.len() + 3);
                url.push_str(scheme);
                url.push_str("://");
                url.push_str(host);
                url.push_str(&path);
                path = url;
            }
            Ok(Url::try_from(path)?)
        } else {
            Err(UrlGenerationError::ResourceNotFound)
        }
    }

    // pub fn has_resource(&self, path: &str) -> bool {
    // let _path = if path.is_empty() { "/" } else { path };

    // for (pattern, rmap) in &self.patterns {
    //     if let Some(ref rmap) = rmap {
    //         if let Some(plen) = pattern.is_prefix_match(path) {
    //             return rmap.has_resource(&path[plen..]);
    //         }
    //     } else if pattern.is_match(path) {
    //         return true;
    //     }
    // }
    // false
    // }

    fn patterns_for<U, I>(
        &self,
        name: &str,
        path: &mut String,
        elements: &mut U,
    ) -> Result<Option<()>, UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if self.pattern_for(name, path, elements)?.is_some() {
            Ok(Some(()))
        } else {
            self.parent_pattern_for(name, path, elements)
        }
    }

    fn pattern_for<U, I>(
        &self,
        name: &str,
        path: &mut String,
        elements: &mut U,
    ) -> Result<Option<()>, UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(pattern) = self.named.get(name) {
            if pattern.pattern().starts_with('/') {
                self.fill_root(path, elements)?;
            }
            build_path(pattern, path, elements)?;
            Ok(Some(()))
        } else {
            for (_, rmap) in &self.patterns {
                if let Some(rmap) = rmap
                    && rmap.pattern_for(name, path, elements)?.is_some()
                {
                    return Ok(Some(()));
                }
            }
            Ok(None)
        }
    }

    fn fill_root<U, I>(&self, path: &mut String, elements: &mut U) -> Result<(), UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(ref parent) = *self.parent.borrow() {
            parent.fill_root(path, elements)?;
        }
        build_path(&self.root, path, elements)
    }

    fn parent_pattern_for<U, I>(
        &self,
        name: &str,
        path: &mut String,
        elements: &mut U,
    ) -> Result<Option<()>, UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(ref parent) = *self.parent.borrow() {
            parent.patterns_for(name, path, elements)
        } else {
            Ok(None)
        }
    }
}

/// Build the path of `pattern`, each element must match its segment pattern
/// and is percent-encoded as one literal URL component.
fn build_path<U, I>(
    pattern: &ResourceDef,
    path: &mut String,
    elements: &mut U,
) -> Result<(), UrlGenerationError>
where
    U: Iterator<Item = I>,
    I: AsRef<str>,
{
    pattern
        .try_build_path(path, elements, |path, el| {
            path.push_str(&quote(el, Component::Opaque));
        })
        .map_err(|err| match err {
            BuildPathError::InvalidElement => UrlGenerationError::InvalidElement,
            _ => UrlGenerationError::NotEnoughElements,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::test::TestRequest;

    /// STATEMENT: Nested resource maps resolve parent and sibling names without adding spurious prefixes.
    ///
    /// HOW:
    /// - Build root, nested, sibling, and external named resources and link the maps.
    /// - Resolve each name from root and nested maps and assert the correct absolute URL.
    /// - Check reserved characters are escaped and missing elements or names return errors.
    #[test]
    fn url_for_parent() {
        // regression: names of the parent map were prefixed with the nested root
        let mut root = ResourceMap::new(ResourceDef::new(""));
        let mut index = ResourceDef::new("/index/{id}");
        index.set_name("index");
        root.add(&mut index, None);
        let mut ext = ResourceDef::new("https://youtube.com/watch/{id}");
        ext.set_name("youtube");
        root.add(&mut ext, None);

        let mut nested = ResourceMap::new(ResourceDef::root_prefix("/a"));
        let mut res = ResourceDef::new("/{id}");
        res.set_name("nested");
        nested.add(&mut res, None);
        let nested = Rc::new(nested);
        root.add(&mut ResourceDef::root_prefix("/a"), Some(nested.clone()));

        let mut sibling = ResourceMap::new(ResourceDef::root_prefix("/b"));
        let mut res = ResourceDef::new("/{id}");
        res.set_name("sibling");
        sibling.add(&mut res, None);
        root.add(&mut ResourceDef::root_prefix("/b"), Some(Rc::new(sibling)));

        let root = Rc::new(root);
        root.build(&root);

        let req = TestRequest::default().to_http_request();
        for rmap in [&root, &nested] {
            assert_eq!(
                rmap.url_for(&req, "index", ["1"]).unwrap().as_str(),
                "http://localhost:8080/index/1"
            );
            assert_eq!(
                rmap.url_for(&req, "youtube", ["2"]).unwrap().as_str(),
                "https://youtube.com/watch/2"
            );
            assert_eq!(
                rmap.url_for(&req, "nested", ["3"]).unwrap().as_str(),
                "http://localhost:8080/a/3"
            );
            assert_eq!(
                rmap.url_for(&req, "sibling", ["4"]).unwrap().as_str(),
                "http://localhost:8080/b/4"
            );
            assert_eq!(
                rmap.url_for(&req, "index", ["a/b?c#d%"]).unwrap().as_str(),
                "http://localhost:8080/index/a%2Fb%3Fc%23d%25"
            );
            assert_eq!(
                rmap.url_for(&req, "youtube", ["a/b?c#d%"])
                    .unwrap()
                    .as_str(),
                "https://youtube.com/watch/a%2Fb%3Fc%23d%25"
            );
            assert!(rmap.url_for(&req, "index", [""; 0]).is_err());
            assert!(rmap.url_for(&req, "unknown", [""; 0]).is_err());
        }
    }

    /// STATEMENT: `url_for` rejects elements that do not match their segment patterns and
    /// extra elements, including those of enclosing scopes.
    ///
    /// HOW:
    /// - Register `/item/{id:[0-9]+}` in a scope with root `/{tenant:[a-z]+}`.
    /// - Expect a URL for valid elements, with an encoded `/` in an unrestricted segment.
    /// - Expect `InvalidElement` for a non-numeric id, a non-letter tenant and an empty
    ///   tenant, and `TooManyElements` or `NotEnoughElements` for wrong element counts.
    #[test]
    fn url_for_validates_elements() {
        let mut root = ResourceMap::new(ResourceDef::new(""));
        let mut nested = ResourceMap::new(ResourceDef::root_prefix("/{tenant:[a-z]+}"));
        let mut item = ResourceDef::new("/item/{id:[0-9]+}");
        item.set_name("item");
        nested.add(&mut item, None);
        let mut file = ResourceDef::new("/file/{name}");
        file.set_name("file");
        root.add(&mut file, None);
        root.add(
            &mut ResourceDef::root_prefix("/{tenant:[a-z]+}"),
            Some(Rc::new(nested)),
        );
        let root = Rc::new(root);
        root.build(&root);

        let req = TestRequest::default().to_http_request();
        assert_eq!(
            root.url_for(&req, "item", ["abc", "12"]).unwrap().as_str(),
            "http://localhost:8080/abc/item/12"
        );
        assert_eq!(
            root.url_for(&req, "file", ["a/b"]).unwrap().as_str(),
            "http://localhost:8080/file/a%2Fb"
        );
        for elements in [["abc", "x1"], ["ab1", "12"], ["", "12"]] {
            assert_eq!(
                root.url_for(&req, "item", elements),
                Err(UrlGenerationError::InvalidElement)
            );
        }
        assert_eq!(
            root.url_for(&req, "file", [""]),
            Err(UrlGenerationError::InvalidElement)
        );
        assert_eq!(
            root.url_for(&req, "item", ["abc", "12", "extra"]),
            Err(UrlGenerationError::TooManyElements)
        );
        assert_eq!(
            root.url_for(&req, "item", ["abc"]),
            Err(UrlGenerationError::NotEnoughElements)
        );
    }

    /// STATEMENT: A resource name can be used only once within a resource map.
    ///
    /// HOW:
    /// - Add two resources named `index` to one map and expect a panic naming it.
    #[test]
    #[should_panic(expected = "Resource name \"index\" is already used")]
    fn duplicate_name_panics() {
        let mut rmap = ResourceMap::new(ResourceDef::new(""));
        let mut a = ResourceDef::new("/a");
        a.set_name("index");
        rmap.add(&mut a, None);
        let mut b = ResourceDef::new("/b");
        b.set_name("index");
        rmap.add(&mut b, None);
    }
}
