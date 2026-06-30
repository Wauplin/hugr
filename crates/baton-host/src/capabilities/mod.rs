//! The batteries-included default capabilities (DESIGN §8): `shell`, `fs read`,
//! `fs write`, `http`, and `blob` (content-addressed store). All are ordinary
//! [`Capability`](crate::Capability)s — there are no privileged built-ins, so a
//! browser host simply doesn't register the ones it can't provide.

mod blob;
mod fs;
mod http;
mod shell;

pub use blob::Blob;
pub use fs::{FsRead, FsWrite};
pub use http::Http;
pub use shell::Shell;
