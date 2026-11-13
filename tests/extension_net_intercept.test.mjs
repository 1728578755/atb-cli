// netIntercept（CDP Fetch 域）回归测试。
//
// 目标不是模拟 Chrome，而是把 background.js 原样塞进 node:vm 里跑，
// 用一个会记录所有 sendCommand 的假 chrome 观察它到底发了什么 CDP 命令。
// 这样能覆盖「patterns 是否多开了响应阶段」「header 是否整体替换丢头」
// 「UTF-8 body 是否正确 base64」这类只有真机才暴露的问题。
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { createContext, runInContext } from 'node:vm';
import { randomUUID, webcrypto } from 'node:crypto';

const HERE = dirname(fileURLToPath(import.meta.url));
const BACKGROUND = join(HERE, '..', 'assets', 'atb_bridge', 'background.js');

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(predicate, timeout = 1500) {
  const started = Date.now();
  while (Date.now() - started < timeout) {
    if (predicate()) return true;
    await wait(5);
  }
  return false;
}

function makeEvent() {
  const listeners = [];
  return {
    addListener: (fn) => { listeners.push(fn); },
    removeListener: (fn) => {
      const at = listeners.indexOf(fn);
      if (at >= 0) listeners.splice(at, 1);
    },
    hasListener: (fn) => listeners.includes(fn),
    hasListeners: () => listeners.length > 0,
    _emit: (...args) => { for (const fn of [...listeners]) fn(...args); },
    _listeners: listeners
  };
}

function buildHarness() {
  const commands = [];
  const messageListeners = [];
  const storage = Object.create(null);
  // 需要单独 switch 的返回值（比如让 continueResponse 报「方法不存在」）由这里覆盖。
  const overrides = new Map();

  const chrome = {
    runtime: {
      id: 'test-extension-id',
      lastError: null,
      getManifest: () => ({ version: '3.1', manifest_version: 3 }),
      getURL: (path) => `chrome-extension://test-extension-id/${path}`,
      reload: () => {},
      onInstalled: makeEvent(),
      onStartup: makeEvent(),
      onConnect: makeEvent(),
      onMessage: {
        addListener: (fn) => { messageListeners.push(fn); },
        removeListener: () => {},
        hasListener: () => false
      }
    },
    tabs: {
      query: async () => [{ id: 1, url: 'https://example.com/', title: 'example', active: true, windowId: 1 }],
      get: async (tabId) => ({ id: tabId, url: 'https://example.com/', title: 'example', windowId: 1 }),
      update: async (tabId, info) => Object.assign({ id: tabId, windowId: 1 }, info),
      remove: async () => {},
      sendMessage: async () => ({}),
      onUpdated: makeEvent(),
      onCreated: makeEvent(),
      onRemoved: makeEvent()
    },
    debugger: {
      attach: async () => {},
      detach: async () => {},
      getTargets: async () => [{ tabId: 1, attached: false }],
      sendCommand: async (target, method, params) => {
        commands.push({ target, method, params });
        if (overrides.has(method)) {
          const handler = overrides.get(method);
          return typeof handler === 'function' ? handler(target, params) : handler;
        }
        if (method === 'Fetch.getResponseBody') return { body: '', base64Encoded: false };
        return {};
      },
      onEvent: makeEvent(),
      onDetach: makeEvent()
    },
    scripting: {
      executeScript: async () => [{ result: 1 }],
      getRegisteredContentScripts: async () => [],
      registerContentScripts: async () => {},
      unregisterContentScripts: async () => {}
    },
    alarms: {
      create: () => {},
      clear: async () => true,
      get: async () => null,
      onAlarm: makeEvent()
    },
    webNavigation: {
      getAllFrames: async () => [],
      onCommitted: makeEvent()
    },
    storage: {
      local: {
        get: async (defaults) => {
          const out = Object.assign({}, defaults || {});
          for (const key of Object.keys(out)) {
            if (storage[key] !== undefined) out[key] = storage[key];
          }
          return out;
        },
        set: async (values) => { Object.assign(storage, values); }
      }
    },
    management: {
      getAll: async () => [],
      setEnabled: async () => {}
    },
    downloads: {
      download: async () => 1,
      search: async () => [],
      cancel: async () => {},
      erase: async () => []
    },
    declarativeNetRequest: {
      updateDynamicRules: async () => {}
    },
    contentSettings: {
      automaticDownloads: { set: async () => {} }
    },
    extension: {
      isAllowedFileSchemeAccess: async () => false
    },
    windows: {
      update: async () => ({}),
      create: async () => ({ id: 1, tabs: [] })
    }
  };

  class FakeWebSocket {
    static OPEN = 1;
    static CONNECTING = 0;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;
    }
    send() {}
    close() { this.readyState = 3; }
    addEventListener() {}
    removeEventListener() {}
  }

  const context = createContext({
    chrome,
    WebSocket: FakeWebSocket,
    // 顶层 connectWS 会立刻探测端口：让它失败，避免真的去连 18765。
    fetch: async () => { throw new Error('offline in test harness'); },
    btoa: (value) => Buffer.from(String(value), 'binary').toString('base64'),
    atob: (value) => Buffer.from(String(value), 'base64').toString('binary'),
    TextEncoder,
    TextDecoder,
    setTimeout,
    setInterval,
    clearTimeout,
    clearInterval,
    console,
    performance,
    URL,
    Date,
    Math,
    JSON,
    Promise,
    crypto: {
      randomUUID: () => randomUUID(),
      subtle: webcrypto.subtle,
      getRandomValues: (buffer) => webcrypto.getRandomValues(buffer)
    },
    AbortController,
    Uint8Array,
    ArrayBuffer
  });

  const source = readFileSync(BACKGROUND, 'utf8');
  runInContext(source, context, { filename: BACKGROUND });

  function dispatch(msg) {
    return new Promise((resolve, reject) => {
      let settled = false;
      const reply = (response) => {
        if (settled) return;
        settled = true;
        resolve(response);
      };
      for (const listener of messageListeners) {
        const ret = listener(msg, {}, reply);
        if (settled) return;
        if (ret === true) break;
      }
      wait(1500).then(() => {
        if (!settled) reject(new Error('no response for cmd ' + msg.cmd));
      });
    });
  }

  function emitEvent(method, params) {
    chrome.debugger.onEvent._emit({ tabId: 1 }, method, params);
  }

  return { chrome, commands, dispatch, emitEvent, overrides, messageListeners };
}

const decodeText = (base64) => Buffer.from(String(base64 || ''), 'base64').toString('utf8');
const encodeText = (text) => Buffer.from(String(text), 'utf8').toString('base64');
const headerValue = (entries, name) => {
  const hit = (entries || []).filter((entry) => String(entry.name).toLowerCase() === name.toLowerCase());
  return hit.length ? hit[0].value : null;
};
const headerNames = (entries) => (entries || []).map((entry) => String(entry.name).toLowerCase());

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  const mark = ok ? 'PASS' : 'FAIL';
  console.log(`${mark}  ${name}${ok || !detail ? '' : '\n        → ' + detail}`);
}

function requestPaused(params) {
  return Object.assign({
    frameId: 'F1',
    resourceType: 'XHR',
    networkId: 'N1'
  }, params);
}

async function main() {
  // harness 不能是模块级单例。每个「场景组」重建一次，组内的累积统计
  // （mock/abort/continue 各一次）是有意的，但组与组之间不继承日志、规则与统计。
  let harness = buildHarness();
  const dispatch = (message) => harness.dispatch(message);
  const emitEvent = (method, params) => harness.emitEvent(method, params);
  const commands = () => harness.commands;

  // 统计是在 CDP 命令 resolve 之后才 +1 的，轮询 status 等它落定。
  async function waitStatus(predicate, timeout = 1500) {
    const started = Date.now();
    let last = null;
    while (Date.now() - started < timeout) {
      last = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
      if (predicate(last)) return last;
      await wait(20);
    }
    return last;
  }

  // ---- 1. start 只开 Request 阶段 ----
  const mockBody = '{"msg":"中文 emoji 🚀"}';
  let before = commands().length;
  const startMock = await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/mock', method: '*', status: 201, body: mockBody }]
  });
  let sent = commands().slice(before);
  const enableMock = sent.filter((c) => c.method === 'Fetch.enable');
  const mockPatterns = enableMock.length ? enableMock[enableMock.length - 1].params.patterns : [];
  check('1. start 调用 Fetch.enable',
    startMock.ok === true && enableMock.length === 1,
    JSON.stringify(startMock));
  check('1b. start 返回结构与 patterns',
    startMock.action === 'start' && startMock.rules === 1 && startMock.fetchEnabled === true
      && Array.isArray(startMock.patterns),
    JSON.stringify(startMock));
  // patterns 现在是「按规则派生」的：每条规则只会为自己命中的 urlPattern 开阶段，
  // 所以这里断言的是「阶段正确」而不是「只有一条 * 兜底」。
  check('1c. 无 response 规则时只开 Request 阶段',
    mockPatterns.length >= 1
      && mockPatterns.every((p) => p.requestStage === 'Request')
      && mockPatterns.some((p) => String(p.urlPattern).includes('api/mock')),
    JSON.stringify(mockPatterns));

  // ---- 3 / 9. 请求阶段伪造响应 + UTF-8 ----
  // 重建 harness，再显式下发本组需要的 mock 规则：不依赖用例 1 留下的状态。
  harness = buildHarness();
  const freshStatus = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
  check(' 重建 harness 后不继承前序规则/日志/统计',
    freshStatus.ok === true && freshStatus.enabled === false && freshStatus.rules === 0 && freshStatus.logCount === 0,
    JSON.stringify(freshStatus));
  await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/mock', method: '*', status: 201, body: mockBody }]
  });
  before = commands().length;
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-mock',
    request: {
      url: 'https://example.com/api/mock',
      method: 'POST',
      headers: { 'content-type': 'application/json' }
    }
  }));
  const gotMock = await waitFor(() => commands().slice(before).some((c) => c.method === 'Fetch.fulfillRequest'));
  const mockCmd = commands().slice(before).find((c) => c.method === 'Fetch.fulfillRequest');
  check('3. mock 命中走 Fetch.fulfillRequest 且 status=201',
    gotMock && mockCmd && mockCmd.params.responseCode === 201,
    JSON.stringify(commands().slice(before)));
  check('3b. mock 未指定 content-type 时补 application/json',
    !!mockCmd && headerValue(mockCmd.params.responseHeaders, 'content-type') === 'application/json',
    JSON.stringify(mockCmd && mockCmd.params.responseHeaders));
  check('9. UTF-8 body base64 往返一致（中文 + emoji）',
    !!mockCmd && decodeText(mockCmd.params.body) === mockBody,
    mockCmd ? decodeText(mockCmd.params.body) : 'no command');
  check('9b. base64 编码后的字节长度与 UTF-8 一致',
    !!mockCmd && Buffer.from(String(mockCmd.params.body), 'base64').equals(Buffer.from(mockBody, 'utf8')),
    'base64 mismatch');

  // ---- 10. 环形日志：每条真正处理过的请求都要留痕 ----
  const findLog = (log, pred) => (Array.isArray(log) ? log.filter((item) => item && pred(item)) : []);
  const logAfterMock = await waitStatus(
    (s) => findLog(s.log, (item) => item.requestId === 'r-mock').length === 1
  );
  const mockEntry = findLog(logAfterMock.log, (item) => item.requestId === 'r-mock')[0];
  check('10b. 日志带 body 预览（截断到 160 以内）',
    !!mockEntry && typeof mockEntry.preview === 'string'
      && mockEntry.preview.includes('"msg"') && mockEntry.preview.length <= 160,
    JSON.stringify(mockEntry && mockEntry.preview));
  check('10. mock 之后 log 记一条 action=mock / stage=request / status=201',
    !!mockEntry && mockEntry.action === 'mock' && mockEntry.stage === 'request'
      && mockEntry.status === 201 && mockEntry.method === 'POST'
      && /api\/mock/.test(String(mockEntry.url)) && mockEntry.match === 'api/mock'
      && typeof mockEntry.at === 'number' && logAfterMock.logCount === 1,
    JSON.stringify({ entry: mockEntry, logCount: logAfterMock.logCount }));

  // ---- 4. abort ----
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/abort', abort: true }]
  });
  before = commands().length;
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-abort',
    request: { url: 'https://example.com/api/abort', method: 'GET', headers: {} }
  }));
  const gotAbort = await waitFor(() => commands().slice(before).some((c) => c.method === 'Fetch.failRequest'));
  const abortCmd = commands().slice(before).find((c) => c.method === 'Fetch.failRequest');
  check('4. abort 走 Fetch.failRequest，errorReason=Failed',
    gotAbort && abortCmd.params.requestId === 'r-abort' && abortCmd.params.errorReason === 'Failed',
    JSON.stringify(commands().slice(before)));
  const logAfterAbort = await waitStatus(
    (s) => findLog(s.log, (item) => item.requestId === 'r-abort').length === 1
  );
  const abortEntry = findLog(logAfterAbort.log, (item) => item.requestId === 'r-abort')[0];
  check('4b. abort 也进 log：action=abort / stage=request / 带 match',
    !!abortEntry && abortEntry.action === 'abort' && abortEntry.stage === 'request'
      && abortEntry.match === 'api/abort',
    JSON.stringify(abortEntry));

  // ---- 5. 改请求头 / 请求体 ----
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/hdr', requestHeaders: { 'x-token': 't2' }, requestBody: '{"a":2}' }]
  });
  before = commands().length;
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-hdr',
    request: {
      url: 'https://example.com/api/hdr',
      method: 'POST',
      headers: { 'x-token': 't1', accept: '*/*' }
    }
  }));
  const gotHdr = await waitFor(() => commands().slice(before).some((c) => c.method === 'Fetch.continueRequest'));
  const hdrCmd = commands().slice(before).find((c) => c.method === 'Fetch.continueRequest');
  const hdrEntries = (hdrCmd && hdrCmd.params.headers) || [];
  const tokenEntries = hdrEntries.filter((entry) => String(entry.name).toLowerCase() === 'x-token');
  check('5. 请求头基于原头合并：x-token 只剩一条且为 t2',
    gotHdr && tokenEntries.length === 1 && tokenEntries[0].value === 't2'
      && headerValue(hdrEntries, 'accept') === '*/*',
    JSON.stringify(hdrEntries));
  check('5b. postData 是 UTF-8 base64，解码后为 {"a":2}',
    !!hdrCmd && decodeText(hdrCmd.params.postData) === '{"a":2}',
    hdrCmd ? String(hdrCmd.params.postData) : 'no command');
  const logAfterHdr = await waitStatus(
    (s) => findLog(s.log, (item) => item.requestId === 'r-hdr').length === 1
  );
  const hdrEntry = findLog(logAfterHdr.log, (item) => item.requestId === 'r-hdr')[0];
  check('5d. 改请求头进 log：action=continue',
    !!hdrEntry && hdrEntry.action === 'continue' && hdrEntry.stage === 'request',
    JSON.stringify(hdrEntry));

  // 规则命中但只是改头时，stats 立刻能看出 mock/abort 的累计。
  // 统计是在 CDP 命令 resolve 之后才 +1 的，这里轮询等它落定，避免读到上一拍的快照。
  const midStatus = await waitStatus((s) => s.stats && s.stats.continued >= 1);
  check('5c. stats 记到 mocked=1 / aborted=1 / continued>=1',
    midStatus.stats.mocked === 1 && midStatus.stats.aborted === 1 && midStatus.stats.continued >= 1,
    JSON.stringify(midStatus.stats));

  // ---- 2. 规则含 response 时追加 Response 阶段 ----
  // 先建立一个只有 Request 阶段的会话，再切到 Response 阶段，才能验证 disable+enable。
  harness = buildHarness();
  await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/rewrite', status: 200 }]
  });
  before = commands().length;
  const startRewrite = await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/rewrite', response: { status: 202, headers: { 'x-mock': '1' } } }]
  });
  sent = commands().slice(before);
  const enableRewrite = sent.filter((c) => c.method === 'Fetch.enable');
  const rewritePatterns = enableRewrite.length ? enableRewrite[enableRewrite.length - 1].params.patterns : [];
  const stages = rewritePatterns.map((p) => p.requestStage);
  // 只改响应体的规则不需要在请求阶段暂停（省一次往返），所以只要求开了 Response 阶段。
  check('2. 含 response 规则时开启 Response 阶段',
    startRewrite.ok === true && stages.includes('Response'),
    JSON.stringify(rewritePatterns));
  check('2c. 同一 urlPattern 不会重复开阶段',
    new Set(rewritePatterns.map((p) => `${p.urlPattern}|${p.requestStage}`)).size === rewritePatterns.length,
    JSON.stringify(rewritePatterns));
  check('2b. 阶段变化会先 Fetch.disable 再 Fetch.enable',
    sent.some((c) => c.method === 'Fetch.disable') && enableRewrite.length === 1,
    JSON.stringify(sent.map((c) => c.method)));

  // ---- 6. 响应阶段改写（不改 body） ----
  const originalBody = '{"n":1}';
  const originalBase64 = encodeText(originalBody);
  const responseParams = requestPaused({
    requestId: 'r-rewrite',
    request: { url: 'https://example.com/api/rewrite', method: 'GET', headers: {} },
    responseStatusCode: 200,
    responseStatusText: 'OK',
    responseHeaders: [
      { name: 'content-type', value: 'application/json' },
      { name: 'content-length', value: String(Buffer.byteLength(originalBody)) },
      { name: 'content-encoding', value: 'gzip' }
    ]
  });
  // ---- 6. 响应阶段只改状态 / 头：走 continueResponse，不碰 body ----
  // 走 getResponseBody + fulfillRequest 会把压缩响应重新编码一次，
  // content-encoding 与真实字节容易对不上；只改元信息时必须原地放行。
  // 重建 harness 并重放「含 response 的 start」：6 不依赖 2 留下的会话。
  harness = buildHarness();
  await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/rewrite', response: { status: 202, headers: { 'x-mock': '1' } } }]
  });
  before = commands().length;
  emitEvent('Fetch.requestPaused', responseParams);
  const gotRewrite = await waitFor(() => commands().slice(before).some((c) => c.method === 'Fetch.continueResponse'));
  sent = commands().slice(before);
  const rewriteCmd = sent.find((c) => c.method === 'Fetch.continueResponse');
  check('6. 只改状态/头时走 continueResponse（不取 body）',
    gotRewrite && !!rewriteCmd && !sent.some((c) => c.method === 'Fetch.getResponseBody'),
    JSON.stringify(sent.map((c) => c.method)));
  check('6b. 响应 status 被改成 202',
    !!rewriteCmd && rewriteCmd.params.responseCode === 202,
    JSON.stringify(rewriteCmd && rewriteCmd.params.responseCode));
  check('6c. 响应头含 x-mock，原始 content-type 仍保留',
    !!rewriteCmd
      && headerValue(rewriteCmd.params.responseHeaders, 'x-mock') === '1'
      && headerValue(rewriteCmd.params.responseHeaders, 'content-type') === 'application/json',
    JSON.stringify(rewriteCmd && rewriteCmd.params.responseHeaders));

  // ---- 6d. 换 body：getResponseBody + fulfillRequest，并去掉长度/编码头 ----
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/rewrite', response: { status: 201, body: '{"n":2}' } }]
  });
  harness.overrides.set('Fetch.getResponseBody', () => ({ body: originalBase64, base64Encoded: true }));
  before = commands().length;
  emitEvent('Fetch.requestPaused', responseParams);
  const gotBodyRewrite = await waitFor(() => commands().slice(before).some((c) => c.method === 'Fetch.fulfillRequest'));
  sent = commands().slice(before);
  const fulfillCmd = sent.find((c) => c.method === 'Fetch.fulfillRequest');
  check('6d. 换 body 时先 getResponseBody 再 fulfillRequest',
    gotBodyRewrite && sent.some((c) => c.method === 'Fetch.getResponseBody') && !!fulfillCmd,
    JSON.stringify(sent.map((c) => c.method)));
  check('6e. fulfill 的 body 是规则值，且去掉 content-length / content-encoding',
    !!fulfillCmd
      && decodeText(fulfillCmd.params.body) === '{"n":2}'
      && fulfillCmd.params.responseCode === 201
      && !headerNames(fulfillCmd.params.responseHeaders).includes('content-length')
      && !headerNames(fulfillCmd.params.responseHeaders).includes('content-encoding'),
    JSON.stringify(fulfillCmd && fulfillCmd.params));
  const logAfterRewrite = await waitStatus(
    (s) => findLog(s.log, (item) => item.requestId === 'r-rewrite' && item.status === 201).length === 1
  );
  const rewriteEntry = findLog(logAfterRewrite.log, (item) => item.requestId === 'r-rewrite' && item.status === 201)[0];
  check('6g. 换 body 的改写带预览，只改状态的不带',
    !!rewriteEntry && rewriteEntry.preview === '{"n":2}',
    JSON.stringify(rewriteEntry && rewriteEntry.preview));
  check('6f. 响应改写进 log：action=rewrite / stage=response / status=201',
    !!rewriteEntry && rewriteEntry.action === 'rewrite' && rewriteEntry.stage === 'response',
    JSON.stringify(rewriteEntry));

  harness.overrides.delete('Fetch.getResponseBody');

  // ---- 7. 响应阶段无命中规则只放行 ----
  // 规则里没有 response 字段 → 阶段回到 Request（顺带验证 disable+enable），
  // 之后再送一次响应阶段事件，模拟「事件在路上时规则已被替换」。
  // 重建 harness：本用例只关心「放行/日志开关」，不继承前面 rewrite 的日志与统计。
  harness = buildHarness();
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/other', status: 200 }]
  });
  before = commands().length;
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-pass',
    request: { url: 'https://example.com/api/rewrite', method: 'GET', headers: {} },
    responseStatusCode: 200,
    responseStatusText: 'OK',
    responseHeaders: [{ name: 'content-type', value: 'application/json' }]
  }));
  const gotPass = await waitFor(() => commands().slice(before).length > 0);
  await wait(60); // 给可能的第二条命令留出时间，确保没有多余调用
  sent = commands().slice(before);
  const passCmds = sent.filter((c) => c.method === 'Fetch.continueResponse' || c.method === 'Fetch.continueRequest');
  check('7. 响应阶段无命中规则只发一次放行命令且不 fulfillRequest',
    gotPass && passCmds.length === 1 && !sent.some((c) => c.method === 'Fetch.fulfillRequest'),
    JSON.stringify(sent.map((c) => c.method)));
  // 放行（pass）是绝大多数请求，默认不落盘——否则 200 条环形缓冲几秒就被挤满，
  // 真正有价值的 mock/rewrite 记录会全部丢失。排查时才用 debugPass 打开。
  const logAfterPass = await waitStatus((s) => findLog(s.log, (item) => item.requestId === 'r-pass').length === 1, 600);
  check('7b. 放行默认不落盘（保住环形缓冲）',
    findLog(logAfterPass.log, (item) => item.requestId === 'r-pass').length === 0,
    JSON.stringify(findLog(logAfterPass.log, (item) => item.requestId === 'r-pass')));

  // 打开 debugPass 后，同一条放行必须能进日志，且字段与 mock/rewrite 记录同形。
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/other', status: 200 }],
    debugPass: true
  });
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-pass-debug',
    request: { url: 'https://example.com/api/rewrite', method: 'GET', headers: {} },
    responseStatusCode: 200,
    responseStatusText: 'OK',
    responseHeaders: [{ name: 'content-type', value: 'application/json' }]
  }));
  const logAfterDebugPass = await waitStatus(
    (s) => findLog(s.log, (item) => item.requestId === 'r-pass-debug').length === 1
  );
  const passEntry = findLog(logAfterDebugPass.log, (item) => item.requestId === 'r-pass-debug')[0];
  check('7c. debugPass=true 时放行进 log：action=pass / stage=response / 带原始 status',
    !!passEntry && passEntry.action === 'pass' && passEntry.stage === 'response'
      && passEntry.status === 200,
    JSON.stringify(passEntry));
  // 复位，避免影响后面的环形缓冲断言。
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/other', status: 200 }],
    debugPass: false
  });

  // ---- 11. 环形上限 ----
  // 独立重建：环形缓冲断言不能受前面 mock/rewrite 日志条数的影响。
  harness = buildHarness();
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/bulk-never-match', status: 200 }]
  });
  for (let i = 0; i < 210; i += 1) {
    emitEvent('Fetch.requestPaused', requestPaused({
      requestId: 'r-bulk-' + i,
      request: { url: 'https://example.com/bulk/' + i, method: 'GET', headers: {} }
    }));
  }
  const logAfterBulk = await waitStatus((s) => s.logCount >= 200, 1200);
  await wait(200);
  const settled = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
  check('11. 210 条放行不占用环形缓冲（pass 默认不落盘）',
    findLog(settled.log, (item) => /^r-bulk-/.test(String(item.requestId))).length === 0,
    JSON.stringify({ logCount: settled.logCount, returned: settled.log?.length }));

  // 打开 debugPass 让放行真正落盘，验证环形缓冲上限与只回最近 100 条。
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/bulk-never-match', status: 200 }],
    debugPass: true
  });
  for (let i = 0; i < 210; i += 1) {
    emitEvent('Fetch.requestPaused', requestPaused({
      requestId: 'r-ring-' + i,
      request: { url: 'https://example.com/ring/' + i, method: 'GET', headers: {} }
    }));
  }
  const ring = await waitStatus((s) => s.logCount >= 200, 4000);
  await wait(200);
  const ringSettled = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
  check('11b. 环形缓冲上限 200 条，status 只回最近 100 条',
    ringSettled.logCount <= 200 && ringSettled.logCount >= 100
      && Array.isArray(ringSettled.log) && ringSettled.log.length <= 100,
    JSON.stringify({ logCount: ringSettled.logCount, returned: ringSettled.log?.length }));
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/bulk-never-match', status: 200 }],
    debugPass: false
  });

  // ---- status / 统计 ----
  // 统计是每组独立重算的：重建后先制造「continueResponse 一次 + fulfill 一次」，
  // 否则 8c 只能靠前面用例累积，顺序一变就红。
  harness = buildHarness();
  await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/rewrite', response: { status: 202, headers: { 'x-mock': '1' } } }]
  });
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-stat-continue',
    request: { url: 'https://example.com/api/rewrite', method: 'GET', headers: {} },
    responseStatusCode: 200,
    responseStatusText: 'OK',
    responseHeaders: [{ name: 'content-type', value: 'application/json' }]
  }));
  await waitFor(() => commands().some((c) => c.method === 'Fetch.continueResponse'));
  await dispatch({
    cmd: 'netIntercept',
    action: 'rules',
    tabId: 1,
    rules: [{ match: 'api/rewrite', response: { status: 201, body: '{"n":2}' } }]
  });
  harness.overrides.set('Fetch.getResponseBody', () => ({ body: encodeText('{"n":1}'), base64Encoded: true }));
  emitEvent('Fetch.requestPaused', requestPaused({
    requestId: 'r-stat-fulfill',
    request: { url: 'https://example.com/api/rewrite', method: 'GET', headers: {} },
    responseStatusCode: 200,
    responseStatusText: 'OK',
    responseHeaders: [{ name: 'content-type', value: 'application/json' }]
  }));
  await waitFor(() => commands().some((c) => c.method === 'Fetch.fulfillRequest'));
  harness.overrides.delete('Fetch.getResponseBody');
  const status = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
  check('8b. status 返回 enabled/rules/patterns/stats',
    status.ok === true && status.enabled === true && status.rules === 1
      && Array.isArray(status.patterns) && status.stats && status.stats.mocked >= 0,
    JSON.stringify(status));
  check('8c. stats 记到了 rewritten（continueResponse 与 fulfill 各一次）',
    status.stats.rewritten >= 2 && status.stats.matched >= 2 && status.stats.failed === 0,
    JSON.stringify(status.stats));

  // ---- 8. stop ----
  // 独立会话：stop 只需要一个已 start 的拦截器，不继承统计组的规则/日志。
  harness = buildHarness();
  await dispatch({
    cmd: 'netIntercept',
    action: 'start',
    tabId: 1,
    rules: [{ match: 'api/stop-me', status: 200 }]
  });
  before = commands().length;
  const stopped = await dispatch({ cmd: 'netIntercept', action: 'stop', tabId: 1 });
  sent = commands().slice(before);
  check('8. stop 调用 Fetch.disable',
    stopped.ok === true && stopped.action === 'stop' && sent.some((c) => c.method === 'Fetch.disable'),
    JSON.stringify(sent.map((c) => c.method)));
  check('8d. stop 返回 stats',
    !!stopped.stats && typeof stopped.stats === 'object',
    JSON.stringify(stopped));

  const afterStop = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
  check('8e. stop 之后 status.enabled 为 false',
    afterStop.ok === true && afterStop.enabled === false,
    JSON.stringify(afterStop));

  // ---- 12. 空规则集：彻底关掉 Fetch，而不是留一个开着但什么都不拦的会话 ----
  // 「开着 CDP 引擎但规则被删空」（net unroute 删最后一条）是真实路径：
  // 此时必须 Fetch.disable，否则每个请求都要过一次 CDP 往返却没有规则可用。
  // 重建 harness：12a 的「重新 start」必须是真正的冷启动，而不是复用 8 的会话。
  harness = buildHarness();
  const restarted = await dispatch({
    cmd: 'netIntercept', action: 'start', tabId: 1,
    rules: [{ match: 'api/after-stop', status: 200 }]
  });
  check('12a. 重新 start 后 enabled=true 且开了 Request 阶段',
    restarted.ok === true && restarted.fetchEnabled === true
      && (restarted.patterns || []).length === 1,
    JSON.stringify(restarted));

  before = commands().length;
  const emptied = await dispatch({ cmd: 'netIntercept', action: 'rules', tabId: 1, rules: [] });
  sent = commands().slice(before);
  check('12. 规则集清空时 Fetch.disable 且 fetchEnabled=false',
    emptied.ok === true && emptied.fetchEnabled === false
      && sent.some((c) => c.method === 'Fetch.disable')
      && !sent.some((c) => c.method === 'Fetch.enable'),
    JSON.stringify({ resp: emptied, cmds: sent.map((c) => c.method) }));
  check('12b. 清空后 patterns 为空数组',
    Array.isArray(emptied.patterns) && emptied.patterns.length === 0,
    JSON.stringify(emptied.patterns));

  const afterEmpty = await dispatch({ cmd: 'netIntercept', action: 'status', tabId: 1 });
  check('12c. status.enabled 同步为 false（不留空拦截）',
    afterEmpty.ok === true && afterEmpty.enabled === false,
    JSON.stringify(afterEmpty.enabled));

  // ---- 汇总 ----
  const failed = results.filter((item) => !item.ok);
  console.log(`\n${results.length - failed.length}/${results.length} passed`);
  if (failed.length) {
    console.log('failed:');
    for (const item of failed) console.log('  - ' + item.name);
    process.exit(1);
  }
  process.exit(0);
}

main().catch((error) => {
  console.error('FATAL', error);
  process.exit(1);
});
