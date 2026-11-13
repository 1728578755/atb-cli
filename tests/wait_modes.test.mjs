// page_wait.js 的「页面内观察器」模式回归测试。
//
// 覆盖：
//   - --dom-stable：连续静默成功 / 变更重置计时 / 超时诊断字段
//   - --network-idle：fetch/XHR hook 幂等、归零后静默成功、在途请求超时
//   - wait --js 超时：最近 5 次求值（值或错误）
//   - 模式优先级：js > selector > text > dom-stable > network-idle（mode_used）
//
// 断言口径与 page_script_closeout.test.mjs 一致：把真实 assets/page_wait.js 放进 vm
// 执行，观察可验证的行为；不读源码猜逻辑。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE = readFileSync(join(HERE, '..', 'assets', 'page_wait.js'), 'utf8');

function json(value) {
  return value === undefined ? value : JSON.parse(JSON.stringify(value));
}

function waitSandbox(extras = {}) {
  const observers = [];
  class FakeMutationObserver {
    constructor(callback) {
      this.callback = callback;
      this.disconnected = false;
      observers.push(this);
    }
    observe() {}
    disconnect() {
      this.disconnected = true;
    }
    emit(count) {
      this.callback(Array.from({ length: count }, () => ({})));
    }
  }
  const sandbox = {
    console: { log() {}, warn() {}, error() {} },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    queueMicrotask,
    performance: { now: () => Date.now() },
    document: {
      title: 'Wait Fixture',
      readyState: 'complete',
      body: null,
      documentElement: {},
      querySelectorAll: () => [],
      addEventListener() {},
      removeEventListener() {}
    },
    MutationObserver: FakeMutationObserver,
    __observers: observers,
    ...extras
  };
  sandbox.window = sandbox;
  return { sandbox, observers };
}

function loadWait(extras) {
  const { sandbox, observers } = waitSandbox(extras);
  vm.createContext(sandbox);
  vm.runInContext(SOURCE, sandbox, { filename: 'page_wait.js' });
  assert.equal(typeof sandbox.__abcWait, 'function');
  return { sandbox, observers, wait: sandbox.__abcWait };
}

// ---------------------------------------------------------------- dom-stable

test('dom-stable：无变更时在静默窗口后成功', async () => {
  const { wait } = loadWait();
  const result = json(await wait({ domStableMs: 40, timeoutMs: 1000 }));
  assert.equal(result.ok, true);
  assert.equal(result.matched, true);
  assert.equal(result.mode, 'dom-stable');
  assert.equal(result.mode_used, 'dom-stable');
  assert.equal(result.stable_ms, 40);
  assert.equal(result.mutations, 0);
  assert.ok(result.elapsed_ms >= 30, `elapsed_ms=${result.elapsed_ms}`);
});

test('dom-stable：DOM 变更会重置静默计时并累计 mutations', async () => {
  const { wait, observers } = loadWait();
  const pending = wait({ domStableMs: 80, timeoutMs: 2000 });
  // 等观察器装好后再制造一次变更（30ms 处）。
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(observers.length, 1);
  observers[0].emit(3);
  const result = json(await pending);
  assert.equal(result.ok, true);
  assert.equal(result.mutations, 3);
  // 若没有重置，40ms 左右就会成功；重置后至少要等到 ~110ms。
  assert.ok(result.elapsed_ms >= 100, `elapsed_ms=${result.elapsed_ms}`);
});

test('dom-stable：超时返回 reason/mutations/last_mutation_ms', async () => {
  const { wait, observers } = loadWait();
  const pending = wait({ domStableMs: 5000, timeoutMs: 80 });
  await new Promise((resolve) => setTimeout(resolve, 20));
  observers[0].emit(2);
  const result = json(await pending);
  assert.equal(result.ok, false);
  assert.equal(result.matched, false);
  assert.equal(result.mode, 'dom-stable');
  assert.equal(result.reason, 'timeout');
  assert.equal(result.mutations, 2);
  assert.ok(result.last_mutation_ms > 0, `last_mutation_ms=${result.last_mutation_ms}`);
});

// ---------------------------------------------------------------- network-idle

test('network-idle：无在途请求时静默窗口后成功，hook 不可枚举且幂等', async () => {
  const { sandbox, wait } = loadWait();
  const result = json(await wait({ networkIdleMs: 40, timeoutMs: 1000 }));
  assert.equal(result.ok, true);
  assert.equal(result.mode, 'network-idle');
  assert.equal(result.mode_used, 'network-idle');
  assert.equal(result.inflight, 0);
  assert.equal(result.idle_ms, 40);
  assert.equal(result.hook_installed, true);
  assert.match(result.note, /fetch\/XHR/);

  const descriptor = Object.getOwnPropertyDescriptor(sandbox, '__abcNetIdleHook');
  assert.ok(descriptor, '__abcNetIdleHook 应挂在 window 上');
  assert.equal(descriptor.enumerable, false);

  // 第二次等待复用同一个 hook，不重复包装 fetch。
  const before = sandbox.fetch;
  const second = json(await wait({ networkIdleMs: 30, timeoutMs: 1000 }));
  assert.equal(second.ok, true);
  assert.equal(sandbox.fetch, before);
  assert.equal(sandbox.__abcNetIdleHook.inflight, 0);
});

test('network-idle：在途 fetch 计入 inflight，超时如实返回', async () => {
  let release = null;
  const { sandbox, wait } = loadWait({
    fetch: function fetch() {
      return new Promise((resolve) => {
        release = resolve;
      });
    }
  });
  const pending = wait({ networkIdleMs: 50, timeoutMs: 120 });
  await new Promise((resolve) => setTimeout(resolve, 10));
  const request = sandbox.fetch('/slow');
  assert.equal(sandbox.__abcNetIdleHook.inflight, 1);
  const result = json(await pending);
  assert.equal(result.ok, false);
  assert.equal(result.reason, 'timeout');
  assert.equal(result.inflight, 1);
  // 收尾：放行请求，避免挂起的 Promise 影响后续用例。
  release('ok');
  await request;
  assert.equal(sandbox.__abcNetIdleHook.inflight, 0);
});

test('network-idle：XHR send 也会被计数并在 loadend 归零', async () => {
  class FakeXHR {
    constructor() {
      this.listeners = {};
    }
    addEventListener(name, fn) {
      this.listeners[name] = fn;
    }
  }
  FakeXHR.prototype.send = function send() {
    return undefined;
  };
  const { sandbox, wait } = loadWait({ XMLHttpRequest: FakeXHR });
  const pending = wait({ networkIdleMs: 40, timeoutMs: 200 });
  await new Promise((resolve) => setTimeout(resolve, 10));
  const xhr = new sandbox.XMLHttpRequest();
  // 直接调用被包装后的原型方法，模拟一次真实请求。
  sandbox.XMLHttpRequest.prototype.send.call(xhr, 'body');
  assert.equal(sandbox.__abcNetIdleHook.inflight, 1);
  xhr.listeners.loadend();
  assert.equal(sandbox.__abcNetIdleHook.inflight, 0);
  const result = json(await pending);
  assert.equal(result.ok, true);
});

// ---------------------------------------------------------------- wait --js

test('wait --js 超时：回传最近 5 次求值（值）', async () => {
  const { wait } = loadWait();
  const result = json(
    await wait({ js: 'return 41 - 41', timeoutMs: 90, minIntervalMs: 20 })
  );
  assert.equal(result.ok, false);
  assert.equal(result.reason, 'timeout');
  assert.equal(result.mode_used, 'js');
  assert.ok(Array.isArray(result.last_values), 'last_values 应为数组');
  assert.ok(result.last_values.length > 0);
  assert.ok(result.last_values.length <= 5);
  const last = result.last_values[result.last_values.length - 1];
  assert.equal(last.value, 0);
  assert.equal(typeof last.at_ms, 'number');
});

test('wait --js 超时：谓词抛错时记录 error', async () => {
  const { wait } = loadWait();
  const result = json(
    await wait({ js: 'throw new Error("boom")', timeoutMs: 70, minIntervalMs: 20 })
  );
  assert.equal(result.ok, false);
  assert.ok(Array.isArray(result.last_values));
  assert.ok(
    result.last_values.some((entry) => /boom/.test(String(entry.error || ''))),
    JSON.stringify(result.last_values)
  );
});

// ---------------------------------------------------------------- 优先级

test('模式优先级：js 高于 dom-stable/network-idle，并由 mode_used 说明', async () => {
  const { wait } = loadWait();
  const result = json(
    await wait({
      js: 'return false',
      domStableMs: 10,
      networkIdleMs: 10,
      timeoutMs: 70,
      minIntervalMs: 20
    })
  );
  assert.equal(result.ok, false);
  assert.equal(result.mode, 'js-observer');
  assert.equal(result.mode_used, 'js');
});

test('模式优先级：selector 高于 dom-stable，旧条件行为不变', async () => {
  const { wait } = loadWait();
  const result = json(
    await wait({
      selector: '#missing',
      domStableMs: 10,
      timeoutMs: 70
    })
  );
  assert.equal(result.ok, false);
  assert.equal(result.mode, 'observer');
  assert.equal(result.mode_used, 'selector');
});

test('page_wait.js 二次注入不报 Identifier has already been declared', async () => {
  const { sandbox } = loadWait();
  // 同一个 context 里再执行一次（等价于扩展每次调用前重新注入）。
  vm.runInContext(SOURCE, sandbox, { filename: 'page_wait.js' });
  const result = json(await sandbox.__abcWait({ domStableMs: 20, timeoutMs: 500 }));
  assert.equal(result.ok, true);
  assert.equal(result.mode_used, 'dom-stable');
});
