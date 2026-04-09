//! String interner for `Arc<str>` — allocate once, share by reference.
//!
//! Source identifiers and destination names are interned at pipeline init.
//! Hot-path code gets `Arc<str>` clones (pointer copy, no allocation).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Thread-safe string interner. Deduplicates `Arc<str>` instances so that
/// identical strings share the same allocation.
#[derive(Debug)]
pub struct StringInterner {
    map: Mutex<HashMap<Arc<str>, ()>>,
}

impl StringInterner {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Intern a string. Returns an `Arc<str>` that may be shared with
    /// previous calls for the same string value.
    pub fn intern(&self, s: &str) -> Arc<str> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        // Check if already interned
        for key in map.keys() {
            if &**key == s {
                return Arc::clone(key);
            }
        }
        // New entry
        let interned: Arc<str> = Arc::from(s);
        map.insert(Arc::clone(&interned), ());
        interned
    }

    /// Number of interned strings.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether the interner is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for StringInterner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_deduplicates() {
        let interner = StringInterner::new();
        let a = interner.intern("aeon-source");
        let b = interner.intern("aeon-source");
        // Same Arc allocation (pointer equality)
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(interner.len(), 1);
    }

    #[test]
    fn intern_different_strings() {
        let interner = StringInterner::new();
        let a = interner.intern("source");
        let b = interner.intern("sink");
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn intern_returns_correct_value() {
        let interner = StringInterner::new();
        let s = interner.intern("hello");
        assert_eq!(&*s, "hello");
    }
}
