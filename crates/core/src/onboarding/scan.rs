//! 项目只读扫描：识别技术栈、结构、CI、API 与数据库定义、Git 历史，并给出建议的基线命令。
//! 全程只读、不调用模型、不执行任何项目代码，所以速度很快，需要时直接重新扫描即可。

use crate::workspace::git_output;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", "out", ".idea", ".vscode", "vendor", ".gradle",
    "__pycache__", ".venv", "venv", ".next", ".nuxt", ".svelte-kit", "coverage", ".tox", ".mypy_cache",
    ".dart_tool", "Pods", ".terraform",
];
const MAX_FILES: usize = 30_000;
const MAX_DEPTH: usize = 14;
const MAX_LIST: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub path: String,
    pub files: usize,
    /// 来源：cargo | maven | npm | go | directory
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineCommand {
    pub id: String,
    pub label: String,
    /// setup | build | test | lint
    pub kind: String,
    pub command: String,
    pub reason: String,
    /// 建议默认勾选
    pub default_on: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub commit_count: Option<u64>,
    pub branch_count: Option<usize>,
    pub tag_count: Option<usize>,
    pub first_commit_date: Option<String>,
    pub last_commit: Option<String>,
    pub recent: Vec<String>,
    pub authors: Vec<(String, u64)>,
    pub remotes: Vec<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanReport {
    pub root: String,
    pub scanned_at: i64,
    pub total_files: usize,
    pub truncated: bool,
    pub languages: Vec<(String, usize)>,
    pub package_managers: Vec<String>,
    pub build_tools: Vec<String>,
    pub frameworks: Vec<String>,
    pub databases: Vec<String>,
    pub test_frameworks: Vec<String>,
    pub ci: Vec<String>,
    pub api_definitions: Vec<String>,
    pub db_schemas: Vec<String>,
    pub docs: Vec<String>,
    pub top_dirs: Vec<(String, usize)>,
    pub modules: Vec<Module>,
    pub git: GitInfo,
    pub commands: Vec<BaselineCommand>,
    pub notes: Vec<String>,
}

#[derive(Default)]
struct Files {
    paths: Vec<String>,
    truncated: bool,
}

fn walk(root: &Path) -> Files {
    let mut out = Files::default();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue; // 不跟随符号链接，避免跳出项目目录或循环
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((e.path(), depth + 1));
                }
            } else if ft.is_file() {
                if out.paths.len() >= MAX_FILES {
                    out.truncated = true;
                    return out;
                }
                if let Ok(rel) = e.path().strip_prefix(root) {
                    out.paths.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    out
}

fn language_of(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "Rust", "java" => "Java", "kt" | "kts" => "Kotlin", "scala" => "Scala", "ts" | "tsx" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript", "py" => "Python", "go" => "Go", "rb" => "Ruby", "php" => "PHP",
        "cs" => "C#", "c" | "h" => "C", "cc" | "cpp" | "cxx" | "hpp" => "C++", "swift" => "Swift", "dart" => "Dart",
        "vue" => "Vue", "sql" => "SQL", "sh" | "bash" => "Shell", "lua" => "Lua", "ex" | "exs" => "Elixir",
        _ => return None,
    })
}

/// 把文本拆成可比较的词（依赖名、artifactId 等）。
fn tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '@')))
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

enum M { Exact(&'static str), Prefix(&'static str) }
use M::{Exact, Prefix};

const FRAMEWORKS: &[(M, &str)] = &[
    (Prefix("spring-boot"), "Spring Boot"), (Prefix("quarkus"), "Quarkus"), (Prefix("micronaut"), "Micronaut"),
    (Exact("react"), "React"), (Exact("vue"), "Vue"), (Exact("next"), "Next.js"), (Exact("nuxt"), "Nuxt"),
    (Exact("@angular/core"), "Angular"), (Exact("svelte"), "Svelte"), (Exact("express"), "Express"),
    (Exact("@nestjs/core"), "NestJS"), (Exact("koa"), "Koa"), (Exact("fastify"), "Fastify"),
    (Exact("django"), "Django"), (Exact("flask"), "Flask"), (Exact("fastapi"), "FastAPI"), (Exact("rails"), "Rails"),
    (Exact("laravel/framework"), "Laravel"), (Exact("actix-web"), "Actix Web"), (Exact("axum"), "Axum"),
    (Exact("rocket"), "Rocket"), (Exact("tauri"), "Tauri"), (Exact("tokio"), "Tokio"),
    (Exact("github.com/gin-gonic/gin"), "Gin"), (Exact("github.com/labstack/echo"), "Echo"),
    (Exact("electron"), "Electron"), (Exact("vite"), "Vite"), (Exact("webpack"), "Webpack"),
];
const DATABASES: &[(M, &str)] = &[
    (Prefix("mysql"), "MySQL"), (Exact("mariadb"), "MariaDB"), (Prefix("postgres"), "PostgreSQL"), (Exact("pg"), "PostgreSQL"),
    (Exact("psycopg2"), "PostgreSQL"), (Prefix("redis"), "Redis"), (Exact("jedis"), "Redis"), (Exact("lettuce-core"), "Redis"),
    (Prefix("mongo"), "MongoDB"), (Exact("mongoose"), "MongoDB"), (Prefix("sqlite"), "SQLite"), (Exact("rusqlite"), "SQLite"),
    (Exact("clickhouse"), "ClickHouse"), (Prefix("kafka"), "Kafka"), (Prefix("rabbitmq"), "RabbitMQ"), (Exact("amqplib"), "RabbitMQ"),
    (Prefix("elasticsearch"), "Elasticsearch"), (Exact("diesel"), "Diesel ORM"), (Exact("sqlx"), "SQLx"),
    (Exact("hibernate-core"), "Hibernate"), (Prefix("mybatis"), "MyBatis"), (Exact("prisma"), "Prisma"), (Exact("@prisma/client"), "Prisma"),
    (Exact("typeorm"), "TypeORM"), (Exact("sequelize"), "Sequelize"), (Exact("sqlalchemy"), "SQLAlchemy"),
];
const TESTS: &[(M, &str)] = &[
    (Prefix("junit"), "JUnit"), (Exact("testng"), "TestNG"), (Prefix("mockito"), "Mockito"), (Exact("jest"), "Jest"),
    (Exact("vitest"), "Vitest"), (Exact("mocha"), "Mocha"), (Exact("jasmine"), "Jasmine"), (Exact("pytest"), "pytest"),
    (Exact("cypress"), "Cypress"), (Exact("@playwright/test"), "Playwright"), (Exact("playwright"), "Playwright"),
    (Exact("testing-library"), "Testing Library"), (Exact("@testing-library/react"), "Testing Library"),
];

fn matches(table: &[(M, &str)], toks: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (m, label) in table {
        let hit = toks.iter().any(|t| match m {
            Exact(e) => t == e,
            Prefix(p) => t.starts_with(p),
        });
        if hit {
            out.insert((*label).to_string());
        }
    }
    out
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_default()
}

/// 展开形如 `crates/*` 的成员通配（只支持末尾的 `*`）。
fn expand_member(root: &Path, pat: &str) -> Vec<String> {
    if let Some(base) = pat.strip_suffix("/*") {
        let mut v: Vec<String> = std::fs::read_dir(root.join(base)).into_iter().flatten().flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| format!("{base}/{}", e.file_name().to_string_lossy()))
            .collect();
        v.sort();
        v
    } else {
        vec![pat.trim_end_matches('/').to_string()]
    }
}

fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn node_pm(root: &Path, dir: &str) -> (&'static str, &'static str) {
    let has = |f: &str| root.join(dir).join(f).exists() || (dir.is_empty() && root.join(f).exists());
    if has("pnpm-lock.yaml") { ("pnpm", "pnpm install --frozen-lockfile") }
    else if has("yarn.lock") { ("yarn", "yarn install --frozen-lockfile") }
    else if has("package-lock.json") { ("npm", "npm ci") }
    else { ("npm", "npm install") }
}

fn cmd(id: &str, label: &str, kind: &str, command: &str, reason: &str, on: bool) -> BaselineCommand {
    BaselineCommand { id: id.into(), label: label.into(), kind: kind.into(), command: command.into(), reason: reason.into(), default_on: on }
}

/// 扫描目录（同步部分）。`git` 信息由调用方另行填充。
pub fn scan_dir(root: &Path) -> ScanReport {
    let files = walk(root);
    let paths = &files.paths;
    let mut r = ScanReport { root: root.to_string_lossy().into_owned(), total_files: paths.len(), truncated: files.truncated, ..Default::default() };
    if files.truncated {
        r.notes.push(format!("文件数超过 {MAX_FILES}，只扫描了一部分，统计可能不完整"));
    }

    // 语言
    let mut langs: BTreeMap<&str, usize> = BTreeMap::new();
    for p in paths {
        if let Some(l) = file_name(p).rsplit_once('.').and_then(|(_, e)| language_of(&e.to_lowercase())) {
            *langs.entry(l).or_default() += 1;
        }
    }
    let mut l: Vec<(String, usize)> = langs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    l.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    r.languages = l;

    // 顶层目录
    let mut tops: BTreeMap<String, usize> = BTreeMap::new();
    for p in paths {
        if let Some((d, _)) = p.split_once('/') {
            *tops.entry(d.to_string()).or_default() += 1;
        }
    }
    let mut t: Vec<(String, usize)> = tops.into_iter().collect();
    t.sort_by(|a, b| b.1.cmp(&a.1));
    t.truncate(MAX_LIST);
    r.top_dirs = t;

    // 清单文件、CI、API、Schema、文档
    let (mut pms, mut builds, mut fws, mut dbs, mut tests) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let (mut ci, mut api, mut schema, mut docs) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut modules: Vec<Module> = Vec::new();
    let mut commands: Vec<BaselineCommand> = Vec::new();
    let count_under = |dir: &str| -> usize {
        if dir.is_empty() || dir == "." { paths.len() } else { let pre = format!("{}/", dir.trim_end_matches('/')); paths.iter().filter(|p| p.starts_with(&pre)).count() }
    };

    for p in paths {
        let name = file_name(p);
        let lower = name.to_lowercase();
        let depth = p.matches('/').count();
        if p.starts_with(".github/workflows/") && (lower.ends_with(".yml") || lower.ends_with(".yaml")) { ci.insert(format!("GitHub Actions ({p})")); }
        match lower.as_str() {
            ".gitlab-ci.yml" => { ci.insert("GitLab CI".into()); }
            "jenkinsfile" => { ci.insert("Jenkins".into()); }
            "azure-pipelines.yml" => { ci.insert("Azure Pipelines".into()); }
            "config.yml" if p == ".circleci/config.yml" => { ci.insert("CircleCI".into()); }
            "readme.md" | "readme" | "claude.md" | "agents.md" | "contributing.md" | "changelog.md" if depth <= 1 => { docs.insert(p.clone()); }
            "dockerfile" => { builds.insert("Docker".to_string()); }
            "makefile" => { builds.insert("Make".to_string()); }
            "prisma/schema.prisma" | "schema.prisma" => { schema.insert(p.clone()); }
            "schema.rb" | "structure.sql" => { schema.insert(p.clone()); }
            _ => {}
        }
        if lower.starts_with("docker-compose") && (lower.ends_with(".yml") || lower.ends_with(".yaml")) {
            dbs.extend(matches(DATABASES, &tokens(&read(root, p))));
            builds.insert("Docker Compose".into());
        }
        if (lower.starts_with("openapi") || lower.starts_with("swagger")) && (lower.ends_with(".yaml") || lower.ends_with(".yml") || lower.ends_with(".json")) { api.insert(p.clone()); }
        if lower.starts_with("asyncapi") && (lower.ends_with(".yaml") || lower.ends_with(".yml")) { api.insert(p.clone()); }
        if lower.ends_with(".proto") || lower.ends_with(".graphql") || lower.ends_with(".gql") { api.insert(p.clone()); }
        if lower.ends_with(".sql") {
            let pl = p.to_lowercase();
            if pl.contains("migration") || pl.contains("schema") || pl.contains("/db/") || pl.starts_with("db/") { schema.insert(p.clone()); }
        }
        if lower.ends_with("_test.go") { tests.insert("go test".into()); }
    }

    // 各生态的根清单（只看深度 ≤ 2，避免把依赖里的 package.json 当成项目）
    let manifest = |name: &str| -> Vec<&String> { paths.iter().filter(|p| file_name(p) == name && p.matches('/').count() <= 2).collect() };

    // Rust
    for m in manifest("Cargo.toml") {
        pms.insert("cargo".to_string()); builds.insert("Cargo".to_string()); tests.insert("cargo test".to_string());
        let text = read(root, m);
        if let Ok(v) = text.parse::<toml::Table>() {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for sec in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(t) = v.get(sec).and_then(|x| x.as_table()) { names.extend(t.keys().map(|k| k.to_lowercase())); }
            }
            if let Some(t) = v.get("workspace").and_then(|w| w.get("dependencies")).and_then(|x| x.as_table()) { names.extend(t.keys().map(|k| k.to_lowercase())); }
            fws.extend(matches(FRAMEWORKS, &names)); dbs.extend(matches(DATABASES, &names));
            if let Some(ms) = v.get("workspace").and_then(|w| w.get("members")).and_then(|x| x.as_array()) {
                let base = m.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                for pat in ms.iter().filter_map(|x| x.as_str()) {
                    let full = if base.is_empty() { pat.to_string() } else { format!("{base}/{pat}") };
                    for dir in expand_member(root, &full) {
                        modules.push(Module { name: file_name(&dir).to_string(), files: count_under(&dir), path: dir, kind: "cargo".into() });
                    }
                }
            }
        }
        if m == "Cargo.toml" {
            commands.push(cmd("cargo-build", "构建", "build", "cargo build --workspace", "Cargo 项目", true));
            commands.push(cmd("cargo-test", "单元/集成测试", "test", "cargo test --workspace", "Cargo 项目", true));
            commands.push(cmd("cargo-clippy", "静态检查 (clippy)", "lint", "cargo clippy --workspace --all-targets", "需要已安装 clippy 组件", false));
        }
    }
    // Node
    for m in manifest("package.json") {
        let dir = m.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let (pm, install) = node_pm(root, dir);
        pms.insert(pm.to_string());
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&read(root, m)) {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for sec in ["dependencies", "devDependencies"] {
                if let Some(o) = v.get(sec).and_then(|x| x.as_object()) { names.extend(o.keys().map(|k| k.to_lowercase())); }
            }
            fws.extend(matches(FRAMEWORKS, &names)); dbs.extend(matches(DATABASES, &names)); tests.extend(matches(TESTS, &names));
            let ws: Vec<String> = match v.get("workspaces") {
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                Some(o) => o.get("packages").and_then(|p| p.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                None => vec![],
            };
            for pat in ws {
                for d in expand_member(root, &pat) {
                    modules.push(Module { name: file_name(&d).to_string(), files: count_under(&d), path: d, kind: "npm".into() });
                }
            }
            if dir.is_empty() {
                let scripts = v.get("scripts").and_then(|s| s.as_object());
                let has = |k: &str| scripts.is_some_and(|s| s.contains_key(k));
                commands.push(cmd("node-install", "安装依赖", "setup", install, "需要先安装依赖才能构建和测试", true));
                if has("build") { commands.push(cmd("node-build", "构建", "build", &format!("{pm} run build"), "package.json 的 build 脚本", true)); }
                if has("test") { commands.push(cmd("node-test", "测试", "test", &format!("{pm} test"), "package.json 的 test 脚本", true)); }
                if has("lint") { commands.push(cmd("node-lint", "Lint", "lint", &format!("{pm} run lint"), "package.json 的 lint 脚本", false)); }
                if !has("build") && !has("test") && !has("lint") { commands.retain(|c| c.id != "node-install"); }
            }
        }
    }
    // Maven / Gradle
    for m in manifest("pom.xml") {
        pms.insert("maven".into()); builds.insert("Maven".into());
        let text = read(root, m);
        let toks = tokens(&text);
        fws.extend(matches(FRAMEWORKS, &toks)); dbs.extend(matches(DATABASES, &toks)); tests.extend(matches(TESTS, &toks));
        let base = m.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for chunk in text.split("<module>").skip(1) {
            if let Some((name, _)) = chunk.split_once("</module>") {
                let dir = if base.is_empty() { name.trim().to_string() } else { format!("{base}/{}", name.trim()) };
                modules.push(Module { name: name.trim().to_string(), files: count_under(&dir), path: dir, kind: "maven".into() });
            }
        }
        if m == "pom.xml" {
            let mvn = if root.join("mvnw").exists() { "./mvnw" } else { "mvn" };
            commands.push(cmd("maven-build", "构建（跳过测试）", "build", &format!("{mvn} -B -DskipTests package"), "Maven 项目", true));
            commands.push(cmd("maven-test", "测试", "test", &format!("{mvn} -B test"), "Maven 项目", true));
        }
    }
    for name in ["build.gradle", "build.gradle.kts"] {
        for m in manifest(name) {
            pms.insert("gradle".into()); builds.insert("Gradle".into());
            let toks = tokens(&read(root, m));
            fws.extend(matches(FRAMEWORKS, &toks)); dbs.extend(matches(DATABASES, &toks)); tests.extend(matches(TESTS, &toks));
            if m == name && !commands.iter().any(|c| c.id.starts_with("gradle")) {
                let g = if root.join("gradlew").exists() { "./gradlew" } else { "gradle" };
                commands.push(cmd("gradle-build", "构建（跳过测试）", "build", &format!("{g} build -x test"), "Gradle 项目", true));
                commands.push(cmd("gradle-test", "测试", "test", &format!("{g} test"), "Gradle 项目", true));
            }
        }
    }
    // Go
    for m in manifest("go.mod") {
        pms.insert("go modules".into()); builds.insert("Go".into());
        let toks = tokens(&read(root, m));
        fws.extend(matches(FRAMEWORKS, &toks)); dbs.extend(matches(DATABASES, &toks));
        if m == "go.mod" {
            commands.push(cmd("go-build", "构建", "build", "go build ./...", "Go 模块", true));
            commands.push(cmd("go-test", "测试", "test", "go test ./...", "Go 模块", true));
            commands.push(cmd("go-vet", "静态检查 (go vet)", "lint", "go vet ./...", "Go 模块", false));
        } else {
            let d = m.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            modules.push(Module { name: file_name(d).to_string(), files: count_under(d), path: d.to_string(), kind: "go".into() });
        }
    }
    // Python
    let py: Vec<&String> = ["pyproject.toml", "requirements.txt", "setup.py", "Pipfile"].iter().flat_map(|n| manifest(n)).collect();
    if !py.is_empty() {
        pms.insert("pip".into());
        for m in &py {
            let toks = tokens(&read(root, m));
            fws.extend(matches(FRAMEWORKS, &toks)); dbs.extend(matches(DATABASES, &toks)); tests.extend(matches(TESTS, &toks));
        }
        if tests.contains("pytest") {
            commands.push(cmd("py-test", "测试 (pytest)", "test", "python3 -m pytest -q", "检测到 pytest；需要已安装依赖", true));
        }
    }
    for (n, label) in [("Gemfile", "bundler"), ("composer.json", "composer")] {
        if !manifest(n).is_empty() { pms.insert(label.into()); }
    }
    if !manifest("CMakeLists.txt").is_empty() { builds.insert("CMake".into()); }
    if paths.iter().any(|p| p.starts_with("tests/") || p.contains("/tests/")) && r.languages.iter().any(|(l, _)| l == "Python") && !tests.contains("pytest") { tests.insert("unittest（推测）".into()); }

    // 模块：去重；没有清单声明时，用含源码的顶层目录兜底
    let mut seen = BTreeSet::new();
    modules.retain(|m| seen.insert(m.path.clone()));
    if modules.is_empty() {
        for (d, n) in &r.top_dirs {
            if d.starts_with('.') || matches!(d.as_str(), "docs" | "doc" | "scripts" | "tools" | "assets" | "deploy") { continue; }
            let has_src = paths.iter().any(|p| p.starts_with(&format!("{d}/")) && file_name(p).rsplit_once('.').is_some_and(|(_, e)| language_of(&e.to_lowercase()).is_some()));
            if has_src { modules.push(Module { name: d.clone(), path: d.clone(), files: *n, kind: "directory".into() }); }
        }
    }
    modules.truncate(60);
    r.modules = modules;

    let cap = |s: BTreeSet<String>| s.into_iter().take(MAX_LIST).collect::<Vec<_>>();
    r.package_managers = cap(pms); r.build_tools = cap(builds); r.frameworks = cap(fws); r.databases = cap(dbs);
    r.test_frameworks = cap(tests); r.ci = cap(ci); r.api_definitions = cap(api); r.db_schemas = cap(schema); r.docs = cap(docs);
    r.commands = commands;
    if r.commands.is_empty() {
        r.notes.push("没有识别出可以自动建议的构建/测试命令，请在基线页手动添加".into());
    }
    if r.languages.is_empty() {
        r.notes.push("没有识别出任何常见编程语言的源码文件".into());
    }
    r
}

fn strip_credentials(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(s), Some(a)) if a > s => format!("{}{}", &url[..s + 3], &url[a + 1..]),
        _ => url.to_string(),
    }
}

pub async fn git_info(root: &Path) -> GitInfo {
    let Ok(top) = git_output(root, &["rev-parse", "--show-toplevel"]).await else { return GitInfo::default() };
    let g = Path::new(top.trim());
    let mut gi = GitInfo { is_repo: true, ..Default::default() };
    gi.branch = git_output(g, &["symbolic-ref", "--short", "-q", "HEAD"]).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    gi.head = git_output(g, &["rev-parse", "HEAD"]).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if gi.head.is_none() {
        return gi; // 还没有任何提交
    }
    gi.commit_count = git_output(g, &["rev-list", "--count", "HEAD"]).await.ok().and_then(|s| s.trim().parse().ok());
    gi.branch_count = git_output(g, &["branch", "-a", "--format=%(refname:short)"]).await.ok().map(|s| s.lines().filter(|l| !l.is_empty()).count());
    gi.tag_count = git_output(g, &["tag", "--list"]).await.ok().map(|s| s.lines().filter(|l| !l.is_empty()).count());
    gi.first_commit_date = git_output(g, &["log", "--reverse", "--format=%ad", "--date=short", "-1", "--max-parents=0"]).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    gi.last_commit = git_output(g, &["log", "-1", "--format=%ad %s", "--date=short"]).await.ok().map(|s| s.trim().to_string());
    gi.recent = git_output(g, &["log", "-10", "--format=%h %ad %s", "--date=short"]).await.ok().map(|s| s.lines().map(String::from).collect()).unwrap_or_default();
    gi.authors = git_output(g, &["shortlog", "-sn", "--no-merges", "HEAD"]).await.ok().map(|s| {
        s.lines().take(5).filter_map(|l| { let (n, name) = l.trim().split_once('\t')?; Some((name.to_string(), n.trim().parse().ok()?)) }).collect()
    }).unwrap_or_default();
    gi.remotes = git_output(g, &["remote", "-v"]).await.ok().map(|s| {
        let set: BTreeSet<String> = s.lines().filter(|l| l.ends_with("(fetch)")).map(|l| { let mut it = l.split_whitespace(); format!("{} {}", it.next().unwrap_or(""), strip_credentials(it.next().unwrap_or(""))) }).collect();
        set.into_iter().collect()
    }).unwrap_or_default();
    gi.dirty = git_output(g, &["status", "--porcelain", "--untracked-files=no"]).await.map(|s| !s.trim().is_empty()).unwrap_or(false);
    gi
}

pub async fn scan_project(root: &Path) -> Result<ScanReport> {
    let owned = root.to_path_buf();
    let mut report = tokio::task::spawn_blocking(move || scan_dir(&owned)).await?;
    report.git = git_info(root).await;
    report.scanned_at = crate::db::now_ms();
    if report.git.dirty {
        report.notes.push("工作区有未提交的改动：基线只针对已提交的 HEAD，不包含这些改动".into());
    }
    if !report.git.is_repo {
        report.notes.push("目录还不是 Git 仓库，导入前需要先初始化".into());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn detects_a_rust_workspace_with_tauri_and_sqlite() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Cargo.toml", "[workspace]\nmembers = [\"crates/*\", \"app\"]\n");
        write(d.path(), "crates/core/Cargo.toml", "[package]\nname='core'\n[dependencies]\nrusqlite='1'\ntokio='1'\n");
        write(d.path(), "crates/core/src/lib.rs", "");
        write(d.path(), "crates/cli/Cargo.toml", "[package]\nname='cli'\n");
        write(d.path(), "crates/cli/src/main.rs", "");
        write(d.path(), "app/Cargo.toml", "[package]\nname='app'\n[dependencies]\ntauri='2'\n");
        write(d.path(), "app/src/lib.rs", "");
        write(d.path(), ".github/workflows/ci.yml", "on: push");
        write(d.path(), "README.md", "# hi");
        write(d.path(), "target/debug/junk.rs", "应被忽略");
        let r = scan_dir(d.path());
        assert_eq!(r.languages[0].0, "Rust");
        assert_eq!(r.total_files, 9, "target 目录被忽略：{:?}", r.total_files);
        let names: Vec<_> = r.modules.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"core") && names.contains(&"cli") && names.contains(&"app"), "{names:?}");
        assert!(r.databases.contains(&"SQLite".to_string()) && r.frameworks.contains(&"Tauri".to_string()) && r.frameworks.contains(&"Tokio".to_string()));
        assert!(r.ci.iter().any(|c| c.contains("GitHub Actions")));
        assert!(r.package_managers.contains(&"cargo".to_string()) && r.test_frameworks.contains(&"cargo test".to_string()));
        assert!(r.commands.iter().any(|c| c.id == "cargo-test" && c.command == "cargo test --workspace" && c.default_on));
        assert!(r.commands.iter().any(|c| c.id == "cargo-clippy" && !c.default_on));
        assert!(r.docs.contains(&"README.md".to_string()));
    }

    #[test]
    fn detects_node_project_commands_by_lockfile_and_scripts() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "package.json", r#"{"scripts":{"build":"vite build","test":"vitest run"},"dependencies":{"react":"18","express":"4"},"devDependencies":{"vitest":"1","@playwright/test":"1"}}"#);
        write(d.path(), "pnpm-lock.yaml", "");
        write(d.path(), "src/App.tsx", "");
        write(d.path(), "openapi.yaml", "openapi: 3");
        write(d.path(), "db/migrations/001.sql", "create table t();");
        write(d.path(), "docker-compose.yml", "services:\n  db:\n    image: postgres:16\n  cache:\n    image: redis:7");
        let r = scan_dir(d.path());
        assert!(r.frameworks.contains(&"React".to_string()) && r.frameworks.contains(&"Express".to_string()));
        assert!(r.test_frameworks.contains(&"Vitest".to_string()) && r.test_frameworks.contains(&"Playwright".to_string()));
        assert!(r.databases.contains(&"PostgreSQL".to_string()) && r.databases.contains(&"Redis".to_string()));
        assert_eq!(r.package_managers, vec!["pnpm"]);
        let ids: Vec<_> = r.commands.iter().map(|c| (c.id.as_str(), c.command.as_str())).collect();
        assert!(ids.contains(&("node-install", "pnpm install --frozen-lockfile")), "{ids:?}");
        assert!(ids.contains(&("node-build", "pnpm run build")) && ids.contains(&("node-test", "pnpm test")));
        assert!(!ids.iter().any(|(i, _)| *i == "node-lint"), "没有 lint 脚本就不建议");
        assert!(r.api_definitions.contains(&"openapi.yaml".to_string()));
        assert!(r.db_schemas.iter().any(|s| s.contains("migrations")));
    }

    #[test]
    fn detects_maven_modules_and_spring() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "pom.xml", "<project><modules><module>svc-a</module><module>svc-b</module></modules></project>");
        write(d.path(), "svc-a/pom.xml", "<dependency><artifactId>spring-boot-starter-web</artifactId></dependency><artifactId>mysql-connector-j</artifactId><artifactId>junit-jupiter</artifactId>");
        write(d.path(), "svc-a/src/Main.java", "");
        write(d.path(), "svc-b/pom.xml", "<project/>");
        write(d.path(), "mvnw", "#!/bin/sh");
        let r = scan_dir(d.path());
        assert!(r.frameworks.contains(&"Spring Boot".to_string()) && r.databases.contains(&"MySQL".to_string()) && r.test_frameworks.contains(&"JUnit".to_string()));
        let m: Vec<_> = r.modules.iter().map(|m| (m.name.as_str(), m.kind.as_str())).collect();
        assert_eq!(m, vec![("svc-a", "maven"), ("svc-b", "maven")]);
        assert!(r.commands.iter().any(|c| c.id == "maven-test" && c.command == "./mvnw -B test"), "有 mvnw 就用它");
    }

    #[test]
    fn unknown_layout_falls_back_to_directories_and_notes() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "backend/app.py", "");
        write(d.path(), "backend/util.py", "");
        write(d.path(), "docs/a.md", "");
        let r = scan_dir(d.path());
        assert_eq!(r.modules.len(), 1);
        assert_eq!((r.modules[0].name.as_str(), r.modules[0].kind.as_str()), ("backend", "directory"));
        assert!(r.commands.is_empty() && r.notes.iter().any(|n| n.contains("没有识别出")));
        assert!(scan_dir(tempfile::tempdir().unwrap().path()).notes.iter().any(|n| n.contains("编程语言")));
    }

    #[test]
    fn strips_credentials_from_remote_urls() {
        assert_eq!(strip_credentials("https://user:tok@github.com/a/b.git"), "https://github.com/a/b.git");
        assert_eq!(strip_credentials("git@github.com:a/b.git"), "git@github.com:a/b.git");
    }

    #[test]
    fn symlinks_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.rs", "");
        write(d.path(), "a.rs", "");
        std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
        assert_eq!(scan_dir(d.path()).total_files, 1);
    }
}
