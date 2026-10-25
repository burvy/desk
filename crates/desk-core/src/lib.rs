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
pub mod transcript;
pub mod workspace;

pub use workspace::Workspace;
