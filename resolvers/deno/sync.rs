// Copyright 2018-2025 the Deno authors. MIT license.

pub use inner::*;

#[cfg(feature = "sync")]
mod inner {
  #![allow(clippy::disallowed_types)]

  pub use std::sync::Arc as MaybeArc;

  pub use dashmap::DashMap as MaybeDashMap;
}

#[cfg(not(feature = "sync"))]
mod inner {
  use std::cell::Ref;
  use std::cell::RefCell;
  use std::collections::HashMap;
  use std::hash::BuildHasher;
  use std::hash::Hash;
  use std::hash::RandomState;
  pub use std::rc::Rc as MaybeArc;

  // Wrapper struct that exposes a subset of `DashMap` API.
  #[derive(Debug)]
  pub struct MaybeDashMap<K, V, S = RandomState>(RefCell<HashMap<K, V, S>>);

  impl<K, V, S> Default for MaybeDashMap<K, V, S>
  where
    K: Eq + Hash,
    S: Default + BuildHasher + Clone,
  {
    fn default() -> Self {
      Self(RefCell::new(Default::default()))
    }
  }

  impl<K: Eq + Hash, V, S: BuildHasher> MaybeDashMap<K, V, S> {
    pub fn get<'a>(&'a self, key: &K) -> Option<Ref<'a, V>> {
      Ref::filter_map(self.0.borrow(), |map| map.get(key)).ok()
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
      let mut inner = self.0.borrow_mut();
      inner.insert(key, value)
    }
  }
}
