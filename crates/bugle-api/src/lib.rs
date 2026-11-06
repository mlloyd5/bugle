//! Agent interfaces: a JSON HTTP API and an MCP (Model Context Protocol)
//! stdio server. Both are thin wrappers over [`actions`], so every tool is
//! available identically through either transport.

pub mod actions;
pub mod http;
pub mod mcp;
