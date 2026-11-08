use crate::approval::Pending;
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::workspace::WorkspaceManager;
use std::path::Path;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use crewforge_core::scheduler::Scheduler;

/// 默认并发上限（首次启动时使用；之后以设置里保存的为准）。
pub const DEFAULT_CONCURRENCY: usize = 3;

pub struct AppState {
    pub db: Arc<Db>,
    pub orch: Arc<Orchestrator>,
    pub ws: Arc<WorkspaceManager>,
    pub pending: Pending,
    /// 额度感知的调度器：所有模型调用都要先在这里取得许可
    pub sched: Arc<Scheduler>,
    /// 正在执行任务的员工名，防止同一员工的会话被并发写入
    pub running: Arc<Mutex<HashSet<String>>>,
    /// 正在进行的耗时操作（基线、检查），防止同一项目/任务被重复触发
    pub busy: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    pub fn new(db: Arc<Db>, data_dir: &Path) -> Self {
        let ws = Arc::new(WorkspaceManager::new(data_dir));
        let db_for_sched = db.clone();
        Self {
            orch: Arc::new(Orchestrator::new(db.clone(), ws.clone())),
            ws,
            db,
            pending: Pending::default(),
            sched: Scheduler::new(db_for_sched, DEFAULT_CONCURRENCY),
            running: Arc::new(Mutex::new(HashSet::new())),
            busy: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}
