// CLI 退出码契约守卫。
//
// 背景：`$?` 是这个 CLI 唯一的机器可读信号。历史上出现过
//   - 命令失败却 exit 0（status 恒 ok:true）→ 智能体判定成功继续跑；
//   - 用 std::process::exit(3) 直接退出 → 绕过 AGENT_BROWSER_CLI_EXIT0 兼容开关；
//   - job wait 只看 running/非 running 字符串，超时后仍然 exit 0。
//
// 这里不做网络测试（起 daemon 会污染当前环境、且 src 正在并行改动），改为对
// src/cli.rs 做源码级不变式断言：只校验「契约形状」，不校验行号，所以重构一般
// 不会误红。
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC_DIR = join(HERE, '..', 'src');
const CLI_RS = join(SRC_DIR, 'cli.rs');

/// 读取源码树：模块化后 `src/cli.rs` 变成 `src/cli/*.rs`。
/// 单文件布局用单文件；目录布局按文件名排序后拼接（函数不会被拆到两个文件，
/// 所以 fnBody / enclosingFn 这类按花括号切分的工具仍然成立）。
function readSourceTree(singleFile, dir) {
  if (existsSync(singleFile)) return readFileSync(singleFile, 'utf8');
  if (!existsSync(dir)) throw new Error(`找不到源码：${singleFile} 或 ${dir}`);
  const files = [];
  const walk = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const full = join(current, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith('.rs')) files.push(full);
    }
  };
  walk(dir);
  files.sort();
  return files.map((file) => readFileSync(file, 'utf8')).join('\n\n');
}

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : '\n        → ' + detail}`);
}

const source = readSourceTree(CLI_RS, join(SRC_DIR, 'cli'));
const mainSource = readFileSync(join(SRC_DIR, 'main.rs'), 'utf8');

// ---- 源码切分工具 -------------------------------------------------------

function sliceBlock(text, openIndex) {
  let depth = 0;
  let inString = false;
  let escaped = false;
  let inLineComment = false;
  let inBlockComment = false;
  for (let i = openIndex; i < text.length; i += 1) {
    const ch = text[i];
    const next = text[i + 1];
    if (inLineComment) {
      if (ch === '\n') inLineComment = false;
      continue;
    }
    if (inBlockComment) {
      if (ch === '*' && next === '/') {
        inBlockComment = false;
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
      inLineComment = true;
      i += 1;
      continue;
    }
    if (ch === '/' && next === '*') {
      inBlockComment = true;
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
      if (depth === 0) return text.slice(openIndex, i + 1);
    }
  }
  return null;
}

function fnBody(text, name) {
  const pattern = new RegExp(`\\bfn\\s+${name}\\s*[(<]`, 'g');
  const match = pattern.exec(text);
  if (!match) return null;
  const open = text.indexOf('{', match.index);
  if (open < 0) return null;
  return sliceBlock(text, open);
}

// 断言「代码里没有 X」时必须先去掉注释，否则解释「为什么不要 X」的注释会把测试判红。
function stripComments(text) {
  return text
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:"'\\])\/\/[^\n]*/g, '$1');
}

function enclosingFn(text, index) {
  const before = text.slice(0, index);
  const matches = [...before.matchAll(/\bfn\s+([A-Za-z0-9_]+)\s*[(<]/g)];
  return matches.length ? matches[matches.length - 1][1] : null;
}

function numberConst(text, name) {
  const match = new RegExp(`const\\s+${name}\\s*:\\s*i32\\s*=\\s*(-?\\d+)`).exec(text);
  return match ? Number(match[1]) : null;
}

// ---- 1. status：ok 必须绑定到 healthy -------------------------

const okBoundToHealthy =
  /\[\s*"ok"\s*\]\s*=\s*json!\(\s*healthy\s*\)/.test(source) ||
  /"ok"\s*:\s*json!\(\s*healthy\s*\)/.test(source);
check('status 的 ok 字段绑定到 healthy（不再恒 true）', okBoundToHealthy);

const healthyAssignment = /let\s*\(\s*healthy\b/.test(source) || /let\s+healthy\s*=/.test(source);
check('healthy 由运行时状态计算得出', healthyAssignment);

// ---- 2. print_json 把 ok:false 映射成非零退出码 ------------------------

const printJsonBody = fnBody(source, 'print_json');
check('找到 print_json 实现', !!printJsonBody);
if (printJsonBody) {
  check(
    'print_json 在 ok:false 时登记失败退出码',
    /FAILURE_EXIT_CODE\s*\.\s*store\s*\(/.test(printJsonBody),
    printJsonBody.slice(0, 200),
  );
  check(
    'print_json 区分 job_wait_timeout 与其他失败',
    /job_wait_timeout/.test(printJsonBody) && /EXIT_JOB_TIMEOUT/.test(printJsonBody),
  );
}

// ---- 3. job wait：轮询状态 + 超时不算成功 ----------------------

const waitJobBody = fnBody(source, 'wait_job');
check('找到 wait_job 实现', !!waitJobBody);
if (waitJobBody) {
  check(
    'wait_job 依据 job 状态字段判断是否结束',
    /\/job\/status/.test(waitJobBody) && /status\s*!=\s*"running"/.test(waitJobBody),
    '未匹配到 /job/status 或 status != "running"',
  );
  check(
    'wait_job 超时返回 ok:false 且带 error_code',
    /"ok"\s*:\s*false/.test(waitJobBody) && /job_wait_timeout/.test(waitJobBody),
  );
  check('wait_job 不使用 process::exit', !/process::exit/.test(stripComments(waitJobBody)));
}

// ---- 4. download wait：不得再用 process::exit(3) -------------

const downloadWaitBody = fnBody(source, 'run_download_wait');
check('找到 run_download_wait 实现', !!downloadWaitBody);
if (downloadWaitBody) {
  check(
    'download-wait 不再调用 std::process::exit(3)',
    !/process::exit/.test(stripComments(downloadWaitBody)),
    '仍存在 process::exit，会绕过 AGENT_BROWSER_CLI_EXIT0 兼容开关',
  );
  check(
    'download-wait 失败通过 FAILURE_EXIT_CODE 上报',
    /FAILURE_EXIT_CODE\s*\.\s*store\s*\(/.test(downloadWaitBody),
  );
}

// ---- 5. 只允许在 run()/main 里真正退出进程 ----------------------------

const exitCalls = [...source.matchAll(/std::process::exit\s*\(/g)].map((m) => m.index);
// 旧断言写的是 `<= 1`，用例名却说「有且仅有一处」——一处都没有时也能 PASS，
// 整个守卫等于空转（v6）。现在拆成两条：
//   1. 至多一处；2. 一处都没有时，main 必须用 ExitCode 当唯一出口。
// 入口改成 `fn main() -> ExitCode` 后允许 0 处，但不能再出现「退出码散落在
// 各命令实现里」的回退，所以 0 处必须能证明出口收敛在 main。
check('std::process::exit 调用不超过一处', exitCalls.length <= 1, `发现 ${exitCalls.length} 处`);
if (exitCalls.length === 0) {
  const entryReturnsExitCode = /fn\s+main\s*\(\s*\)\s*->\s*ExitCode/.test(mainSource);
  const entryDelegatesToCli = /cli::run\s*\(\s*\)/.test(mainSource);
  check(
    '无 process::exit 时，退出码收敛到 main 的 ExitCode 出口',
    entryReturnsExitCode && entryDelegatesToCli,
    'main.rs 既没有 `fn main() -> ExitCode`，也没有委托 cli::run()',
  );
}
for (const at of exitCalls) {
  const owner = enclosingFn(source, at);
  check(
    `process::exit 位于入口套壳函数内（当前：${owner}）`,
    owner === 'run' || owner === 'main',
    `出现在 fn ${owner} 中，命令实现不应直接退出进程`,
  );
}

// ---- 6. EXIT0 兼容开关必须保留 ---------------------------------------

// 命令改名为 atb 后，开关先读 ATB_EXIT0、再回退旧名 AGENT_BROWSER_CLI_EXIT0；
// 只认 1/true。2026-11-06 收拢轮把「读取 + 解析」移进 src/settings.rs
// （exit0_enabled，经 read_env 统一处理旧前缀），cli/mod.rs 只留调用点
// exit0_compat() -> settings::exit0_enabled()。这里分层锁住四件事：
// 调用点形状、委托关系、解析语义、旧名回退——少任何一层， （失败却
// exit 0）或「旧名静默失效」都会复发。
//
// 正则必须落在**同一个语义单元**里。旧写法在整个 cli 源码上分别
// grep，`code != 0 && exit0_compat()` 和 `matches!(..., "1" | "true")` 来自两个
// 不同函数也能同时命中——只要任何地方出现过一次就绿，等于没锁住「同一开关」。
const CONFIG_RS = join(HERE, '..', 'src', 'config.rs');
const SETTINGS_RS = join(SRC_DIR, 'settings.rs');
const configSource = readFileSync(CONFIG_RS, 'utf8');
const settingsSource = readFileSync(SETTINGS_RS, 'utf8');

const exitCodeBody = fnBody(source, 'exit_code');
const exit0Body = fnBody(source, 'exit0_compat');
const exit0EnabledBody = fnBody(settingsSource, 'exit0_enabled');
check('找到 exit_code 实现', !!exitCodeBody);
check('找到 exit0_compat 实现', !!exit0Body);
check('找到 settings::exit0_enabled 实现', !!exit0EnabledBody);
if (exitCodeBody && exit0Body) {
  check(
    'EXIT0 只在 code != 0 时把退出码改成 0（判断在 exit_code 内）',
    /code\s*!=\s*0\s*&&\s*exit0_compat\s*\(\s*\)/.test(exitCodeBody),
    exitCodeBody.slice(0, 200),
  );
  check(
    'exit0_compat 委托给 settings::exit0_enabled（收拢后解析只在 settings.rs）',
    /crate::settings::exit0_enabled\s*\(\s*\)/.test(exit0Body),
    exit0Body.slice(0, 200),
  );
}
if (exit0EnabledBody) {
  check(
    'EXIT0 只认 1/true（解析在 exit0_enabled 内）',
    /matches!\(\s*value\.trim\(\),\s*"1"\s*\|\s*"true"\s*\)/.test(exit0EnabledBody),
    exit0EnabledBody.slice(0, 200),
  );
  check(
    'EXIT0 读取走 read_env（旧前缀 AGENT_BROWSER_CLI_ 仍生效）',
    /read_env\("ATB_EXIT0"\)/.test(exit0EnabledBody),
    exit0EnabledBody.slice(0, 200),
  );
}
check(
  '兼容开关仍然认旧变量名 AGENT_BROWSER_CLI_',
  /AGENT_BROWSER_CLI_\{suffix\}/.test(configSource),
);

// ---- 7. 退出码常量互不冲突 -------------------------------------------

const exitFailure = numberConst(source, 'EXIT_FAILURE');
const exitTimeout = numberConst(source, 'EXIT_JOB_TIMEOUT');
check('EXIT_FAILURE 为 1', exitFailure === 1, String(exitFailure));
// 契约基线（CLI 退出码契约）：
// job wait / download-wait 的超时退出码固定为 3，且必须与普通失败 1 区分。
// 旧断言只要求「非 0 且不等于 1」，改成 2 或 99 也能绿 —— 调用方按 3 分支就失效。
check(
  'EXIT_JOB_TIMEOUT 为 3（契约基线：等待超时专用退出码）',
  exitTimeout === 3,
  String(exitTimeout),
);

// ---- 汇总 ------------------------------------------------------------

const failed = results.filter((item) => !item.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
if (failed.length) {
  console.log('failed:');
  for (const item of failed) console.log('  - ' + item.name);
  process.exit(1);
}
process.exit(0);
