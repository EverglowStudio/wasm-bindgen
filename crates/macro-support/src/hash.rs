//! Common utility function for manipulating syn types and
//! handling parsed values

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Small utility used when generating symbol names.
///
/// The caller is responsible for including an explicit expansion-context salt
/// in `T` when symbols need to be unique across crates.  Keeping this helper
/// free of process environment and cached global state makes programmatic
/// expansion deterministic and safe to reuse for multiple crates in one
/// process.
#[derive(Debug)]
pub struct ShortHash<T>(pub T);

impl<T: Hash> fmt::Display for ShortHash<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut h = DefaultHasher::new();
        self.0.hash(&mut h);
        write!(f, "{:016x}", h.finish())
    }
}
