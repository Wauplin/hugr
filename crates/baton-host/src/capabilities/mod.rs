//! The batteries-included default capabilities (DESIGN §8): `shell`, `fs read`,
//! `fs write`, and `http`. All are ordinary [`Capability`](crate::Capability)s —
//! there are no privileged built-ins, so a browser host simply doesn't register
//! the ones it can't provide.

mod fs;
mod http;
mod shell;

pub use fs::{FsRead, FsWrite};
pub use http::Http;
pub use shell::Shell;
