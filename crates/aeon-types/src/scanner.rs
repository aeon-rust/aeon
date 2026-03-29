//! SIMD-accelerated byte scanning for hot-path payload inspection.
//!
//! Uses `memchr` for hardware-accelerated (SIMD) byte searches.
//! These functions inspect raw bytes without parsing — no JSON deserialization,
//! no regex, no allocations. Used for content-based routing and filtering.
//!
//! Rule: Never use `.contains()`, `.find()`, or iterator chains on raw byte data
//! on the hot path (CLAUDE.md rule #5).

/// Find the first occurrence of a byte in a slice.
///
/// SIMD-accelerated via `memchr`. O(n) but with wide SIMD lanes.
#[inline]
pub fn find_byte(needle: u8, haystack: &[u8]) -> Option<usize> {
    memchr::memchr(needle, haystack)
}

/// Find the first occurrence of any of two bytes.
#[inline]
pub fn find_byte2(b1: u8, b2: u8, haystack: &[u8]) -> Option<usize> {
    memchr::memchr2(b1, b2, haystack)
}

/// Find the first occurrence of any of three bytes.
#[inline]
pub fn find_byte3(b1: u8, b2: u8, b3: u8, haystack: &[u8]) -> Option<usize> {
    memchr::memchr3(b1, b2, b3, haystack)
}

/// Check if a byte slice contains a specific byte. SIMD-accelerated.
#[inline]
pub fn contains_byte(needle: u8, haystack: &[u8]) -> bool {
    memchr::memchr(needle, haystack).is_some()
}

/// Find a substring (needle) in a byte slice (haystack).
///
/// Uses `memchr::memmem` — SIMD-accelerated Two-Way or SIMD substring search.
/// Much faster than `haystack.windows(n).any(|w| w == needle)`.
#[inline]
pub fn find_bytes(needle: &[u8], haystack: &[u8]) -> Option<usize> {
    memchr::memmem::find(haystack, needle)
}

/// Check if a byte slice contains a substring. SIMD-accelerated.
#[inline]
pub fn contains_bytes(needle: &[u8], haystack: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

/// Pre-compiled substring finder for repeated searches with the same needle.
///
/// Amortizes the cost of building the search automaton across many haystack searches.
/// Use this when routing events by content pattern — build the finder once at pipeline
/// init, then call `find()` per event.
pub struct BytesFinder {
    inner: memchr::memmem::Finder<'static>,
}

impl BytesFinder {
    /// Create a new finder for the given needle. The needle is copied internally.
    pub fn new(needle: &[u8]) -> Self {
        // Leak the needle bytes so the Finder gets a 'static reference.
        // This is intentional — finders are created at pipeline init and live forever.
        let owned = needle.to_vec().into_boxed_slice();
        let leaked: &'static [u8] = Box::leak(owned);
        Self {
            inner: memchr::memmem::Finder::new(leaked),
        }
    }

    /// Find the needle in the haystack. SIMD-accelerated.
    #[inline]
    pub fn find(&self, haystack: &[u8]) -> Option<usize> {
        self.inner.find(haystack)
    }

    /// Check if the haystack contains the needle.
    #[inline]
    pub fn is_match(&self, haystack: &[u8]) -> bool {
        self.inner.find(haystack).is_some()
    }
}

/// Extract a JSON field value without parsing the entire document.
///
/// Scans for `"field_name":` and extracts the value bytes that follow.
/// Works for simple string and numeric values. Does NOT handle nested objects,
/// escaped quotes in values, or whitespace variations beyond `": ` vs `":`.
///
/// This is deliberately limited — it's a hot-path shortcut for routing,
/// not a general-purpose JSON parser. If you need full parsing, do it
/// in the processor, not the router.
///
/// Returns the raw value bytes (including quotes for strings).
pub fn json_field_value<'a>(field: &str, payload: &'a [u8]) -> Option<&'a [u8]> {
    // Build the search pattern: "field_name":
    let mut pattern = Vec::with_capacity(field.len() + 3);
    pattern.push(b'"');
    pattern.extend_from_slice(field.as_bytes());
    pattern.extend_from_slice(b"\":");

    let pos = memchr::memmem::find(payload, &pattern)?;
    let value_start = pos + pattern.len();

    // Skip optional whitespace after colon
    let rest = payload.get(value_start..)?;
    let trimmed = rest.iter().position(|&b| b != b' ' && b != b'\t')?;
    let rest = &rest[trimmed..];

    match rest.first()? {
        b'"' => {
            // String value — find closing quote (naive: no escaped quotes)
            let end = memchr::memchr(b'"', &rest[1..])? + 2; // include both quotes
            Some(&rest[..end])
        }
        b'0'..=b'9' | b'-' => {
            // Numeric value — read until non-numeric
            let end = rest
                .iter()
                .position(|&b| !matches!(b, b'0'..=b'9' | b'.' | b'-' | b'e' | b'E' | b'+'))
                .unwrap_or(rest.len());
            Some(&rest[..end])
        }
        b't' | b'f' => {
            // Boolean
            if rest.starts_with(b"true") {
                Some(&rest[..4])
            } else if rest.starts_with(b"false") {
                Some(&rest[..5])
            } else {
                None
            }
        }
        b'n' => {
            // null
            if rest.starts_with(b"null") {
                Some(&rest[..4])
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_byte_basic() {
        assert_eq!(find_byte(b'o', b"hello world"), Some(4));
        assert_eq!(find_byte(b'z', b"hello world"), None);
    }

    #[test]
    fn contains_byte_basic() {
        assert!(contains_byte(b'{', b"{\"key\":\"value\"}"));
        assert!(!contains_byte(b'!', b"no exclamation"));
    }

    #[test]
    fn find_bytes_substring() {
        assert_eq!(find_bytes(b"world", b"hello world"), Some(6));
        assert_eq!(find_bytes(b"missing", b"hello world"), None);
    }

    #[test]
    fn contains_bytes_substring() {
        assert!(contains_bytes(b"error", b"this is an error message"));
        assert!(!contains_bytes(b"warning", b"this is an error message"));
    }

    #[test]
    fn bytes_finder_reuse() {
        let finder = BytesFinder::new(b"event_type");
        assert!(finder.is_match(b"{\"event_type\":\"click\"}"));
        assert!(finder.is_match(b"another event_type here"));
        assert!(!finder.is_match(b"no match here"));
        assert_eq!(finder.find(b"prefix event_type suffix"), Some(7));
    }

    #[test]
    fn json_field_string_value() {
        let payload = b"{\"event_type\":\"click\",\"user\":\"alice\"}";
        assert_eq!(
            json_field_value("event_type", payload),
            Some(b"\"click\"" as &[u8])
        );
        assert_eq!(
            json_field_value("user", payload),
            Some(b"\"alice\"" as &[u8])
        );
    }

    #[test]
    fn json_field_numeric_value() {
        let payload = b"{\"count\":42,\"rate\":3.14}";
        assert_eq!(json_field_value("count", payload), Some(b"42" as &[u8]));
        assert_eq!(json_field_value("rate", payload), Some(b"3.14" as &[u8]));
    }

    #[test]
    fn json_field_boolean_null() {
        let payload = b"{\"active\":true,\"deleted\":false,\"data\":null}";
        assert_eq!(json_field_value("active", payload), Some(b"true" as &[u8]));
        assert_eq!(
            json_field_value("deleted", payload),
            Some(b"false" as &[u8])
        );
        assert_eq!(json_field_value("data", payload), Some(b"null" as &[u8]));
    }

    #[test]
    fn json_field_with_space_after_colon() {
        let payload = b"{\"key\": \"value\"}";
        assert_eq!(
            json_field_value("key", payload),
            Some(b"\"value\"" as &[u8])
        );
    }

    #[test]
    fn json_field_not_found() {
        let payload = b"{\"key\":\"value\"}";
        assert_eq!(json_field_value("missing", payload), None);
    }

    #[test]
    fn find_byte2_test() {
        assert_eq!(find_byte2(b'{', b'[', b"start{"), Some(5));
        assert_eq!(find_byte2(b'{', b'[', b"start["), Some(5));
        assert_eq!(find_byte2(b'{', b'[', b"nothing"), None);
    }
}
