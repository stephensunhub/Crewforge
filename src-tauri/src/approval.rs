//! 把核心库的审批请求转成界面事件，并等待人类在界面上点击。

use crewforge_core::provider::claude::{ApprovalRequest, Decision};
use crewforge_core::runner::Approver;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

#[derive(Clone, Default)]
pub struct Pending(Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>);

impl Pending {
    pub fn resolve(&self, id: &str, decision: Decision) -> bool {
        match self.0.lock().unwrap().remove(id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }
}

#[derive(Serialize, Clone)]
struct ApprovalPayload<'a> {
    id: &'a str,
    employee: &'a str,
    tool: &'a str,
    input: &'a Value,
    reason: &'a str,
}

pub struct UiApprover {
    pub app: AppHandle,
    pub pending: Pending,
}

impl Approver for UiApprover {
    async fn ask(&self, employee: &str, req: &ApprovalRequest, reason: &str) -> Decision {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.0.lock().unwrap().insert(id.clone(), tx);
        let _ = self.app.emit(
            "crewforge://approval",
            ApprovalPayload { id: &id, employee, tool: &req.tool_name, input: &req.input, reason },
        );
        // 界面关闭或任务被取消导致发送端被丢弃时，按拒绝处理
        rx.await.unwrap_or(Decision::Deny("审批请求已失效".into()))
    }
}
