use crate::approval::Pending;
use crewforge_core::db::Db;
use crewforge_core::orchestrator::Orchestrator;
use crewforge_core::workspace::WorkspaceManager;
use std::path::Path;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

/// 默认并发上限。注意：员工数量不设上限，这个值限制的是"同时运行的 CLI 进程数"，
/// 因为所有员工共享同一份订阅额度。超出的任务排队等待。
pub const DEFAULT_CONCURRENCY: usize = 3;
pub const MAX_CONCURRENCY: usize = 50;

pub struct AppState {
    pub db: Arc<Db>,
    pub orch: Arc<Orchestrator>,
    pub ws: Arc<WorkspaceManager>,
    pub pending: Pending,
    pub sem: Arc<Semaphore>,
    limit: Arc<Mutex<usize>>,
    /// 正在执行任务的员工名，防止同一员工的会话被并发写入
    pub running: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    pub fn new(db: Arc<Db>, data_dir: &Path) -> Self {
        let ws = Arc::new(WorkspaceManager::new(data_dir));
        Self {
            orch: Arc::new(Orchestrator::new(db.clone(), ws.clone())),
            ws,
            db,
            pending: Pending::default(),
            sem: Arc::new(Semaphore::new(DEFAULT_CONCURRENCY)),
            limit: Arc::new(Mutex::new(DEFAULT_CONCURRENCY)),
            running: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl AppState {
    pub fn concurrency(&self) -> usize {
        *self.limit.lock().unwrap()
    }

    /// 调整并发上限。调大立即生效；调小时，等运行中的任务释放后再回收许可，不打断正在执行的任务。
    pub fn set_concurrency(&self, n: usize) -> usize {
        let n = n.clamp(1, MAX_CONCURRENCY);
        let mut cur = self.limit.lock().unwrap();
        if n > *cur {
            self.sem.add_permits(n - *cur);
        } else if n < *cur {
            let diff = (*cur - n) as u32;
            let sem = self.sem.clone();
            tauri::async_runtime::spawn(async move {
                if let Ok(p) = sem.acquire_many_owned(diff).await {
                    p.forget();
                }
            });
        }
        *cur = n;
        n
    }
}
