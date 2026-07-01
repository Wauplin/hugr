//! The batteries-included default capabilities (DESIGN §8): `shell`, `fs read`,
//! `fs write`, repo-orientation tools, `http`, and `blob` (content-addressed
//! store). All are ordinary [`Capability`](crate::Capability)s — there are no
//! privileged built-ins, so a browser host simply doesn't register the ones it
//! can't provide.

mod blob;
mod fs;
mod http;
mod patch;
mod repo;
mod shell;
mod verify;

pub use blob::Blob;
pub use fs::{FsRead, FsWrite};
pub use http::Http;
pub use patch::PatchApply;
pub use repo::{GitDiff, GitLog, GitStatus, PackageMetadata, RepoFiles, RepoRead, RepoSearch};
pub use shell::Shell;
pub use verify::CargoVerify;
