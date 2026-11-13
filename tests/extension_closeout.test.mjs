// 轮 · 扩展域回归。
//
// 扩展运行时没有构建步骤、也无法在 CI 里起真实 Chrome，所以这里走两条路：
//   1) 能在 Node 里真跑的（节流排队、命令账本）就抽出函数体真跑；
//   2) 依赖 chrome.debugger / WebSocket 的路径用「锁定契约分支」的静态断言，
//      只匹配语义结构，不匹配行号与措辞。
import assert from 'node:assert/strict';
import { createHmac, webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const EXT = join(ROOT, 'assets', 'atb_bridge');
const readExt = (name) => readFileSync(join(EXT, name), 'utf8');
const BG = readExt('background.js');

/// 按花括号配平截出 `function name(...) { ... }` 的原文。
/// 骨架复制自 tests/regressions.test.mjs，额外支持反引号模板与**正则字面量**
/// （deriveUrlPattern 里 `/^[\w\-.~:/?@!$&'%*+,;=]+$/` 带单引号，会把朴素的解析器带进字符串态）。
function extractFn(source, name) {
  const match = new RegExp(`(?:async\\s+)?function\\s+${name}\\s*\\(`).exec(source);
  if (!match) return null;
  let index = source.indexOf('{', match.index);
  if (index < 0) return null;
  let depth = 0;
  let quote = null;
  let escaped = false;
  let lineComment = false;
  let blockComment = false;
  let prev = '';
  // 正则字面量的 `/` 只可能出现在「需要一个表达式」的位置。
  const regexAllowed = (ch) => ch === '' || /[=(:,!&|?{};+\-*%~^<>[\]]/.test(ch);
  for (let i = index; i < source.length; i += 1) {
    const ch = source[i];
    const next = source[i + 1];
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
    if (quote) {
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === quote) quote = null;
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
    if (ch === '/' && regexAllowed(prev)) {
      let inClass = false;
      for (i += 1; i < source.length; i += 1) {
        const c = source[i];
        if (c === '\\') {
          i += 1;
          continue;
        }
        if (c === '[') inClass = true;
        else if (c === ']') inClass = false;
        else if (c === '/' && !inClass) break;
      }
      prev = '/';
      continue;
    }
    if (ch === '"' || ch === "'" || ch === '`') {
      quote = ch;
      continue;
    }
    if (!/\s/.test(ch)) prev = ch;
    if (ch === '{') depth += 1;
    else if (ch === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(match.index, i + 1);
    }
  }
  return null;
}

/// 截出 `socket.onopen = guardSocketHandler(...)` 到 `socket.onmessage =` 之间的源码。
function sliceOnOpen(source) {
  const start = source.indexOf("socket.onopen = guardSocketHandler(");
  const end = source.indexOf("socket.onmessage = guardSocketHandler(");
  assert.ok(start > 0 && end > start, '未找到 onopen / onmessage 赋值，守卫可能已脱节');
  return source.slice(start, end);
}

// ---------------------------------------------------------------------------
// 契约 §3.1：token 模式下 onopen 不得立即发 ext_ready
// ---------------------------------------------------------------------------

test('token 模式下 onopen 只启动鉴权等待，ext_ready 统一由 sendExtReady 发出', () => {
  const onopen = sliceOnOpen(BG);
  assert.doesNotMatch(onopen, /type:\s*'ext_ready'/, 'onopen 内不允许直接发 ext_ready（必须先通过 ws_hello 校验）');
  assert.match(onopen, /if\s*\(\s*wsToken\s*\)\s*\{[\s\S]*?beginDaemonAuth\(/,
    'onopen 必须在 wsToken 非空时进入鉴权等待');
  assert.match(onopen, /await sendExtReady\(socket\)/, '无 token 时必须保持历史行为：onopen 直接发 ext_ready');

  const sendExtReady = extractFn(BG, 'sendExtReady');
  assert.ok(sendExtReady, '缺少 sendExtReady：两处入口必须共用同一份 ext_ready 实现');
  assert.match(sendExtReady, /type:\s*'ext_ready'/, 'sendExtReady 必须包含 ext_ready 帧');
  assert.match(sendExtReady, /finally\s*\{[\s\S]*?type:\s*'ext_ready'/, 'ext_ready 仍必须在 finally 里保证发出一次');
});

test('ws_hello 校验：HMAC-SHA256 + 恒定时间比较 + 5s 超时', () => {
  const hello = extractFn(BG, 'handleDaemonAuthHello');
  assert.ok(hello, '缺少 handleDaemonAuthHello');
  assert.match(hello, /hmacSha256Hex\(\s*wsToken\s*,\s*`\$\{nonce\}\|\$\{chrome\.runtime\.id\}`\s*\)/,
    '证明必须按 nonce + "|" + chrome.runtime.id 计算');
  assert.match(hello, /constantTimeEqualHex\(/, '比较必须走恒定时间实现');
  assert.match(hello, /auth_required\s*===\s*false[\s\S]*?failDaemonAuth\(/, '本地有 token 但对端声明不需要鉴权时必须按失败处理');
  assert.match(hello, /failDaemonAuth\(socket, 'daemon_auth_failed'/, '失败路径必须走统一的失败处理');
  const fail = extractFn(BG, 'failDaemonAuth');
  assert.match(fail, /recordWsConnectError\(/, '失败路径必须落 lastConnectError 供 popup 展示');
  assert.match(fail, /discardSocket\(socket, 'daemon-auth-failed'\)/, '失败后必须丢弃这条 socket');

  const hmac = extractFn(BG, 'hmacSha256Hex');
  assert.match(hmac, /subtle\.importKey[\s\S]*HMAC[\s\S]*SHA-256/, 'HMAC 必须走 WebCrypto 的 HMAC/SHA-256');
  assert.match(hmac, /subtle\.sign/, 'HMAC 必须走 WebCrypto 的 sign');

  const equal = extractFn(BG, 'constantTimeEqualHex');
  assert.doesNotMatch(equal, /return\s+false;\s*\}/, '恒定时间比较不能在逐字符阶段提前返回');
  assert.match(equal, /diff\s*\|=/, '恒定时间比较必须按位累积差异');

  assert.match(BG, /const WS_AUTH_WAIT_MS = 5000;/, '鉴权等待窗口必须是 5s');
  const begin = extractFn(BG, 'beginDaemonAuth');
  assert.match(begin, /setTimeout\([\s\S]*WS_AUTH_WAIT_MS\s*\)/, 'beginDaemonAuth 必须按 WS_AUTH_WAIT_MS 起超时定时器');
});

test('ws_hello 真跑：正确证明才发 ext_ready，错误证明/超时按失败处理', async () => {
  const TOKEN = 'shared-secret-token';
  const EXT_ID = 'abcdefghijklmnop';
  const code = [
    `var wsToken = ${JSON.stringify(TOKEN)};`,
    'var ws = { id: 1 };',
    'var lastConnectErrorKind = null;',
    'var readyCount = 0;',
    'var discardedReason = null;',
    'var fakeTimers = [];',
    'var clearTimeout = function (id) { fakeTimers[id - 1] = null; };',
    'var setTimeout = function (fn, delay) { fakeTimers.push({ fn: fn, delay: delay }); return fakeTimers.length; };',
    'async function recordWsConnectError(kind, message) { lastConnectErrorKind = kind; }',
    'function discardSocket(socket, reason) { discardedReason = reason; }',
    'function scheduleProbe() {}',
    'function scheduleFastReconnect() {}',
    'async function clearWsHeartbeatAlarm() {}',
    'async function sendExtReady(socket) { readyCount += 1; }',
    'var wsAuthPending = null;',
    'var WS_AUTH_WAIT_MS = 5000;',
    'function clearDaemonAuth() {',
    '  if (wsAuthPending && wsAuthPending.timer) { try { clearTimeout(wsAuthPending.timer); } catch (_) {} }',
    '  wsAuthPending = null;',
    '}',
    extractFn(BG, 'beginDaemonAuth'),
    extractFn(BG, 'failDaemonAuth'),
    extractFn(BG, 'hmacSha256Hex'),
    extractFn(BG, 'constantTimeEqualHex'),
    extractFn(BG, 'handleDaemonAuthHello'),
  ].join('\n');

  const makeSandbox = () => {
    const sandbox = {
    chrome: { runtime: { id: EXT_ID } },
    crypto: webcrypto,
    TextEncoder,
    console,
    Promise,
  };
    vm.runInNewContext(code, sandbox);
    return sandbox;
  };

  const proofOf = (nonce) => createHmac('sha256', TOKEN).update(`${nonce}|${EXT_ID}`).digest('hex');

  // 正确证明 → 发 ext_ready，无错误记录，定时器被清掉
  {
    const box = makeSandbox();
    const nonce = 'a'.repeat(32);
    box.beginDaemonAuth(box.ws);
    assert.equal(box.fakeTimers[0].delay, 5000, 'onopen 之后必须进入 5s 等待');
    await box.handleDaemonAuthHello(box.ws, { type: 'ws_hello', nonce, proof: proofOf(nonce), auth_required: true });
    assert.equal(box.readyCount, 1, '证明一致时必须发 ext_ready');
    assert.equal(box.lastConnectErrorKind, null, '成功路径不能写 lastConnectError');
    assert.equal(box.wsAuthPending, null, '成功后必须清掉等待状态');
  }

  // 错误证明 → 不发 ext_ready，记 daemon_auth_failed 并丢弃 socket
  {
    const box = makeSandbox();
    const nonce = 'b'.repeat(32);
    box.beginDaemonAuth(box.ws);
    await box.handleDaemonAuthHello(box.ws, { type: 'ws_hello', nonce, proof: 'f'.repeat(64), auth_required: true });
    assert.equal(box.readyCount, 0, '证明不一致时绝不能发 ext_ready');
    assert.equal(box.lastConnectErrorKind, 'daemon_auth_failed');
    assert.equal(box.discardedReason, 'daemon-auth-failed');
  }

  // 本地有 token 但对端 auth_required=false → 视为非预期 daemon
  {
    const box = makeSandbox();
    box.beginDaemonAuth(box.ws);
    await box.handleDaemonAuthHello(box.ws, { type: 'ws_hello', nonce: 'c'.repeat(32), proof: null, auth_required: false });
    assert.equal(box.readyCount, 0);
    assert.equal(box.lastConnectErrorKind, 'daemon_auth_failed');
  }

  // 5s 内没收到 ws_hello → 超时按失败处理
  {
    const box = makeSandbox();
    box.beginDaemonAuth(box.ws);
    const timer = box.fakeTimers[0];
    assert.ok(timer && typeof timer.fn === 'function', '必须挂上超时回调');
    timer.fn();
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(box.readyCount, 0, '超时不能发 ext_ready');
    assert.equal(box.lastConnectErrorKind, 'daemon_auth_timeout');
  }
});

// ---------------------------------------------------------------------------
// captureVisibleTab 节流改为 promise 链排队
// ---------------------------------------------------------------------------

test('captureVisibleTabThrottled 并发调用被串行化且相邻间隔 ≥ 550ms', async () => {
  const fnSource = extractFn(BG, 'captureVisibleTabThrottled');
  assert.ok(fnSource, '缺少 captureVisibleTabThrottled');
  assert.match(fnSource, /lastCaptureGate\s*=\s*run\.then/, '必须用 promise 链排队，而不是 check-then-set');
  assert.match(fnSource, /lastCaptureGate\.then/, '每次调用必须排在上一轮之后');

  const gapMatch = /const CAPTURE_VISIBLE_MIN_GAP_MS = (\d+);/.exec(BG);
  assert.ok(gapMatch, '缺少 CAPTURE_VISIBLE_MIN_GAP_MS');
  const MIN_GAP = Number(gapMatch[1]);

  const starts = [];
  let inFlight = 0;
  let maxInFlight = 0;
  const sandbox = {
    Date,
    Promise,
    setTimeout,
    clearTimeout,
    lastCaptureVisibleAt: 0,
    lastCaptureGate: Promise.resolve(),
    CAPTURE_VISIBLE_MIN_GAP_MS: MIN_GAP,
    chrome: {
      tabs: {
        captureVisibleTab: async () => {
          starts.push(Date.now());
          inFlight += 1;
          maxInFlight = Math.max(maxInFlight, inFlight);
          await new Promise((resolve) => setTimeout(resolve, 30));
          inFlight -= 1;
          return 'data:image/png;base64,AAAA';
        },
      },
    },
  };
  vm.runInNewContext(fnSource, sandbox);

  const results = await Promise.all([
    sandbox.captureVisibleTabThrottled(1, {}),
    sandbox.captureVisibleTabThrottled(1, {}),
    sandbox.captureVisibleTabThrottled(1, {}),
  ]);
  assert.equal(results.length, 3);
  assert.equal(maxInFlight, 1, '并发调用不能同时穿透到 chrome.tabs（Chrome 硬限流 2 次/秒）');
  assert.equal(starts.length, 3);
  // 定时器精度有限，留 20% 余量；但必须明显 > 0（旧实现在这条上是 ≈0ms）。
  const floor = Math.floor(MIN_GAP * 0.8);
  assert.ok(starts[1] - starts[0] >= floor, `第 2 次调用与第 1 次间隔过小：${starts[1] - starts[0]}ms`);
  assert.ok(starts[2] - starts[1] >= floor, `第 3 次调用与第 2 次间隔过小：${starts[2] - starts[1]}ms`);
});

// ---------------------------------------------------------------------------
// running 重复等待原 promise 后回放
// ---------------------------------------------------------------------------

function buildLedgerSandbox() {
  const consts = [...BG.matchAll(/^const (WS_COMMAND_[A-Z_]+) = .*;$/gm)].map((m) => m[0]).join('\n');
  assert.ok(consts.includes('WS_COMMAND_DUPLICATE_WAIT_MS'), '缺少 WS_COMMAND_DUPLICATE_WAIT_MS');
  const code = [
    consts,
    'var wsCommandLedger = new Map();',
    'var sent = [];',
    'function socketSend(socket, payload, kind) { sent.push({ payload: payload, kind: kind }); return true; }',
    extractFn(BG, 'wsCommandSweep'),
    extractFn(BG, 'wsCommandBegin'),
    extractFn(BG, 'wsCommandWaitForResult'),
    extractFn(BG, 'sendWsCommandResponse'),
    extractFn(BG, 'wsCommandReleaseIfRunning'),
  ].join('\n');
  const sandbox = { Date, Promise, setTimeout, clearTimeout, Map };
  vm.runInNewContext(code, sandbox);
  return sandbox;
}

test('命令账本：running 重复挂起等待并回放同一条结果', async () => {
  const box = buildLedgerSandbox();
  assert.equal(box.wsCommandBegin('cmd-1').duplicate, false);

  const again = box.wsCommandBegin('cmd-1');
  assert.equal(again.duplicate, true);
  assert.equal(again.prior.state, 'running', '第一次重复必须命中 running，而不是被当成新命令');

  const payload = { type: 'result', id: 'cmd-1', result: { ok: true } };
  let resolved = null;
  const waiting = box.wsCommandWaitForResult('cmd-1', 5000).then((value) => { resolved = value; });
  assert.equal(resolved, null, '等待者必须挂起，不能立即拿到空结果');

  setTimeout(() => box.sendWsCommandResponse({}, 'cmd-1', payload), 25);
  await waiting;
  assert.equal(resolved, payload, 'running 命令结束后必须把同一条结果回放给重复请求');
  assert.equal(box.sent.length, 1, '回放只应产生一条下行帧');
  assert.equal(box.sent[0].kind, 'result');
  assert.equal(box.wsCommandBegin('cmd-1').prior.state, 'done');
});

test('命令账本：等不到终态时超时兜底，running 条目不被 TTL 清掉', async () => {
  const box = buildLedgerSandbox();
  box.wsCommandBegin('cmd-slow');
  const value = await box.wsCommandWaitForResult('cmd-slow', 30);
  assert.equal(value, null, '超时必须返回 null，让调用方回 duplicate_inflight');

  // 时间推过 TTL 之后 running 条目仍在：否则 daemon 用同一 id 重试会重新执行副作用。
  const entry = box.wsCommandLedger.get('cmd-slow');
  entry.at = Date.now() - 10 * 60 * 1000;
  box.wsCommandSweep(Date.now());
  assert.ok(box.wsCommandLedger.has('cmd-slow'), 'running 条目不能被 TTL 扫掉');

  // 命令确实没有回包时由 release 释放等待者并允许重跑。
  let released = 'pending';
  const waiting = box.wsCommandWaitForResult('cmd-slow', 60000).then((v) => { released = v; });
  box.wsCommandReleaseIfRunning('cmd-slow');
  await waiting;
  assert.equal(released, null);
  assert.equal(box.wsCommandLedger.has('cmd-slow'), false);
});

// ---------------------------------------------------------------------------
// ：锁序与恢复记忆
// ---------------------------------------------------------------------------

test('锁序：enableFetchInterception 内不再落盘，persist 提到 tab 锁外', () => {
  const enable = extractFn(BG, 'enableFetchInterception');
  assert.ok(enable);
  assert.doesNotMatch(enable, /persistDebugInterceptState\s*\(/,
    'tab 锁内不允许调用 persistDebugInterceptState（会把 restore 的锁序交叉成死锁）');

  const handler = extractFn(BG, 'handleNetIntercept');
  assert.match(handler, /let patterns;[\s\S]*?withDebugTabLock\(tabId[\s\S]*?await persistDebugInterceptState\(\);/,
    'handleNetIntercept 必须在 withDebugTabLock 之后统一落盘');
  assert.match(handler, /catch \(e\) \{\s*lockError = e;/, '锁内错误要留到落盘之后再抛，保证失败路径也落盘');

  // 同源残留：detach 本体也不能在 tab 锁内落盘。
  const detach = extractFn(BG, 'detachDebugSessionLocked');
  assert.doesNotMatch(detach, /await persistDebugInterceptState/, 'detachDebugSessionLocked 必须在锁外落盘');
  assert.match(detach, /return true;/, 'detach 本体应返回「需要落盘」标志');

  // 两把锁的注释里必须写明锁序（防止后来者又顺手加回去）。
  assert.match(BG, /持有 tab 锁时\*\*禁止\*\*等待状态锁/, 'withDebugTabLock 的注释必须写明「不可反向等状态锁」');
  assert.match(BG, /本锁内可以获取 tab 锁/, 'withDebugStateLock 的注释必须写明「state → tab 可行」');
});

test('断线后 debugRestorePromise 作废，重连会重跑恢复', () => {
  const detachAll = extractFn(BG, 'detachAllDebugSessions');
  assert.match(detachAll, /reason === 'ws-close' \|\| reason === 'keepalive-lost'\) forgetDebugRestoreMemo\(\)/,
    'ws-close / keepalive-lost 必须清掉恢复记忆');
  const closed = extractFn(BG, 'onSocketClosed');
  assert.match(closed, /forgetDebugRestoreMemo\(\)/, 'onSocketClosed 也要清（detach 抛错时会跳过上面的分支）');
  const forget = extractFn(BG, 'forgetDebugRestoreMemo');
  assert.match(forget, /debugRestorePromise = null/, '必须置空单飞 promise');
  assert.match(forget, /restoreInFlight = null/, '同一次 onopen 的收敛变量也要清');
});

// ---------------------------------------------------------------------------
// UI 与重连回归
// ---------------------------------------------------------------------------

test('cookie 取标签口径收敛到 ui/common.js，popup 页面模式复用', () => {
  const common = readExt('ui/common.js');
  const options = readExt('options.js');
  const popup = readExt('popup.js');

  assert.match(common, /async function pickCookieTab\(\)/, 'pickCookieTab 必须在 ui/common.js 里实现');
  assert.match(common, /\/\^\(https\?\|file\):\/i\.test/, '必须按 http(s)/file 过滤');
  assert.match(common, /lastAccessed/, '必须按 lastAccessed 取最近使用的标签页');
  assert.match(common, /pickCookieTab,/, '必须导出给两个页面用');

  assert.match(options, /await ATB\.pickCookieTab\(\)/, 'options.js 必须改为调用共用实现');
  assert.doesNotMatch(options, /chrome\.tabs\.query\(\{ currentWindow: true \}\)\s*;?\s*\n\s*const usable/,
    'options.js 不能再保留一份本地实现');

  assert.match(popup, /pageMode\s*\? await ATB\.pickCookieTab\(\)/, 'popup 页面模式必须复用同一口径');
  assert.match(popup, /chrome\.tabs\.query\(\{ active: true, currentWindow: true \}\)\)\[0\]/,
    'popup 模式仍按当前 active 标签页取站点');
});

test('options 概览区「打开弹窗页面」按钮已接线', () => {
  const html = readExt('options.html');
  const options = readExt('options.js');
  assert.match(html, /id="ovOpenPopup"/, 'HTML 里的按钮 id 变了就会失配');
  assert.match(options, /\$\('ovOpenPopup'\)\?\.addEventListener\('click'/, '#ovOpenPopup 必须有 click 绑定');
  assert.match(options, /\$\('ovOpenPopup'\)\?\.addEventListener\('click'[\s\S]{0,400}?popup\.html\?page=1/,
    '必须打开 popup.html?page=1');
  assert.match(options, /\$\('ovOpenPopup'\)\?\.addEventListener\('click'[\s\S]{0,600}?ATB\.toast\(/,
    '点击后必须有可见反馈');
});

test('popup 的 toastMsg 适配层存在', () => {
  const popup = readExt('popup.js');
  const common = readExt('ui/common.js');
  assert.match(common, /setMsg,/, 'ui/common.js 仍应导出 setMsg');
  const defined = popup.match(/const toastMsg = \([^)]*\) => ATB\.setMsg\(/);
  assert.ok(defined, 'popup.js 必须定义 toastMsg → ATB.setMsg 的适配层，否则所有反馈都会 ReferenceError');
  // 反向守卫：所有 toastMsg 调用点在第一参数是字符串字面量时必须能在 HTML 里找到对应元素。
  const html = readExt('popup.html');
  for (const m of popup.matchAll(/toastMsg\('([A-Za-z0-9_-]+)'/g)) {
    assert.match(html, new RegExp(`id="${m[1]}"`), `toastMsg 指向不存在的元素：${m[1]}`);
  }
});

// ---------------------------------------------------------------------------
// known-answer：扩展的 WebCrypto HMAC 必须与 daemon（src/hmac.rs）逐字节一致
// ---------------------------------------------------------------------------

test('HMAC-SHA256 已知答案与 daemon 对拍向量一致', async () => {
  const sandbox = { crypto: webcrypto, TextEncoder, Promise, Uint8Array };
  vm.runInNewContext(`${extractFn(BG, 'hmacSha256Hex')}\nthis.hmac = hmacSha256Hex;`, sandbox);

  // 向量由 daemon 侧 src/hmac.rs 与 Node/OpenSSL createHmac 双向对拍确认。
  const nonce = '0123456789abcdef0123456789abcdef';
  const extId = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
  const expected = '74449fac6d934bb2f057e6804daa06b19a372d64d4c17ea7a85b37d05eb45958';
  const got = await sandbox.hmac('s3cret', `${nonce}|${extId}`);
  assert.equal(got, expected, '扩展侧 proof 与 daemon 不一致：真机会被判 daemon_auth_failed');
  assert.equal(got.length, 64, 'proof 必须是 64 位小写 hex');
  assert.match(got, /^[0-9a-f]{64}$/, 'proof 必须是小写 hex');
});

// ---------------------------------------------------------------------------
// tabs.onActivated 必须刷新快照，且快照带 active
// ---------------------------------------------------------------------------

test('tabs.onActivated 监听存在，tabs_update 每次都是带 active 的全量快照', () => {
  assert.match(BG, /chrome\.tabs\?\.onActivated\?\.addListener\?\.\(\(\) => scheduleTabsUpdate\(\)\)/,
    '缺 onActivated 监听：切换标签后 daemon 快照里的 active 会一直是旧值，--tab current 打错标签');

  const update = extractFn(BG, 'sendTabsUpdate');
  assert.ok(update, '缺少 sendTabsUpdate');
  assert.match(update, /active:\s*t\.active/, 'tabs_update 必须带 active 字段');
  // 必须是「重新查询后整体发送」而不是增量补丁，否则 active 翻转无法覆盖旧值。
  assert.match(update, /queryScriptableTabs\(\)/, '必须每次重新查询标签（全量快照）');
  assert.match(update, /type:\s*'tabs_update'/, '必须是 tabs_update 帧');
});

// ---------------------------------------------------------------------------
// 推导出的 urlPattern 必须能匹配原始 URL（端口不能被吞掉）
// ---------------------------------------------------------------------------

/// 按 CDP 的 urlPattern 语义（只有 * 与 ? 是通配）判断是否命中。
function cdpPatternMatches(pattern, url) {
  const escaped = pattern
    .replace(/[.+^${}()|[\]\\]/g, '\\$&')
    .replace(/\*/g, '.*')
    .replace(/\?/g, '.');
  return new RegExp(`^${escaped}$`).test(url);
}

test('deriveUrlPattern：端口段必须保留，推导结果要能匹配原始 URL', () => {
  const fnSource = extractFn(BG, 'deriveUrlPattern');
  assert.ok(fnSource, '缺少 deriveUrlPattern');
  const sandbox = {};
  vm.runInNewContext(fnSource, sandbox);
  const derive = sandbox.deriveUrlPattern;

  const cases = [
    ['localhost:8080/api', 'http://localhost:8080/api/users'],
    ['localhost:8080/api', 'https://localhost:8080/api'],
    ['api/mock', 'https://example.com/api/mock'],
    ['*/api/*', 'https://example.com/v1/api/list'],
    ['*/api/*', 'http://localhost:3000/api/list'],
    ['host:1234/path', 'http://host:1234/path?x=1'],
  ];
  for (const [match, url] of cases) {
    const { urlPattern, source } = derive(match);
    assert.ok(
      cdpPatternMatches(urlPattern, url),
      `match=${match} 推出 ${urlPattern}（${source}）却不匹配 ${url}`,
    );
  }

  // 端口未被删除：这是回归的核心断言（旧实现推出 *localhost/api*）。
  assert.doesNotMatch(derive('localhost:8080/api').urlPattern, /\*localhost\/api\*/,
    '端口被整段删掉了，规则会静默失效');

  // 无法安全推导的仍然退化成 *（策略不变）。
  for (const fallback of ['http://example.com/api', '/foo.*bar/', 'a#b']) {
    assert.equal(derive(fallback).source, 'fallback', `${fallback} 应退化为 *`);
  }
});

// ---------------------------------------------------------------------------
// 同名响应头（Set-Cookie）不能被去重丢掉
// ---------------------------------------------------------------------------

test('mergeHeaderEntries：多条 Set-Cookie 逐条保留，仅显式覆盖的头被替换', () => {
  const code = [extractFn(BG, 'headersToEntries'), extractFn(BG, 'mergeHeaderEntries')].join('\n');
  const sandbox = { Map, Set, Array };
  vm.runInNewContext(code, sandbox);
  const merge = sandbox.mergeHeaderEntries;

  const base = [
    { name: 'Set-Cookie', value: 'a=1; Path=/' },
    { name: 'Set-Cookie', value: 'b=2; Path=/' },
    { name: 'Content-Type', value: 'text/plain' },
  ];

  // 只改 status（没有任何 headers patch）→ 两条 cookie 都必须原样保留，且不能重复。
  const untouched = merge(base);
  assert.deepEqual(Array.from(untouched, (e) => e.value), ['a=1; Path=/', 'b=2; Path=/', 'text/plain']);

  // 覆盖另一个头 → Set-Cookie 仍然两条，且第一个 cookie 不重复出现。
  const withType = merge(base, { 'Content-Type': 'application/json' });
  const cookies = Array.from(withType.filter((e) => e.name.toLowerCase() === 'set-cookie'), (e) => e.value);
  assert.deepEqual(cookies, ['a=1; Path=/', 'b=2; Path=/'], `Set-Cookie 丢失或被重复：${JSON.stringify(withType)}`);
  assert.equal(withType.filter((e) => e.name.toLowerCase() === 'content-type')[0].value, 'application/json');

  // 规则显式覆盖 Set-Cookie（单值）→ 整组替换，旧值不残留。
  const overridden = merge(base, { 'Set-Cookie': 'c=3; Path=/' });
  assert.deepEqual(
    Array.from(overridden.filter((e) => e.name.toLowerCase() === 'set-cookie'), (e) => e.value),
    ['c=3; Path=/'],
  );

  // 规则自己给多条同名头 → 按顺序全部保留（不是互相覆盖）。
  const multiPatch = merge(base, [{ name: 'Set-Cookie', value: 'x=1' }, { name: 'Set-Cookie', value: 'y=2' }]);
  assert.deepEqual(
    Array.from(multiPatch.filter((e) => e.name.toLowerCase() === 'set-cookie'), (e) => e.value),
    ['x=1', 'y=2'],
  );
});

// ---------------------------------------------------------------------------
// AsyncFunction 执行期异常不得导致整段重跑
// ---------------------------------------------------------------------------

test('AsyncFunction 只有构造期失败才换写法，执行期异常原样上抛', () => {
  const builder = extractFn(BG, 'buildExecScript');
  assert.ok(builder, '缺少 buildExecScript');
  // 旧写法：`await (new AsyncFunction(...))()` 在同一个 try 里，执行期异常会命中 catch 再跑一次。
  assert.doesNotMatch(builder, /await\s*\(\s*new AsyncFunction\(/,
    'AsyncFunction 必须在独立 try 里只做构造，不能把「构造 + 调用」写在一起');
  assert.match(builder, /runner = new AsyncFunction\(primaryBody\)/, '主写法应只构造不执行');
  assert.match(builder, /runner = new AsyncFunction\(fallbackBody\)/, '兜底写法同样只构造');
  assert.match(builder, /r = await runner\(\)/, '调用必须在 try/catch 之外，执行期异常原样上抛');
  // 兜底只允许出现在构造失败的 catch 里。
  const callIndex = builder.indexOf('r = await runner();');
  const catchIndex = builder.indexOf('} catch (compileError) {');
  assert.ok(catchIndex > 0 && callIndex > catchIndex, '调用点必须在构造 catch 之后');
});

// ---------------------------------------------------------------------------
// world 大小写归一化，小写 isolated 不能落 MAIN
// ---------------------------------------------------------------------------

test('world 归一化：isolated 任意大小写都进隔离世界，非法值回落 op 决策表', () => {
  const fnSource = extractFn(BG, 'normalizeWorld');
  assert.ok(fnSource, '缺少 normalizeWorld');
  const sandbox = {};
  vm.runInNewContext(fnSource, sandbox);
  const normalize = sandbox.normalizeWorld;

  assert.equal(normalize('isolated'), 'ISOLATED');
  assert.equal(normalize('Isolated'), 'ISOLATED');
  assert.equal(normalize(' ISOLATED '), 'ISOLATED');
  assert.equal(normalize('main'), 'MAIN');
  assert.equal(normalize(''), '');
  assert.equal(normalize(undefined), '');
  assert.equal(normalize('world'), '');

  // 旧的三元写法必须全部消失（包括 exec 的 WS 路径与 handleInject）。
  assert.doesNotMatch(BG, /world\s*===\s*'ISOLATED'\s*\?\s*'ISOLATED'\s*:\s*'MAIN'/,
    '仍有旧写法：小写 isolated 会被静默落到 MAIN');
  assert.match(BG, /world:\s*normalizeWorld\(world\) \|\| 'MAIN'/, 'runUserCodeInTab 必须归一化');
  assert.match(BG, /const world = normalizeWorld\(msg\.world\) \|\| 'MAIN'/, 'handleInject 必须归一化');
  assert.match(BG, /world:\s*normalizeWorld\(data\.world\) \|\| 'MAIN'/, 'handleWsExec 必须归一化');
  assert.match(BG, /const world = normalizeWorld\(msg\.world\) \|\| \(PAGE_OP_WORLD\[op\] \|\| 'ISOLATED'\)/,
    'pageOp 必须归一化后回落到 op 决策表');
});
