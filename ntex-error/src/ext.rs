use std::{any::Any, any::TypeId, collections::HashMap, sync::Arc};

use foldhash::fast::RandomState;

#[derive(Clone)]
/// A type map of request extensions.
pub(crate) struct Extensions {
    pub(crate) map: HashMap<TypeId, Arc<dyn Any + Sync + Send>, RandomState>,
}

impl Default for Extensions {
    fn default() -> Self {
        Extensions {
            map: HashMap::with_capacity_and_hasher(0, RandomState::default()),
        }
    }
}

impl Extensions {
    /// Insert a type into this `Extensions`.
    pub(crate) fn insert<T: Sync + Send + 'static>(&mut self, val: T) {
        self.map.insert(TypeId::of::<T>(), Arc::new(val));
    }

    /// Get a reference to a type previously inserted on this `Extensions`.
    pub(crate) fn get<T: 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }
}
