pub mod domain;
pub mod traits;

pub use domain::{ForgeEvent, RepoId, Subject, ThreadKey};
pub use traits::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error("forge: {0}")]
    Forge(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
