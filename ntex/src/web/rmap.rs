use std::{cell::RefCell, rc::Rc};

#[cfg(feature = "url")]
use url_pkg::Url;

use crate::router::ResourceDef;
use crate::util::HashMap;
#[cfg(feature = "url")]
use crate::web::httprequest::HttpRequest;

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
    pub fn add(&mut self, pattern: &mut ResourceDef, nested: Option<Rc<ResourceMap>>) {
        pattern.set_id(self.patterns.len() as u16);
        self.patterns.push((pattern.clone(), nested));
        if !pattern.name().is_empty() {
            self.named
                .insert(pattern.name().to_string(), pattern.clone());
        }
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

#[cfg(feature = "url")]
impl ResourceMap {
    /// Generate url for named resource
    ///
    /// Check [`HttpRequest::url_for()`](crate::web::HttpRequest::url_for) for detailed information.
    pub fn url_for<U, I>(
        &self,
        req: &HttpRequest,
        name: &str,
        elements: U,
    ) -> Result<Url, super::error::UrlGenerationError>
    where
        U: IntoIterator<Item = I>,
        I: AsRef<str>,
    {
        let mut path = String::new();
        let mut elements = elements.into_iter();

        if self.patterns_for(name, &mut path, &mut elements)?.is_some() {
            if path.starts_with('/') {
                let conn = req.connection_info();
                Ok(Url::parse(&format!(
                    "{}://{}{}",
                    conn.scheme(),
                    conn.host(),
                    path
                ))?)
            } else {
                Ok(Url::parse(&path)?)
            }
        } else {
            Err(super::error::UrlGenerationError::ResourceNotFound)
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
    ) -> Result<Option<()>, super::error::UrlGenerationError>
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
    ) -> Result<Option<()>, super::error::UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(pattern) = self.named.get(name) {
            if pattern.pattern().starts_with('/') {
                self.fill_root(path, elements)?;
            }
            if pattern.resource_path(path, elements) {
                Ok(Some(()))
            } else {
                Err(super::error::UrlGenerationError::NotEnoughElements)
            }
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

    fn fill_root<U, I>(
        &self,
        path: &mut String,
        elements: &mut U,
    ) -> Result<(), super::error::UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(ref parent) = *self.parent.borrow() {
            parent.fill_root(path, elements)?;
        }
        if self.root.resource_path(path, elements) {
            Ok(())
        } else {
            Err(super::error::UrlGenerationError::NotEnoughElements)
        }
    }

    fn parent_pattern_for<U, I>(
        &self,
        name: &str,
        path: &mut String,
        elements: &mut U,
    ) -> Result<Option<()>, super::error::UrlGenerationError>
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        if let Some(ref parent) = *self.parent.borrow() {
            if let Some(pattern) = parent.named.get(name) {
                self.fill_root(path, elements)?;
                if pattern.resource_path(path, elements) {
                    Ok(Some(()))
                } else {
                    Err(super::error::UrlGenerationError::NotEnoughElements)
                }
            } else {
                parent.parent_pattern_for(name, path, elements)
            }
        } else {
            Ok(None)
        }
    }
}
