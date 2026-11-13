// 轮 · CLI 契约守卫。
//
// 背景：`--dry-run --async` 曾经在 exec 以外的 14 条命令上**真实执行**动作
// （`atb click '#pay' --async --dry-run` 会真的点下去）。dry-run 的承诺是
// 「预检通过就输出计划，不产生任何页面副作用」，这类静默照做比报错危险。
//
// 这里不做网络测试（起 daemon 会污染环境、且 src 正在并行改动），改为源码级
// 不变式断言：只校验「守卫存在且顺序正确」「计划字段齐全」，不校验行号与措辞，
// 所以重构一般不会误红。行为侧由 src/cli/cmd_pipeline.rs 的 #[test] 覆盖。
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const CLI_DIR = join(ROOT, 'src', 'cli');

const stripComments = (text) =>
  text
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:"'\\])\/\/[^\n]*/g, '$1');

const sources = new Map(
  readdirSync(CLI_DIR)
    .filter((name) => name.endsWith('.rs'))
    .map((name) => [name, stripComments(readFileSync(join(CLI_DIR, name), 'utf8'))]),
);
const file = (name) => {
  const text = sources.get(name);
  assert.ok(text, `应能读到 src/cli/${name}`);
  return text;
};

/** 按花括号配平截出函数体（处理字符串/注释，避免正则跨函数）。 */
function fnBody(text, name) {
  const match = new RegExp(`\\bfn\\s+${name}\\s*[(<]`).exec(text);
  if (!match) return null;
  const index = text.indexOf('{', match.index);
  if (index < 0) return null;
  let depth = 0;
  let inString = false;
  let escaped = false;
  let lineComment = false;
  let blockComment = false;
  for (let i = index; i < text.length; i += 1) {
    const ch = text[i];
    const next = text[i + 1];
    if (lineComment) {
      if (ch === '\n') lineComment = false;
      continue;
    }
    if (blockComment) {
      if (ch === '*' && next === '/') {
        blockComment = false;
        i += 1;
      }
      continue;
    }
    if (inString) {
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '/' && next === '/') {
      lineComment = true;
      i += 1;
      continue;
    }
    if (ch === '/' && next === '*') {
      blockComment = true;
      i += 1;
      continue;
    }
    if (ch === '"') {
      inString = true;
      continue;
    }
    if (ch === '{') depth += 1;
    else if (ch === '}') {
      depth -= 1;
      if (depth === 0) return text.slice(index, i + 1);
    }
  }
  return null;
}

/** 枚举文件里所有 `fn name` 及其函数体。 */
function allFnBodies(text) {
  const out = [];
  const re = /\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(<]/g;
  let match;
  while ((match = re.exec(text)) !== null) {
    const body = fnBody(text, match[1]);
    if (body) out.push({ name: match[1], body });
  }
  return out;
}

// ---- dry-run × async 必须在任何 HTTP 之前短路 ---------------------

test('submit_job 自身在 DRY_RUN 下返回计划，绝不提交（防御性第二道闸）', () => {
  const pipeline = file('cmd_pipeline.rs');
  const body = fnBody(pipeline, 'submit_job');
  assert.ok(body, '应存在 submit_job');
  const guard = body.indexOf('DRY_RUN');
  const post = body.indexOf('/job/submit');
  assert.ok(guard >= 0, 'submit_job 必须读 DRY_RUN');
  assert.ok(post > guard, 'DRY_RUN 判定必须在 POST /job/submit 之前');
  assert.match(
    body.slice(guard, post),
    /async_plan_json\s*\(/,
    'DRY_RUN 分支必须直接返回计划，而不是继续往下走',
  );
});

test('async_dry_run_plan 只接管 is_async + DRY_RUN 的组合', () => {
  const body = fnBody(file('cmd_pipeline.rs'), 'async_dry_run_plan');
  assert.ok(body, '应存在 async_dry_run_plan（统一短路点）');
  assert.match(
    body,
    /if\s*!\s*is_async\s*\|\|\s*!\s*DRY_RUN\.load/,
    '非 async 或非 dry-run 时必须放行',
  );
  assert.match(body, /Ok\(false\)/, '未命中时必须返回 Ok(false) 让调用方继续');
  // 短路点自身不能再拉 daemon。
  assert.ok(
    !body.includes('ensure_server()'),
    'async_dry_run_plan 不得调用 ensure_server()',
  );
  // 计划里的 args 必须走与真实提交同一份收尾（绝对化 + 预检 + 目标语法）。
  assert.match(body, /finalize_page_payload\s*\(/, '计划必须走同一份参数收尾');
});

test('计划体字段固定为 ok/dry_run/async/would{op,args}', () => {
  const body = fnBody(file('cmd_pipeline.rs'), 'async_plan_json');
  assert.ok(body, '应存在 async_plan_json');
  assert.match(body, /"dry_run"\s*:\s*true/);
  assert.match(body, /"async"\s*:\s*true/);
  assert.match(body, /"would"\s*:/);
  assert.match(body, /"op"\s*:\s*op/);
  assert.match(body, /"args"\s*:\s*args/);
});

test('每个 submit_job 调用点都在 ensure_server/HTTP 之前做了 dry-run 短路', () => {
  const offenders = [];
  for (const [name, text] of sources) {
    // 单测模块里的调用是「故意绕过调用点」的回归用例，不参与本断言。
    const testMod = text.indexOf('mod tests {');
    for (const { name: fn, body } of allFnBodies(text)) {
      if (fn === 'submit_job') continue; // 定义本身
      if (!body.includes('submit_job(')) continue;
      if (testMod >= 0 && text.indexOf(body) > testMod) continue;
      const guard = body.indexOf('async_dry_run_plan(');
      if (guard < 0) {
        offenders.push(`${name}::${fn} 没有 async_dry_run_plan 守卫`);
        continue;
      }
      const server = body.indexOf('ensure_server()');
      if (server >= 0 && server < guard) {
        offenders.push(`${name}::${fn} 先 ensure_server() 再判 dry-run`);
      }
    }
  }
  assert.deepEqual(offenders, [], offenders.join('\n'));
});

test('分发层不能把 click/fill/scan 等命令的 --dry-run 静默拒绝或忽略', () => {
  const mod = file('mod.rs');
  const dry = fnBody(mod, 'reject_dry_run_if_unsupported');
  assert.ok(dry, '应存在 reject_dry_run_if_unsupported');
  for (const cmd of ['Click', 'Fill', 'Scan', 'Screenshot', 'Snapshot', 'Upload']) {
    assert.ok(
      !dry.includes(`CommandKind::${cmd}`),
      `dry-run 已在 ${cmd} 上实现，不能再进拒绝清单（否则 flag 被静默挡掉）`,
    );
  }
  const async = fnBody(mod, 'reject_async_if_unsupported');
  assert.ok(async, '应存在 reject_async_if_unsupported');
  for (const cmd of ['Click', 'Fill', 'Scan', 'Exec', 'Snapshot', 'Upload']) {
    assert.ok(
      !async.includes(`CommandKind::${cmd}`),
      `${cmd} 支持 --async，不能被分发层拒绝`,
    );
  }
});

test('全局 --async / --dry-run 是 global flag（所有子命令都能解析）', () => {
  const args = file('args.rs');
  assert.match(args, /#\[arg\(long = "async", global = true\)\]/);
  assert.match(args, /#\[arg\(long = "dry-run", global = true\)\]/);
});

// ---- set-extension-port 关闭失败不得谎报 restarted:true -----------------

test('set-extension-port 用共用原语确认关闭，失败时不得报 restarted:true', () => {
  const open = file('cmd_open.rs');
  const body = fnBody(open, 'cmd_set_extension_port');
  assert.ok(body, '应存在 cmd_set_extension_port');
  assert.match(body, /stop_daemon_confirmed\s*\(/, '必须走共用关闭确认原语');
  assert.ok(
    !/let _ = request\(\s*"POST",\s*"\/shutdown"/.test(body),
    '/shutdown 的失败不得再用 `let _ =` 吞掉',
  );
  // 失败分支必须显式写 restarted:false（且整体 ok:false）。
  const failBranch = body.slice(body.indexOf('stop_daemon_confirmed'));
  assert.match(
    failBranch,
    /"restarted"\s*:\s*false|\["restarted"\]\s*=\s*json!\(false\)/,
  );
  assert.match(failBranch, /config_written/);
});

test('restart 与 set-extension-port 共用「关闭确认」原语', () => {
  const open = file('cmd_open.rs');
  const restart = fnBody(open, 'cmd_restart');
  assert.ok(restart, '应存在 cmd_restart');
  assert.match(restart, /stop_daemon_confirmed\s*\(/);
  const primitive = fnBody(open, 'stop_daemon_confirmed');
  assert.ok(primitive, '应存在 stop_daemon_confirmed');
  assert.match(primitive, /wait_server_stopped\s*\(/, '必须校验端口真的释放');
  assert.match(primitive, /shutdown_failed/, '未停住时给 shutdown_failed');
  assert.match(primitive, /daemon_token_mismatch/, 'token 不匹配时给专属错误码');
});

// ---- /：tabs --ids --limit 0 与 full 视图同语义 ------------------

test('tabs --ids 的 --limit 与 full 视图同样把 0 当「不截断」', () => {
  const tabs = file('cmd_tabs.rs');
  const view = fnBody(tabs, 'tabs_ids_view');
  assert.ok(view, '--ids 视图应抽成可单测的函数');
  assert.match(
    view,
    /limit\.filter\(\|value\| \*value > 0\)/,
    'truncate 前必须过滤 0，否则 --limit 0 返回空列表',
  );
  const full = fnBody(tabs, 'apply_fields_limit');
  assert.ok(full, '应存在 apply_fields_limit');
  assert.match(full, /limit\.filter\(\|value\| \*value > 0\)/, '两个视图口径必须一致');
});

// ---- /：Windows 盘符相对路径必须绝对化 ---------------------------

test('absolutize_path 处理 Windows 盘符相对路径（C:foo）', () => {
  const http = file('http.rs');
  const body = fnBody(http, 'absolutize_path_inner');
  assert.ok(body, '应存在 absolutize_path_inner');
  assert.match(
    body,
    /std::path::absolute\s*\(/,
    '盘符相对路径必须用 std::path::absolute 展开（cwd.join 会被盘符截断）',
  );
  assert.match(body, /is_drive_relative\s*\(/, '应有盘符相对路径判定');
});

// ---- 本轮新增：schema 错误码目录 / status.security / doctor.ws_trust --------

test('schema 顶层提供 error_code_hints，且覆盖本轮新增的 5 个码', () => {
  const schema = file('cmd_schema.rs');
  assert.match(schema, /"error_code_hints"\s*:\s*hints/, 'cmd_schema 必须输出 error_code_hints');
  const catalogue = fnBody(schema, 'schema_entries');
  assert.ok(catalogue, '应存在 schema_entries');
  for (const code of [
    'ws_backpressure',
    'ws_disconnected',
    'task_panicked',
    'wait_js_eval_blocked',
    'too_many_rules',
  ]) {
    assert.match(
      schema,
      new RegExp(`"${code}"`),
      `错误码目录缺少 ${code}（Agent 拿到 ok:false 时无从判断怎么处置）`,
    );
  }
  // 页面命令必须自动带上 WS 下行通道的两类失败码（否则新增命令会漏）。
  assert.match(schema, /PAGE_OP_COMMANDS/, '应有页面命令清单用于自动补 WS 码');
  assert.match(
    schema,
    /for code in \["ws_backpressure", "ws_disconnected"\]/,
    'push 里必须自动补 ws_backpressure / ws_disconnected',
  );
});

test('atb status 输出 security 块', () => {
  const status = file('cmd_status.rs');
  const body = fnBody(status, 'security_value');
  assert.ok(body, '应存在 security_value');
  for (const key of ['ws_trust_mode', 'token_configured', 'extension_ids', 'mutual_auth']) {
    assert.match(body, new RegExp(`"${key}"`), `security 块缺少 ${key}`);
  }
  // mutual_auth 只在配了 token 时才 on（daemon 会下发 ws_hello proof）。
  assert.match(
    body,
    /"mutual_auth"\s*:\s*if token_configured \{ "on" \} else \{ "off" \}/,
    'mutual_auth 必须由 token_configured 决定',
  );
  // 只读本地配置：不得出现写文件调用。
  assert.ok(!/fs::write|OpenOptions|write_file/.test(body), 'security_value 不能写任何文件');
  const assignments = (stripComments(status).match(/value\["security"\]\s*=/g) || []).length;
  assert.equal(assignments, 1, `security 块只能有一个挂载点（当前 ${assignments} 处）`);
  assert.match(fnBody(status, 'status_value'), /value\["security"\]\s*=\s*security/);
});

test('atb doctor 增加 ws_trust 检查但不改退出码', () => {
  const doctor = file('cmd_status.rs');
  const body = fnBody(doctor, 'doctor_value');
  assert.ok(body, '应存在 doctor_value');
  const occurrences = body.split('"name": "ws_trust"').length - 1;
  assert.equal(occurrences, 1, `ws_trust 检查只能有一条（当前 ${occurrences} 条）`);
  // ok 恒 true + optional：兼容档只是「可以更安全」，不能把 doctor 变成非 0。
  const check = body.slice(body.indexOf('"name": "ws_trust"'));
  assert.match(check.slice(0, 200), /"ok"\s*:\s*true/);
  assert.match(check.slice(0, 400), /"optional"\s*:\s*true/);
  assert.match(check.slice(0, 800), /"status"/, '必须带 status: warn/ok');
  assert.match(check.slice(0, 1200), /trust-extension/, 'fix 必须给出命令模板');
});

test('幂等键按 method|url|body 派生子键（多请求命令不会 409）', () => {
  const http = file('http.rs');
  const derive = fnBody(http, 'derive_idempotency_key');
  assert.ok(derive, '应存在 derive_idempotency_key');
  assert.match(derive, /method\.hash/, '子键必须包含方法');
  assert.match(derive, /url\.hash/, '子键必须包含 URL');
  assert.match(derive, /payload[\s\S]{0,200}hash\(&mut hasher\)/, '子键必须包含请求体');
  // 下发点必须真的用它，不能退回裸 key。
  const send = fnBody(http, 'send_once');
  assert.ok(send, '应存在 send_once');
  assert.match(send, /derive_idempotency_key\s*\(/, 'send_once 必须经派生后再挂 header');
  assert.ok(
    !/IDEMPOTENCY_KEY\s*\.get\(\)\s*\.map\(\|key\| key\.clone\(\)\)/.test(send),
    '不能把用户 key 原样挂到每个 POST 上',
  );
});

test('challenge --at 仅 click 可用，click 判据看 clicked', () => {
  const action = file('cmd_action.rs');
  const body = fnBody(action, 'cmd_challenge');
  assert.ok(body, '应存在 cmd_challenge');
  assert.match(
    body,
    /if args\.at\.is_some\(\) && action != "click"/,
    '其它 action 带 --at 必须 exit 2（否则 status --at 会意外点一次）',
  );
  const achieved = body.slice(body.indexOf('/result/clicked'));
  assert.ok(achieved.length > 0, 'click 判据必须读 /result/clicked');
  assert.match(achieved.slice(0, 700), /\/result\/clicked/, 'click 的成功判据是 clicked');
});

test('--world 只接受 main/isolated（exec 与 inject），下发前统一大写', () => {
  for (const name of ['args.rs', 'args_net.rs']) {
    const text = file(name);
    const world = /pub\(crate\) world: String/.test(text);
    assert.ok(world, `${name} 应有 world 字段`);
    assert.match(
      text,
      /value_parser = \["main", "isolated"\]/,
      `${name} 的 --world 必须是枚举，不能是自由字符串`,
    );
    assert.match(text, /ignore_case = true/, `${name} 的 --world 应大小写不敏感`);
  }
  // 扩展只认大写：下发前必须归一（否则 isolated 静默落成 MAIN）。
  const page = file('cmd_page.rs');
  const upper = (page.match(/"world": args\.world\.to_ascii_uppercase\(\)/g) || []).length;
  assert.ok(upper >= 2, `exec 与 inject 下发前都要 to_ascii_uppercase（当前 ${upper} 处）`);
});

test('too_many_rules 归退出码 2（改命令/先清规则），WS 类仍归 1', () => {
  const exit = file('exit.rs');
  const prefixes = fnBody(exit, 'is_usage_error_code');
  assert.ok(prefixes, '应存在 is_usage_error_code');
  assert.match(
    prefixes,
    /USAGE_PREFIXES[^;]*"too_many_"/,
    'too_many_ 必须进 USAGE_PREFIXES（否则 too_many_rules 落 1，Agent 会反复重试一条永远满的配置）',
  );
  // 反例：WS 通道/panic 是可重试的执行失败，不能被顺手加进前缀。
  assert.ok(
    !/USAGE_PREFIXES[^;]*"(ws_|task_)"/.test(prefixes),
    'ws_* / task_* 不能进使用错误前缀',
  );
});

test('net_rule_too_large 用精确码归 2（不是 net_rule_ 前缀）', () => {
  const exit = file('exit.rs');
  const body = fnBody(exit, 'is_usage_error_code');
  assert.ok(body, '应存在 is_usage_error_code');
  assert.match(body, /\|\s*"net_rule_too_large"/, 'net_rule_too_large 必须在白名单里');
  assert.ok(
    !/USAGE_PREFIXES[^;]*"net_rule_"/.test(body),
    '不能用 net_rule_ 前缀（会把未来可重试的 net_rule_* 一起归成用法错误）',
  );
});
