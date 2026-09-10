//! The long-lived forgeclaw daemon.
//!
//! The webhook router and plugin-tool endpoint share [`grants::GrantStore`]. This is
//! deliberately process-local: a grant disappears if the daemon restarts.

pub mod authorization;
pub mod grants;
pub mod router;
