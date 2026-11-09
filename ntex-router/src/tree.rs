use std::borrow::Cow;
use std::{marker::PhantomData, mem};

use super::path::{Path, PathItem};
use super::resource::{ResourceDef, Segment};
use super::{Resource, ResourcePath};

#[derive(Debug, Clone, Default)]
pub(super) struct Tree {
    key: Vec<Segment>,
    items: Vec<Item>,
    /// Hash table of first static segments of sub trees, empty slots have
    /// `pos == u32::MAX`
    statics: Vec<Static>,
    /// Positions of all other items, in order
    rest: Vec<u32>,
    /// Hash table of whole paths of static resources, only at the root
    exact: Vec<Exact>,
}

/// Static path with the result of matching it, empty slots have
/// `value == u32::MAX`
#[derive(Debug, Clone)]
struct Exact {
    key: StaticKey,
    path: Box<str>,
    value: u32,
    skip: u32,
}

/// Children that can match a path segment, in `items` order.
///
/// A sub tree that starts with a different static segment cannot match, it
/// is skipped. Yields `true` with a sub tree whose first static segment is
/// already known to match.
enum Children<'a, 'b> {
    Indexed {
        tree: &'a Tree,
        rest: usize,
        stat: Option<u32>,
    },
    Filtered {
        items: std::slice::Iter<'a, Item>,
        seg: &'b str,
    },
}

impl<'a> Iterator for Children<'a, '_> {
    type Item = (&'a Item, bool);

    #[inline]
    fn next(&mut self) -> Option<(&'a Item, bool)> {
        match self {
            Children::Indexed { tree, rest, stat } => {
                let (pos, matched) = match (tree.rest.get(*rest).copied(), *stat) {
                    (Some(r), Some(s)) if s > r => {
                        *rest += 1;
                        (r, false)
                    }
                    (_, Some(s)) => {
                        *stat = None;
                        (s, true)
                    }
                    (Some(r), None) => {
                        *rest += 1;
                        (r, false)
                    }
                    (None, None) => return None,
                };
                Some((&tree.items[pos as usize], matched))
            }
            Children::Filtered { items, seg } => items.find_map(|item| match item {
                Item::Subtree(tree) => match tree.key.first() {
                    Some(Segment::Static(pattern)) => {
                        pattern.eq_ignore_ascii_case(seg).then_some((item, true))
                    }
                    _ => Some((item, false)),
                },
                Item::Value(_) => Some((item, false)),
            }),
        }
    }
}

/// First static segment of a sub tree, with its position in `items`
#[derive(Debug, Clone)]
struct Static {
    key: StaticKey,
    pos: u32,
}

/// Length, leading and trailing bytes of a segment, cheap to compare.
///
/// Segments up to 16 bytes long are equal if their keys are equal.
type StaticKey = (usize, u64, u64);

#[inline]
fn static_key(s: &str) -> StaticKey {
    let b = s.as_bytes();
    let n = b.len();
    let (head, tail) = if n >= 8 {
        (
            u64::from_le_bytes(*b.first_chunk::<8>().unwrap()),
            u64::from_le_bytes(*b.last_chunk::<8>().unwrap()),
        )
    } else if n >= 4 {
        (
            u64::from(u32::from_le_bytes(*b.first_chunk::<4>().unwrap())),
            u64::from(u32::from_le_bytes(*b.last_chunk::<4>().unwrap())),
        )
    } else if n > 0 {
        let k = u64::from(b[0]) | u64::from(b[n / 2]) << 8 | u64::from(b[n - 1]) << 16;
        (k, 0)
    } else {
        (0, 0)
    };
    (n, head, tail)
}

#[inline]
fn static_slot(key: StaticKey, mask: usize) -> usize {
    let h = (key.1 ^ key.2.rotate_left(29) ^ key.0 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> 32) as usize & mask
}

/// Position of the first `/`, or the length of the path.
///
/// Faster than `str::find` for short segments.
#[inline]
fn find_slash(path: &str) -> usize {
    path.bytes().position(|b| b == b'/').unwrap_or(path.len())
}

/// First segment of the path, as it is matched by a sub tree
fn first_segment<T: ResourcePath>(path: &str, unquote: bool) -> (usize, Cow<'_, str>) {
    let idx = find_slash(path);
    let seg = &path[..idx];
    (
        idx,
        if unquote {
            T::unquote(seg)
        } else {
            Cow::Borrowed(seg)
        },
    )
}

/// State shared by all levels of a match
struct Ctx<'a, T, R, F> {
    resource: &'a R,
    check: &'a F,
    segments: &'a mut Vec<(&'static str, PathItem)>,
    insensitive: bool,
    /// Segments may need decoding
    unquote: bool,
    /// Position of the matched path in the full path, minus one
    base_skip: isize,
    _t: PhantomData<T>,
}

impl<T, R, F> Ctx<'_, T, R, F>
where
    F: Fn(usize, &R) -> bool,
{
    fn check(&self, v: usize) -> bool {
        (self.check)(v, self.resource)
    }
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

impl Value {
    fn value(&self) -> usize {
        match self {
            Value::Val(v) | Value::Slash(v) | Value::Prefix(v) | Value::PrefixSlash(v) => *v,
        }
    }
}

impl Tree {
    pub(crate) fn new(resource: &ResourceDef, value: usize) -> Tree {
        assert!(!resource.tp.is_empty(), "Non empty key is expected");

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

        let mut tree = Tree::child(resource.tp[0].tp.clone(), val);

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
            tree.insert_path(&seg.tp, val);
        }
        tree.build_index();
        tree
    }

    fn child(key: Vec<Segment>, value: Value) -> Tree {
        Tree {
            key,
            items: vec![Item::Value(value)],
            ..Default::default()
        }
    }

    fn build_index(&mut self) {
        let mut statics = Vec::new();
        self.rest.clear();
        for (pos, item) in self.items.iter_mut().enumerate() {
            let pos = pos as u32;
            if let Item::Subtree(tree) = item {
                tree.build_index();
                if let Some(Segment::Static(s)) = tree.key.first() {
                    statics.push((static_key(s), s.as_str(), pos));
                    continue;
                }
            }
            self.rest.push(pos);
        }

        // `insert_path` merges sub trees with the same first segment
        statics.sort_unstable();
        let empty = Static {
            key: (0, 0, 0),
            pos: u32::MAX,
        };
        let size = (statics.len() * 2).next_power_of_two();
        let mut table = vec![empty; size];
        for (i, (key, s, pos)) in statics.iter().enumerate() {
            if i > 0 && statics[i - 1].1 == *s {
                unreachable!("sub trees with the same first segment: {s}");
            }
            let mut slot = static_slot(*key, size - 1);
            while table[slot].pos != u32::MAX {
                slot = (slot + 1) & (size - 1);
            }
            table[slot] = Static {
                key: *key,
                pos: *pos,
            };
        }
        if statics.is_empty() {
            table.clear();
        }
        self.statics = table;
        self.rest.sort_unstable();
    }

    #[inline]
    fn children<'a, 'b>(&'a self, seg: &'b str, insensitive: bool) -> Children<'a, 'b> {
        // without static sub trees every item is in `rest`
        if insensitive && !self.statics.is_empty() {
            Children::Filtered {
                items: self.items.iter(),
                seg,
            }
        } else {
            let key = static_key(seg);
            let stat = self.find_static(key, seg);
            Children::Indexed {
                tree: self,
                rest: 0,
                stat,
            }
        }
    }

    #[inline]
    fn find_static(&self, key: StaticKey, seg: &str) -> Option<u32> {
        if self.statics.is_empty() {
            return None;
        }
        let mask = self.statics.len() - 1;
        let mut slot = static_slot(key, mask);
        loop {
            let s = &self.statics[slot];
            if s.pos == u32::MAX {
                return None;
            }
            if s.key == key && (key.0 <= 16 || self.static_at(s.pos) == seg.as_bytes()) {
                return Some(s.pos);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// First static segment of the sub tree at `pos`
    fn static_at(&self, pos: u32) -> &[u8] {
        match &self.items[pos as usize] {
            Item::Subtree(tree) => match tree.key.first() {
                Some(Segment::Static(s)) => s.as_bytes(),
                _ => unreachable!("the static table holds only static sub trees"),
            },
            Item::Value(_) => unreachable!("the static table holds only sub trees"),
        }
    }

    /// Builds the table of whole static paths.
    ///
    /// A path is added only if the tree itself matches it to the same
    /// resource first, without extracting segments, so a lookup hit gives
    /// the same result as the tree.
    pub(super) fn set_exact<'a, I>(&mut self, paths: I, insensitive: bool)
    where
        I: IntoIterator<Item = (usize, &'a str)>,
    {
        self.exact.clear();
        let mut found: Vec<(&str, u32, u32)> = Vec::new();
        for (value, path) in paths {
            if found.iter().any(|(p, _, _)| *p == path) {
                continue;
            }
            let mut res = Path::new(path);
            if self.find_checked_inner(&mut res, insensitive, &|_, _| true) == Some(value)
                && res.segments.is_empty()
                && let Ok(value) = u32::try_from(value)
                && value != u32::MAX
            {
                found.push((path, value, res.skip));
            }
        }
        if found.is_empty() {
            return;
        }

        let size = (found.len() * 2).next_power_of_two();
        let mut table = vec![
            Exact {
                key: (0, 0, 0),
                path: Box::default(),
                value: u32::MAX,
                skip: 0,
            };
            size
        ];
        for (path, value, skip) in found {
            let key = static_key(path);
            let mut slot = static_slot(key, size - 1);
            while table[slot].value != u32::MAX {
                slot = (slot + 1) & (size - 1);
            }
            table[slot] = Exact {
                key,
                path: path.into(),
                value,
                skip,
            };
        }
        self.exact = table;
    }

    #[inline]
    fn find_exact(&self, path: &str) -> Option<&Exact> {
        let mask = self.exact.len().checked_sub(1)?;
        let key = static_key(path);
        let mut slot = static_slot(key, mask);
        loop {
            let e = &self.exact[slot];
            if e.value == u32::MAX {
                return None;
            }
            if e.key == key && (key.0 <= 16 || &*e.path == path) {
                return Some(e);
            }
            slot = (slot + 1) & mask;
        }
    }

    pub(super) fn insert(&mut self, resource: &ResourceDef, value: usize) {
        self.exact.clear();
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
            self.insert_path(&seg.tp, value);
        }
        self.build_index();
    }

    fn insert_path(&mut self, key: &[Segment], value: Value) {
        let p = common_prefix(&self.key, key);

        // split current key, and move all children to sub tree
        if p < self.key.len() {
            let child = Tree {
                key: self.key.split_off(p),
                items: mem::take(&mut self.items),
                ..Default::default()
            };
            self.items.push(Item::Subtree(child));
        }

        // update value if key is the same
        if p == key.len() {
            match value {
                Value::PrefixSlash(v) => {
                    self.items
                        .push(Item::Subtree(Tree::child(Vec::new(), Value::Prefix(v))));
                }
                value => self.items.push(Item::Value(value)),
            }
        } else {
            // insert into sub tree
            for child in &mut self.items {
                if let Item::Subtree(tree) = child
                    && common_prefix(&tree.key, &key[p..]) > 0
                {
                    tree.insert_path(&key[p..], value);
                    return;
                }
            }
            self.items
                .push(Item::Subtree(Tree::child(key[p..].to_vec(), value)));
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
        let base_skip = path.skip as isize;
        // `check` sees no segments, they are restored after a match
        let mut segments = mem::take(&mut path.segments);

        let res = {
            let resource = &*resource;
            let path = resource.path();
            let unquote = T::needs_unquote(path);
            if let Some(e) = self.find_exact(path)
                && (!unquote || matches!(T::unquote(path), Cow::Borrowed(_)))
                && check(e.value as usize, resource)
            {
                Some((e.value as usize, Some(e.skip as usize)))
            } else {
                let mut ctx = Ctx {
                    resource,
                    check,
                    segments: &mut segments,
                    insensitive,
                    unquote,
                    base_skip,
                    _t: PhantomData,
                };
                self.find_root(&mut ctx, path)
            }
        };
        let (v, skip) = res?;

        let path = resource.resource_path();
        path.segments = segments;
        if let Some(skip) = skip {
            path.skip(skip as u32);
        }
        Some(v)
    }

    /// Matches the path at the root, returns the value and how much of the
    /// path to skip if a sub tree matched it
    fn find_root<T, R, F>(
        &self,
        ctx: &mut Ctx<'_, T, R, F>,
        path: &str,
    ) -> Option<(usize, Option<usize>)>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        if self.key.is_empty() {
            if path == "/" {
                ctx.base_skip -= 1;
                for (val, matched) in self.children("", ctx.insensitive) {
                    match val {
                        Item::Value(val) => {
                            let v = match val {
                                Value::Slash(v) | Value::Prefix(v) | Value::PrefixSlash(v) => *v,
                                Value::Val(_) => continue,
                            };
                            if ctx.check(v) {
                                return Some((v, None));
                            }
                        }
                        Item::Subtree(tree) => {
                            let first = Some((0, "", false, matched));
                            if let Some((v, skip)) = tree.find_inner_wrapped(ctx, "", 1, first) {
                                return Some((v, Some(skip)));
                            }
                        }
                    }
                }
            } else if path.is_empty() {
                for val in &self.items {
                    if let Item::Value(val) = val {
                        let v = match val {
                            Value::Val(v) | Value::Prefix(v) => *v,
                            _ => continue,
                        };
                        if ctx.check(v) {
                            return Some((v, None));
                        }
                    }
                }
            } else {
                let subtree_path = if let Some(spath) = path.strip_prefix('/') {
                    spath
                } else {
                    ctx.base_skip -= 1;
                    path
                };

                let (idx, seg) = first_segment::<T>(subtree_path, ctx.unquote);
                let quoted = matches!(seg, Cow::Owned(_));
                for (val, matched) in self.children(&seg, ctx.insensitive) {
                    match val {
                        Item::Value(val) => {
                            let v = match val {
                                Value::PrefixSlash(v) => *v,
                                _ => continue,
                            };
                            if ctx.check(v) {
                                return Some((v, None));
                            }
                        }
                        Item::Subtree(tree) => {
                            let first = Some((idx, &*seg, quoted, matched));
                            let result = tree.find_inner_wrapped(ctx, subtree_path, 1, first);
                            if let Some((v, skip)) = result {
                                return Some((v, Some(skip)));
                            }
                        }
                    }
                }
            }
        } else if !path.is_empty() {
            let subtree_path = if let Some(spath) = path.strip_prefix('/') {
                spath
            } else {
                ctx.base_skip -= 1;
                path
            };
            let (v, skip) = self.find_inner_wrapped(ctx, subtree_path, 1, None)?;
            return Some((v, Some(skip)));
        }
        None
    }

    fn find_inner_wrapped<T, R, F>(
        &self,
        ctx: &mut Ctx<'_, T, R, F>,
        path: &str,
        skip: usize,
        first: Option<(usize, &str, bool, bool)>,
    ) -> Option<(usize, usize)>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        let len = ctx.segments.len();
        let res = self.find_inner2(ctx, path, skip, first);
        if res.is_none() {
            ctx.segments.truncate(len);
        }
        res
    }

    fn find_inner2<T, R, F>(
        &self,
        ctx: &mut Ctx<'_, T, R, F>,
        mut path: &str,
        mut skip: usize,
        first: Option<(usize, &str, bool, bool)>,
    ) -> Option<(usize, usize)>
    where
        T: ResourcePath,
        R: Resource<T>,
        F: Fn(usize, &R) -> bool,
    {
        if self.key.is_empty() {
            // only a `/` prefix adds a sub tree with an empty key, see `insert_path`
            for val in &self.items {
                let v = match val {
                    Item::Value(Value::Prefix(v)) => *v,
                    _ => unreachable!("an empty key holds only prefix values"),
                };
                if ctx.check(v) {
                    return Some((v, skip));
                }
            }
            return None;
        }
        let mut key: &[_] = &self.key;
        let mut first = first;

        loop {
            // the first segment may be already split by the parent
            let (idx, segment, quoted, matched) =
                if let Some((idx, seg, quoted, matched)) = first.take() {
                    (idx, Cow::Borrowed(seg), quoted, matched)
                } else {
                    let idx = find_slash(path);
                    let segment = if ctx.unquote {
                        T::unquote(&path[..idx])
                    } else {
                        Cow::Borrowed(&path[..idx])
                    };
                    let quoted = matches!(segment, Cow::Owned(_));
                    (idx, segment, quoted, false)
                };

            // check segment match
            let is_match = match key[0] {
                Segment::Static(ref pattern) => {
                    if matched {
                        // the parent index already compared it
                        true
                    } else if ctx.insensitive {
                        pattern.eq_ignore_ascii_case(segment.as_ref())
                    } else {
                        pattern == segment.as_ref()
                    }
                }
                Segment::Dynamic {
                    ref pattern,
                    ref names,
                    tail,
                    whole,
                } => {
                    // special treatment for tail, it matches regardless of sleshes
                    let seg = if tail { path } else { segment.as_ref() };

                    if whole {
                        // same result as `^(?P<name>.+)$`, `.` does not match `\n`
                        if seg.is_empty() || seg.contains('\n') {
                            false
                        } else {
                            let item = if quoted {
                                PathItem::Segment(seg.to_string())
                            } else {
                                PathItem::IdxSegment(
                                    (ctx.base_skip + skip as isize) as u32,
                                    (ctx.base_skip + (skip + seg.len()) as isize) as u32,
                                )
                            };
                            ctx.segments.push((names[0].0, item));
                            true
                        }
                    } else if let Some(captures) = pattern.captures(seg) {
                        // groups of custom regexes may not participate in the
                        // match, they are left out of the segments
                        let mut is_match = true;
                        for &(name, required) in names {
                            if let Some(m) = captures.name(name) {
                                let item = if quoted {
                                    PathItem::Segment(m.as_str().to_string())
                                } else {
                                    PathItem::IdxSegment(
                                        (ctx.base_skip + (skip + m.start()) as isize) as u32,
                                        (ctx.base_skip + (skip + m.end()) as isize) as u32,
                                    )
                                };
                                ctx.segments.push((name, item));
                            } else if required {
                                is_match = false;
                                break;
                            }
                        }

                        // we have to process checker for tail matches separately
                        if tail && is_match {
                            // checker
                            for val in &self.items {
                                let Item::Value(val) = val else {
                                    unreachable!("a tail has no sub trees");
                                };
                                let v = val.value();
                                if ctx.check(v) {
                                    return Some((v, skip + idx));
                                }
                            }
                            // tail consumed the rest of the path, values are
                            // already checked
                            if key.len() != 1 {
                                unreachable!("a tail is the last segment");
                            }
                            return None;
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
                                    if ctx.check(v) {
                                        return Some((v, skip));
                                    }
                                }
                            }
                        }
                        None
                    };
                } else if key.is_empty() {
                    // `idx < path.len()`, so the rest of the path is not empty
                    path = &path[idx..];
                    let subtree_path = if path.len() == 1 { path } else { &path[1..] };
                    let slash = path == "/";

                    let (sidx, seg) = first_segment::<T>(subtree_path, ctx.unquote);
                    let squoted = matches!(seg, Cow::Owned(_));
                    for (val, matched) in self.children(&seg, ctx.insensitive) {
                        match val {
                            Item::Value(val) => {
                                let v = match val {
                                    Value::Val(_) => continue,
                                    Value::Slash(v) => {
                                        if slash {
                                            *v
                                        } else {
                                            continue;
                                        }
                                    }
                                    Value::Prefix(v) | Value::PrefixSlash(v) => *v,
                                };
                                if ctx.check(v) {
                                    return Some((v, skip - 1));
                                }
                            }
                            Item::Subtree(tree) => {
                                let result = tree.find_inner_wrapped(
                                    ctx,
                                    subtree_path,
                                    skip,
                                    Some((sidx, &seg, squoted, matched)),
                                );
                                if result.is_some() {
                                    return result;
                                }
                            }
                        }
                    }
                    return None;
                }
                path = &path[idx + 1..];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::Path;

    impl Tree {
        /// Visit every child in order, like before the index existed, without
        /// the table of static paths
        fn unindexed(&mut self) {
            self.exact.clear();
            self.statics.clear();
            self.rest = (0..self.items.len() as u32).collect();
            for item in &mut self.items {
                if let Item::Subtree(tree) = item {
                    tree.unindexed();
                }
            }
        }
    }

    /// STATEMENT: Indexed child lookup selects the same resource and segments as visiting
    /// every child in order.
    ///
    /// HOW:
    /// - Build a tree from overlapping static, dynamic, prefix, tail, slash and
    ///   multi-pattern resources, registered so dynamic routes precede static ones.
    ///   Static segments include short ones, and long ones that share length,
    ///   leading and trailing bytes, a duplicate, a percent sign and a route
    ///   shadowed by a dynamic one. Repeat without the root `/{name}` route that
    ///   shadows most static paths.
    /// - Copy it and make the copy visit every child, and add the table of
    ///   static paths to other copies, case sensitive and insensitive.
    /// - Match plain and percent-encoded paths with both trees, case sensitive and
    ///   insensitive, while the check rejects different sets of resources to force
    ///   backtracking. Also match each path after a skipped prefix with an
    ///   extracted outer segment.
    /// - Compare the resource, the unmatched path and the extracted segments.
    ///   `check` passes only when it sees no segments, like inside the tree.
    #[test]
    fn indexed_children_match_like_linear_scan() {
        let defs = [
            ResourceDef::new("/"),
            ResourceDef::new(""),
            ResourceDef::new("/{name}"),
            ResourceDef::new("/echo"),
            ResourceDef::new("/Echo"),
            ResourceDef::new("/json/{count}"),
            ResourceDef::new("/json/{count}/x"),
            ResourceDef::new("/json/list"),
            ResourceDef::new("/a/{x}/c"),
            ResourceDef::new("/a/b/d"),
            ResourceDef::prefix("/api"),
            ResourceDef::prefix("/api/v1"),
            ResourceDef::new("/files/{tail}*"),
            ResourceDef::new("/user/{id}/"),
            ResourceDef::new("/pipeline"),
            ResourceDef::new("/\u{e9}"),
            ResourceDef::new(["/m1", "/m2/{x}"]),
            ResourceDef::new("/v{ver}/info"),
            ResourceDef::new("/ab"),
            ResourceDef::new("/abc"),
            ResourceDef::new("/baseline1"),
            ResourceDef::new("/baseline11"),
            ResourceDef::new("/aaaaaaaa-x-bbbbbbbb"),
            ResourceDef::new("/aaaaaaaa-y-bbbbbbbb"),
            ResourceDef::new("/echo"),
            ResourceDef::new("/a%2F"),
            ResourceDef::new("/user/{id}"),
            ResourceDef::new("/user/me"),
            ResourceDef::new("/slash/"),
            ResourceDef::new(["/m3", "/m1"]),
        ];
        // without `/{name}`, which shadows most static paths
        let unshadowed: Vec<_> = defs
            .iter()
            .filter(|d| d.pattern() != "/{name}")
            .cloned()
            .collect();
        for (defs, min_exact) in [(&defs[..], 3), (&unshadowed[..], 15)] {
            compare_with_linear_scan(defs, min_exact);
        }
    }

    fn compare_with_linear_scan(defs: &[ResourceDef], min_exact: usize) {
        let mut tree = Tree::new(&defs[0], 0);
        for (idx, def) in defs.iter().enumerate().skip(1) {
            tree.insert(def, idx);
        }
        let mut slow = tree.clone();
        slow.unindexed();
        assert!(tree.statics.iter().filter(|s| s.pos != u32::MAX).count() > 3);
        let statics = defs.iter().enumerate().filter(|(_, d)| !d.prefix);
        let statics = statics.flat_map(|(idx, d)| {
            d.patterns()
                .iter()
                .filter(|p| !p.contains(['{', '}']))
                .map(move |p| (idx, p.as_str()))
        });
        let mut fast = tree.clone();
        fast.set_exact(statics.clone(), false);
        let mut fast_i = tree;
        fast_i.set_exact(statics, true);
        assert!(fast.exact.iter().filter(|e| e.value != u32::MAX).count() > min_exact);

        let paths = [
            "/",
            "/echo",
            "/ECHO",
            "/Echo",
            "/e%63ho",
            "/json/12",
            "/json/list",
            "/json/12/x",
            "/json/",
            "/json",
            "/a/b/c",
            "/a/b/d",
            "/a/z/d",
            "/api",
            "/api/",
            "/api/v1/x",
            "/api/v2",
            "/files/a/b",
            "/user/1/",
            "/user/1",
            "/pipeline",
            "/%C3%A9",
            "/m1",
            "/m2/q",
            "/v2/info",
            "/unknown/deep",
            "//",
            "/a//c",
            "/ab",
            "/abc",
            "/abd",
            "/baseline1",
            "/baseline11",
            "/baseline12",
            "/aaaaaaaa-x-bbbbbbbb",
            "/aaaaaaaa-y-bbbbbbbb",
            "/aaaaaaaa-z-bbbbbbbb",
            "/a%2F",
            "/a%2f",
            "/user/me",
            "/slash/",
            "/slash",
            "/m3",
        ];
        let rejects: [&[usize]; 5] = [&[], &[2], &[2, 3, 5], &[8, 10, 13], &[0, 1, 2, 4, 6]];

        for (p, pre) in paths.iter().flat_map(|p| [(*p, ""), (*p, "/pre")]) {
            for reject in rejects {
                for insensitive in [false, true] {
                    // the tree hides matched segments from `check`
                    let check =
                        |v: usize, r: &Path<urly::Url>| r.is_empty() && !reject.contains(&v);
                    let new = || {
                        let mut r = Path::new(urly::Url::try_from(format!("{pre}{p}")).unwrap());
                        if !pre.is_empty() {
                            r.skip(pre.len() as u32);
                            r.add_static("outer", "x");
                        }
                        r
                    };
                    let (mut r1, mut r2) = (new(), new());
                    let (v1, v2) = if insensitive {
                        (
                            fast_i.find_checked_insensitive(&mut r1, &check),
                            slow.find_checked_insensitive(&mut r2, &check),
                        )
                    } else {
                        (
                            fast.find_checked(&mut r1, &check),
                            slow.find_checked(&mut r2, &check),
                        )
                    };
                    let ctx =
                        format!("{p:?} pre={pre:?} reject={reject:?} insensitive={insensitive}");
                    assert_eq!(v1, v2, "{ctx}");
                    assert_eq!(r1.path(), r2.path(), "{ctx}");
                    assert_eq!(
                        r1.iter().collect::<Vec<_>>(),
                        r2.iter().collect::<Vec<_>>(),
                        "{ctx}"
                    );
                }
            }
        }
    }
}
