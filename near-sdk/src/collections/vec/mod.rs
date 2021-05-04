//! A vector implemented on a trie. Unlike standard vector does not support insertion and removal
//! of an element results in the last element being placed in the empty position.

mod impls;
mod iter;

use crate::collections::append_slice;
use crate::{env, CacheCell, CacheEntry, EntryState, IntoStorageKey};
use borsh::{BorshDeserialize, BorshSerialize};
use std::collections::{btree_map::Entry, BTreeMap};
use std::ptr::NonNull;

const ERR_INCONSISTENT_STATE: &[u8] = b"The collection is an inconsistent state. Did previous smart contract execution terminate unexpectedly?";
const ERR_ELEMENT_DESERIALIZATION: &[u8] = b"Cannot deserialize element";
const ERR_ELEMENT_SERIALIZATION: &[u8] = b"Cannot serialize element";
const ERR_INDEX_OUT_OF_BOUNDS: &[u8] = b"Index out of bounds";

fn expect_consistent_state<T>(val: Option<T>) -> T {
    val.unwrap_or_else(|| env::panic(ERR_INCONSISTENT_STATE))
}

/// An iterable implementation of vector that stores its content on the trie.
/// Uses the following map: index -> element.
///
/// This implementation will cache all changes and loads and only updates values that are changed
/// in storage after it's dropped through it's [`Drop`] implementation.
///
/// TODO examples
#[derive(BorshSerialize, BorshDeserialize)]
#[cfg_attr(not(feature = "expensive-debug"), derive(Debug))]
pub struct Vector<T>
where
    T: BorshSerialize,
{
    // TODO: determine why u64 was used previously -- is it required? u32 faster in wasm env
    len: u32,
    prefix: Vec<u8>,
    #[borsh_skip]
    /// Cache for loads and intermediate changes to the underlying vector.
    /// The cached entries are wrapped in a [`Box`] to avoid existing pointers from being
    /// invalidated.
    cache: CacheCell<BTreeMap<u32, Box<CacheEntry<T>>>>,
}

impl<T> Vector<T>
where
    T: BorshSerialize,
{
    /// Returns the number of elements in the vector, also referred to as its size.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Returns `true` if the vector contains no elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Create new vector with zero elements. Use `id` as a unique identifier on the trie.
    pub fn new<S>(prefix: S) -> Self
    where
        S: IntoStorageKey,
    {
        Self { len: 0, prefix: prefix.into_storage_key(), cache: Default::default() }
    }

    fn index_to_lookup_key(&self, index: u32) -> Vec<u8> {
        append_slice(&self.prefix, &index.to_le_bytes()[..])
    }

    /// Removes all elements from the collection. This will remove all storage values for the
    /// length of the [`Vector`].
    pub fn clear(&mut self) {
        for i in 0..self.len {
            let lookup_key = self.index_to_lookup_key(i);
            env::storage_remove(&lookup_key);
        }
        self.len = 0;
        self.cache.as_inner_mut().clear();
    }

    // TODO expose this? Could be useful to not force a user to drop to persist changes
    /// Flushes the cache and writes all modified values to storage.
    fn flush(&mut self) {
        for (k, v) in self.cache.as_inner_mut().iter_mut() {
            if v.is_modified() {
                let key = append_slice(&self.prefix, &k.to_le_bytes()[..]);
                match v.value().as_ref() {
                    Some(modified) => {
                        // Value was modified, write the updated value to storage
                        env::storage_write(&key, &Self::serialize_element(modified));
                    }
                    None => {
                        // Element was removed, clear the storage for the value
                        env::storage_remove(&key);
                    }
                }

                // Update state of flushed state as cached, to avoid duplicate writes/removes
                // while also keeping the cached values in memory.
                v.replace_state(EntryState::Cached);
            }
        }
    }

    /// Sets a value at a given index to the value provided. This does not shift values after the
    /// index to the right.
    pub fn set(&mut self, index: u32, value: T) {
        if index >= self.len() {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        match self.cache.as_inner_mut().entry(index) {
            Entry::Occupied(mut occupied) => {
                occupied.get_mut().replace(Some(value));
            }
            Entry::Vacant(vacant) => {
                vacant.insert(Box::new(CacheEntry::new_modified(Some(value))));
            }
        }
    }

    fn serialize_element(element: &T) -> Vec<u8> {
        element.try_to_vec().unwrap_or_else(|_| env::panic(ERR_ELEMENT_SERIALIZATION))
    }

    /// Appends an element to the back of the collection.
    pub fn push(&mut self, element: T) {
        if self.len() >= u32::MAX {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        let last_idx = self.len();
        self.len += 1;
        self.set(last_idx, element)
    }

    // TODO move this to extend trait
    /// Extends vector from the given collection.
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for el in iter {
            self.push(el)
        }
    }
}

impl<T> Vector<T>
where
    T: BorshSerialize + BorshDeserialize,
{
    fn deserialize_element(raw_element: &[u8]) -> T {
        T::try_from_slice(&raw_element).unwrap_or_else(|_| env::panic(ERR_ELEMENT_DESERIALIZATION))
    }

    /// Loads value from storage into cache, if it does not already exist.
    /// This function must be unsafe because it requires modifying the cache with an immutable
    /// reference.
    unsafe fn load(&self, index: u32) -> NonNull<CacheEntry<T>> {
        // TODO safety docs
        match self.cache.get_ptr().as_mut().entry(index) {
            Entry::Occupied(mut occupied) => NonNull::from(&mut **occupied.get_mut()),
            Entry::Vacant(vacant) => {
                let value = env::storage_read(&self.index_to_lookup_key(index))
                    .map(|v| Self::deserialize_element(&v));
                NonNull::from(&mut **vacant.insert(Box::new(CacheEntry::new_cached(value))))
            }
        }
    }

    /// Loads value from storage into cache, and returns a mutable reference to the loaded value.
    /// This function is safe because a mutable reference of self is used.
    fn load_mut(&mut self, index: u32) -> &mut CacheEntry<T> {
        // * SAFETY: A mutable reference can be returned here because it references a value in a
        //           `Box` and no other references should exist given function takes a mutable
        //           reference. This has the assumption that other references are not kept around
        //           past this function call.
        unsafe { &mut *self.load(index).as_ptr() }
    }

    /// Returns the element by index or `None` if it is not present.
    pub fn get(&self, index: u32) -> Option<&T> {
        // TODO doc safety
        unsafe { &*self.load(index).as_ptr() }.value().as_ref()
    }

    fn swap(&mut self, a: u32, b: u32) {
        if a >= self.len() || b >= self.len() {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        if a == b {
            // Short circuit if indices are the same, also guarantees uniqueness below
            return;
        }

        // * SAFETY: references are guaranteed to be distinct because the indices are checked to not
        //           be equal above. These mutable references will both be dropped before the end
        //           of the scope of the swap call.
        let a_value = unsafe { &mut *self.load(a).as_ptr() };
        let b_value = unsafe { &mut *self.load(b).as_ptr() };

        if a_value.value().is_none() || b_value.value().is_none() {
            // Should never be able to swap a filled value with an empty value in a vec.
            env::panic(ERR_INCONSISTENT_STATE);
        }

        core::mem::swap(a_value.value_mut(), b_value.value_mut());
    }

    /// Removes an element from the vector and returns it.
    /// The removed element is replaced by the last element of the vector.
    /// Does not preserve ordering, but is `O(1)`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn swap_remove(&mut self, index: u32) -> T {
        if self.is_empty() {
            env::panic(ERR_INDEX_OUT_OF_BOUNDS);
        }

        self.swap(index, self.len() - 1);
        expect_consistent_state(self.pop())
    }

    /// Removes the last element from a vector and returns it, or `None` if it is empty.
    pub fn pop(&mut self) -> Option<T> {
        if self.is_empty() {
            None
        } else {
            let last_idx = self.len - 1;
            self.len = last_idx;

            // Replace current value with none, and return the existing value
            let popped_value = expect_consistent_state(self.load_mut(last_idx).replace(None));
            Some(popped_value)
        }
    }
}
