use std::borrow::Cow;
use std::mem;

use super::path::PathItem;
use super::resource::{ResourceDef, Segment};
use super::{Resource, ResourcePath};

#[derive(Debug)]
pub(super) struct Tree {
    key: Vec<Segment>,
    value: Vec<Value>,
    children: Vec<Tree>,
}

#[derive(Copy, Clone, Debug)]
enum Value {
    Val(usize),
    Slesh(usize),
    Prefix(usize),
    PrefixSlesh(usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PathState {
    Empty,
    Slesh,
    Tail,
}

impl Value {
    fn value(&self) -> usize {
        match self {
            Value::Val(v) => *v,
            Value::Slesh(v) => *v,
            Value::Prefix(v) => *v,
            Value::PrefixSlesh(v) => *v,
        }
    }
}

impl Default for Tree {
    fn default() -> Tree {
        Tree {
            key: Vec::new(),
            value: Vec::new(),
            children: Vec::new(),
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
                Value::PrefixSlesh(value)
            } else {
                Value::Slesh(value)
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
                    Value::PrefixSlesh(value)
                } else {
                    Value::Slesh(value)
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
        let value = if let Some(val) = value {
            vec![val]
        } else {
            Vec::new()
        };
        Tree {
            key,
            value,
            children: Vec::new(),
        }
    }

    pub(super) fn insert(&mut self, resource: &ResourceDef, value: usize) {
        for seg in &resource.tp {
            let value = if seg.slesh {
                if resource.prefix {
                    Value::PrefixSlesh(value)
                } else {
                    Value::Slesh(value)
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
                value: mem::replace(&mut self.value, Vec::new()),
                children: mem::take(&mut self.children),
            };
            self.children.push(child);
        }
        // update value if key is the same
        if p == key.len() {
            self.value.push(value);
        } else {
            // insert into sub tree
            let mut child = self
                .children
                .iter_mut()
                .find(|x| common_prefix(&x.key, &key[p..]) > 0);
            if let Some(ref mut child) = child {
                child.insert_path(key[p..].to_vec(), value)
            } else {
                self.children
                    .push(Tree::child(key[p..].to_vec(), Some(value)));
            }
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

    pub(crate) fn find_checked<T, R, F>(
        &self,
        resource: &mut R,
        check: &F,
    ) -> Option<usize>
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
                for val in &self.value {
                    let v = match val {
                        Value::Slesh(v) | Value::Prefix(v) => *v,
                        _ => continue,
                    };
                    if check(v, resource) {
                        return Some(v);
                    }
                }
            } else if path.is_empty() {
                for val in &self.value {
                    let v = match val {
                        Value::Val(v) | Value::Prefix(v) => *v,
                        _ => continue,
                    };
                    if check(v, resource) {
                        return Some(v);
                    }
                }
            }

            let path = if path.starts_with('/') {
                &path[1..]
            } else {
                base_skip -= 1;
                path
            };

            let res = self
                .children
                .iter()
                .map(|x| {
                    x.find_inner2(
                        path,
                        resource,
                        check,
                        1,
                        &mut segments,
                        insensitive,
                        base_skip,
                    )
                })
                .filter_map(|x| x)
                .next();

            return if let Some((val, skip)) = res {
                let path = resource.resource_path();
                path.segments = segments;
                path.skip += skip as u16;
                Some(val)
            } else {
                None
            };
        }

        if path.is_empty() {
            return None;
        }

        let path = if path.starts_with('/') {
            &path[1..]
        } else {
            base_skip -= 1;
            path
        };

        if let Some((val, skip)) = self.find_inner2(
            path,
            resource,
            check,
            1,
            &mut segments,
            insensitive,
            base_skip,
        ) {
            let path = resource.resource_path();
            path.segments = segments;
            path.skip += skip as u16;
            Some(val)
        } else {
            None
        }
    }

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
        let mut key: &[_] = &self.key;

        loop {
            let idx = if let Some(idx) = path.find('/') {
                idx
            } else {
                path.len()
            };
            let segment = T::unquote(&path[..idx]);
            let quoted = if let Cow::Owned(_) = segment {
                true
            } else {
                false
            };

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
                            if let Some(m) = captures.name(&name) {
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
                                    "Dynamic path match but not all segments found: {}",
                                    name
                                );
                                is_match = false;
                                break;
                            }
                        }

                        // we have to process checker for tail matches separately
                        if tail && is_match {
                            // checker
                            for val in &self.value {
                                let v = val.value();
                                if check(v, resource) {
                                    return Some((v, skip + idx));
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
                            for val in &self.value {
                                let v = match val {
                                    Value::Val(v) | Value::Prefix(v) => *v,
                                    Value::Slesh(_) | Value::PrefixSlesh(_) => continue,
                                };
                                if check(v, resource) {
                                    return Some((v, skip));
                                }
                            }
                        }
                        None
                    };
                } else if key.is_empty() {
                    path = &path[idx..];

                    let p = if path.is_empty() {
                        PathState::Empty
                    } else if path == "/" {
                        PathState::Slesh
                    } else {
                        PathState::Tail
                    };

                    for val in &self.value {
                        let v = match val {
                            Value::Val(v) => {
                                if p == PathState::Empty {
                                    *v
                                } else {
                                    continue;
                                }
                            }
                            Value::Slesh(v) => {
                                if p == PathState::Slesh {
                                    *v
                                } else {
                                    continue;
                                }
                            }
                            Value::Prefix(v) => {
                                if p == PathState::Slesh || p == PathState::Tail {
                                    *v
                                } else {
                                    continue;
                                }
                            }
                            Value::PrefixSlesh(v) => {
                                if p == PathState::Slesh || p == PathState::Tail {
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

                    let path = if path.len() != 1 { &path[1..] } else { path };

                    return self
                        .children
                        .iter()
                        .map(|x| {
                            x.find_inner2(
                                path,
                                resource,
                                check,
                                skip,
                                segments,
                                insensitive,
                                base_skip,
                            )
                        })
                        .filter_map(|x| x)
                        .next();
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
