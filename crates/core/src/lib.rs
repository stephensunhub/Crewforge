//! Crewforge 核心业务库。不依赖任何 UI 框架，可由命令行或 Tauri 外壳调用。

pub mod board;
pub mod cost;
pub mod db;
pub mod mcp;
pub mod meeting;
pub mod onboarding;
pub mod memory;
pub mod orchestrator;
pub mod policy;
pub mod provider;
pub mod runner;
pub mod workspace;
