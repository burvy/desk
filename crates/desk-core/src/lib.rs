//! desk core: the workspace's file formats, and the supervisor that runs
//! agents against them. The CLI and the GUI are both thin layers over this.

pub mod adapter;
pub mod agent;
pub mod ask;
pub mod frontmatter;
pub mod fsutil;
pub mod lock;
pub mod message;
pub mod models;
pub mod prompt;
pub mod supervisor;
pub mod task;
#[cfg(feature = "terminal")]
mod term_run;
#[cfg(feature = "terminal")]
pub mod terminal;
pub mod transcript;
pub mod workspace;

#[cfg(feature = "terminal")]
pub use term_run::Registry;
pub use workspace::Workspace;
