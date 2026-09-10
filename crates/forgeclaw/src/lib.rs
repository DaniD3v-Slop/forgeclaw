//! The long-lived forgeclaw daemon.
//!
//! The webhook router and plugin-tool endpoint share [`grants::GrantStore`]. This is
//! deliberately process-local: a grant and any queued turn disappear if the
//! daemon restarts.

pub mod grants;
pub mod http_tools;
pub mod router;
