use std::{any::Any, any::TypeId, fmt};

#[derive(Default)]
/// A type map of request extensions.
pub struct Extensions {
    map: crate::HashMap<TypeId, Box<dyn Any>>,
}

impl Extensions {
    /// Creates an empty `Extensions` map.
    #[inline]
    pub fn new() -> Extensions {
        Extensions {
            map: crate::HashMap::default(),
        }
    }

    /// Insert a type into this `Extensions`.
    ///
    /// Returns the previous value of the same type, if present.
    pub fn insert<T: 'static>(&mut self, val: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(val))
            .and_then(|item| item.downcast::<T>().map(|boxed| *boxed).ok())
    }

    /// Returns `true` if the map contains a value of type `T`.
    pub fn contains<T: 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    /// Returns a reference to a previously inserted value of type `T`.
    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }

    /// Returns a mutable reference to a previously inserted value of type `T`.
    pub fn get_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_mut())
    }

    /// Remove a type from this `Extensions`.
    ///
    /// Returns the removed value, if present.
    pub fn remove<T: 'static>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast().ok().map(|boxed| *boxed))
    }

    /// Moves all values from `other` into this map.
    pub fn extend(&mut self, other: Extensions) {
        self.map.extend(other.map);
    }

    #[inline]
    /// Returns `true` if the map contains no values.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Removes all values from the map.
    #[inline]
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("size", &self.map.len())
            .finish()
    }
}

/// STATEMENT: Extension debug output reports the number of stored types.
///
/// HOW:
/// - Insert one typed value and compare the complete debug representation.
#[test]
fn test_debug() {
    let mut map = Extensions::new();
    map.insert::<i8>(123);
    assert_eq!(format!("{map:?}"), "Extensions { size: 1 }");
}

/// STATEMENT: Inserting a duplicate type replaces its value, and removal clears it.
///
/// HOW:
/// - Insert two `i8` values and verify the second insertion returns the first.
/// - Remove the type and check that lookup no longer finds it.
#[test]
fn test_remove() {
    let mut map = Extensions::new();

    map.insert::<i8>(123);
    assert!(map.get::<i8>().is_some());
    assert_eq!(map.insert::<i8>(10), Some(123));

    map.remove::<i8>();
    assert!(map.get::<i8>().is_none());
}

/// STATEMENT: Clearing extensions removes every type and leaves the map reusable.
///
/// HOW:
/// - Insert three integer types and verify they are present.
/// - Clear them, check their absence, and insert and retrieve a new value.
#[test]
fn test_clear() {
    let mut map = Extensions::new();

    map.insert::<i8>(8);
    map.insert::<i16>(16);
    map.insert::<i32>(32);

    assert!(map.contains::<i8>());
    assert!(map.contains::<i16>());
    assert!(map.contains::<i32>());

    map.clear();

    assert!(!map.contains::<i8>());
    assert!(!map.contains::<i16>());
    assert!(!map.contains::<i32>());

    map.insert::<i8>(10);
    assert_eq!(*map.get::<i8>().unwrap(), 10);
}

/// STATEMENT: Signed and unsigned integer types occupy distinct extension entries.
///
/// HOW:
/// - Insert values for ten integer types and verify each typed lookup succeeds.
#[test]
fn test_integers() {
    let mut map = Extensions::new();

    map.insert::<i8>(8);
    map.insert::<i16>(16);
    map.insert::<i32>(32);
    map.insert::<i64>(64);
    map.insert::<i128>(128);
    map.insert::<u8>(8);
    map.insert::<u16>(16);
    map.insert::<u32>(32);
    map.insert::<u64>(64);
    map.insert::<u128>(128);
    assert!(map.get::<i8>().is_some());
    assert!(map.get::<i16>().is_some());
    assert!(map.get::<i32>().is_some());
    assert!(map.get::<i64>().is_some());
    assert!(map.get::<i128>().is_some());
    assert!(map.get::<u8>().is_some());
    assert!(map.get::<u16>().is_some());
    assert!(map.get::<u32>().is_some());
    assert!(map.get::<u64>().is_some());
    assert!(map.get::<u128>().is_some());
}

/// STATEMENT: Different generic instantiations have independent extension entries.
///
/// HOW:
/// - Insert three payload types wrapped in the same generic type.
/// - Retrieve each instantiation and verify its original fields.
#[test]
fn test_composition() {
    struct Magi<T>(T);

    struct Madoka {
        god: bool,
    }

    struct Homura {
        attempts: usize,
    }

    struct Mami {
        guns: usize,
    }

    let mut map = Extensions::new();

    map.insert(Magi(Madoka { god: false }));
    map.insert(Magi(Homura { attempts: 0 }));
    map.insert(Magi(Mami { guns: 999 }));

    assert!(!map.get::<Magi<Madoka>>().unwrap().0.god);
    assert_eq!(0, map.get::<Magi<Homura>>().unwrap().0.attempts);
    assert_eq!(999, map.get::<Magi<Mami>>().unwrap().0.guns);
}

/// STATEMENT: Typed access and removal preserve unrelated extension values.
///
/// HOW:
/// - Insert an integer and a custom type, then inspect shared and mutable integer references.
/// - Remove the integer and check missing-type lookups and the surviving custom value.
#[test]
fn test_extensions() {
    #[derive(Debug, PartialEq)]
    struct MyType(i32);

    let mut extensions = Extensions::new();

    extensions.insert(5i32);
    extensions.insert(MyType(10));

    assert_eq!(extensions.get(), Some(&5i32));
    assert_eq!(extensions.get_mut(), Some(&mut 5i32));

    assert_eq!(extensions.remove::<i32>(), Some(5i32));
    assert!(extensions.get::<i32>().is_none());

    assert_eq!(extensions.get::<bool>(), None);
    assert_eq!(extensions.get(), Some(&MyType(10)));
}

/// STATEMENT: Extending an extension map retains existing entries and adds incoming types.
///
/// HOW:
/// - Merge a custom-type map into an integer map after checking empty-state transitions.
/// - Access and remove the integer while verifying the imported custom value remains.
#[test]
fn test_extend() {
    #[derive(Debug, PartialEq)]
    struct MyType(i32);

    let mut extensions2 = Extensions::new();
    assert!(extensions2.is_empty());
    extensions2.insert(MyType(10));
    assert!(!extensions2.is_empty());

    let mut extensions = Extensions::new();
    extensions.insert(5i32);
    extensions.extend(extensions2);

    assert_eq!(extensions.get(), Some(&5i32));
    assert_eq!(extensions.get_mut(), Some(&mut 5i32));

    assert_eq!(extensions.remove::<i32>(), Some(5i32));
    assert!(extensions.get::<i32>().is_none());

    assert_eq!(extensions.get::<bool>(), None);
    assert_eq!(extensions.get(), Some(&MyType(10)));
}
