//! `atb focus` / `atb use`（2026-11-13）。
//!
//! 两条命令都只有一次 HTTP 往返，不走 `/multi` 与 job 通道：
//! - focus：把目标标签切到前台（daemon 侧调用扩展 `focusTab`）；
//! - use：把当前 agent 的默认目标固定到指定标签（daemon 侧改会话状态）。
//!
//! `--dry-run` 在本地短路：只输出计划，不拉起 daemon、不发请求。

use super::*;

pub(crate) fn cmd_focus(args: FocusArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "no_window": args.no_window,
        "timeout": args.timeout,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    // （分发层已拒绝 `focus --async`，这里保留同一套入口以防未来放开。）
    if async_dry_run_plan("focus", &payload, is_async)? {
        return Ok(());
    }
    if DRY_RUN.load(Ordering::Relaxed) {
        // 目标的真实解析要 daemon 往返；dry-run 只做本地语法校验并输出计划，
        // 保证「计划里能看出来的错误」不会推迟到真跑。
        let spec = TargetSpec::parse(args.tab.as_deref().unwrap_or(""))
            .map_err(|err| usage_failure(err.code, err.message, err.hint))?;
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "focus",
                "target": spec.describe(),
                "tab": args.tab,
                "browser": args.browser,
                "profile": args.profile,
                "window_focused": !args.no_window,
                "would": "POST /focus（把目标标签切到前台；--no-window 时不聚焦系统窗口）",
                "note": "去掉 --dry-run 即执行；目标存在性由 daemon 解析",
            },
        }));
        return Ok(());
    }
    ensure_server()?;
    let value = request("POST", "/focus", Some(payload), http_budget(args.timeout))?;
    print_json(value);
    Ok(())
}

pub(crate) fn cmd_use(args: UseArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "target": args.target,
        "browser": args.browser,
        "profile": args.profile,
    });
    if async_dry_run_plan("use", &payload, is_async)? {
        return Ok(());
    }
    if DRY_RUN.load(Ordering::Relaxed) {
        let spec = TargetSpec::parse(&args.target)
            .map_err(|err| usage_failure(err.code, err.message, err.hint))?;
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "command": "use",
                "target": spec.describe(),
                "browser": args.browser,
                "profile": args.profile,
                "would": "POST /use（把当前 agent 的默认目标固定到解析出的标签页）",
                "note": "去掉 --dry-run 即执行；目标存在性与唯一性由 daemon 校验",
            },
        }));
        return Ok(());
    }
    ensure_server()?;
    let value = request("POST", "/use", Some(payload), http_budget(15.0))?;
    print_json(value);
    Ok(())
}
