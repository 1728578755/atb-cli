use super::*;
#[allow(unused_imports)]
use clap::{Args, Parser, Subcommand};

pub(crate) fn run_action_command(op: &str, args: ActionArgs, is_async: bool) -> Result<()> {
    if args.target.is_none() && op != "scroll" {
        // 文案必须和预检一致：`text=` 是 Playwright 语法，preflight 必然拒绝，
        // 旧文案却写着「支持 text=可见文本」，用户照着写只会拿到 invalid_selector。
        return Err(usage_err!(
            "{op}: 需要 target（CSS 选择器；按可见文本定位请先 find --text '<文本>' 再用其结果）"
        ));
    }
    let mut params = json!({});
    if let Some(index) = args.index {
        params["index"] = json!(index);
    }
    if let Some(value) = args.value {
        params["value"] = json!(value);
    }
    if let Some(label) = args.label {
        params["label"] = json!(label);
    }
    if let Some(dy) = args.dy {
        params["dy"] = json!(dy);
    }
    if let Some(to) = args.to {
        params["to"] = json!(to);
    }
    let payload = json!({
        "op": op,
        "target": args.target,
        "params": params,
        "frame_id": args.frame,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        "timeout": args.timeout,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan(op, &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job(op, payload)?);
    } else {
        dispatch_page(
            op,
            "/action",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

/// 解析 `challenge click --at x,y` 的坐标。
///
/// 只接受非负数字：页面视口坐标不可能是负的，负值基本是把窗口坐标/屏幕坐标混进来了。
pub(crate) fn parse_point(raw: &str) -> Result<[f64; 2]> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    // `--at 1,2,3` 以前静默忽略第三段；`inf`/`nan` 能通过 `>= 0.0` 校验，
    // 随后被 `json!` 序列化成 null 下发。
    if parts.len() != 2 {
        return Err(usage_err!(
            "--at 需要正好两个数字（x,y），当前给了 {} 段：{raw}",
            parts.len()
        ));
    }
    let parse = |value: &str| {
        value
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite() && *number >= 0.0)
    };
    match (parse(parts[0]), parse(parts[1])) {
        (Some(x), Some(y)) => Ok([x, y]),
        _ => Err(usage_err!(
            "--at 需要 `x,y` 两个非负有限数字（页面视口坐标），例如 --at 63,313；坐标可用 `atb challenge widget` 获取"
        )),
    }
}

pub(crate) fn cmd_mouse_click(args: TargetCommandArgs, is_async: bool) -> Result<()> {
    // is_async 由下游处理（run_target_command 支持后台提交）
    run_target_command("/mouse-click", "mouse-click", args, is_async)
}

pub(crate) fn cmd_wait(args: WaitArgs, is_async: bool) -> Result<()> {
    let payload = json!({
        "selector": args.selector,
        "text": args.text,
        "js": args.js,
        "visible": args.visible,
        "timeout": args.timeout,
        "interval": args.interval,
        "frame_id": args.frame,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
        // 0 = 用户写了不带值的 --dom-stable / --network-idle，由 daemon
        // 按 ATB_WAIT_DOM_STABLE_MS / ATB_SCREENSHOT_NETWORK_IDLE_MS 取默认值。
        "dom_stable": args.dom_stable,
        "network_idle": args.network_idle,
    });
    // ：dry-run + async 必须在 ensure_server()/HTTP 之前短路。
    if async_dry_run_plan("wait", &payload, is_async)? {
        return Ok(());
    }
    ensure_server()?;
    if is_async {
        print_json(submit_job("wait", payload)?);
    } else {
        dispatch_page(
            "wait",
            "/wait",
            payload,
            args.tab.as_deref(),
            args.browser.as_deref(),
            args.profile.as_deref(),
            http_budget(args.timeout),
        )?;
    }
    Ok(())
}

/// dry-run 时的「将要做什么」：不解析目标（需要 daemon 往返），但把动作语义写清楚。
fn challenge_plan(action: &str, args: &ChallengeArgs, point: Option<[f64; 2]>) -> Vec<String> {
    let mut would = vec![match action {
        "status" | "detect" => {
            "识别当前页是否处于挑战页（POST /challenge action=status，不等待）".to_string()
        }
        "wait" => format!(
            "等待挑战页自动放行：每 {}s 检查一次，最多 {}s{}",
            args.interval,
            args.timeout.unwrap_or(30.0),
            if args.reload_after > 0.0 {
                format!("，被拦满 {}s 后 reload 一次", args.reload_after)
            } else {
                String::new()
            }
        ),
        "solve" => format!(
            "主动求解挑战并等结果：最多 {}s",
            args.timeout.unwrap_or(120.0)
        ),
        "auto" => "主动求解一次（不等结果）".to_string(),
        "click" => match point {
            Some([x, y]) => format!("在页面坐标 ({x}, {y}) 点一次"),
            None => "点一次（缺 --at X,Y，真跑会直接报错）".to_string(),
        },
        "widget" => "返回挑战组件的可点区域与建议坐标".to_string(),
        "focus" => "把目标标签页带到前台（不操作页面内容）".to_string(),
        _ => format!("challenge {action}"),
    }];
    if let Some(confirm_ms) = args.confirm_ms {
        would.push(format!("点击后等待页面确认 {confirm_ms}ms"));
    }
    would.push(
        "dry-run 不解析目标标签页（需要 daemon 往返）；真跑时按 --tab/--browser/--profile 选中目标"
            .to_string(),
    );
    would
}

pub(crate) fn cmd_challenge(args: ChallengeArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("challenge", is_async)?;
    let action = args.action.to_lowercase();
    // action 是字符串枚举：拼错以前会一路发到 daemon，回来一个页面级错误。
    // `detect` 是 daemon 侧 `status` 的历史别名，保留兼容。
    if !matches!(
        action.as_str(),
        "status" | "detect" | "wait" | "solve" | "auto" | "click" | "widget" | "focus"
    ) {
        return Err(usage_failure(
            "invalid_action",
            format!("challenge: 未知 action {action}"),
            "可选 status（识别，detect 同义）| wait（等放行）| solve（求解并等结果）| auto（求解一次）| click（按坐标点）| widget（取组件坐标）| focus（带到前台）",
        ));
    }
    // 坐标是纯本地解析（不碰 daemon），dry-run 也要照常校验。
    //
    // v11：`--at` 以前对所有 action 生效，`challenge status --at 1,2`
    // 会在识别阶段意外点一次鼠标（坐标点击是不可撤销的副作用）。只有 click
    // 需要坐标；其它 action 带了就直接判用法错误，而不是静默忽略。
    if args.at.is_some() && action != "click" {
        return Err(usage_failure(
            "unsupported_combination",
            format!("challenge {action} 不接受 --at（只有 click 按坐标点击）"),
            "去掉 --at；需要按坐标点击时用 `challenge click --at x,y --tab <id>`",
        ));
    }
    let point = args.at.as_deref().map(parse_point).transpose()?;
    if DRY_RUN.load(Ordering::Relaxed) {
        // dry-run 的承诺是「什么动作都不做」：目标标签页解析需要 daemon 往返，
        // 这里跳过，并把「实际会怎么选目标」写进 plan。
        let would = challenge_plan(&action, &args, point);
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "op": "challenge",
                "action": action,
                "target": {
                    "tab": args.tab,
                    "browser": args.browser,
                    "profile": args.profile,
                    "frame": args.frame,
                    "resolved": false,
                },
                "would": would,
            },
        }));
        return Ok(());
    }
    let mut payload = json!({
        "action": action,
        "interval": args.interval,
        "focus_after": args.focus_after,
        "focus": !args.no_focus,
        "point": point,
        "frame_id": args.frame,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
    });
    // 不传就不下发：让服务端按 action 取默认（wait 30s / solve 120s）。
    // 否则 CLI 的 30s 默认值会把服务端的 120s 默认值永久压掉（N-04）。
    if let Some(timeout) = args.timeout {
        payload["timeout"] = json!(timeout);
    }
    if let Some(confirm_ms) = args.confirm_ms {
        payload["confirm_ms"] = json!(confirm_ms);
    }
    // reload_after 的默认值 0 就是「不 reload」，不下发也能得到同样行为；
    // 显式写过才带上，避免 0 撞上预检区间。
    if args.reload_after > 0.0 {
        payload["reload_after"] = json!(args.reload_after);
    }
    // 数值预检：`--timeout nan` 会被 `json!` 序列化成 null 静默丢掉，
    // 服务端于是按默认值执行——调用方以为限了 5s，实际等了 30s。
    if let Err(problem) = preflight::preflight_payload("challenge", &payload) {
        return usage_error(&problem);
    }
    // 预检通过再起 daemon：参数写错时不该留下一个后台进程。
    ensure_server()?;
    // 等服务端的时间要比 wait 的总超时更长，否则 CLI 先断、命令还在跑。
    let effective_timeout = args
        .timeout
        .unwrap_or(if action == "solve" { 120.0 } else { 30.0 });
    // auto / click 是一次性主动求解，预算就是 --timeout，不能再按 30s 掐断。
    let long_running = matches!(action.as_str(), "wait" | "solve" | "auto" | "click");
    let http_timeout = if long_running {
        // 统一「业务超时 + slack」，不再各命令各写一个魔法余量。
        http_budget(effective_timeout + 5.0)
    } else {
        http_budget(30.0)
    };
    let mut value = request("POST", "/challenge", Some(payload), http_timeout)?;
    // 超时 / 未通过不是成功：把顶层 ok 改成 false，统一由 print_json 的
    // 「ok:false → 退出码 1」处理，避免这类命令只能靠 cleared/passed 字段判断（N-09）。
    if long_running && value.get("ok").and_then(Value::as_bool) == Some(true) {
        // `passed` 是 wait/solve 的主判据，`cleared` 是兜底：wait 的目标就是
        // 「离开挑战页」，而 daemon 曾经只在 solve 分支写 `passed`，于是成功清除
        // 被判成 challenge_timeout。
        //
        // v11：click 是「派发一次点击」，它不是等待类动作 —— daemon 的 click
        // 分支只回 `clicked` + `cleared`，没有 `passed`。旧实现用 passed/cleared
        // 判 click，点击成功后 `cleared` 仍为 false（页面不可能一次点击就通过），
        // 于是把成功改写成 challenge_timeout。现在 click 只看 `clicked`，
        // `cleared` 仅作为独立诊断字段透传。
        let achieved = if action == "click" {
            // 两种 click 回包形状都要接受：
            // - `click --at x,y`：坐标分支回 `clicked`（点了就算达成，页面不会瞬间通过）；
            // - `click`（无 --at）：走一次性 auto_solve 分支，回 `passed`/`cleared`。
            // 只看 `clicked` 会把后者的成功也改写成 challenge_timeout。
            value
                .pointer("/result/clicked")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || value
                    .pointer("/result/passed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                || value
                    .pointer("/result/cleared")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        } else {
            value
                .pointer("/result/passed")
                .or_else(|| value.pointer("/result/cleared"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if !achieved {
            value["ok"] = json!(false);
            value["error_code"] = json!("challenge_timeout");
            value["error"] = json!(format!(
                "challenge {action}: {}",
                if action == "click" {
                    "点击未能派发（坐标越界 / CDP 不可用，见 result.diag）"
                } else {
                    "超时未达成目标（页面仍被挑战页拦截），可用 --timeout 延长等待"
                }
            ));
        }
    }
    print_json(value);
    Ok(())
}

pub(crate) fn cmd_stealth(args: StealthArgs, is_async: bool) -> Result<()> {
    reject_async_if_set("stealth", is_async)?;
    let action = args.action.to_lowercase();
    if !matches!(action.as_str(), "audit" | "status" | "scrub" | "clean") {
        return usage_error(
            &preflight::Problem::new(
                "unknown_stealth_action",
                format!("stealth: 未知 action {action}"),
                "可选 audit（审计当前页）或 scrub（清除本工具痕迹）",
            )
            .with_example("atb stealth audit"),
        );
    }
    if !args.timeout.is_finite() || args.timeout <= 0.0 {
        return usage_error(&preflight::Problem::new(
            "invalid_timeout",
            "stealth --timeout 必须是大于 0 的有限秒数",
            "默认 15 秒；页面很大或扩展刚唤醒时可以调到 30",
        ));
    }
    if DRY_RUN.load(Ordering::Relaxed) {
        // 目标解析需要 daemon 往返，dry-run 跳过，只把将要执行的动作说清楚。
        let would = match action.as_str() {
            "audit" => "审计当前页可被站点读到的自动化信号（POST /stealth action=audit）",
            "status" => "查询本工具在目标页上的痕迹状态（POST /stealth action=status）",
            _ => "清除本工具在目标页留下的痕迹（POST /stealth action=scrub）",
        };
        print_json(json!({
            "ok": true,
            "result": {
                "dry_run": true,
                "op": "stealth",
                "action": action,
                "target": {
                    "tab": args.tab,
                    "browser": args.browser,
                    "profile": args.profile,
                    "frame": args.frame,
                    "resolved": false,
                },
                "would": [
                    would,
                    "dry-run 不解析目标标签页（需要 daemon 往返）；真跑时按 --tab/--browser/--profile 选中目标",
                ],
            },
        }));
        return Ok(());
    }
    let payload = json!({
        "action": action,
        "timeout": args.timeout,
        "frame_id": args.frame,
        "switch_tab_id": args.tab,
        "browser": args.browser,
        "profile": args.profile,
    });
    ensure_server()?;
    let value = request("POST", "/stealth", Some(payload), http_budget(args.timeout))?;
    print_json(value);
    Ok(())
}
