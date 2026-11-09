//! Crewforge 核心业务库。不依赖任何 UI 框架，可由命令行或 Tauri 外壳调用。

pub mod actions;
pub mod board;
pub mod cost;
pub mod db;
pub mod guest;
pub mod mcp;
pub mod meeting;
pub mod models;
pub mod onboarding;
pub mod memory;
pub mod orchestrator;
pub mod plan;
pub mod policy;
pub mod protocol;
pub mod provider;
pub mod runner;
pub mod scheduler;
pub mod standup;
pub mod shell_env;
pub mod workspace;
