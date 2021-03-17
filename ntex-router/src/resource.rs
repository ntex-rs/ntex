use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use regex::{escape, Regex};

use super::IntoPattern;

#[derive(Clone, Debug)]
pub(super) struct Segments {
    pub(super) tp: Vec<Segment>,
    pub(super) slesh: bool,
}

/// ResourceDef describes an entry in resources table
///
/// Resource definition can contain only 16 dynamic segments
#[derive(Clone, Debug)]
pub struct ResourceDef {
    id: u16,
    pub(super) tp: Vec<Segments>, // set of matching paths
    name: String,
    pattern: String,
    elements: Vec<PathElement>,
    pub(super) prefix: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum PathElement {
    Str(String),
    Var(String),
}

impl PathElement {
    fn is_str(&self) -> bool {
        matches!(self, PathElement::Str(_))
    }

    fn into_str(self) -> String {
        match self {
            PathElement::Str(s) => s,
            _ => panic!(),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            PathElement::Str(s) => s.as_str(),
            PathElement::Var(s) => s.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Segment {
    Static(String),
    Dynamic {
        pattern: Regex,
        names: Vec<&'static str>,
        len: usize,
        tail: bool,
    },
}

impl Eq for Segment {}

impl PartialEq for Segment {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Segment::Static(ref p1) => match other {
                Segment::Static(p2) => p1 == p2,
                _ => false,
            },
            Segment::Dynamic {
                pattern: ref p1,
                tail: t1,
                ..
            } => match other {
                Segment::Static { .. } => false,
                Segment::Dynamic {
                    pattern: ref p2,
                    tail: t2,
                    ..
                } => p1.as_str() == p2.as_str() && t1 == t2,
            },
        }
    }
}

impl ResourceDef {
    /// Parse path pattern and create new `ResourceDef` instance.
    ///
    /// Path segments are separatted by `/`. Pattern must start
    /// with segment separator. Static segments could be
    /// case insensitive.
    ///
    /// Panics if path pattern is malformed.
    pub fn new<T: IntoPattern>(path: T) -> Self {
        let set = path.patterns();
        let mut p = String::new();
        let mut tp = Vec::new();
        let mut elements = Vec::new();

        for path in set {
            p = path.clone();
            let (pelems, elems) = ResourceDef::parse(&path);
            tp.push(pelems);
            elements = elems;
        }

        ResourceDef {
            tp,
            elements,
            id: 0,
            name: String::new(),
            pattern: p,
            prefix: false,
        }
    }

    /// Parse path pattern and create new `ResourceDef` instance.
    ///
    /// Use `prefix` type instead of `static`.
    ///
    /// Panics if path regex pattern is malformed.
    pub fn prefix<T: IntoPattern>(path: T) -> Self {
        ResourceDef::with_prefix(path)
    }

    /// Parse path pattern and create new `ResourceDef` instance.
    /// Inserts `/` to the start of the pattern.
    ///
    /// Panics if path regex pattern is malformed.
    pub fn root_prefix<T: IntoPattern>(path: T) -> Self {
        let mut patterns = path.patterns();
        for path in &mut patterns {
            let p = insert_slash(path.as_str());
            *path = p
        }

        ResourceDef::with_prefix(patterns)
    }

    /// Resource id
    pub fn id(&self) -> u16 {
        self.id
    }

    /// Set resource id
    pub fn set_id(&mut self, id: u16) {
        self.id = id;
    }

    /// Parse path pattern and create new `Pattern` instance with custom prefix
    fn with_prefix<T: IntoPattern>(path: T) -> Self {
        let patterns = path.patterns();

        let mut p = String::new();
        let mut tp = Vec::new();
        let mut elements = Vec::new();

        for path in patterns {
            p = path.clone();
            let (pelems, elems) = ResourceDef::parse(&path);
            tp.push(pelems);
            elements = elems;
        }

        ResourceDef {
            tp,
            elements,
            id: 0,
            name: String::new(),
            pattern: p,
            prefix: true,
        }
    }

    /// Resource pattern name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Mutable reference to a name of a resource definition.
    pub fn name_mut(&mut self) -> &mut String {
        &mut self.name
    }

    /// Path pattern of the resource
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Build resource path from elements. Returns `true` on success.
    pub fn resource_path<U, I>(&self, path: &mut String, elements: &mut U) -> bool
    where
        U: Iterator<Item = I>,
        I: AsRef<str>,
    {
        for el in &self.elements {
            match *el {
                PathElement::Str(ref s) => path.push_str(s),
                PathElement::Var(_) => {
                    if let Some(val) = elements.next() {
                        path.push_str(val.as_ref())
                    } else {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Build resource path from elements. Returns `true` on success.
    pub fn resource_path_named<K, V, S>(
        &self,
        path: &mut String,
        elements: &HashMap<K, V, S>,
    ) -> bool
    where
        K: std::borrow::Borrow<str> + Eq + Hash,
        V: AsRef<str>,
        S: std::hash::BuildHasher,
    {
        for el in &self.elements {
            match *el {
                PathElement::Str(ref s) => path.push_str(s),
                PathElement::Var(ref name) => {
                    if let Some(val) = elements.get(name) {
                        path.push_str(val.as_ref())
                    } else {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn parse_segment<'a>(
        pattern: &'a str,
        elems: &mut Vec<PathElement>,
    ) -> (String, &'a str, bool) {
        const DEFAULT_PATTERN: &str = ".+";
        const DEFAULT_PATTERN_TAIL: &str = ".*";

        let mut re = "^".to_string();
        let mut end = None;
        let mut tail = false;
        let mut rem = pattern;
        let start = if pattern.starts_with('/') { 1 } else { 0 };
        let mut pattern = &pattern[start..];
        elems.push(PathElement::Str('/'.to_string()));

        while let Some(start_idx) = pattern.find('{') {
            if let Some(end) = end {
                if start_idx > end {
                    break;
                }
            }
            let p = pattern.split_at(start_idx);
            pattern = p.1;
            re.push_str(&escape(&p.0));
            elems.push(PathElement::Str(p.0.to_string()));

            // find closing }
            let mut params_nesting = 0usize;
            let close_idx = pattern
                .find(|c| match c {
                    '{' => {
                        params_nesting += 1;
                        false
                    }
                    '}' => {
                        params_nesting -= 1;
                        params_nesting == 0
                    }
                    _ => false,
                })
                .expect("malformed dynamic segment");

            let p = pattern.split_at(close_idx + 1);
            rem = p.1;
            let param = &p.0[1..p.0.len() - 1]; // Remove outer brackets
            tail = rem == "*"; // tail match (should match regardless of segments)

            let (name, pat) = match param.find(':') {
                Some(idx) => {
                    if tail {
                        panic!("Custom regex is not supported for remainder match");
                    }
                    let (name, pattern) = param.split_at(idx);
                    (name, &pattern[1..])
                }
                None => (
                    param,
                    if tail {
                        rem = &rem[1..];
                        DEFAULT_PATTERN_TAIL
                    } else {
                        DEFAULT_PATTERN
                    },
                ),
            };

            re.push_str(&format!(r"(?P<{}>{})", &escape(name), pat));

            elems.push(PathElement::Var(name.to_string()));

            if let Some(idx) = rem.find(|c| c == '{' || c == '/') {
                end = Some(idx);
                pattern = rem;
                continue;
            } else {
                re += rem;
                rem = "";
                break;
            }
        }

        // find end of segment
        if let Some(idx) = rem.find('/') {
            re.push_str(&escape(&rem[..idx]));
            rem = &rem[idx..];
        } else {
            re.push_str(&escape(rem));
            rem = "";
        };
        re.push('$');

        (re, rem, tail)
    }

    fn parse(mut pattern: &str) -> (Segments, Vec<PathElement>) {
        let mut elems = Vec::new();
        let mut pelems = Vec::new();

        if pattern.is_empty() {
            return (
                Segments {
                    tp: Vec::new(),
                    slesh: false,
                },
                Vec::new(),
            );
        }

        loop {
            let start = if pattern.starts_with('/') { 1 } else { 0 };
            let idx = if let Some(idx) = pattern[start..].find(|c| c == '{' || c == '/')
            {
                idx + start
            } else {
                break;
            };

            // static segment
            if let Some(i) = pattern[start..idx + 1].find('/') {
                elems.push(PathElement::Str(pattern[..i + start].to_string()));
                pelems.push(Segment::Static(pattern[start..i + start].to_string()));
                pattern = &pattern[i + start..];
                continue;
            }

            // dynamic segment
            let (re_part, rem, tail) = Self::parse_segment(pattern, &mut elems);
            let re = Regex::new(&re_part).unwrap();
            let names: Vec<_> = re
                .capture_names()
                .filter_map(|name| {
                    name.map(|name| Box::leak(Box::new(name.to_owned())).as_str())
                })
                .collect();
            pelems.push(Segment::Dynamic {
                names,
                tail,
                pattern: re,
                len: 0,
            });

            pattern = rem;
            if pattern.is_empty() {
                break;
            }
        }

        // tail
        let slesh = pattern.ends_with('/');
        if slesh {
            pattern = &pattern[..pattern.len() - 1];
        }
        elems.push(PathElement::Str(pattern.to_string()));
        if pattern.starts_with('/') {
            pattern = &pattern[1..];
        }
        if !pattern.is_empty() {
            // handle tail expression for static segment
            if let Some(stripped) = pattern.strip_suffix('*') {
                let pattern = Regex::new(&format!("^{}(.+)", stripped)).unwrap();
                pelems.push(Segment::Dynamic {
                    pattern,
                    names: Vec::new(),
                    tail: true,
                    len: 0,
                });
            } else {
                pelems.push(Segment::Static(pattern.to_string()));
            }
        }

        // insert last slesh
        if slesh {
            elems.push(PathElement::Str("/".to_string()))
        }

        // merge path elements
        let mut idx = 0;
        while idx + 1 < elems.len() {
            if elems[idx + 1].is_str() && elems[idx + 1].as_str().is_empty() {
                elems.remove(idx + 1);
                continue;
            }
            if elems[idx].is_str() && elems[idx + 1].is_str() {
                let s2 = elems.remove(idx + 1).into_str();
                if let PathElement::Str(ref mut s1) = elems[idx] {
                    s1.push_str(&s2);
                    continue;
                }
            }
            idx += 1;
        }

        (Segments { tp: pelems, slesh }, elems)
    }
}

impl Eq for ResourceDef {}

impl PartialEq for ResourceDef {
    fn eq(&self, other: &ResourceDef) -> bool {
        self.pattern == other.pattern
    }
}

impl Hash for ResourceDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pattern.hash(state);
    }
}

impl<'a> From<&'a str> for ResourceDef {
    fn from(path: &'a str) -> ResourceDef {
        ResourceDef::new(path)
    }
}

impl From<String> for ResourceDef {
    fn from(path: String) -> ResourceDef {
        ResourceDef::new(path)
    }
}

pub(crate) fn insert_slash(path: &str) -> String {
    let mut path = path.to_owned();
    if !path.is_empty() && !path.starts_with('/') {
        path.insert(0, '/');
    };
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::Path;
    use crate::tree::Tree;

    #[test]
    fn test_parse_static() {
        let re = ResourceDef::new("/");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/a")), None);

        let re = ResourceDef::new("/name");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);
        assert_eq!(tree.find(&mut Path::new("/name/")), None);
        assert_eq!(tree.find(&mut Path::new("/name~")), None);

        let re = ResourceDef::new("/name/");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name/gs")), None);

        let re = ResourceDef::new("/user/profile");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/profile/profile")), None);

        let mut tree = Tree::new(&ResourceDef::new("/name"), 1);
        tree.insert(&ResourceDef::new("/name/"), 2);
        assert_eq!(tree.find(&mut Path::new("/name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(2));
    }

    #[test]
    fn test_parse_param() {
        let tree = Tree::new(&ResourceDef::new("/{id}"), 1);
        assert_eq!(tree.find(&mut Path::new("/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2345/")), None);
        assert_eq!(tree.find(&mut Path::new("/2345/sdg")), None);

        let re = ResourceDef::new("/user/{id}");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), None);
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), None);

        let mut resource = Path::new("/user/profile");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "profile");

        let mut resource = Path::new("/user/1245125");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "1245125");

        let tree = Tree::new(&ResourceDef::new("/v{version}/resource/{id}"), 1);
        assert_eq!(tree.find(&mut Path::new("/v1/resource/320120")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v1/resource/320120/")), None,);
        assert_eq!(tree.find(&mut Path::new("/v/resource/1")), None);
        assert_eq!(tree.find(&mut Path::new("/resource")), None);

        let mut resource = Path::new("/v151/resource/adahg32");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("version").unwrap(), "151");
        assert_eq!(resource.get("id").unwrap(), "adahg32");

        let re = ResourceDef::new("/{id:[[:digit:]]{6}}");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/012345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/012345/")), None);
        assert_eq!(tree.find(&mut Path::new("/012345/index")), None);
        assert_eq!(tree.find(&mut Path::new("/012")), None);
        assert_eq!(tree.find(&mut Path::new("/01234567")), None);
        assert_eq!(tree.find(&mut Path::new("/XXXXXX")), None);

        let mut resource = Path::new("/012345");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "012345");

        let re =
            ResourceDef::new("/u/test/v{version}-no-{minor}xx/resource/{id}/{name}");
        let tree = Tree::new(&re, 1);
        let mut resource = Path::new("/u/test/v1-no-3xx/resource/320120/name");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("version").unwrap(), "1");
        assert_eq!(resource.get("minor").unwrap(), "3");
        assert_eq!(resource.get("id").unwrap(), "320120");
        assert_eq!(resource.get("name").unwrap(), "name");
    }

    #[test]
    fn test_dynamic_set() {
        let re = ResourceDef::new(vec![
            "/user/{id}",
            "/v{version}/resource/{id}",
            "/{id:[[:digit:]]{6}}",
        ]);
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), None);
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), None);

        let mut resource = Path::new("/user/profile");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "profile");

        let mut resource = Path::new("/user/1245125");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "1245125");

        assert_eq!(tree.find(&mut Path::new("/v1/resource/320120")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/resource/1")), None);
        assert_eq!(tree.find(&mut Path::new("/resource")), None);

        let mut resource = Path::new("/v151/resource/adahg32");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("version").unwrap(), "151");
        assert_eq!(resource.get("id").unwrap(), "adahg32");

        assert_eq!(tree.find(&mut Path::new("/012345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/012")), None);
        assert_eq!(tree.find(&mut Path::new("/01234567")), None);
        assert_eq!(tree.find(&mut Path::new("/XXXXXX")), None);

        let mut resource = Path::new("/012345");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "012345");

        let re = ResourceDef::new([
            "/user/{id}",
            "/v{version}/resource/{id}",
            "/{id:[[:digit:]]{6}}",
        ]);
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), None);
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), None);

        let re = ResourceDef::new([
            "/user/{id}".to_string(),
            "/v{version}/resource/{id}".to_string(),
            "/{id:[[:digit:]]{6}}".to_string(),
        ]);
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), None);
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), None);
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_parse_urlencoded() {
        use http::Uri;
        use std::convert::TryFrom;

        let tree = Tree::new(&ResourceDef::new("/user/{id}/test"), 1);
        let uri = Uri::try_from("/user/2345/test").unwrap();
        let mut resource = Path::new(uri);
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "2345");

        let uri = Uri::try_from("/user/qwe%25/test").unwrap();
        let mut resource = Path::new(uri);
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "qwe%");

        let uri = Uri::try_from("/user/qwe%25rty/test").unwrap();
        let mut resource = Path::new(uri);
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "qwe%rty");

        let uri = Uri::try_from("/user/foo-%2f-%252f-bar/test").unwrap();
        let mut resource = Path::new(uri);
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "foo-/-%2f-bar");

        let uri = Uri::try_from(
            "/user/http%3A%2F%2Flocalhost%3A80%2Ffile%2F%2Fvar%2Flog%2Fsyslog/test",
        )
        .unwrap();
        let mut resource = Path::new(uri);
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(
            resource.get("id").unwrap(),
            "http://localhost:80/file//var/log/syslog"
        );
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_extract_path_decode() {
        use http::Uri;
        use std::convert::TryFrom;

        let tree = Tree::new(&ResourceDef::new("/{id}/"), 1);

        macro_rules! test_single_value {
            ($value:expr, $expected:expr) => {{
                let uri = Uri::try_from($value).unwrap();
                let mut resource = Path::new(uri);
                assert_eq!(tree.find(&mut resource), Some(1));
                assert_eq!(resource.get("id").unwrap(), $expected);
            }};
        }

        test_single_value!("/%25/", "%");
        test_single_value!("/%40%C2%A3%24%25%5E%26%2B%3D/", "@£$%^&+=");
        test_single_value!("/%2B/", "+");
        test_single_value!("/%252B/", "%2B");
        test_single_value!("/%2F/", "/");
        test_single_value!("/test%2Ftest/", "test/test");
        test_single_value!("/%252F/", "%2F");
        test_single_value!("/%m/", "%m");
        test_single_value!("/%mm/", "%mm");
        test_single_value!("/test%mm/", "test%mm");
        test_single_value!(
            "/http%3A%2F%2Flocalhost%3A80%2Ffoo/",
            "http://localhost:80/foo"
        );
        test_single_value!("/%2Fvar%2Flog%2Fsyslog/", "/var/log/syslog");
        test_single_value!(
            "/http%3A%2F%2Flocalhost%3A80%2Ffile%2F%252Fvar%252Flog%252Fsyslog/",
            "http://localhost:80/file/%2Fvar%2Flog%2Fsyslog"
        );
    }

    #[test]
    fn test_def() {
        let re = ResourceDef::new("/user/-{id}*");
        assert_eq!(re, ResourceDef::from("/user/-{id}*"));
        assert_eq!(re, ResourceDef::from("/user/-{id}*".to_string()));

        let mut h = HashMap::new();
        h.insert(re.clone(), 1);
        assert!(h.contains_key(&re));

        let seg = Segment::Static("s".to_string());
        assert_eq!(seg, Segment::Static("s".to_string()));

        let seg2 = Segment::Dynamic {
            pattern: Regex::new("test").unwrap(),
            names: Vec::new(),
            len: 1,
            tail: false,
        };
        assert!(seg != seg2);
        assert_eq!(seg2, seg2);
    }

    #[test]
    fn test_parse_tail() {
        let re = ResourceDef::new("/user/-{id}*");
        let tree = Tree::new(&re, 1);

        let mut resource = Path::new("/user/-profile");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "profile");

        let mut resource = Path::new("/user/-2345");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "2345");

        let mut resource = Path::new("/user/-2345/");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "2345/");

        let mut resource = Path::new("/user/-2345/sdg");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "2345/sdg");
    }

    #[test]
    fn test_static_tail() {
        let re = ResourceDef::new("/*".to_string());
        let tree = Tree::new(&re, 1);
        assert_eq!(
            tree.find(&mut Path::new(bytestring::ByteString::from_static("/"))),
            None
        );
        assert_eq!(tree.find(&mut Path::new("/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2345/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2345/sdg")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), Some(1));

        let re = ResourceDef::new(&("/user*".to_string()));
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), Some(1));

        let re = ResourceDef::new("/v/user*");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/v/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/2345/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/2345/sdg")), Some(1));

        let re = ResourceDef::new("/user/*");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/2345/sdg")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/")), None);
        assert_eq!(tree.find(&mut Path::new("/user")), None);

        let re = ResourceDef::new("/v/user/*");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("/v/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/2345/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/2345/sdg")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/v/user/")), None);
        assert_eq!(tree.find(&mut Path::new("/v/user")), None);
    }

    #[test]
    fn test_resource_prefix() {
        let tree = Tree::new(&ResourceDef::prefix("/"), 1);
        assert_eq!(tree.find(&mut Path::new("/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/a")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/a/test/test")), Some(1));

        let tree = Tree::new(&ResourceDef::prefix("/name"), 1);
        assert_eq!(tree.find(&mut Path::new("/name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name1")), None);
        assert_eq!(tree.find(&mut Path::new("/name~")), None);

        let mut resource = Path::new("/name/subpath1/subpath2/index.html");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.path(), "/subpath1/subpath2/index.html");

        let tree = Tree::new(&ResourceDef::prefix("/name/"), 1);
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);

        let tree = Tree::new(&ResourceDef::prefix(vec!["/name/", "/name2/"]), 1);
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name2/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name2/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);

        let tree = Tree::new(&ResourceDef::root_prefix("name/"), 1);
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);

        let tree = Tree::new(&ResourceDef::root_prefix(vec!["name/", "name2/"]), 1);
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name2/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name2/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);

        let mut resource = Path::new("/name/subpath1/subpath2/index.html");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.path(), "/subpath1/subpath2/index.html");
    }

    #[test]
    fn test_reousrce_prefix_dynamic() {
        let tree = Tree::new(&ResourceDef::prefix("/{name}/"), 1);
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);
        assert_eq!(tree.find(&mut Path::new("/name~")), None);

        let mut resource = Path::new("/test2/");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["name"], "test2");
        assert_eq!(&resource[0], "test2");

        let mut resource = Path::new("/test2/subpath1/subpath2/index.html");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["name"], "test2");
        assert_eq!(&resource[0], "test2");
        assert_eq!(resource.path(), "/subpath1/subpath2/index.html");

        let tree = Tree::new(&ResourceDef::prefix("/{name}"), 1);
        assert_eq!(tree.find(&mut Path::new("/name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name1")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name~")), Some(1));

        let tree = Tree::new(&ResourceDef::prefix(vec!["/1/{name}/", "/2/{name}/"]), 1);
        assert_eq!(tree.find(&mut Path::new("/1/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/1/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2/name/test/test")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/1/name")), None);
        assert_eq!(tree.find(&mut Path::new("/1/name1")), None);
        assert_eq!(tree.find(&mut Path::new("/1/name~")), None);
        assert_eq!(tree.find(&mut Path::new("/2/name")), None);
        assert_eq!(tree.find(&mut Path::new("/2/name1")), None);
        assert_eq!(tree.find(&mut Path::new("/2/name~")), None);

        let mut resource = Path::new("/1/test2/subpath1/subpath2/index.html");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["name"], "test2");
        assert_eq!(&resource[0], "test2");
        assert_eq!(resource.path(), "/subpath1/subpath2/index.html");

        let mut resource = Path::new("/2/test3/subpath1/subpath2/index.html");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["name"], "test3");
        assert_eq!(&resource[0], "test3");
        assert_eq!(resource.path(), "/subpath1/subpath2/index.html");

        // nested
        let mut tree = Tree::new(&ResourceDef::prefix("/prefix/{v1}/second/{v2}"), 1);
        tree.insert(&ResourceDef::prefix("/prefix/{v1}"), 2);

        let mut resource = Path::new("/prefix/1/second/2");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["v1"], "1");
        assert_eq!(&resource["v2"], "2");

        let mut resource = Path::new("/prefix/1/second");
        assert_eq!(tree.find(&mut resource), Some(2));
        assert_eq!(&resource["v1"], "1");
        assert_eq!(tree.find(&mut Path::new("/prefix/1")), Some(2));

        // nested
        let mut tree = Tree::new(
            &ResourceDef::prefix(vec![
                "/prefix/{v1}/second/{v2}",
                "/prefix2/{v1}/second/{v2}",
            ]),
            1,
        );
        tree.insert(&ResourceDef::prefix("/prefix/{v1}"), 2);
        tree.insert(&ResourceDef::prefix("/prefix2/{v1}"), 3);

        let mut resource = Path::new("/prefix/1/second/2");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["v1"], "1");
        assert_eq!(&resource["v2"], "2");

        let mut resource = Path::new("/prefix2/1/second/2");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(&resource["v1"], "1");
        assert_eq!(&resource["v2"], "2");

        let mut resource = Path::new("/prefix/1/second");
        assert_eq!(tree.find(&mut resource), Some(2));
        assert_eq!(&resource["v1"], "1");
        assert_eq!(tree.find(&mut Path::new("/prefix/1")), Some(2));

        let mut resource = Path::new("/prefix2/1/second");
        assert_eq!(tree.find(&mut resource), Some(3));
        assert_eq!(&resource["v1"], "1");
        assert_eq!(tree.find(&mut Path::new("/prefix2/1")), Some(3));
    }

    #[test]
    fn test_resource_path() {
        let mut s = String::new();
        let resource = ResourceDef::new("/user/{item1}/test");
        assert!(resource.resource_path(&mut s, &mut (&["user1"]).iter()));
        assert_eq!(s, "/user/user1/test");

        let mut s = String::new();
        let resource = ResourceDef::new("/user/{item1}/{item2}/test");
        assert!(resource.resource_path(&mut s, &mut (&["item", "item2"]).iter()));
        assert_eq!(s, "/user/item/item2/test");

        let mut s = String::new();
        let resource = ResourceDef::new("/user/{item1}/{item2}");
        assert!(resource.resource_path(&mut s, &mut (&["item", "item2"]).iter()));
        assert_eq!(s, "/user/item/item2");

        let mut s = String::new();
        let resource = ResourceDef::new("/user/{item1}/{item2}/");
        assert!(resource.resource_path(&mut s, &mut (&["item", "item2"]).iter()));
        assert_eq!(s, "/user/item/item2/");

        let mut s = String::new();
        assert!(!resource.resource_path(&mut s, &mut (&["item"]).iter()));

        let mut s = String::new();
        assert!(resource.resource_path(&mut s, &mut (&["item", "item2"]).iter()));
        assert_eq!(s, "/user/item/item2/");
        assert!(!resource.resource_path(&mut s, &mut (&["item"]).iter()));

        let mut s = String::new();
        assert!(resource.resource_path(&mut s, &mut vec!["item", "item2"].into_iter()));
        assert_eq!(s, "/user/item/item2/");

        let mut map = HashMap::new();
        map.insert("item1", "item");

        let mut s = String::new();
        assert!(!resource.resource_path_named(&mut s, &map));

        let mut s = String::new();
        map.insert("item2", "item2");
        assert!(resource.resource_path_named(&mut s, &map));
        assert_eq!(s, "/user/item/item2/");
    }

    #[test]
    fn test_non_rooted() {
        let tree = Tree::new(&ResourceDef::new("name"), 1);
        assert_eq!(tree.find(&mut Path::new("name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/")), None);
        assert_eq!(tree.find(&mut Path::new("/name1")), None);
        assert_eq!(tree.find(&mut Path::new("/name/")), None);
        assert_eq!(tree.find(&mut Path::new("/name~")), None);

        let tree = Tree::new(&ResourceDef::new("name/"), 1);
        assert_eq!(tree.find(&mut Path::new("name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name")), None);
        assert_eq!(tree.find(&mut Path::new("/name/gs")), None);

        let tree = Tree::new(&ResourceDef::new("user/profile"), 1);
        assert_eq!(tree.find(&mut Path::new("user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/profile/profile")), None);

        let tree = Tree::new(&ResourceDef::new("{id}"), 1);
        assert_eq!(tree.find(&mut Path::new("profile")), Some(1));
        assert_eq!(tree.find(&mut Path::new("2345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/2345/")), None);
        assert_eq!(tree.find(&mut Path::new("/2345/sdg")), None);

        let tree = Tree::new(&ResourceDef::new("{user}/profile/{no}"), 1);
        assert_eq!(tree.find(&mut Path::new("user/profile/123")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/profile/123")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/user/profile/p/test/")), None);

        let tree = Tree::new(&ResourceDef::new("v{version}/resource/{id}/test"), 1);
        assert_eq!(
            tree.find(&mut Path::new("v1/resource/320120/test")),
            Some(1)
        );
        assert_eq!(tree.find(&mut Path::new("v/resource/1/test")), None);

        let mut resource = Path::new("v151/resource/adahg32/test");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("version").unwrap(), "151");
        assert_eq!(resource.get("id").unwrap(), "adahg32");

        let re = ResourceDef::new("v/{id:[[:digit:]]{6}}");
        let tree = Tree::new(&re, 1);
        assert_eq!(tree.find(&mut Path::new("v/012345")), Some(1));
        assert_eq!(tree.find(&mut Path::new("v/012345/")), None);
        assert_eq!(tree.find(&mut Path::new("v/012345/index")), None);
        assert_eq!(tree.find(&mut Path::new("v/012")), None);
        assert_eq!(tree.find(&mut Path::new("v/01234567")), None);
        assert_eq!(tree.find(&mut Path::new("v/XXXXXX")), None);

        let mut resource = Path::new("v/012345");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("id").unwrap(), "012345");

        let re = ResourceDef::new("u/test/v{version}-no-{minor}xx/resource/{id}/{name}");
        let tree = Tree::new(&re, 1);
        let mut resource = Path::new("u/test/v1-no-3xx/resource/320120/name");
        assert_eq!(tree.find(&mut resource), Some(1));
        assert_eq!(resource.get("version").unwrap(), "1");
        assert_eq!(resource.get("minor").unwrap(), "3");
        assert_eq!(resource.get("id").unwrap(), "320120");
        assert_eq!(resource.get("name").unwrap(), "name");
    }

    #[test]
    fn test_recursive() {
        let mut tree = Tree::new(&ResourceDef::new("/name"), 1);
        tree.insert(&ResourceDef::new("/name/"), 2);
        tree.insert(&ResourceDef::new("/name/index.html"), 3);
        tree.insert(&ResourceDef::prefix("/"), 4);

        assert_eq!(tree.find(&mut Path::new("/name")), Some(1));
        assert_eq!(tree.find(&mut Path::new("/name/")), Some(2));
        assert_eq!(tree.find(&mut Path::new("/name/index.html")), Some(3));
        assert_eq!(tree.find(&mut Path::new("/")), Some(4));
        assert_eq!(tree.find(&mut Path::new("/test")), Some(4));
        assert_eq!(tree.find(&mut Path::new("/test/index.html")), Some(4));
    }

    #[test]
    fn test_with_some_match() {
        let mut tree = Tree::new(&ResourceDef::new("/p/{tp}/{id}/{r}"), 1);
        tree.insert(&ResourceDef::new("/p/ih/{tp}/d/{id}/sid/{r}/r/{s}"), 3);

        let mut p = Path::new("/p/ih/def/d/abc/sid/5bddc58f/r/srv");
        assert_eq!(tree.find(&mut p), Some(3));
        assert_eq!(p.get("tp"), Some("def"));
        assert_eq!(p.get("id"), Some("abc"));
        assert_eq!(p.get("r"), Some("5bddc58f"));
        assert_eq!(p.get("s"), Some("srv"));
        assert_eq!(p.len(), 4);
    }
}
