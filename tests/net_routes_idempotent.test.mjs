// net routes 只读性守卫。
//
// `net routes` 是查询命令，却会顺手把规则下发给标签。历史 bug：下发的第 4 个
// 实参是整份 config（含页面级 routes/matches/persist），扩展侧按 cdp 规则解析时
// 会把页面规则当成 CDP 规则，导致一次「只读命令」把拦截状态改坏。
//
// Rust 侧没有更好的可测入口（要起 daemon + 真扩展），所以在源码上做不变式断言：
// 定位 `action == "routes"` 分支 → 抓出 net_cdp_broadcast 的实参列表 → 断言第 4
// 个实参是 cdp 规则而不是整份 config。
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const SRC_DIR = join(HERE, '..', 'src');
const SERVER_RS = join(SRC_DIR, 'server.rs');

// 模块化后 `src/server.rs` 变成 `src/server/*.rs`；单文件布局继续支持。
// 按文件名排序拼接，函数不会被拆到两个文件，所以花括号切分仍然成立。
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

const source = readSourceTree(SERVER_RS, join(SRC_DIR, 'server'));

// 找到 routes 分支的第一个 '{'，再按花括号配平截出整段（跳过字符串与字符字面量）。
function sliceBlock(text, openIndex) {
  let depth = 0;
  let inString = false;
  let inChar = false;
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
    if (inChar) {
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === "'") inChar = false;
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
    if (ch === "'") {
      // 生命周期标注（'a）不算字符字面量。
      if (!/[A-Za-z_]/.test(next || '')) {
        inChar = true;
        continue;
      }
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

function blockAfter(text, anchorIndex) {
  const open = text.indexOf('{', anchorIndex);
  if (open < 0) return null;
  return sliceBlock(text, open);
}

/// 定位某个 action 的分支代码块。
///
/// 兼容三种写法：`"route" => {}`、`"route" | "mock" => {}`、`action == "route" {}`。
/// 注意 `[^;{}]*` 限定在 `=>` 之前不能跨过 `{`——否则 `matches!(action.as_str(), "route" | ...) {`
/// 这种判断语句会被误认成分支，导致对错误位置做断言（v6 的「fixture 与实现脱节」）。
function findBranchBlock(text, action) {
  const patterns = [
    new RegExp(`"${action}"\\s*\\|[^;{}]*=>`),
    new RegExp(`"${action}"\\s*=>`),
    new RegExp(`action\\s*==\\s*"${action}"`),
  ];
  for (const pattern of patterns) {
    const matched = pattern.exec(text);
    if (!matched) continue;
    const block = blockAfter(text, matched.index);
    if (block) return block;
  }
  return null;
}

// 拆出某个函数调用的实参（按顶层逗号切分）。
function callArgs(text, callStart) {
  const open = text.indexOf('(', callStart);
  if (open < 0) return null;
  let depth = 0;
  let inString = false;
  let escaped = false;
  const args = [];
  let current = '';
  for (let i = open; i < text.length; i += 1) {
    const ch = text[i];
    if (inString) {
      current += ch;
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') {
      inString = true;
      current += ch;
      continue;
    }
    if (ch === '(' || ch === '[' || ch === '{') {
      depth += 1;
      if (depth === 1 && (ch === '(')) continue; // 跳过调用自身的左括号
      current += ch;
      continue;
    }
    if (ch === ')' || ch === ']' || ch === '}') {
      depth -= 1;
      if (depth === 0) {
        if (current.trim()) args.push(current.trim());
        return args;
      }
      current += ch;
      continue;
    }
    if (ch === ',' && depth === 1) {
      args.push(current.trim());
      current = '';
      continue;
    }
    current += ch;
  }
  return null;
}

function findCalls(text, name) {
  const calls = [];
  const needle = `${name}(`;
  let at = text.indexOf(needle);
  while (at >= 0) {
    const prev = text[at - 1];
    // 排除函数定义（`fn net_cdp_broadcast(`）与限定名。
    const isDefinition = /\bfn\s+$/.test(text.slice(Math.max(0, at - 8), at));
    if (!isDefinition && (prev === undefined || /[\s&(,]/.test(prev))) {
      calls.push({ index: at, args: callArgs(text, at) });
    }
    at = text.indexOf(needle, at + needle.length);
  }
  return calls;
}

// 1) 定位 routes 分支（两种常见写法都试）。
let routesBlock = null;
for (const anchor of ['action == "routes"', '"routes" =>']) {
  const at = source.indexOf(anchor);
  if (at >= 0) {
    routesBlock = blockAfter(source, at);
    if (routesBlock) break;
  }
}

check('找到 src/server 中 net 的 routes 分支', !!routesBlock, '未匹配到 action == "routes" 或 "routes" =>');

if (routesBlock) {
  const calls = findCalls(routesBlock, 'net_cdp_broadcast');

  if (calls.length === 0) {
    // 旧写法在这里 check(..., true)：一旦实现改了、fixture 找不到调用，断言自动通过，
    // 守卫彻底失效（v6）。「找不到」必须失败，并把原因说清楚。
    check(
      'routes 分支仍能定位到 net_cdp_broadcast 调用',
      false,
      '分支存在但没有 net_cdp_broadcast：要么实现删除了下发（需人工确认只读语义），要么测试锚点与实现脱节',
    );
  } else {
    const offenders = calls
      .map((call) => call.args || [])
      .filter((args) => {
        if (args.length < 4) return true;
        const fourth = args[3];
        const looksLikeCdpRules = /cdp/i.test(fourth);
        const isWholeConfig = /^&\s*config$/.test(fourth) || /^config$/.test(fourth);
        return !looksLikeCdpRules || isWholeConfig;
      });

    check(
      'routes 分支下发的第 4 个实参是 cdp 规则而不是整份 config',
      offenders.length === 0,
      offenders.length
        ? '违规实参：' + offenders.map((args) => `[${args.join(' | ')}]`).join(' ; ')
        : undefined,
    );
  }

  // routes 是只读命令：不得写回持久化配置。
  check(
    'routes 分支不写入持久化配置（无 store_net_config）',
    !/store_net_config\s*\(/.test(routesBlock),
  );

  // 不得清空/覆盖 config 里的规则集合。
  check(
    'routes 分支不给 config 赋新值',
    !/config\s*=\s*[^=]/.test(routesBlock) && !/config\s*\[[^\]]+\]\s*=/.test(routesBlock),
  );
}

// 2) 其他 action（route/unroute）下发时必须用 rules 数组，不能整份 config。
for (const [action, label] of [
  ['route', 'route'],
  ['unroute', 'unroute'],
]) {
  const block = findBranchBlock(source, action);
  // 旧写法 `if (!block) continue;`：一旦分支改名或文件被拆走，这个 action 一条断言
  // 都不产生，脚本仍然全绿（v6）。找不到分支必须显式失败。
  if (!block) {
    check(`找到 net ${label} 分支`, false, `未匹配到 "${action}" 分支（实现可能改名/拆分，测试锚点已脱节）`);
    continue;
  }
  const calls = findCalls(block, 'net_cdp_broadcast');
  if (calls.length === 0) {
    check(
      `net ${label} 分支下发的是规则数组`,
      false,
      '分支存在但未找到 net_cdp_broadcast：fixture 与实现脱节，无法验证下发实参',
    );
    continue;
  }
  const bad = calls.filter((call) => {
    const args = call.args || [];
    if (args.length < 4) return false;
    return !/rules|cdp/i.test(args[3]) || /^&\s*config$/.test(args[3]);
  });
  check(`net ${label} 分支下发的是规则数组`, bad.length === 0, bad.map((c) => (c.args || []).join(' | ')).join(' ; '));
}

// 3) 死代码 / 旧字段不该被捡回来（「腰带」）。
// 真正的行为断言在 mock_extension 的 6c2（走真 daemon 看 change 形状）；
// 这里只做低成本的静态兜底，防止整页 HTML 对比那套实现复活。
//
// 必须先去注释再断言。解释「为什么不要 top_change」的注释、以及
// monitor.rs 里描述旧实现的注释都含这个词，不去注释会把说明文字判成回归。
const stripComments = (text) =>
  text
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:"'\\])\/\/[^\n]*/g, '$1');
const html = readFileSync(join(HERE, '..', 'src', 'html.rs'), 'utf8');
check(
  'src/html.rs 不再有 fn changed_elements',
  !/\bfn\s+changed_elements\b/.test(stripComments(html)),
  '旧的「动作前后整页 HTML 对比」实现被恢复了？',
);
check(
  'src/server 不再产出 top_change 字段',
  !/["']top_change["']\s*:/.test(stripComments(source)),
  'top_change 回传原始 HTML 片段，是回归测试点',
);

const failed = results.filter((item) => !item.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
if (failed.length) {
  console.log('failed:');
  for (const item of failed) console.log('  - ' + item.name);
  process.exit(1);
}
process.exit(0);
