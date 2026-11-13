// page_net 的「响应改写」安全性（N3-03）。
//
// 旧实现只要规则里出现 `response`，就会用 `await response.clone().text()` 取原响应体；
// 取不到时 body 保持空串，照样 `new Response('')` 返回给页面，日志还写
// `response_modified: true`。结果：页面拿到「200 + 空 body」——既不是原响应，
// 也不是改写后的响应，真实数据被静默吞掉。
//
// 正确行为：读不到原始响应体、且规则**没有显式给 body** 时，必须放行原响应，
// 并在日志里标 `response_modified: false` + `response_rewrite_skipped`。
// 这条测试锁的就是这个分叉——它是「假成功」里代价最高的一类（数据丢失）。
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const HERE = dirname(fileURLToPath(import.meta.url));
const PAGE_NET = join(HERE, '..', 'assets', 'page_net.js');

// 脚本是顶层 await，任意一次 fetch 卡住都会让 CI 永久挂起。
// 所有跨进程/页面边界都套一层硬超时，失败时给出明确断言而不是无声等待。
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

// 原响应：clone().text() 抛错，模拟「body 已被消费 / 不透明响应」。
function makeUnreadableResponse(status = 200) {
  return {
    status,
    statusText: 'OK',
    ok: true,
    headers: new Headers({ 'content-type': 'application/json' }),
    clone() {
      return {
        text: async () => {
          throw new TypeError('body stream already read');
        },
      };
    },
    text: async () => {
      throw new TypeError('body stream already read');
    },
  };
}

function loadPageNet(originalFetch, XhrClass) {
  // page_net 的 install() 会同时包装 fetch 与 XHR，必须给一个最小的 XHR 形状。
  class FakeXhr {
    open() {}
    send() {}
    setRequestHeader() {}
    abort() {}
    addEventListener() {}
    removeEventListener() {}
  }
  FakeXhr.prototype.readyState = 0;
  FakeXhr.prototype.status = 0;
  FakeXhr.prototype.responseText = '';
  const Xhr = XhrClass || FakeXhr;

  const listeners = new Map();
  const window = {
    fetch: originalFetch,
    addEventListener: (type, fn) => listeners.set(type, fn),
    XMLHttpRequest: Xhr,
    removeEventListener: () => {},
    location: { href: 'https://example.com/page' },
    navigator: { userAgent: 'Mozilla/5.0 (ATB test)', language: 'en-US' },
    performance: { now: () => Date.now() },
    setTimeout,
    clearTimeout,
  };
  window.window = window;
  const context = vm.createContext({
    window,
    self: window,
    navigator: window.navigator,
    document: { addEventListener: () => {}, readyState: 'complete', documentElement: {}, title: 'ATB test' },
    location: window.location,
    console,
    setTimeout,
    clearTimeout,
    Response,
    Headers,
    Request,
    URL,
    URLSearchParams,
    TextDecoder,
    TextEncoder,
    JSON,
    Math,
    Date,
    Object,
    Array,
    String,
    Number,
    Boolean,
    Promise,
    Error,
    TypeError,
    performance: window.performance,
    fetch: originalFetch,
    XMLHttpRequest: Xhr,
  });
  vm.runInContext(readFileSync(PAGE_NET, 'utf8'), context, { filename: PAGE_NET });
  const fn = context.__abcNet || window.__abcNet;
  if (typeof fn !== 'function') throw new Error('page_net.js 未暴露 __abcNet');
  return { net: fn, window };
}

// ---- 场景 1：只有 status 改写、原响应体读不出来 → 必须放行原响应 ----
//
// 注：状态码用 201 而不是 204。204/304 属「无内容」状态码，规则明确要求 204 时
// 正确的行为是**返回一个没有 body 的 204**（见场景 1b）；那属于主动丢弃内容，
// 与这里要防的「读不出来却假装改写成功」是两件事。
{
  const original = makeUnreadableResponse(200);
  const { net, window } = loadPageNet(async () => original);
  net({ action: 'hook', routes: [{ match: '/api/thing', response: { status: 201, headers: { 'x-rewritten': '1' } } }] });

  let returned;
  let threw = null;
  try {
    returned = await withTimeout(window.fetch('https://example.com/api/thing'), 'fetch thing');
  } catch (err) {
    threw = err;
  }

  check('未抛错（放行是正常路径，不是失败）', threw === null, threw && threw.message);
  check(
    '返回的就是原响应对象（不是新建的空 body Response）',
    returned === original,
    returned === original ? '' : `返回了不同类型的对象：status=${returned && returned.status}`,
  );

  const log = net({ action: 'log' });
  // 同一请求会先落一条「请求开始」日志，改写结果在随后那条上；
  // 这里取「带 response_modified 字段」的那条，否则会拿到还没改写的记录。
  const entries = (log.items || log.entries || []).filter((item) => String(item.url || '').includes('/api/thing'));
  const entry = entries.find((item) => 'response_modified' in item) || entries[entries.length - 1];
  check('日志里有这条请求', Boolean(entry), JSON.stringify(log).slice(0, 300));
  if (entry) {
    check('日志标记未改写（response_modified=false）', entry.response_modified === false, JSON.stringify(entry));
    check(
      '日志给出跳过原因（response_rewrite_skipped=body_unreadable）',
      entry.response_rewrite_skipped === 'body_unreadable',
      JSON.stringify(entry.response_rewrite_skipped),
    );
  }
}

// ---- 场景 1b：规则明确要求 204 → 返回真正的空响应，而不是原 200 ----
//
// `new Response(stream, {status: 204})` 在浏览器里会抛 TypeError（无内容状态码
// 不允许带 body），所以这条路径必须显式把 body 置空；否则页面 fetch 直接 reject。
{
  const original = makeUnreadableResponse(200);
  const { net, window } = loadPageNet(async () => original);
  net({ action: 'hook', routes: [{ match: '/api/no-content', response: { status: 204 } }] });

  let returned = null;
  let threw = null;
  try {
    returned = await withTimeout(window.fetch('https://example.com/api/no-content'), 'fetch no-content');
  } catch (err) {
    threw = err;
  }
  check('204 改写不抛错', threw === null, threw && threw.message);
  check('204 改写返回新响应', returned && returned !== original, '返回了原对象');
  check('204 改写状态码正确', returned && returned.status === 204, String(returned && returned.status));
  const entry = (net({ action: 'log' }).items || []).find((item) => item.body_dropped);
  check('204 改写日志标明 body 被主动丢弃', Boolean(entry), JSON.stringify(net({ action: 'log' }).items?.slice(-2)));
}

// ---- 场景 2：规则显式给了 body → 即使原响应体读不出来，也按规则改写 ----
{
  const original = makeUnreadableResponse(200);
  const { net, window } = loadPageNet(async () => original);
  net({ action: 'hook', routes: [{ match: '/api/patched', response: { status: 201, body: '{"patched":true}' } }] });

  let returned;
  let threw = null;
  try {
    returned = await withTimeout(window.fetch('https://example.com/api/patched'), 'fetch patched');
  } catch (err) {
    threw = err;
  }

  check('显式 body 时不抛错', threw === null, threw && threw.message);
  check('返回的是新响应，不是原对象', returned !== original, '返回了原对象，说明改写被跳过');
  if (returned && returned !== original) {
    const text = await withTimeout(returned.text(), '读取改写后的 body');
    check('新响应带规则里的 body', text === '{"patched":true}', text);
    check('新响应带规则里的 status', returned.status === 201, String(returned.status));
  }

  const log = net({ action: 'log' });
  const patchedEntries = (log.items || log.entries || []).filter((item) => String(item.url || '').includes('/api/patched'));
  const entry = patchedEntries.find((item) => 'response_modified' in item) || patchedEntries[patchedEntries.length - 1];
  check('显式 body 场景有日志（兜底，避免整组断言静默跳过）', Boolean(entry), JSON.stringify(log).slice(0, 300));
  if (entry) {
    check('显式 body 时日志标记已改写', entry.response_modified === true, JSON.stringify(entry));
  }
}

// ---- 场景 3：原响应体可读、规则只改 status → 正常改写（不该被上面的保护误伤） ----
{
  const original = {
    status: 200,
    statusText: 'OK',
    ok: true,
    headers: new Headers({ 'content-type': 'text/plain' }),
    clone() {
      return { text: async () => 'real-body' };
    },
  };
  const { net, window } = loadPageNet(async () => original);
  net({ action: 'hook', routes: [{ match: '/api/readable', response: { status: 202 } }] });

  let returned;
  try {
    returned = await withTimeout(window.fetch('https://example.com/api/readable'), 'fetch readable');
  } catch (err) {
    returned = null;
    check('可读 body 场景不应抛错', false, err.message);
  }
  check('可读 body 场景有返回对象（兜底，避免整组断言静默跳过）', Boolean(returned), 'fetch 没有返回响应');
  if (returned) {
    check('可读 body 时仍然改写（未被保护误伤）', returned !== original, '返回了原对象');
    check('body 保留原内容', (await withTimeout(returned.text(), '读取透传后的 body')) === 'real-body', '');
    check('status 已按规则改写', returned.status === 202, String(returned.status));
  }
}

// ---- 场景 4：XHR 的「跳过改写」分支（v4 §4.3：此前只有代码、没有断言）----
//
// XHR 不会构造新 response（改写通过 responseText getter 生效），所以这里要验证的是
// **日志是否如实**：跳过改写时必须给出 response_rewrite_skipped，排查"为什么这条
// 请求没被 mock"全靠它。
//
// 桩必须凑齐四个原生访问器：page_net 的 guardXhr() 要求
// `props.responseText && props.response` 同时存在才接管，否则整条 XHR 路径不生效
// （第一次写测试就踩了这个坑——只给 responseText，结果永远是 body_unreadable）。
{
  class XhrBase {
    constructor() {
      this.listeners = new Map();
      this._status = 200;
      this._statusText = 'OK';
      this._text = '';
      this.responseType = '';
      this.readyState = 4;
    }
    open(method, url) {
      this._method = method;
      this._url = url;
    }
    send() {}
    setRequestHeader() {}
    abort() {}
    addEventListener(type, fn) {
      if (!this.listeners.has(type)) this.listeners.set(type, []);
      this.listeners.get(type).push(fn);
    }
    removeEventListener() {}
    fire(type) {
      for (const fn of this.listeners.get(type) || []) fn.call(this, { type });
    }
  }
  for (const [name, getter] of [
    ['status', function () { return this._status; }],
    ['statusText', function () { return this._statusText; }],
    ['responseText', function () { return this._text; }],
    ['response', function () { return this._text; }],
  ]) {
    Object.defineProperty(XhrBase.prototype, name, { configurable: true, get: getter });
  }

  function xhrLog(net, needle) {
    const entries = (net({ action: 'log' }).items || []).filter((item) =>
      String(item.url || '').includes(needle),
    );
    return entries.find((item) => 'response_modified' in item) || entries[entries.length - 1];
  }

  // A) 规则只改 status、响应体读不出来（accessor 直接抛错）
  // 的新契约：只改 status/headers 时**不读 body**，status 照样改写生效，
  //    responseText / response 原样透传（这里就是继续抛错），日志标 response_modified=true
  //    且**没有** response_rewrite_skipped —— 旧实现因为读不出 body 就把整条改写
  //    （含 status）都作废，等于「只想要一个 204」也永远改不掉。
  class UnreadableXhr extends XhrBase {
    get responseText() {
      throw new Error('body unreadable');
    }
  }
  {
    const { net, window } = loadPageNet(async () => null, UnreadableXhr);
    net({ action: 'hook', routes: [{ match: '/api/xhr-unreadable', response: { status: 204 } }] });
    const xhr = new window.XMLHttpRequest();
    xhr.open('GET', 'https://example.com/api/xhr-unreadable');
    xhr.send();
    xhr.fire('loadend');

    const entry = xhrLog(net, '/api/xhr-unreadable');
    check('XHR 体读不出来时有日志', Boolean(entry), '没有该请求的日志');
    if (entry) {
      check('XHR 只改 status 时照常标记已改写', entry.response_modified === true, JSON.stringify(entry));
      check('XHR 只改 status 时改写生效（204）', entry.status === 204, String(entry.status));
      check(
        'XHR 只改 status 时不读 body（无 body_unreadable 跳过原因）',
        entry.response_rewrite_skipped === undefined,
        JSON.stringify(entry.response_rewrite_skipped),
      );
    }
    // 原样透传：不该被替换成空串。
    let passthroughThrew = false;
    try { xhr.responseText; } catch (_) { passthroughThrew = true; }
    check('XHR 只改 status 时 responseText 原样透传（不被伪造为空串）', passthroughThrew, '没有抛错，说明响应体被替换了');
  }

  // B) blob 响应 + 只改 status → 不 decode / 不 encode 二进制，只改 status。
  //    旧实现会去 TextDecoder 解码 arraybuffer、对 blob 直接放弃整条改写；
  // 要求二进制体保持原样，访问器放行原生 response。
  class BlobXhr extends XhrBase {
    constructor() {
      super();
      this.responseType = 'blob';
      this._blob = { size: 3, type: 'application/octet-stream' };
    }
    get response() { return this._blob; }
  }
  {
    const { net, window } = loadPageNet(async () => null, BlobXhr);
    net({ action: 'hook', routes: [{ match: '/api/xhr-blob', response: { status: 204 } }] });
    const xhr = new window.XMLHttpRequest();
    xhr.open('GET', 'https://example.com/api/xhr-blob');
    xhr.send();
    xhr.fire('loadend');

    const entry = xhrLog(net, '/api/xhr-blob');
    check('XHR blob 场景有日志（兜底，避免整组断言静默跳过）', Boolean(entry), '没有该请求的日志');
    if (entry) {
      check('XHR blob 只改 status 时标记已改写', entry.response_modified === true, JSON.stringify(entry));
      check(
        'XHR blob 只改 status 时不出现 blob 跳过原因（未尝试 decode）',
        entry.response_rewrite_skipped === undefined,
        JSON.stringify(entry.response_rewrite_skipped),
      );
      check('XHR blob 响应体原样透传', xhr.response === xhr._blob, 'response 被替换了');
    }
  }

  // C) 体可读 + 规则改 status → 正常改写，日志说改了
  {
    const { net, window } = loadPageNet(async () => null, XhrBase);
    net({ action: 'hook', routes: [{ match: '/api/xhr-readable', response: { status: 201 } }] });
    const xhr = new window.XMLHttpRequest();
    xhr.open('GET', 'https://example.com/api/xhr-readable');
    xhr._text = 'orig-body';
    xhr.send();
    xhr.fire('loadend');

    const entry = xhrLog(net, '/api/xhr-readable');
    check('XHR 可读体时有日志', Boolean(entry), '没有该请求的日志');
    if (entry) {
      check('XHR 可读体时标记为已改写', entry.response_modified === true, JSON.stringify(entry));
      check('XHR 改写后 status 生效', entry.status === 201, String(entry.status));
    }
  }
}

const failed = results.filter((item) => !item.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
process.exitCode = failed.length ? 1 : 0;
if (failed.length) {
  console.log('failed:');
  for (const item of failed) console.log('  - ' + item.name);
  process.exit(1);
}
process.exit(0);
