// page_net.js 几条「静默失效」的回归测试。
//
// 这些都是「跑起来不报错、但行为和文档不一致」的问题，靠读代码很难守住：
// 相对 URL 不解析 → 用户按直觉写的 match 永远不命中
// 子串被当正则 → `app.js` 会命中 `appXjs`
// 更新规则排到队尾 → 命中顺序翻转，宽规则吃掉具体规则
// net fetch 走被自己 hook 的 fetch → 自证循环的假象
// capture 阻塞返回 → SSE 页面 fetch 永不 resolve
// mock 构造抛错 → 页面 reject 且日志里查不到这次请求
// 响应改写丢 url / 非必要缓冲
//
// 这里把 page_net.js 放进 vm 跑，用一个能模拟「无内容状态码不能带 body」的
// FakeResponse 当浏览器，逐条断言可观察行为。
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const HERE = dirname(fileURLToPath(import.meta.url));
const PAGE_NET = join(HERE, '..', 'assets', 'page_net.js');
const SOURCE = readFileSync(PAGE_NET, 'utf8');

// 顶层 await 脚本必须自带硬超时，否则一次卡住的 fetch 会让 CI 永久挂起。
const TIMEOUT_MS = 5000;
function withTimeout(promise, label) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} 超过 ${TIMEOUT_MS}ms 未返回`)), TIMEOUT_MS);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : '\n        → ' + detail}`);
}

class FakeHeaders {
  constructor(input) {
    this.map = new Map();
    if (input && typeof input === 'object') {
      for (const key of Object.keys(input)) this.map.set(key.toLowerCase(), String(input[key]));
    }
  }
  get(name) {
    const key = String(name).toLowerCase();
    return this.map.has(key) ? this.map.get(key) : null;
  }
  forEach(fn) {
    for (const [key, value] of this.map) fn(value, key);
  }
}

class FakeResponse {
  constructor(body, init) {
    const options = init || {};
    const status = options.status === undefined ? 200 : options.status;
    // 复刻浏览器的硬约束：101/204/205/304 不允许带 body。
    if ([101, 204, 205, 304].includes(status) && body !== null && body !== undefined) {
      throw new TypeError('Response with null body status cannot have body: ' + status);
    }
    this.status = status;
    this.statusText = options.statusText || '';
    this.headers = new FakeHeaders(options.headers);
    this.body = body === null || body === undefined ? null : String(body);
    this.url = '';
    this.type = 'default';
    this.redirected = false;
    this.ok = status >= 200 && status < 300;
    this.textBody = this.body === null ? '' : this.body;
  }
  clone() {
    const copy = new FakeResponse(this.body, { status: this.status, statusText: this.statusText });
    copy.url = this.url;
    return copy;
  }
  async text() {
    return this.textBody;
  }
}

/// 装一个可控的页面现场：fetch 由测试自己决定返回什么。
///
/// 注意 `window` 必须**就是全局对象本身**：真实页面里 window === globalThis，
/// `install()` 靠 `window.fetch = wrapper` 替换掉全局 fetch。vm 里如果给一个
/// 独立的小对象，`state.originals.fetch` 与全局 fetch 永远是同一个函数，
/// 「net fetch 绕开 hook」这条就测不出来（假绿）。
function createContext(handler) {
  const context = vm.createContext({
    location: { href: 'https://example.com/', pathname: '/', hostname: 'example.com' },
    navigator: { userAgent: 'Mozilla/5.0 (ATB test)', language: 'en-US', platform: 'Win32' },
    document: {
      querySelector: () => null,
      addEventListener() {},
      readyState: 'complete',
      title: 'ATB test',
      documentElement: {},
    },
    console: { log() {}, warn() {}, error() {} },
    XMLHttpRequest: class {},
    Headers: FakeHeaders,
    Request: class {},
    Response: FakeResponse,
    performance: { now: () => Date.now() },
    setTimeout,
    clearTimeout,
    Promise,
    JSON,
    Math,
    Date,
    URL,
    TextDecoder: class {},
    TextEncoder: class {},
  });
  context.window = context;
  const fetchImpl = handler || (async () => {
    const response = new FakeResponse('real', { status: 200, headers: { 'content-type': 'application/json' } });
    response.url = 'https://example.com/real';
    return response;
  });
  context.fetch = fetchImpl;
  vm.runInContext(SOURCE, context, { filename: PAGE_NET });
  const net = context.window.__abcNet;
  if (typeof net !== 'function') throw new Error('page_net.js 未暴露 __abcNet');
  return { context, net };
}

function logs(net) {
  const out = net({ action: 'log', limit: 100 });
  return Array.isArray(out.items) ? out.items : [];
}

// ---- 相对 URL 也要能命中按域名写的规则 ----
{
  const { context, net } = createContext();
  net({ action: 'hook', routes: [{ match: 'example.com/api', status: 418, body: 'teapot' }] });
  const response = await withTimeout(context.window.fetch('/api/user'), 'fetch /api/user');
  check(
    'JS-09 相对 URL 能命中按绝对地址写的规则',
    response && response.status === 418,
    'status=' + (response && response.status),
  );
}

// ---- 字面量匹配不再被当正则 ----
{
  const { context, net } = createContext();
  net({ action: 'hook', routes: [{ match: 'app.js', status: 418, body: 'teapot' }] });
  const wrong = await withTimeout(context.window.fetch('https://example.com/appXjs'), 'fetch appXjs');
  const right = await withTimeout(context.window.fetch('https://example.com/app.js'), 'fetch app.js');
  check(
    'JS-16 字面量 `app.js` 不再命中 `appXjs`',
    wrong && wrong.status === 200,
    'status=' + (wrong && wrong.status),
  );
  check(' 字面量仍然命中同名文件', right && right.status === 418, 'status=' + (right && right.status));
}

// ---- b：显式 regex:true 才启用正则 ----
{
  const { context, net } = createContext();
  net({ action: 'hook', routes: [{ match: '^https://example\\.com/v\\d+/', regex: true, status: 418, body: 'x' }] });
  const hit = await withTimeout(context.window.fetch('https://example.com/v2/items'), 'fetch v2/items');
  check(' 显式 regex:true 时按正则匹配', hit && hit.status === 418, 'status=' + (hit && hit.status));
}

// ---- 更新规则是原位替换，不改变命中优先级 ----
{
  const { context, net } = createContext();
  // 具体规则在前、宽规则在后（用户配的顺序）。更新具体规则时如果被「先删再 push」
  // 挪到队尾，宽规则就会抢在前面，具体规则静默失效 —— 这正是 说的翻转。
  net({ action: 'route', rule: { match: '/api/user', status: 201, body: 'user' } });
  net({ action: 'route', rule: { match: '/api/', status: 200, body: 'wide' } });
  const replaced = net({ action: 'route', rule: { match: '/api/user', status: 202, body: 'user2' } });
  const order = replaced.all.map((rule) => rule.match).join(',');
  check(' 更新规则后条数不变', replaced.routes === 2, 'routes=' + replaced.routes);
  check(' 更新规则保持原位置（不挪到队尾）', order === '/api/user,/api/', order);
  check(' 回显 replaced=true', replaced.replaced === true, JSON.stringify(replaced.replaced));
  net({ action: 'hook' });
  const response = await withTimeout(context.window.fetch('https://example.com/api/user'), 'fetch api/user');
  check(
    'JS-17 具体规则仍然优先于宽规则',
    response && response.status === 202,
    'status=' + (response && response.status),
  );
}

// ---- net fetch 走原始 fetch，不被自己的规则骗 ----
{
  const { context, net } = createContext();
  net({ action: 'hook', routes: [{ match: 'example.com/data', status: 418, body: 'mocked' }] });
  const diagnostic = await net({ action: 'fetch', url: 'https://example.com/data' });
  check(
    'JS-21 net fetch 返回真实响应而不是自己的 mock',
    diagnostic.status === 200,
    'status=' + diagnostic.status,
  );
  check(' 结果标注 bypassed_hooks', diagnostic.bypassed_hooks === true, JSON.stringify(diagnostic.bypassed_hooks));
}

// ---- captureBodies 不能阻塞页面自己的 fetch ----
{
  const { context, net } = createContext(async () => {
    const response = new FakeResponse('stream', { status: 200, headers: { 'content-type': 'text/plain' } });
    // 模拟「body 永远读不完」：capture 一旦 await 就会卡死页面。
    response.clone = () => ({ text: () => new Promise(() => {}) });
    return response;
  });
  net({ action: 'hook', captureBodies: true });
  const winner = await Promise.race([
    withTimeout(context.window.fetch('https://example.com/slow'), 'fetch slow').then(() => 'fetch'),
    new Promise((resolve) => setTimeout(() => resolve('timeout'), 300)),
  ]);
  check(' body 读不完时 fetch 仍然立即返回', winner === 'fetch', winner);
}

// ---- b：SSE 直接跳过捕获并写明原因 ----
{
  const { context, net } = createContext(async () => {
    const response = new FakeResponse('data: 1\n\n', {
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
    });
    response.clone = () => ({ text: () => new Promise(() => {}) });
    return response;
  });
  net({ action: 'hook', captureBodies: true });
  await withTimeout(context.window.fetch('https://example.com/stream'), 'fetch stream');
  const entry = logs(net).find((item) => String(item.url).includes('/stream'));
  check(
    'JS-02 流式响应标注 body_capture_skipped',
    !!entry && /event-stream/.test(String(entry.body_capture_skipped || '')),
    JSON.stringify(entry && entry.body_capture_skipped),
  );
}

// ---- 非法 status 的 mock 不把异常抛给页面，且留下日志 ----
{
  const { context, net } = createContext();
  net({ action: 'hook', routes: [{ match: '/bad', status: 999, body: 'x' }] });
  let threw = null;
  let response = null;
  try {
    response = await withTimeout(context.window.fetch('https://example.com/bad'), 'fetch bad');
  } catch (e) {
    threw = e;
  }
  check(' 非法 status 不抛给页面', threw === null, String(threw && threw.message));
  check(' 放行真实请求', response && response.status === 200, 'status=' + (response && response.status));
  const entry = logs(net).find((item) => item.mock_invalid);
  check(' 日志里能查到 mock_invalid', !!entry, JSON.stringify(logs(net).slice(-2)));
}

// ---- b：204 + body 不再抛（无内容状态码强制清空 body） ----
{
  const { context, net } = createContext();
  net({ action: 'hook', routes: [{ match: '/no-content', status: 204, body: 'should-be-dropped' }] });
  let threw = null;
  let response = null;
  try {
    response = await withTimeout(context.window.fetch('https://example.com/no-content'), 'fetch no-content');
  } catch (e) {
    threw = e;
  }
  check(' 204 带 body 不抛 TypeError', threw === null, String(threw && threw.message));
  check(' 204 正常返回空响应', response && response.status === 204, 'status=' + (response && response.status));
}

// ---- 只改 status/headers 时透传 body 流，并保留 url ----
{
  const { context, net } = createContext(async () => {
    const response = new FakeResponse('payload', { status: 200, headers: { 'content-type': 'application/json' } });
    response.url = 'https://example.com/original';
    return response;
  });
  net({ action: 'hook', routes: [{ match: '/rewrite', response: { status: 201, headers: { 'x-rewritten': '1' } } }] });
  const response = await withTimeout(context.window.fetch('https://example.com/rewrite'), 'fetch rewrite');
  check(' 改写后 status 生效', response && response.status === 201, 'status=' + (response && response.status));
  check(
    'JS-03 改写后 url 不丢',
    response && response.url === 'https://example.com/original',
    'url=' + (response && response.url),
  );
  check(
    'JS-03 改写后 type 不丢',
    response && response.type === 'default',
    'type=' + (response && response.type),
  );
  const entry = logs(net).find((item) => item.body_stream_passthrough);
  check(' 日志标明走的是流式透传（没有整段缓冲）', !!entry, JSON.stringify(logs(net).slice(-2)));
}

// ---- 结果汇总 ----
const failed = results.filter((item) => !item.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
process.exitCode = failed.length ? 1 : 0;
if (failed.length) process.exit(1);
process.exit(0);
