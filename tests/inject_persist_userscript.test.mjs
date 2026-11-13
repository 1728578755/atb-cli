// R2（2026-11-13）：inject 持久注入 TTL / 用户脚本 GM shim 的纯函数回归。
//
// 扩展运行时没有构建步骤，这里只抽取不依赖 chrome.* 的纯函数在 Node 里真跑：
//   * parseUserscriptMetadata / sanitizeGmNamespace / buildUserscriptSource
//   * persistEntryExpiresAt / isPersistEntryExpired
//   * describeValue
// 与 closeout_ext.test.mjs 相同的抽取策略：按花括号配平截函数体，跳过字符串/注释/正则。
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const BG = readFileSync(join(ROOT, 'assets', 'atb_bridge', 'background.js'), 'utf8');

/// 按花括号配平截出 `function name(...) { ... }` 的原文。
function extractFn(source, name) {
  const match = new RegExp(`(?:async\\s+)?function\\s+${name}\\s*\\(`).exec(source);
  if (!match) return null;
  const index = source.indexOf('{', match.index);
  if (index < 0) return null;
  let depth = 0;
  let quote = null;
  let escaped = false;
  let lineComment = false;
  let blockComment = false;
  let prev = '';
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
    if (ch === '{') depth += 1;
    else if (ch === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(match.index, i + 1);
    }
    prev = ch;
  }
  return null;
}

const NAMES = [
  'parseUserscriptMetadata',
  'sanitizeGmNamespace',
  'buildUserscriptSource',
  'prepareInjectCode',
  'persistEntryExpiresAt',
  'isPersistEntryExpired',
  'describeValue'
];

function loadHelpers() {
  const body = NAMES.map((name) => {
    const source = extractFn(BG, name);
    assert.ok(source, `background.js 缺少函数 ${name}`);
    return source;
  }).join('\n');
  const factory = new Function('extensionVersion', `${body}\nreturn {${NAMES.join(',')}};`);
  return factory('9.9.9-test');
}

const helpers = loadHelpers();

test('userscript 元数据解析：@name/@version/@match/@grant/@run-at', () => {
  const meta = helpers.parseUserscriptMetadata(`// ==UserScript==
// @name         Demo Script
// @namespace    demo.ns
// @version      1.2.3
// @match        https://example.com/*
// @grant        GM_getValue
// @run-at       document-start
// ==/UserScript==
console.log('hi');
`);
  assert.deepEqual(meta, {
    name: 'Demo Script',
    namespace: 'demo.ns',
    version: '1.2.3',
    matches: ['https://example.com/*'],
    grant: ['GM_getValue'],
    runAt: 'document-start'
  });
  assert.equal(helpers.parseUserscriptMetadata('console.log(1)'), null);
  assert.equal(helpers.parseUserscriptMetadata('// ==UserScript==\n// 没有可识别字段\n// ==/UserScript=='), null);
});

test('GM 命名空间清洗：只保留 [A-Za-z0-9._-]，空值回退默认', () => {
  assert.equal(helpers.sanitizeGmNamespace('demo.ns-1_ok'), 'demo.ns-1_ok');
  assert.equal(helpers.sanitizeGmNamespace('demo ns/测试'), 'demo_ns___');
  assert.equal(helpers.sanitizeGmNamespace('   '), 'atb-userscript');
  assert.equal(helpers.sanitizeGmNamespace(null), 'atb-userscript');
});

test('GM shim 可执行：存储前缀、GM_info、菜单、GM_xmlhttpRequest 明确不支持', async () => {
  const pageCode = `
    GM_setValue('k', { n: 1 });
    const value = GM_getValue('k', null);
    const missing = GM_getValue('missing', 'fallback');
    const id = GM_registerMenuCommand('do it', () => {});
    GM_log('hello');
    GM_addStyle('body { color: red; }');
    let xhrError = '';
    try { GM_xmlhttpRequest({ url: 'https://example.com' }); } catch (e) { xhrError = e.message; }
    return {
      value,
      missing,
      keys: GM_listValues(),
      name: GM_info.script.name,
      handler: GM_info.scriptHandler,
      unsafe: unsafeWindow === window,
      menuId: id,
      xhrError
    };
  `;
  const prepared = helpers.prepareInjectCode(pageCode, {
    userscript: true,
    gmNamespace: 'demo ns'
  });
  assert.ok(prepared.code.includes('__atb_gm__:demo_ns:'), 'GM 存储 key 必须带命名空间前缀');

  const store = new Map();
  const fakeLocalStorage = {
    getItem: (key) => (store.has(key) ? store.get(key) : null),
    setItem: (key, value) => store.set(key, String(value)),
    removeItem: (key) => store.delete(key),
    key: (index) => Array.from(store.keys())[index] ?? null,
    get length() { return store.size; }
  };
  const fakeDocument = {
    createElement: () => ({ textContent: '' }),
    head: { appendChild: () => {} },
    documentElement: { appendChild: () => {} }
  };
  const logs = [];
  const fakeConsole = { log: (...args) => logs.push(args.join(' ')) };
  const fakeWindow = {};
  const runner = new Function(
    'window', 'document', 'localStorage', 'console',
    `return ${prepared.code}`
  );
  const out = await runner(fakeWindow, fakeDocument, fakeLocalStorage, fakeConsole);

  assert.deepEqual(out.value, { n: 1 });
  assert.equal(out.missing, 'fallback');
  assert.deepEqual(out.keys, ['k']);
  assert.equal(out.name, null, '没有 ==UserScript== 时 GM_info.script.name 为 null');
  assert.equal(out.handler, 'atb-userscript');
  assert.equal(out.unsafe, true);
  assert.equal(out.menuId, 1);
  assert.deepEqual(fakeWindow.__atbGmMenu.map((item) => item.name), ['do it']);
  assert.deepEqual(Array.from(store.keys()), ['__atb_gm__:demo_ns:k']);
  assert.equal(out.xhrError, 'GM_xmlhttpRequest is not supported by atb userscript shim');
  assert.ok(logs.some((line) => line.includes('menu registered')), logs.join('\n'));
});

test('持久注入过期判定：缺失/显式 null 都不算过期，过期时间戳算过期', () => {
  const now = Date.now();
  assert.equal(helpers.isPersistEntryExpired({}, now), false, '旧条目（无 expiresAt）按永久处理');
  assert.equal(helpers.isPersistEntryExpired({ expiresAt: null }, now), false, '--persist-forever');
  assert.equal(helpers.isPersistEntryExpired({ expiresAt: now + 60_000 }, now), false);
  assert.equal(helpers.isPersistEntryExpired({ expiresAt: now - 1 }, now), true);
  assert.equal(helpers.persistEntryExpiresAt({ expiresAt: 'not-a-number' }), null);
});

test('describeValue：函数/错误给出 __type 摘要，普通对象/数组/原始值保持原样', () => {
  assert.deepEqual(helpers.describeValue(function named() {}), { __type: 'function', name: 'named' });
  assert.deepEqual(helpers.describeValue(new Error('boom')), { __type: 'error', name: 'Error', message: 'boom' });
  const obj = { a: 1 };
  assert.equal(helpers.describeValue(obj), obj);
  const arr = [1, 2];
  assert.equal(helpers.describeValue(arr), arr);
  assert.equal(helpers.describeValue(42), 42);
  assert.equal(helpers.describeValue(null), null);
});

test('新命令已接线且 persist_boot 过滤过期条目（静态契约）', () => {
  assert.match(BG, /msg\.cmd === 'focusTab'/);
  assert.match(BG, /msg\.cmd === 'listExtensions'/);
  assert.match(BG, /permission_required/, '未授权 management 时必须给稳定 error_code');
  assert.match(BG, /inject_script_threw/);
  assert.match(BG, /__atb_gm__:/);
  const boot = readFileSync(join(ROOT, 'assets', 'atb_bridge', 'persist_boot.js'), 'utf8');
  assert.match(boot, /expiresAt/, 'persist_boot 必须在读取时过滤过期条目');
  assert.match(boot, /persist-prune-expired/, 'persist_boot 必须通知 SW 清理过期条目');
});
