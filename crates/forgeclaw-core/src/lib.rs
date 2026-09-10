pub mod domain;
pub mod traits;

pub use domain::{ForgeEvent, RepoId, Subject, Task, TaskStatus, ThreadKey, Trigger, backoff};
pub use traits::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Template(#[from] minijinja::Error),
    #[error("forge: {0}")]
    Forge(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
