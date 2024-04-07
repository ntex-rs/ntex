use std::ops::Index;

use serde::de;

use crate::de::PathDeserializer;
use crate::{Resource, ResourcePath};

#[derive(Debug, Clone)]
pub(super) enum PathItem {
    Static(&'static str),
    Segment(String),
    IdxSegment(u16, u16),
}

/// Resource path match information
///
/// If resource path contains variable patterns, `Path` stores them.
#[derive(Debug)]
pub struct Path<T> {
    path: T,
    pub(super) skip: u16,
    pub(super) segments: Vec<(&'static str, PathItem)>,
}

impl<T: Default> Default for Path<T> {
    fn default() -> Self {
        Path {
            path: T::default(),
            skip: 0,
            segments: Vec::new(),
        }
    }
}

impl<T: Clone> Clone for Path<T> {
    fn clone(&self) -> Self {
        Path {
            path: self.path.clone(),
            skip: self.skip,
            segments: self.segments.clone(),
        }
    }
}

impl<T: ResourcePath> Path<T> {
    pub fn new(path: T) -> Path<T> {
        Path {
            path,
            skip: 0,
            segments: Vec::new(),
        }
    }

    #[inline]
    /// Get reference to inner path instance
    pub fn get_ref(&self) -> &T {
        &self.path
    }

    #[inline]
    /// Get mutable reference to inner path instance
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.path
    }

    #[inline]
    /// Path
    pub fn path(&self) -> &str {
        let skip = self.skip as usize;
        let path = self.path.path();
        if skip <= path.len() {
            &path[skip..]
        } else {
            ""
        }
    }

    #[inline]
    /// Set new path
    pub fn set(&mut self, path: T) {
        self.skip = 0;
        self.path = path;
        self.segments.clear();
    }

    #[inline]
    /// Reset state
    pub fn reset(&mut self) {
        self.skip = 0;
        self.segments.clear();
    }

    #[inline]
    /// Skip first `n` chars in path
    pub fn skip(&mut self, n: u16) {
        self.skip += n;
    }

    // pub(crate) fn add(&mut self, name: &'static str, value: String) {
    //     self.segments.push((name, PathItem::Segment(value)))
    // }

    #[doc(hidden)]
    pub fn add_static(&mut self, name: &'static str, value: &'static str) {
        self.segments.push((name, PathItem::Static(value)));
    }

    #[inline]
    /// Check if there are any matched patterns
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    #[inline]
    /// Check number of extracted parameters
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Get matched parameter by name without type conversion
    pub fn get(&self, key: &str) -> Option<&str> {
        for item in self.segments.iter() {
            if key == item.0 {
                return match item.1 {
                    PathItem::Static(s) => Some(s),
                    PathItem::Segment(ref s) => Some(s),
                    PathItem::IdxSegment(s, e) => {
                        Some(&self.path.path()[(s as usize)..(e as usize)])
                    }
                };
            }
        }
        if key == "tail" {
            Some(&self.path.path()[(self.skip as usize)..])
        } else {
            None
        }
    }

    /// Get unprocessed part of the path
    pub fn unprocessed(&self) -> &str {
        &self.path.path()[(self.skip as usize)..]
    }

    /// Get matched parameter by name.
    ///
    /// If keyed parameter is not available empty string is used as default
    /// value.
    pub fn query(&self, key: &str) -> &str {
        self.get(key).unwrap_or_default()
    }

    /// Return iterator to items in parameter container
    pub fn iter(&self) -> PathIter<'_, T> {
        PathIter {
            idx: 0,
            params: self,
        }
    }

    /// Try to deserialize matching parameters to a specified type `U`
    pub fn load<'de, U: serde::Deserialize<'de>>(&'de self) -> Result<U, de::value::Error> {
        de::Deserialize::deserialize(PathDeserializer::new(self))
    }
}

/// Iterator to items in parameter container
#[derive(Debug)]
pub struct PathIter<'a, T> {
    idx: usize,
    params: &'a Path<T>,
}

impl<'a, T: ResourcePath> Iterator for PathIter<'a, T> {
    type Item = (&'a str, &'a str);

    #[inline]
    fn next(&mut self) -> Option<(&'a str, &'a str)> {
        if self.idx < self.params.len() {
            let idx = self.idx;
            let res = match self.params.segments[idx].1 {
                PathItem::Static(s) => s,
                PathItem::Segment(ref s) => s.as_str(),
                PathItem::IdxSegment(s, e) => {
                    &self.params.path.path()[(s as usize)..(e as usize)]
                }
            };
            self.idx += 1;
            return Some((self.params.segments[idx].0, res));
        }
        None
    }
}

impl<'a, T: ResourcePath> Index<&'a str> for Path<T> {
    type Output = str;

    fn index(&self, name: &'a str) -> &str {
        self.get(name)
            .expect("Value for parameter is not available")
    }
}

impl<T: ResourcePath> Index<usize> for Path<T> {
    type Output = str;

    fn index(&self, idx: usize) -> &str {
        match self.segments[idx].1 {
            PathItem::Static(s) => s,
            PathItem::Segment(ref s) => s,
            PathItem::IdxSegment(s, e) => &self.path.path()[(s as usize)..(e as usize)],
        }
    }
}

impl<T: ResourcePath> Resource<T> for Path<T> {
    fn path(&self) -> &str {
        self.path()
    }

    fn resource_path(&mut self) -> &mut Path<T> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path() {
        let mut p: Path<String> = Path::default();
        assert_eq!(p.get_ref(), &String::new());
        p.get_mut().push_str("test");
        assert_eq!(p.get_ref().as_str(), "test");
        let p2 = p.clone();
        assert_eq!(p2.get_ref().as_str(), "test");

        p.skip(2);
        assert_eq!(p.get("tail").unwrap(), "st");
        assert_eq!(p.get("unknown"), None);
        assert_eq!(p.query("tail"), "st");
        assert_eq!(p.query("unknown"), "");
        assert_eq!(p.unprocessed(), "st");

        p.reset();
        assert_eq!(p.unprocessed(), "test");

        p.segments.push(("k1", PathItem::IdxSegment(0, 2)));
        assert_eq!(p.get("k1").unwrap(), "te");
    }
}
