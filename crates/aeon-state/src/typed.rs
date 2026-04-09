//! Typed state wrappers over raw StateOps.
//!
//! Provides high-level, type-safe access to the state store.
//! All serialization is handled internally via bincode.
//!
//! - `ValueState<T>`: single value per key
//! - `MapState<V>`: sub-key → value map under a namespace
//! - `ListState<T>`: ordered list stored as a single serialized blob
//! - `CounterState`: atomic increment/decrement counter

use aeon_types::{AeonError, StateOps};
use serde::{Deserialize, Serialize};

/// Single-value state associated with a key prefix.
///
/// Stores one typed value. Common for per-key aggregations
/// (e.g., running total, last seen timestamp).
pub struct ValueState<'a, S: StateOps> {
    store: &'a S,
    key: Vec<u8>,
}

impl<'a, S: StateOps> ValueState<'a, S> {
    /// Create a ValueState bound to a specific key.
    pub fn new(store: &'a S, key: impl AsRef<[u8]>) -> Self {
        Self {
            store,
            key: key.as_ref().to_vec(),
        }
    }

    /// Get the current value, or None if not set.
    pub async fn get<T: for<'de> Deserialize<'de>>(&self) -> Result<Option<T>, AeonError> {
        match self.store.get(&self.key).await? {
            Some(bytes) => {
                let value: T = bincode::deserialize(&bytes).map_err(|e| {
                    AeonError::serialization(format!("ValueState deserialize failed: {e}"))
                })?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Set the value.
    pub async fn set<T: Serialize>(&self, value: &T) -> Result<(), AeonError> {
        let bytes = bincode::serialize(value)
            .map_err(|e| AeonError::serialization(format!("ValueState serialize failed: {e}")))?;
        self.store.put(&self.key, &bytes).await
    }

    /// Delete the value.
    pub async fn delete(&self) -> Result<(), AeonError> {
        self.store.delete(&self.key).await
    }

    /// Get the value or return a default if not set.
    pub async fn get_or_default<T: for<'de> Deserialize<'de> + Default>(
        &self,
    ) -> Result<T, AeonError> {
        Ok(self.get::<T>().await?.unwrap_or_default())
    }

    /// Update the value using a function. If no value exists, starts from default.
    pub async fn update<T: Serialize + for<'de> Deserialize<'de> + Default>(
        &self,
        f: impl FnOnce(T) -> T,
    ) -> Result<(), AeonError> {
        let current = self.get_or_default::<T>().await?;
        let updated = f(current);
        self.set(&updated).await
    }
}

/// Map state — a namespace of sub-key → value pairs.
///
/// Stores multiple typed values under a common prefix.
/// Example: MapState("user:123") → { "name": "alice", "age": 30 }
pub struct MapState<'a, S: StateOps> {
    store: &'a S,
    prefix: Vec<u8>,
}

impl<'a, S: StateOps> MapState<'a, S> {
    /// Create a MapState with a key prefix.
    pub fn new(store: &'a S, prefix: impl AsRef<[u8]>) -> Self {
        let mut p = prefix.as_ref().to_vec();
        // Ensure prefix ends with separator
        if !p.ends_with(b":") {
            p.push(b':');
        }
        Self { store, prefix: p }
    }

    fn sub_key(&self, field: &[u8]) -> Vec<u8> {
        let mut key = self.prefix.clone();
        key.extend_from_slice(field);
        key
    }

    /// Get a field value from the map.
    pub async fn get<V: for<'de> Deserialize<'de>>(
        &self,
        field: impl AsRef<[u8]>,
    ) -> Result<Option<V>, AeonError> {
        let key = self.sub_key(field.as_ref());
        match self.store.get(&key).await? {
            Some(bytes) => {
                let value: V = bincode::deserialize(&bytes).map_err(|e| {
                    AeonError::serialization(format!("MapState deserialize failed: {e}"))
                })?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Set a field value in the map.
    pub async fn put<V: Serialize>(
        &self,
        field: impl AsRef<[u8]>,
        value: &V,
    ) -> Result<(), AeonError> {
        let key = self.sub_key(field.as_ref());
        let bytes = bincode::serialize(value)
            .map_err(|e| AeonError::serialization(format!("MapState serialize failed: {e}")))?;
        self.store.put(&key, &bytes).await
    }

    /// Delete a field from the map.
    pub async fn delete(&self, field: impl AsRef<[u8]>) -> Result<(), AeonError> {
        let key = self.sub_key(field.as_ref());
        self.store.delete(&key).await
    }

    /// Check if a field exists.
    pub async fn contains(&self, field: impl AsRef<[u8]>) -> Result<bool, AeonError> {
        let key = self.sub_key(field.as_ref());
        Ok(self.store.get(&key).await?.is_some())
    }
}

/// List state — an ordered list stored as a single serialized value.
///
/// Suitable for small lists (up to ~1000 elements). For larger collections,
/// use MapState with numeric keys.
pub struct ListState<'a, S: StateOps> {
    store: &'a S,
    key: Vec<u8>,
}

impl<'a, S: StateOps> ListState<'a, S> {
    /// Create a ListState bound to a specific key.
    pub fn new(store: &'a S, key: impl AsRef<[u8]>) -> Self {
        Self {
            store,
            key: key.as_ref().to_vec(),
        }
    }

    /// Get the entire list.
    pub async fn get<T: for<'de> Deserialize<'de>>(&self) -> Result<Vec<T>, AeonError> {
        match self.store.get(&self.key).await? {
            Some(bytes) => {
                let list: Vec<T> = bincode::deserialize(&bytes).map_err(|e| {
                    AeonError::serialization(format!("ListState deserialize failed: {e}"))
                })?;
                Ok(list)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Append an element to the list.
    pub async fn push<T: Serialize + for<'de> Deserialize<'de>>(
        &self,
        item: &T,
    ) -> Result<(), AeonError> {
        let mut list: Vec<T> = self.get().await?;
        list.push(
            bincode::deserialize(&bincode::serialize(item).map_err(|e| {
                AeonError::serialization(format!("ListState serialize failed: {e}"))
            })?)
            .map_err(|e| AeonError::serialization(format!("ListState deserialize failed: {e}")))?,
        );
        let bytes = bincode::serialize(&list)
            .map_err(|e| AeonError::serialization(format!("ListState serialize failed: {e}")))?;
        self.store.put(&self.key, &bytes).await
    }

    /// Get the number of elements.
    pub async fn len<T: for<'de> Deserialize<'de>>(&self) -> Result<usize, AeonError> {
        Ok(self.get::<T>().await?.len())
    }

    /// Check if the list is empty.
    pub async fn is_empty<T: for<'de> Deserialize<'de>>(&self) -> Result<bool, AeonError> {
        Ok(self.get::<T>().await?.is_empty())
    }

    /// Clear the list.
    pub async fn clear(&self) -> Result<(), AeonError> {
        self.store.delete(&self.key).await
    }
}

/// Counter state — integer counter with increment/decrement.
///
/// Stored as a single i64 value. Supports atomic-style operations
/// (read-modify-write via the state store).
pub struct CounterState<'a, S: StateOps> {
    store: &'a S,
    key: Vec<u8>,
}

impl<'a, S: StateOps> CounterState<'a, S> {
    /// Create a CounterState bound to a specific key.
    pub fn new(store: &'a S, key: impl AsRef<[u8]>) -> Self {
        Self {
            store,
            key: key.as_ref().to_vec(),
        }
    }

    /// Get the current counter value (0 if not set).
    pub async fn get(&self) -> Result<i64, AeonError> {
        match self.store.get(&self.key).await? {
            Some(bytes) => {
                let value: i64 = bincode::deserialize(&bytes).map_err(|e| {
                    AeonError::serialization(format!("CounterState deserialize failed: {e}"))
                })?;
                Ok(value)
            }
            None => Ok(0),
        }
    }

    /// Increment the counter by `delta` and return the new value.
    pub async fn increment(&self, delta: i64) -> Result<i64, AeonError> {
        let current = self.get().await?;
        let new_value = current + delta;
        let bytes = bincode::serialize(&new_value)
            .map_err(|e| AeonError::serialization(format!("CounterState serialize failed: {e}")))?;
        self.store.put(&self.key, &bytes).await?;
        Ok(new_value)
    }

    /// Decrement the counter by `delta` and return the new value.
    pub async fn decrement(&self, delta: i64) -> Result<i64, AeonError> {
        self.increment(-delta).await
    }

    /// Reset the counter to zero.
    pub async fn reset(&self) -> Result<(), AeonError> {
        self.store.delete(&self.key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l1::L1Store;

    #[tokio::test]
    async fn value_state_get_set() {
        let store = L1Store::new();
        let vs = ValueState::new(&store, "count");

        assert_eq!(vs.get::<i64>().await.unwrap(), None);

        vs.set(&42i64).await.unwrap();
        assert_eq!(vs.get::<i64>().await.unwrap(), Some(42));
    }

    #[tokio::test]
    async fn value_state_update() {
        let store = L1Store::new();
        let vs = ValueState::new(&store, "total");

        vs.set(&10i64).await.unwrap();
        vs.update::<i64>(|v| v + 5).await.unwrap();
        assert_eq!(vs.get::<i64>().await.unwrap(), Some(15));
    }

    #[tokio::test]
    async fn value_state_get_or_default() {
        let store = L1Store::new();
        let vs = ValueState::new(&store, "missing");
        assert_eq!(vs.get_or_default::<i64>().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn value_state_delete() {
        let store = L1Store::new();
        let vs = ValueState::new(&store, "temp");
        vs.set(&"hello").await.unwrap();
        vs.delete().await.unwrap();
        assert_eq!(vs.get::<String>().await.unwrap(), None);
    }

    #[tokio::test]
    async fn value_state_string() {
        let store = L1Store::new();
        let vs = ValueState::new(&store, "name");
        vs.set(&String::from("alice")).await.unwrap();
        assert_eq!(
            vs.get::<String>().await.unwrap(),
            Some(String::from("alice"))
        );
    }

    #[tokio::test]
    async fn map_state_put_get() {
        let store = L1Store::new();
        let ms = MapState::new(&store, "user:1");

        ms.put("name", &String::from("alice")).await.unwrap();
        ms.put("age", &30i64).await.unwrap();

        assert_eq!(
            ms.get::<String>("name").await.unwrap(),
            Some(String::from("alice"))
        );
        assert_eq!(ms.get::<i64>("age").await.unwrap(), Some(30));
    }

    #[tokio::test]
    async fn map_state_delete_field() {
        let store = L1Store::new();
        let ms = MapState::new(&store, "user:2");

        ms.put("name", &String::from("bob")).await.unwrap();
        assert!(ms.contains("name").await.unwrap());

        ms.delete("name").await.unwrap();
        assert!(!ms.contains("name").await.unwrap());
    }

    #[tokio::test]
    async fn map_state_independent_namespaces() {
        let store = L1Store::new();
        let ms1 = MapState::new(&store, "ns1");
        let ms2 = MapState::new(&store, "ns2");

        ms1.put("key", &1i64).await.unwrap();
        ms2.put("key", &2i64).await.unwrap();

        assert_eq!(ms1.get::<i64>("key").await.unwrap(), Some(1));
        assert_eq!(ms2.get::<i64>("key").await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn list_state_push_get() {
        let store = L1Store::new();
        let ls = ListState::new(&store, "events");

        ls.push(&1i64).await.unwrap();
        ls.push(&2i64).await.unwrap();
        ls.push(&3i64).await.unwrap();

        let list = ls.get::<i64>().await.unwrap();
        assert_eq!(list, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn list_state_empty() {
        let store = L1Store::new();
        let ls = ListState::new(&store, "empty");
        let list = ls.get::<i64>().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn list_state_clear() {
        let store = L1Store::new();
        let ls = ListState::new(&store, "temp");
        ls.push(&42i64).await.unwrap();
        ls.clear().await.unwrap();
        assert!(ls.get::<i64>().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn counter_state_increment() {
        let store = L1Store::new();
        let cs = CounterState::new(&store, "clicks");

        assert_eq!(cs.get().await.unwrap(), 0);
        assert_eq!(cs.increment(1).await.unwrap(), 1);
        assert_eq!(cs.increment(5).await.unwrap(), 6);
        assert_eq!(cs.get().await.unwrap(), 6);
    }

    #[tokio::test]
    async fn counter_state_decrement() {
        let store = L1Store::new();
        let cs = CounterState::new(&store, "balance");

        cs.increment(100).await.unwrap();
        assert_eq!(cs.decrement(30).await.unwrap(), 70);
    }

    #[tokio::test]
    async fn counter_state_reset() {
        let store = L1Store::new();
        let cs = CounterState::new(&store, "counter");

        cs.increment(50).await.unwrap();
        cs.reset().await.unwrap();
        assert_eq!(cs.get().await.unwrap(), 0);
    }
}
