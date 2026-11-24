//! Generation hashing. The golden vectors in `tests.rs` were recorded on main
//! before this module owned any hasher (design S3): they pin the exact bytes of
//! every generation and revision clients hold today.

#[cfg(test)]
mod tests;
