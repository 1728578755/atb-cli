// 轮 · 页面脚本域（pages）回归测试。
//
// 覆盖属于 pages 的条目：
// （v11）page_action 的 setValue 内层变量遮蔽外层结果对象 → fill 回包丢 ok
// find / snapshot / action 顶层 const → 二次注入 SyntaxError
// （v11）page_challenge 仅凭脚本标签就判 interactive → challenge wait 耗满预算
// wait --js 被 CSP 拒绝时给 wait_js_eval_blocked
// read / snapshot 的 name / placeholder 与 find 同口径掩码
// page_challenge 不再整页序列化 body.textContent
// page_net 的 XHR 三个方法在 install #2 重定基线
// simphtml 顶层函数收进 IIFE、入口不可枚举且用完即撤
//
// 注意： （端口隔离）**不在**本组用例里 —— 契约已改为「daemon 返回 warning +
// 文档说明 match pattern 不支持端口」，页面脚本不做端口跳过（多 host 规则集合用单一
// port 会误跳过其它端口的规则）。
//
// 断言口径：不读源码猜逻辑，一律把真实文件放进 vm 执行、观察可验证的行为；
// 只有「形如某个表达式」的少数一致性检查
// 用源码正则，因为那三条的产出点各自的 DOM 前置条件差得太远，硬造 stub 反而更脆。
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const HERE = dirname(fileURLToPath(import.meta.url));
const ASSETS = join(HERE, '..', 'assets');
const source = (name) => readFileSync(join(ASSETS, name), 'utf8');

/// vm 里返回的对象属于另一个 realm：`assert.deepEqual` 会因 [[Prototype]] 不同
/// 而失败，`instanceof` 也全错。统一走一次 JSON 往返，既消除 realm 差异，也顺带
/// 模拟扩展通道的序列化（String 包装对象在这一步就会塌成字符串）。
function visibleJson(value) {
  return value === undefined ? value : JSON.parse(JSON.stringify(value));
}

/// 页面脚本里出现最多的几个宿主 API。给「只做定义、不执行主体」的注入用。
function baseSandbox(extras = {}) {
  const timers = [];
  const sandbox = {
    console: { log() {}, warn() {}, error() {} },
    setTimeout: (fn) => {
      timers.push(fn);
      return timers.length;
    },
    clearTimeout() {},
    setInterval: () => 0,
    clearInterval() {},
    queueMicrotask: (fn) => fn(),
    ...extras
  };
  sandbox.window = sandbox;
  return { sandbox, timers };
}

// ----------------------------------------------------------------
/// 三个脚本在扩展通道上都是「每次调用前重新注入」，所以同一 world 里会执行多次。
///
/// 旧实现把常量写成顶层 const/let（page_find.js 的 __ABC_FIND_*、page_snapshot.js 的
/// __ABC_SNAP_*、page_action.js 的 __ABC_TEXT_INNER_LIMIT / let __abcActNodeCount）：
/// classic script 的顶层词法声明进入全局词法环境并跨脚本持久，第二次注入就是
/// 「Identifier ... has already been declared」的早期错误 —— 整个 pageOp reject，
/// 直到页面导航。这里用同一份 context 连注两次，并把入口真的调一次。
const REPEAT_CASES = [
  { file: 'page_find.js', entry: '__abcFind', call: (win) => win.__abcFind({ selector: '#nope' }) },
  { file: 'page_snapshot.js', entry: '__abcSnapshot', call: (win) => win.__abcSnapshot({ limit: 5 }) },
  { file: 'page_action.js', entry: '__abcAct', call: (win) => win.__abcAct('scroll', null, { dy: 10 }) },
];

function pageSandbox() {
  const { sandbox } = baseSandbox();
  sandbox.location = { href: 'https://example.test/page', protocol: 'https:', port: '' };
  sandbox.document = {
    nodeType: 9,
    title: 'Example',
    readyState: 'complete',
    scripts: [],
    documentElement: { lang: 'zh-CN' },
    // 三个脚本在空 DOM 上都要能跑完：返回空 NodeList 形状即可。
    body: { querySelectorAll: () => [], getElementsByTagName: () => ({ length: 0 }) },
    querySelectorAll: () => [],
    querySelector: () => null,
    getElementsByTagName: () => ({ length: 0 })
  };
  sandbox.scrollTo = () => {};
  sandbox.scrollBy = () => {};
  sandbox.scrollY = 0;
  sandbox.scrollX = 0;
  sandbox.CSS = { escape: (value) => String(value) };
  sandbox.getComputedStyle = () => ({ visibility: 'visible', display: 'block', opacity: '1' });
  return sandbox;
}

test('vm 能复现顶层 const 二次声明错误（本组用例有效性的前置守卫）', () => {
  const context = vm.createContext({});
  vm.runInContext('const PROBE = 1;', context);
  assert.throws(
    () => vm.runInContext('const PROBE = 2;', context),
    /already been declared/,
    'vm 复现不出经典脚本的顶层词法重复声明，后面的「可以重复注入」断言就没有意义'
  );
});

for (const item of REPEAT_CASES) {
  test(`${item.file} 连注两次不抛 SyntaxError，且入口不可枚举/可配置/可删除`, () => {
    const sandbox = pageSandbox();
    const context = vm.createContext(sandbox);
    const code = source(item.file);

    assert.doesNotThrow(
      () => vm.runInContext(code, context, { filename: item.file }),
      `${item.file} 首次注入就报错`
    );
    assert.doesNotThrow(
      () => vm.runInContext(code, context, { filename: item.file }),
      `${item.file} 二次注入抛错了（顶层 const/let 没进函数作用域）`
    );

    const fn = sandbox[item.entry];
    assert.equal(typeof fn, 'function', `${item.entry} 没有挂到 window 上`);

    // daemon 的 eval 通道用的是裸标识符（`return __abcFind({...})`），不是 window[x]：
    // 非枚举的全局属性同样参与标识符解析，这一条把这个前提锁住。
    assert.equal(
      vm.runInContext(`typeof ${item.entry}`, context),
      'function',
      `${item.entry} 不能按裸标识符解析，daemon 的 eval 通道会 ReferenceError`
    );

    // 站点最常见的两种枚举手法都看不到它。
    assert.equal(Object.keys(sandbox).includes(item.entry), false, 'Object.keys 泄露了入口名');
    const enumerated = [];
    for (const key in sandbox) enumerated.push(key);
    assert.equal(enumerated.includes(item.entry), false, 'for...in 泄露了入口名');

    // 但必须在 window 上（daemon 用裸标识符 / 扩展用 window[name] 解析，两条都靠它）。
    const desc = Object.getOwnPropertyDescriptor(sandbox, item.entry);
    assert.equal(desc.enumerable, false);
    assert.equal(desc.configurable, true, 'scrub 靠可配置才能删掉入口');
    assert.equal(desc.writable, true);

    // 第二次注入之后入口仍然可用 —— 这才是「find 用一次就坏」的真正验收口径。
    const value = item.call(sandbox);
    assert.equal(value.ok, true, `${item.entry} 二次注入后调用失败：${JSON.stringify(value)}`);

    // 自己打的实现标记要在（与 page_net / page_challenge 同一套）。
    assert.equal(fn.__abcOwn, true, '__abcOwn 标记丢了');

    assert.equal(delete sandbox[item.entry], true, '入口不可删 —— scrub 永远清不掉');
    assert.equal('__abcOwn' in fn, true);
    assert.equal(item.entry in sandbox, false);
  });
}

// ----------------------------------------------------------------
/// setValue 的内层变量曾经叫 `base`，与外层的结果对象 `const base = {...}` 同名。
/// 字符串把对象遮蔽之后，末尾 `Object.assign(base, state, { ok:true })` 只是往
/// String 包装对象上挂属性 —— 经 executeScript 序列化（结构克隆 / JSON 化）就只剩
/// 旧值字符串，daemon 的 ensure_page_action_ok 拿不到 `ok`，报「页面动作没有返回
/// 结论」，于是 `fill` 明明写成功了却被当成通道错误。
///
/// 断言必须走「序列化之后再检查」这一层：直接读 vm 里的返回值时，包装对象上确实
/// 有 ok/applied（这也是这个 bug 难查的原因）。JSON.parse(JSON.stringify(...))
/// 与扩展回传的语义等价。
function actionSandbox() {
  const { sandbox } = baseSandbox();

  class FakeInput {
    constructor() {
      this.nodeType = 1;
      this.tagName = 'INPUT';
      this.type = 'text';
      this.id = 'q';
      this.className = '';
      this.readOnly = false;
      this.disabled = false;
      this._value = '';
      this.dataset = {};
    }
    get selected() { return ''; }
    getAttribute() { return null; }
    hasAttribute() { return false; }
    setAttribute() {}
    getBoundingClientRect() { return { x: 0, y: 0, width: 200, height: 24 }; }
    scrollIntoView() {}
    contains() { return false; }
    dispatchEvent() { return true; }
  }
  // 页面脚本用原生 setter 写入（React 受控组件的正确写法），必须在原型上给描述符。
  Object.defineProperty(FakeInput.prototype, 'value', {
    configurable: true,
    get() { return this._value; },
    set(next) { this._value = String(next); }
  });

  const input = new FakeInput();
  input._value = 'pre';

  sandbox.HTMLInputElement = FakeInput;
  sandbox.HTMLTextAreaElement = class HTMLTextAreaElement {};
  sandbox.HTMLSelectElement = class HTMLSelectElement {};
  sandbox.Event = class Event {
    constructor(type, init) {
      this.type = type;
      Object.assign(this, init || {});
    }
  };
  sandbox.PointerEvent = undefined;
  sandbox.MouseEvent = undefined;
  sandbox.getComputedStyle = () => ({ visibility: 'visible', display: 'block', opacity: '1' });
  sandbox.location = { href: 'https://example.test/form' };
  sandbox.document = {
    nodeType: 9,
    title: 'Form',
    readyState: 'complete',
    scripts: [],
    documentElement: {},
    body: { querySelectorAll: () => [input] },
    querySelectorAll: (selector) => (selector === 'input#q' ? [input] : []),
    querySelector: () => null,
    elementFromPoint: () => input,
    activeElement: null
  };
  return { sandbox, input };
}

test(' setValue 返回的结果必须是可序列化的对象（ok/applied 不能丢）', () => {
  const { sandbox, input } = actionSandbox();
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_action.js'), context, { filename: 'page_action.js' });

  const raw = sandbox.__abcAct('setValue', 'input#q', { value: 'hello', append: false });
  assert.equal(typeof raw, 'object', 'setValue 返回的不是对象（内层 base 又遮蔽了外层结果对象）');
  assert.equal(typeof raw.ok, 'boolean');
  assert.equal(raw.ok, true);
  assert.equal(raw.applied, true);
  assert.equal(input._value, 'hello');

  // 扩展通道的真实语义：回包会被序列化，String 包装对象会在这一步塌成字符串。
  const wire = JSON.parse(JSON.stringify(raw));
  assert.equal(typeof wire, 'object', `序列化后塌成 ${typeof wire}：${JSON.stringify(wire)}`);
  assert.equal(wire.ok, true, '序列化后丢了 ok —— daemon 会报「页面动作没有返回结论」');
  assert.equal(wire.applied, true, '序列化后丢了 applied');
  assert.equal(wire.tag, 'input');
});

test(' setValue append 仍以写入前的值为基准', () => {
  const { sandbox, input } = actionSandbox();
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_action.js'), context, { filename: 'page_action.js' });

  const raw = sandbox.__abcAct('setValue', 'input#q', { value: ' world', append: true });
  assert.equal(raw.ok, true);
  assert.equal(input._value, 'pre world');
  const wire = JSON.parse(JSON.stringify(raw));
  assert.equal(wire.ok, true);
  assert.equal(wire.applied, true);
});

// ----------------------------------------------------------------
/// page_wait.js 用 `new AsyncFunction(...)` 在页面里编译字符串谓词，等于 eval：
/// 页面 CSP 禁 unsafe-eval 时必然失败。契约 §3.2 要求这种失败给
/// error_code=wait_js_eval_blocked（daemon 据此改走 CDP Runtime.evaluate），
/// 而「谓词本身语法错」必须继续报 js_predicate_compile_failed —— 换通道救不回来。
///
/// 模拟方式：页面脚本取编译器的路径是 `Object.getPrototypeOf(async function(){}).constructor`，
/// 所以在 sandbox 里换掉 Object.getPrototypeOf，让它返回一个「一 new 就抛」的构造器。
function waitSandbox(thrown) {
  const { sandbox } = baseSandbox();
  sandbox.document = {
    title: 'Example',
    readyState: 'complete',
    body: null,
    documentElement: {},
    querySelectorAll: () => [],
    addEventListener() {},
    removeEventListener() {}
  };
  sandbox.location = { href: 'https://example.test/page' };
  function RejectingAsyncFunction() {
    throw thrown();
  }
  sandbox.Object = { getPrototypeOf: () => ({ constructor: RejectingAsyncFunction }), assign: Object.assign };
  return sandbox;
}

function cspError() {
  const error = new Error(
    "Refused to evaluate a string as JavaScript because 'unsafe-eval' is not an allowed source of script in the following Content Security Policy directive: \"script-src 'self'\"."
  );
  error.name = 'EvalError';
  return error;
}

test('wait --js 被 CSP 拒绝：返回 wait_js_eval_blocked，不再谎称页面内回退', () => {
  const sandbox = waitSandbox(cspError);
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_wait.js'), context, { filename: 'page_wait.js' });

  const wait = sandbox.__abcWait;
  assert.equal(typeof wait, 'function');

  const result = wait({ js: 'document.title === "Example"', timeoutMs: 1000 });
  assert.equal(result.ok, false);
  assert.equal(result.matched, false);
  assert.equal(result.error_code, 'wait_js_eval_blocked');
  assert.match(result.error, /wait_js_eval_blocked/);
  assert.equal(result.polls, 0);
  assert.equal(result.checks, 0);
  // 页面脚本没有 CDP 通道，任何「已回退 / 本通道自动转 CDP」的措辞都是假话。
  assert.doesNotMatch(result.hint, /已回退/);
  assert.doesNotMatch(result.hint, /本通道.{0,4}(自动)?回退/);
});

test('wait --js 语法错误：仍然是 js_predicate_compile_failed（不能一律甩给 CSP）', () => {
  const sandbox = waitSandbox(() => new SyntaxError('Unexpected token }'));
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_wait.js'), context, { filename: 'page_wait.js' });

  const result = sandbox.__abcWait({ js: 'foo(}', timeoutMs: 1000 });
  assert.equal(result.ok, false);
  assert.equal(result.error_code, 'js_predicate_compile_failed');
  assert.match(result.error, /Unexpected token/);
});

// ----------------------------------------------------------------
function netSandbox() {
  const { sandbox } = baseSandbox();
  const nativeFetch = function nativeFetch() {
    return Promise.resolve({ ok: true, status: 200, headers: { forEach() {} }, clone: () => ({ text: async () => '' }) });
  };
  class FakeXhr {
    open() {}
    send() {}
    setRequestHeader() {}
    addEventListener() {}
    removeEventListener() {}
  }
  sandbox.fetch = nativeFetch;
  sandbox.XMLHttpRequest = FakeXhr;
  sandbox.Headers = class Headers {};
  sandbox.Response = class Response {};
  sandbox.location = { href: 'https://example.test/api', protocol: 'https:', port: '' };
  sandbox.document = { querySelector: () => null, addEventListener() {} };
  sandbox.performance = { now: () => 0 };
  sandbox.URL = URL;
  return { sandbox, nativeFetch, FakeXhr };
}

test('install #2 重定 XHR 基线：页面后装的补丁留在调用链上且不成环', () => {
  const { sandbox, FakeXhr } = netSandbox();
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_net.js'), context, { filename: 'page_net.js' });
  const net = sandbox.__abcNet;

  const first = net({ action: 'hook', routes: [] });
  assert.equal(first.installed, true);

  // 页面监控 SDK 在我们之后替换 XHR.open（典型：Sentry 包一层做埋点）。
  let pageOpenCalls = 0;
  const wrappedOpen = FakeXhr.prototype.open;
  FakeXhr.prototype.open = function patchedOpen() {
    pageOpenCalls += 1;
    return wrappedOpen.apply(this, arguments);
  };
  // 同时把 fetch 换掉 —— 这是走到 install() 分支③（重装 wrapper）的触发条件。
  sandbox.fetch = function pageFetch() { return Promise.resolve({}); };

  const second = net({ action: 'hook', routes: [] });
  assert.equal(second.installed, true);
  assert.equal(second.xhr_replaced_by_page, true, '页面替换过 XHR 方法却没回报');

  const xhr = new FakeXhr();
  xhr.open('GET', '/api/x');
  // 1 = 我们的新 wrapper → 页面的补丁 → 我们的老 wrapper → 原生。
  // 旧实现会跳过页面的补丁（pageOpenCalls 恒 0）；把基线就地改写则会成环（RangeError）。
  assert.equal(pageOpenCalls, 1, '页面自己的 XHR.open 补丁被剪掉了');
});

// ---------------------------------------------------------------- /

test('规则条数上限：hook 重放最多收 64 条，与 daemon 的 ATB_NET_RULE_MAX_RULES 同口径', () => {
  // 历史实现这里截 50、daemon 管 64：51~64 条规则刷新后被页面静默裁掉，
  // `net routes` 显示全量、浏览器实际只生效前 50 条。
  const { sandbox } = netSandbox();
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_net.js'), context, { filename: 'page_net.js' });

  const routes = Array.from({ length: 70 }, (_, i) => ({ match: '/api/' + i, status: 200 }));
  const out = visibleJson(sandbox.__abcNet({ action: 'hook', routes }));

  assert.equal(out.installed, true);
  assert.equal(out.routes_list.length, 64, '重放上限必须是 64');
  assert.equal(out.routes, 64, 'routes 计数与实际条数要一致');
  assert.equal(out.routes_dropped, 6, '被裁掉的条数必须回传，不能静默丢');
  // daemon 用这个字段把告警文案说准（缺字段时它回落 64）。值必须来自同一个常量，
  // 否则又是「页面改了、daemon 文案没改」的漂移。
  assert.equal(out.routes_cap, 64, 'routes_cap 必须等于实际裁剪上限');
  // 顺序即 first-match 优先级：裁掉的必须是尾部，不能把前面的具体规则挤掉。
  assert.equal(out.routes_list[0].match, '/api/0');
  assert.equal(out.routes_list[63].match, '/api/63');
});

test('未超限时不产生 routes_dropped，且规则原样保留', () => {
  const { sandbox } = netSandbox();
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_net.js'), context, { filename: 'page_net.js' });

  const routes = Array.from({ length: 64 }, (_, i) => ({ match: '/api/' + i }));
  const out = visibleJson(sandbox.__abcNet({ action: 'hook', routes }));
  assert.equal(out.routes_list.length, 64);
  assert.equal(out.routes_dropped, undefined, '刚好 64 条不该报丢弃');
  // 没有丢弃时也要给上限：daemon 的告警文案可能由别的分支触发（例如 env 放大到 128）。
  assert.equal(out.routes_cap, 64);
});

// ----------------------------------------------------------------
test('find / read / snapshot 的派生文本掩码口径一致', () => {
  // 三份文件各留一份实现、各叫各的名字（不能共享全局，见 page_action.js 头部约定），
  // 所以这里锁的是**口径**：空值 → null，secret 字段非空 → '•••'。
  for (const file of ['page_find.js', 'page_read.js', 'page_snapshot.js']) {
    const code = source(file);
    assert.match(code, /if \(!\w*[sS]ecret\w*\(el\)\) return raw \|\| null;/, `${file} 的掩码空值口径漂了`);
    assert.match(code, /return raw \? '•••' : null;/, `${file} 的掩码 secret 口径漂了`);
  }
});

test('snapshot：口令字段的 name / placeholder 打码并置 redacted，非敏感结构字段保留', () => {
  const { sandbox } = baseSandbox();
  sandbox.CSS = { escape: (value) => String(value) };
  sandbox.getComputedStyle = () => ({ visibility: 'visible', display: 'block', opacity: '1' });
  sandbox.location = { href: 'https://example.test/login' };

  function makeInput(attrs) {
    return {
      nodeType: 1,
      tagName: 'INPUT',
      children: [],
      className: '',
      id: attrs.id || '',
      type: attrs.type || 'text',
      value: attrs.value || '',
      disabled: false,
      getAttribute: (name) => (Object.prototype.hasOwnProperty.call(attrs, name) ? attrs[name] : null),
      hasAttribute: (name) => Object.prototype.hasOwnProperty.call(attrs, name),
      getRootNode: () => sandbox.document,
      getBoundingClientRect: () => ({ x: 0, y: 0, width: 120, height: 20 }),
      closest: () => null
    };
  }

  const password = makeInput({
    id: 'pw',
    type: 'password',
    value: 'hunter2',
    placeholder: '请输入密码',
    name: 'password'
  });
  const account = makeInput({
    id: 'account',
    type: 'text',
    value: 'alice',
    placeholder: '用户名',
    name: 'account'
  });

  sandbox.document = {
    nodeType: 9,
    title: 'Login',
    readyState: 'complete',
    scripts: [],
    documentElement: { lang: 'en' },
    body: { querySelectorAll: () => [password, account] },
    // 唯一性判定靠 querySelectorAll 的条数：让两个元素的 id 选择器都「唯一」，
    // 路径才会退化成 #pw / #account（否则会拼出 'input' 这类无法区分两者的路径）。
    querySelectorAll: (selector) => {
      if (selector === '#pw') return [password];
      if (selector === '#account') return [account];
      return [];
    },
    querySelector: () => null,
    getElementById: () => null
  };
  sandbox.window = sandbox;

  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_snapshot.js'), context, { filename: 'page_snapshot.js' });

  const snap = sandbox.__abcSnapshot({});
  assert.equal(snap.ok, true, JSON.stringify(snap));
  assert.equal(snap.items.length, 2);

  const pwItem = snap.items.find((item) => item.selector === '#pw');
  const accountItem = snap.items.find((item) => item.selector === '#account');
  assert.ok(pwItem && accountItem, `产出不对：${JSON.stringify(snap.items)}`);

  assert.equal(pwItem.name, '•••', '口令字段的派生 name 没打码');
  assert.equal(pwItem.value, '•••', '口令字段的值没打码');
  assert.equal(pwItem.redacted, true);
  assert.equal(pwItem.role, 'textbox');

  // 非敏感字段一律保持可用：name/placeholder 是 agent 定位元素的唯一线索。
  assert.equal(accountItem.name, '用户名');
  assert.equal(accountItem.value, 'alice');
  assert.equal(accountItem.redacted, undefined);
});

// ----------------------------------------------------------------
test('challenge 正文判定不再整页序列化 body.textContent', () => {
  const code = source('page_challenge.js');
  // 注释里会引用旧写法作反例，比对前先剥掉注释，只看真实代码。
  const stripped = code.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');
  const hits = stripped.match(/body\.textContent/g) || [];
  assert.equal(hits.length, 1, `body.textContent 出现 ${hits.length} 次（应只有 bodyTextHead 的降级兜底）`);
  assert.match(code, /function bodyTextHeadCached\(/, '缺少共享的节流取头部文本函数');
  for (const name of ['cfBlockEvidence', 'blockTextHit']) {
    const body = stripped.slice(stripped.indexOf(`function ${name}(`));
    const end = body.indexOf('\n  }');
    assert.match(
      body.slice(0, end < 0 ? body.length : end),
      /bodyTextHeadCached\(/,
      `${name} 还在自己读整页正文`
    );
  }
});

// ----------------------------------------------------------------
/// `challenge wait` 的等待条件是「识别出厂商 && interactive && 还没 token」。
/// 旧实现只要 `scriptHas('recaptcha'|'hcaptcha.com')` 就置 interactive=true ——
/// 而这两个 api.js 几乎在所有接过风控的站点上都会加载（reCAPTCHA v3 根本不渲染
/// 组件），于是 wait 永远等不到 cleared，必然耗满预算。
/// 现在脚本标签只进 signals，interactive 必须由**可见挂载点**确认。
function challengeSandbox(options = {}) {
  const { sandbox } = baseSandbox();
  const mount = options.mount || null;
  const scripts = (options.scripts || []).map((src) => ({ src }));
  sandbox.document = {
    title: options.title || '登录',
    readyState: 'complete',
    visibilityState: 'visible',
    hidden: false,
    hasFocus: () => true,
    scripts,
    body: { textContent: 'hello' },
    documentElement: {},
    querySelector(selector) {
      if (mount && selector.indexOf('.g-recaptcha') >= 0) return mount;
      return null;
    },
    querySelectorAll(selector) {
      if (mount && selector === '.g-recaptcha') return [mount];
      return [];
    }
  };
  sandbox.location = { href: 'https://example.test/login' };
  sandbox.innerWidth = 1280;
  sandbox.innerHeight = 800;
  sandbox.outerWidth = 1280;
  sandbox.outerHeight = 800;
  sandbox.getComputedStyle = () => ({ display: 'block', visibility: 'visible', opacity: '1' });
  return sandbox;
}

function visibleMount(attrs = {}) {
  return {
    nodeType: 1,
    src: '',
    getAttribute: (name) => (Object.prototype.hasOwnProperty.call(attrs, name) ? attrs[name] : null),
    getBoundingClientRect: () => (attrs.hiddenBox
      ? { x: 0, y: 0, width: 0, height: 0 }
      : { x: 0, y: 0, width: 304, height: 78 })
  };
}

test(' 只有 reCAPTCHA 脚本标签、没有可见挂载点：不判 interactive', () => {
  const sandbox = challengeSandbox({
    scripts: ['https://www.google.com/recaptcha/api.js?render=explicit']
  });
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_challenge.js'), context, { filename: 'page_challenge.js' });

  const out = visibleJson(sandbox.__abcChallenge({ action: 'status' }));
  assert.equal(out.detected, false, `仅脚本标签不应判出厂商：${JSON.stringify(out.signals)}`);
  assert.equal(out.interactive, false, '仅脚本标签就把页面判成「有组件待完成」→ challenge wait 会耗满预算');
  assert.equal(out.blocking, false);
  assert.equal(out.widget_pending, false);
  // 线索仍然要能看到，别把「为什么没识别」也藏掉。
  assert.ok((out.signals || []).includes('recaptcha-script'), `signals=${JSON.stringify(out.signals)}`);
});

test(' 隐形挂件（data-size=invisible / 0×0）不算可交互', () => {
  const sandbox = challengeSandbox({
    scripts: ['https://www.google.com/recaptcha/api.js'],
    mount: visibleMount({ 'data-size': 'invisible' })
  });
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_challenge.js'), context, { filename: 'page_challenge.js' });
  const out = visibleJson(sandbox.__abcChallenge({ action: 'status' }));
  assert.equal(out.interactive, false, 'invisible 变体没有可点的勾选框');

  const hiddenBox = challengeSandbox({
    scripts: ['https://www.google.com/recaptcha/api.js'],
    mount: visibleMount({ hiddenBox: true })
  });
  const context2 = vm.createContext(hiddenBox);
  vm.runInContext(source('page_challenge.js'), context2, { filename: 'page_challenge.js' });
  const out2 = visibleJson(hiddenBox.__abcChallenge({ action: 'status' }));
  assert.equal(out2.interactive, false, '0×0 的占位点不到');
});

test(' 真的有可见挂载点：interactive + widget_pending 中间态', () => {
  const sandbox = challengeSandbox({
    scripts: ['https://www.google.com/recaptcha/api.js'],
    mount: visibleMount()
  });
  const context = vm.createContext(sandbox);
  vm.runInContext(source('page_challenge.js'), context, { filename: 'page_challenge.js' });

  const out = visibleJson(sandbox.__abcChallenge({ action: 'status' }));
  assert.equal(out.detected, true);
  assert.equal(out.vendor, 'recaptcha');
  assert.equal(out.kind, 'widget');
  assert.equal(out.blocking, false);
  assert.equal(out.interactive, true);
  assert.equal(out.widget_pending, true, '组件在、token 没拿到 → 中间态');
});

// ----------------------------------------------------------------
test('simphtml_opt：顶层函数收进 IIFE，入口不可枚举且用完即撤', () => {
  const { sandbox, timers } = baseSandbox();
  sandbox.document = {};
  const context = vm.createContext(sandbox);

  vm.runInContext(source('simphtml_opt.js'), context, { filename: 'simphtml_opt.js' });

  // 入口必须能被调用方的 `return optHTML(...)` 按裸标识符解析到。
  assert.equal(typeof sandbox.optHTML, 'function');
  assert.equal(vm.runInContext('typeof optHTML', context), 'function', 'optHTML 不能按裸标识符解析');
  assert.equal(Object.keys(sandbox).includes('optHTML'), false, 'Object.keys 泄露了 optHTML');
  const desc = Object.getOwnPropertyDescriptor(sandbox, 'optHTML');
  assert.equal(desc.enumerable, false);
  assert.equal(desc.configurable, true);

  // 其余顶层函数不应再是全局（旧实现每个都是可枚举全局）。
  for (const name of ['__abcOptSecret', '__abcOptHTMLBody', 'analyzeNode', 'handleOverlayContainer', 'hasOverlap']) {
    assert.equal(name in sandbox, false, `${name} 仍然泄露在全局上`);
  }

  // 调用方在同一段脚本里同步 return，之后没有任何消费者 → 本次任务末尾撤掉。
  assert.equal(timers.length > 0, true, '没有安排「用完即撤」');
  timers.splice(0).forEach((fn) => fn());
  assert.equal('optHTML' in sandbox, false, 'optHTML 留在页面上了');
});

test('simphtml_find_list：findMainList 同样收进 IIFE 且不覆盖页面已有的同名全局', () => {
  const { sandbox, timers } = baseSandbox();
  sandbox.document = {};
  sandbox.findMainList = 'page-owned';
  const context = vm.createContext(sandbox);

  vm.runInContext(source('simphtml_find_list.js'), context, { filename: 'simphtml_find_list.js' });

  assert.equal(typeof sandbox.findMainList, 'function');
  assert.equal(vm.runInContext('typeof findMainList', context), 'function', 'findMainList 不能按裸标识符解析');
  assert.equal(Object.keys(sandbox).includes('findMainList'), false);
  for (const name of ['findTopGroups', 'findMatchingElements', 'scoreContainer']) {
    assert.equal(name in sandbox, false, `${name} 仍然泄露在全局上`);
  }

  timers.splice(0).forEach((fn) => fn());
  // 页面自己占用的同名全局必须按快照还原，不能删掉用户的属性。
  assert.equal(sandbox.findMainList, 'page-owned', '注入把页面自己的 findMainList 弄丢了');
});

test('stealth 的自查清单包含 simphtml 的两个入口（audit 能报、scrub 能删）', () => {
  const code = source('page_stealth.js');
  const start = code.indexOf('const OWN_GLOBALS = [');
  const end = code.indexOf('];', start);
  assert.ok(start > 0 && end > start, '找不到 OWN_GLOBALS');
  const list = code.slice(start, end);
  for (const name of ["'optHTML'", "'findMainList'"]) {
    assert.ok(list.includes(name), `OWN_GLOBALS 漏了 ${name}`);
  }
});
