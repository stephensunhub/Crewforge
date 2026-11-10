//! 沙箱网络白名单：内置的包管理器域名 + 用户自己加的域名（全局设置）。
//!
//! 员工的命令跑在沙箱里，只能连白名单里的域名；其他域名每次都要问人。用户可以在设置里把常用的加进来，
//! 也可以在审批弹窗里一键“允许并加入白名单”。
//! 共享托管类域名（任何人都能往上放内容、也能往里传东西）加进来有数据外传风险，会给出提醒，但决定权在用户。

use crate::db::Db;
use anyhow::{Result, bail};
use serde::Serialize;

/// 内置白名单（包管理器与代码托管）。
pub const BUILTIN_DOMAINS: &[&str] = &[
    "registry.npmjs.org", "crates.io", "index.crates.io", "static.crates.io",
    "static.rust-lang.org", "github.com", "pypi.org", "files.pythonhosted.org",
    // Go：模块代理和校验库（下载依赖、校验哈希）
    "proxy.golang.org", "sum.golang.org",
];

/// 共享托管类域名：不是某个项目专用的，任何人都能在上面放东西。
const SHARED_HOSTS: &[&str] = &[
    "storage.googleapis.com", "amazonaws.com", "github.io", "githubusercontent.com", "blob.core.windows.net",
    "cloudfront.net", "herokuapp.com", "vercel.app", "netlify.app", "web.app", "firebaseapp.com",
    "pastebin.com", "dropbox.com", "drive.google.com", "workers.dev", "pages.dev", "ngrok.io", "trycloudflare.com",
];

/// 规整用户输入的域名：去掉协议、路径、端口，转小写；只接受普通主机名（可以以 `*.` 开头）。
pub fn normalize_domain(input: &str) -> Result<String> {
    let mut d = input.trim().to_lowercase();
    for p in ["https://", "http://"] {
        if let Some(r) = d.strip_prefix(p) { d = r.to_string(); }
    }
    d = d.split(['/', '?', '#']).next().unwrap_or("").to_string();
    if let Some((h, port)) = d.rsplit_once(':') {
        if port.chars().all(|c| c.is_ascii_digit()) { d = h.to_string(); }
    }
    let body = d.strip_prefix("*.").unwrap_or(&d);
    let ok = !body.is_empty() && body.len() <= 253 && body.contains('.')
        && body.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    if !ok {
        bail!("域名格式不对：{input}（只填主机名，例如 proxy.golang.org，可以以 *. 开头）");
    }
    if body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        bail!("不支持直接填 IP 地址");
    }
    Ok(d)
}

/// 共享托管域名的风险提醒；不是共享托管域名返回 None。
pub fn shared_host_warning(domain: &str) -> Option<String> {
    let d = domain.trim_start_matches("*.");
    SHARED_HOSTS.iter().find(|s| d == **s || d.ends_with(&format!(".{s}"))).map(|s| format!(
        "{s} 是共享托管域名：任何人都能往上面放内容，也能往上面上传。放进白名单后，沙箱里的命令可以把工作区里的代码传到任意存储桶或页面。只在确实需要、并且信任这个员工的任务时才加。"))
}

#[derive(Debug, Clone, Serialize)]
pub struct DomainEntry {
    pub domain: String,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SandboxDomains {
    pub builtin: Vec<String>,
    pub custom: Vec<DomainEntry>,
}

impl Db {
    pub fn custom_sandbox_domains(&self) -> Vec<String> {
        self.get_setting("sandbox_domains").ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn sandbox_domains(&self) -> SandboxDomains {
        SandboxDomains {
            builtin: BUILTIN_DOMAINS.iter().map(|s| s.to_string()).collect(),
            custom: self.custom_sandbox_domains().into_iter().map(|d| DomainEntry { warning: shared_host_warning(&d), domain: d }).collect(),
        }
    }

    /// 已经在白名单里（内置或自己加的）。
    pub fn is_sandbox_domain_allowed(&self, domain: &str) -> bool {
        let d = domain.to_lowercase();
        let hit = |x: &str| d == x || x.strip_prefix("*.").is_some_and(|b| d.ends_with(&format!(".{b}")));
        BUILTIN_DOMAINS.iter().any(|b| hit(b)) || self.custom_sandbox_domains().iter().any(|c| hit(c))
    }

    /// 加入全局白名单。返回规整后的域名；已经在里面就原样返回，不重复。
    pub fn add_sandbox_domain(&self, input: &str) -> Result<String> {
        let d = normalize_domain(input)?;
        let mut list = self.custom_sandbox_domains();
        if !list.contains(&d) && !BUILTIN_DOMAINS.contains(&d.as_str()) {
            list.push(d.clone());
            self.set_setting("sandbox_domains", &serde_json::to_string(&list)?)?;
            self.append_event("sandbox_domain_added", None, None, &serde_json::json!({"domain": d, "warning": shared_host_warning(&d).is_some()}))?;
        }
        Ok(d)
    }

    pub fn remove_sandbox_domain(&self, domain: &str) -> Result<()> {
        let mut list = self.custom_sandbox_domains();
        let before = list.len();
        list.retain(|d| d != domain);
        if list.len() == before {
            bail!("白名单里没有 {domain}（内置的域名不能删除）");
        }
        self.set_setting("sandbox_domains", &serde_json::to_string(&list)?)?;
        self.append_event("sandbox_domain_removed", None, None, &serde_json::json!({"domain": domain}))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_normalized_and_validated() {
        assert_eq!(normalize_domain(" HTTPS://Proxy.Golang.org:443/foo?x=1 ").unwrap(), "proxy.golang.org");
        assert_eq!(normalize_domain("*.example.com").unwrap(), "*.example.com");
        for bad in ["", "localhost", "a b.com", "-x.com", "http://", "1.2.3.4", "foo..com", "ex ample.com"] {
            assert!(normalize_domain(bad).is_err(), "{bad} 应该被拒绝");
        }
    }

    #[test]
    fn shared_hosts_get_a_warning_but_dedicated_ones_do_not() {
        assert!(shared_host_warning("storage.googleapis.com").unwrap().contains("共享托管"));
        assert!(shared_host_warning("my-bucket.s3.amazonaws.com").is_some(), "子域名同样提醒");
        assert!(shared_host_warning("foo.github.io").is_some());
        assert!(shared_host_warning("proxy.golang.org").is_none() && shared_host_warning("crates.io").is_none());
    }

    #[test]
    fn global_whitelist_add_remove_and_lookup() {
        let db = Db::open_memory().unwrap();
        assert!(db.is_sandbox_domain_allowed("proxy.golang.org"), "内置的");
        assert!(!db.is_sandbox_domain_allowed("storage.googleapis.com"));
        assert_eq!(db.add_sandbox_domain("https://storage.googleapis.com/x").unwrap(), "storage.googleapis.com");
        db.add_sandbox_domain("storage.googleapis.com").unwrap();
        db.add_sandbox_domain("proxy.golang.org").unwrap(); // 内置的不重复写入
        let all = db.sandbox_domains();
        assert_eq!(all.custom.len(), 1, "不重复");
        assert!(all.custom[0].warning.is_some() && all.builtin.contains(&"crates.io".to_string()));
        assert!(db.is_sandbox_domain_allowed("storage.googleapis.com"));
        db.add_sandbox_domain("*.example.com").unwrap();
        assert!(db.is_sandbox_domain_allowed("a.example.com") && !db.is_sandbox_domain_allowed("example.org"));
        db.remove_sandbox_domain("storage.googleapis.com").unwrap();
        assert!(!db.is_sandbox_domain_allowed("storage.googleapis.com"));
        assert!(db.remove_sandbox_domain("crates.io").is_err(), "内置的不能删");
    }
}
