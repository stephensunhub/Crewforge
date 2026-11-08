#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 应用自身也兼作 MCP 服务器：员工进程会以 `<应用> mcp-serve ...` 启动它，此时不创建窗口
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp-serve") {
        if let Err(e) = crewforge_core::mcp::run_from_args(&args[1..]) {
            eprintln!("mcp-serve 失败: {e}");
            std::process::exit(1);
        }
        return;
    }
    crewforge_app_lib::run()
}
