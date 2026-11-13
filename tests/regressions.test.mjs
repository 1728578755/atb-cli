// v10 关键回归清单。
//
// 报告只给了「应该补什么」，没有给断言。这里挑 5 条最高价值、且能在 Node 下
// 低成本守卫的路径：能在 JS 侧真跑的就真跑，Rust 侧用「锁定关键代码路径」的
// 静态断言（只匹配契约分支，不匹配行号/措辞）。
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const read = (...parts) => readFileSync(join(ROOT, ...parts), 'utf8');

const stripComments = (text) =>
  text
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/(^|[^:"'\\])\/\/[^\n]*/g, '$1');

/// 按花括号配平截出函数体（处理字符串/注释，避免正则跨函数）。
function fnBody(text, name) {
  const match = new RegExp(`\\bfn\\s+${name}\\s*[(<]`).exec(text);
  if (!match) return null;
  let index = text.indexOf('{', match.index);
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

test('wait-js 超时必须在顶层产出 ok:false + wait_js_failed', () => {
  const exec = stripComments(read('src', 'server', 'exec.rs'));
  // 内层：wait 结果 ok=false 时打标，而不是当作正常结果继续。
  assert.ok(
    /wait_js_failed"\s*\]\s*=\s*json!\(\s*true\s*\)/.test(exec),
    'exec.rs 应在 wait 失败时写入 result["wait_js_failed"]=true',
  );
  // 外层：exec 组装点必须把标记翻译成顶层 ok:false + error_code。
  assert.ok(
    /wait_js_failed/.test(exec) && /"error_code":\s*"wait_js_failed"/.test(exec),
    'exec 顶层应输出 error_code=wait_js_failed，而不是只在内层留痕',
  );
  // 反例守卫：不能出现「检查了标记却仍然 ok:true」的分支。
  assert.ok(
    !/wait_js_failed"\s*\]\s*=\s*json!\(\s*true\s*\);[\s\S]{0,200}?"ok"\s*:\s*true/.test(exec),
    'wait_js_failed 之后不能继续报 ok:true',
  );
});

test('幂等缓存不缓存失败响应', () => {
  const state = stripComments(read('src', 'server', 'state.rs'));
  const cacheable = fnBody(state, 'cacheable_response');
  assert.ok(cacheable, '应存在 cacheable_response（缓存准入）');
  assert.match(cacheable, /status\s*<\s*400/, '只缓存 2xx/3xx');
  assert.match(cacheable, /get\("ok"\)[\s\S]*?as_bool\)[\s\S]*?!=\s*Some\(false\)/, 'ok:false 必须被排除');
  const yieldSlot = fnBody(state, 'yield_slot');
  assert.ok(yieldSlot, '应存在 yield_slot');
  assert.match(yieldSlot, /cache\.remove\(cache_key\)/, '失败响应应释放占位而不是写缓存');
});

test('popup 的 cookie 值默认掩码，只有显式展开才显示明文', () => {
  const popup = stripComments(read('assets', 'atb_bridge', 'popup.js'));
  assert.match(popup, /let\s+cookieRevealed\s*=\s*false/, '默认必须是掩码状态');
  assert.match(
    popup,
    /cookieRevealed\s*\?\s*String\(c\.value[^)]*\)\s*:\s*'•••'/,
    '渲染值必须由 cookieRevealed 决定，默认显示 •••',
  );
  assert.match(popup, /cookieRevealed\s*=\s*false/, '重新打开/重置时必须回到掩码');
});

test('challenge status 载荷不可用时不得当成「没有验证码」', () => {
  const challenge = stripComments(read('src', 'server', 'challenge.rs'));
  const usable = fnBody(challenge, 'challenge_status_usable');
  assert.ok(usable, '应存在 challenge_status_usable');
  assert.match(usable, /get\("detected"\)[\s\S]*?as_bool\)[\s\S]*?is_some\(\)/, 'detected 必须是布尔字段');
  assert.match(usable, /get\("blocking"\)[\s\S]*?as_bool\)[\s\S]*?is_some\(\)/, '或 blocking 必须是布尔字段');
  // 关键调用点：status 分支与 solve 循环都必须先过滤不可用载荷。
  const callSites = (challenge.match(/challenge_status_usable\s*\(/g) || []).length;
  assert.ok(callSites >= 3, `challenge_status_usable 应在多处调用（当前 ${callSites}）`);
  assert.match(challenge, /Ok\(status\)\s+if\s+challenge_status_usable\(&status\)/, 'status 分支必须做可用性守卫');
});

test('WS 断线重连调度写在 finally 里', () => {
  const background = stripComments(read('assets', 'atb_bridge', 'background.js'));
  // 只截 onclose 赋值附近：避免在整份文件里跨函数匹配。
  const at = background.indexOf('socket.onclose = async');
  assert.ok(at > 0, '应能找到 socket.onclose = async');
  const segment = background.slice(at, at + 1800);
  assert.match(segment, /finally\s*\{/, 'onclose 必须用 finally 兜住清理异常');
  assert.match(segment, /scheduleProbe\s*\(\s*\)/, 'finally 里必须重新排 probe');
  assert.match(segment, /scheduleFastReconnect\s*\(\s*\)/, 'finally 里必须排快速重连');
  // 行为侧由 tests/ws_lifecycle.test.mjs 覆盖（让 detach 抛错仍然重连）。
});
