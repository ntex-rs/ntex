use std::borrow::Cow;
use std::mem;

use super::path::PathItem;
use super::resource::{ResourceDef, Segment};
use super::{Resource, ResourcePath};

#[derive(Debug, Clone, Default)]
pub(super) struct Tree {
    key: Vec<Segment>,
    items: Vec<Item>,
}

#[derive(Clone, Debug)]
enum Item {
    Value(Value),
    Subtree(Tree),
}

#[derive(Copy, Clone, Debug)]
enum Value {
    Val(usize),
    Slash(usize),
    Prefix(usize),
    PrefixSlash(usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PathState {
    Empty,
    Slash,
    Tail,
}

impl Value {
    fn value(&self) -> usize {
        match self {
            Value::Val(v) => *v,
            Value::Slash(v) => *v,
            Value::Prefix(v) => *v,
            Value::PrefixSlash(v) => *v,
        }
    }
}

impl Tree {
    pub(crate) fn new(resource: &ResourceDef, value: usize) -> Tree {
        if resource.tp.is_empty() {
            panic!("Non empty key is expected");
        }

        let val = if resource.tp[0].slesh {
            if resource.prefix {
                Value::PrefixSlash(value)
            } else {
                Value::Slash(value)
            }
        } else if resource.prefix {
            Value::Prefix(value)
        } else {
            Value::Val(value)
        };

        let mut tree = Tree::child(resource.tp[0].tp.clone(), Some(val));

        for seg in &resource.tp[1..] {
            let val = if seg.slesh {
                if resource.prefix {
                    Value::PrefixSlash(value)
                } else {
                    Value::Slash(value)
                }
            } else if resource.prefix {
                Value::Prefix(value)
            } else {
                Value::Val(value)
            };
            tree.insert_path(seg.tp.clone(), val)
        }
        tree
    }

    fn child(key: Vec<Segment>, value: Option<Value>) -> Tree {
        let items = if let Some(val) = value {
            vec![Item::Value(val)]
        } else {
            Vec::new()
        };
        Tree { key, items }
    }

    pub(super) fn insert(&mut self, resource: &ResourceDef, value: usize) {
        for seg in &resource.tp {
            let value = if seg.slesh {
                if resource.prefix {
                    Value::PrefixSlash(value)
                } else {
                    Value::Slash(value)
                }
            } else if resource.prefix {
                Value::Prefix(value)
            } else {
                Value::Val(value)
            };
            let key: Vec<_> = seg.tp.to_vec();
            self.insert_path(key, value);
        }
    }

    fn insert_path(&mut self, key: Vec<Segment>, value: Value) {
        let p = common_prefix(&self.key, &key);

        // split current key, and move all children to sub tree
        if p < self.key.len() {
            let child = Tree {
                key: self.key.split_off(p),
                items: mem::take(&mut self.items),
            };
            self.items.push(Item::Subtree(child));
        }

        // update value if key is the same
        if p == key.len() {
            match value {
                Value::PrefixSlash(v) => {
                    self.items.push(Item::Subtree(Tree::child(
                        Vec::new(),
                        Some(Value::Prefix(v)),
                    )));
                }
                value => self.items.push(Item::Value(value)),
            }
        } else {
            // insert into sub tree
            for child in &mut self.items {
                if let Item::Subtree(tree) = child {
                    if common_prefix(&tree.key, &key[p..]) > 0 {
                        tree.insert_path(key[p..].to_vec(), value);
                        return;
                    }
                }
            }
            self.items
                .push(Item::Subtree(Tree::child(key[p..].to_vec(), Some(value))));
        }
    }

    pub(crate) fn find<T, R>(&self, resource: &mut R) -> Option<usize>
    where
        T: ResourcePath,
        R: Resource<T>,
    {
        self.find_checked_inner(resource, false, &|_, _| true)
    }

    pub(crate) fn find_insensitive<T, R>(&self, resource: &mut R) -> Option<usize>
    where
        T: ResourcePath,
        R: Resource<T>,
    {
        self.find_checked_inner(resource, true, &|_, _| true)
    }

    pub(crate) fn find_checked<T, R, F>(&self, resource: &mut R, check: &F) -> Option<usize>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        self.find_checked_inner(resource, false, check)
    }

    pub(crate) fn find_checked_insensitive<T, R, F>(
        &self,
        resource: &mut R,
        check: &F,
    ) -> Option<usize>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        self.find_checked_inner(resource, true, check)
    }

    pub(crate) fn find_checked_inner<T, R, F>(
        &self,
        resource: &mut R,
        insensitive: bool,
        check: &F,
    ) -> Option<usize>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        let path = resource.resource_path();
        let mut base_skip = path.skip as isize;
        let mut segments = mem::take(&mut path.segments);
        let path = resource.path();

        if self.key.is_empty() {
            if path == "/" {
                for val in &self.items {
                    match val {
                        Item::Value(val) => {
                            let v = match val {
                                Value::Slash(v)
                                | Value::Prefix(v)
                                | Value::PrefixSlash(v) => *v,
                                _ => continue,
                            };
                            if check(v, resource) {
                                resource.resource_path().segments = segments;
                                return Some(v);
                            }
                        }
                        Item::Subtree(tree) => {
                            let result = tree.find_inner_wrapped(
                                "",
                                resource,
                                check,
                                1,
                                &mut segments,
                                insensitive,
                                base_skip - 1,
                            );
                            if let Some((val, skip)) = result {
                                let path = resource.resource_path();
                                path.segments = segments;
                                path.skip += skip as u16;
                                return Some(val);
                            }
                        }
                    }
                }
            } else if path.is_empty() {
                for val in &self.items {
                    match val {
                        Item::Value(val) => {
                            let v = match val {
                                Value::Val(v) | Value::Prefix(v) => *v,
                                _ => continue,
                            };
                            if check(v, resource) {
                                resource.resource_path().segments = segments;
                                return Some(v);
                            }
                        }
                        _ => continue,
                    }
                }
            } else {
                let subtree_path = if let Some(spath) = path.strip_prefix('/') {
                    spath
                } else {
                    base_skip -= 1;
                    path
                };

                for val in &self.items {
                    match val {
                        Item::Value(val) => {
                            let v = match val {
                                Value::PrefixSlash(v) => *v,
                                _ => continue,
                            };
                            if check(v, resource) {
                                resource.resource_path().segments = segments;
                                return Some(v);
                            }
                        }
                        Item::Subtree(tree) => {
                            let result = tree.find_inner_wrapped(
                                subtree_path,
                                resource,
                                check,
                                1,
                                &mut segments,
                                insensitive,
                                base_skip,
                            );
                            if let Some((val, skip)) = result {
                                let path = resource.resource_path();
                                path.segments = segments;
                                path.skip += skip as u16;
                                return Some(val);
                            }
                        }
                    }
                }
            }
        } else if !path.is_empty() {
            let subtree_path = if let Some(spath) = path.strip_prefix('/') {
                spath
            } else {
                base_skip -= 1;
                path
            };
            let res = self.find_inner_wrapped(
                subtree_path,
                resource,
                check,
                1,
                &mut segments,
                insensitive,
                base_skip,
            );

            if let Some((val, skip)) = res {
                let path = resource.resource_path();
                path.segments = segments;
                path.skip += skip as u16;
                return Some(val);
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn find_inner_wrapped<T, R, F>(
        &self,
        path: &str,
        resource: &R,
        check: &F,
        skip: usize,
        segments: &mut Vec<(&'static str, PathItem)>,
        insensitive: bool,
        base_skip: isize,
    ) -> Option<(usize, usize)>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        let len = segments.len();
        let res = self.find_inner2(
            path,
            resource,
            check,
            skip,
            segments,
            insensitive,
            base_skip,
        );
        if res.is_none() {
            segments.truncate(len);
        }
        res
    }

    #[allow(clippy::too_many_arguments)]
    fn find_inner2<T, R, F>(
        &self,
        mut path: &str,
        resource: &R,
        check: &F,
        mut skip: usize,
        segments: &mut Vec<(&'static str, PathItem)>,
        insensitive: bool,
        base_skip: isize,
    ) -> Option<(usize, usize)>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        if self.key.is_empty() {
            if path.is_empty() {
                for val in &self.items {
                    let v = match val {
                        Item::Value(Value::Val(v)) | Item::Value(Value::Prefix(v)) => *v,
                        _ => continue,
                    };
                    if check(v, resource) {
                        return Some((v, skip));
                    }
                }
            } else {
                for val in &self.items {
                    let v = match val {
                        Item::Value(Value::Prefix(v)) => *v,
                        _ => continue,
                    };
                    if check(v, resource) {
                        return Some((v, skip));
                    }
                }
            }
            return None;
        }
        let mut key: &[_] = &self.key;

        loop {
            let idx = if let Some(idx) = path.find('/') {
                idx
            } else {
                path.len()
            };
            let segment = T::unquote(&path[..idx]);
            let quoted = matches!(segment, Cow::Owned(_));

            // check segment match
            let is_match = match key[0] {
                Segment::Static(ref pattern) => {
                    if insensitive {
                        pattern.eq_ignore_ascii_case(segment.as_ref())
                    } else {
                        pattern == segment.as_ref()
                    }
                }
                Segment::Dynamic {
                    ref pattern,
                    ref names,
                    tail,
                    ..
                } => {
                    // special treatment for tail, it matches regardless of sleshes
                    let seg = if tail { path } else { segment.as_ref() };

                    if let Some(captures) = pattern.captures(seg) {
                        let mut is_match = true;
                        for name in names.iter() {
                            if let Some(m) = captures.name(name) {
                                let item = if quoted {
                                    PathItem::Segment(m.as_str().to_string())
                                } else {
                                    PathItem::IdxSegment(
                                        (base_skip + (skip + m.start()) as isize) as u16,
                                        (base_skip + (skip + m.end()) as isize) as u16,
                                    )
                                };
                                segments.push((name, item));
                            } else {
                                log::error!(
                                    "Dynamic path match but not all segments found: {name}"
                                );
                                is_match = false;
                                break;
                            }
                        }

                        // we have to process checker for tail matches separately
                        if tail && is_match {
                            // checker
                            for val in &self.items {
                                if let Item::Value(val) = val {
                                    let v = val.value();
                                    if check(v, resource) {
                                        return Some((v, skip + idx));
                                    }
                                }
                            }
                        }

                        is_match
                    } else {
                        false
                    }
                }
            };

            if is_match {
                key = &key[1..];
                skip += idx + 1;

                // check path length (path is consumed)
                if idx == path.len() {
                    return {
                        if key.is_empty() {
                            // checker
                            for val in &self.items {
                                if let Item::Value(val) = val {
                                    let v = match val {
                                        Value::Val(v) | Value::Prefix(v) => *v,
                                        Value::Slash(_) | Value::PrefixSlash(_) => continue,
                                    };
                                    if check(v, resource) {
                                        return Some((v, skip));
                                    }
                                }
                            }
                        }
                        None
                    };
                } else if key.is_empty() {
                    path = &path[idx..];
                    let subtree_path = if path.len() != 1 { &path[1..] } else { path };

                    let p = if path.is_empty() {
                        PathState::Empty
                    } else if path == "/" {
                        PathState::Slash
                    } else {
                        PathState::Tail
                    };

                    for val in &self.items {
                        match val {
                            Item::Value(val) => {
                                let v = match val {
                                    Value::Val(v) => {
                                        if p == PathState::Empty {
                                            *v
                                        } else {
                                            continue;
                                        }
                                    }
                                    Value::Slash(v) => {
                                        if p == PathState::Slash {
                                            *v
                                        } else {
                                            continue;
                                        }
                                    }
                                    Value::Prefix(v) => {
                                        if p == PathState::Slash || p == PathState::Tail {
                                            *v
                                        } else {
                                            continue;
                                        }
                                    }
                                    Value::PrefixSlash(v) => {
                                        if p == PathState::Slash || p == PathState::Tail {
                                            *v
                                        } else {
                                            continue;
                                        }
                                    }
                                };
                                if check(v, resource) {
                                    return Some((v, skip - 1));
                                }
                            }
                            Item::Subtree(tree) => {
                                let result = tree.find_inner_wrapped(
                                    subtree_path,
                                    resource,
                                    check,
                                    skip,
                                    segments,
                                    insensitive,
                                    base_skip,
                                );
                                if result.is_some() {
                                    return result;
                                }
                            }
                        }
                    }
                    return None;
                } else {
                    path = &path[idx + 1..];
                }
            } else {
                return None;
            }
        }
    }
}

fn common_prefix(k1: &[Segment], k2: &[Segment]) -> usize {
    k1.iter()
        .zip(k2.iter())
        .take_while(|&(a, b)| a == b)
        .count()
}
