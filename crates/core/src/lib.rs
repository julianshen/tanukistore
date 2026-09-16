//! Pure protocol core for tanukistore: types, key layout, rollout bucketing,
//! version resolution, and Squirrel manifest derivation. No I/O lives here.

pub mod model;
pub mod keys;
pub mod rollout;
pub mod resolve;
pub mod derive;
