//! Pure protocol core for tanukistore: types, key layout, rollout bucketing,
//! version resolution, and Squirrel manifest derivation, plus the storage
//! boundary the server and publisher share. The only I/O is behind the
//! `ObjectStore` trait; the S3 implementation sits behind the `s3` feature.

pub mod model;
pub mod keys;
pub mod rollout;
pub mod resolve;
pub mod derive;
pub mod store;
pub mod breaker;
pub mod cache;
