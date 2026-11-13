// 页面网络能力：
//   fetch  —— 在页面上下文里发请求（带 cookie / 同源策略 / 页面 CSP 视角）
//   hook   —— 包装 window.fetch 与 XMLHttpRequest，记录、改写请求、伪造/改写响应
//   log    —— 读取拦截记录
//   route  —— 增加 / 删除规则
//   reset  —— 还原所有包装并清空状态
//
// 规则字段（都可选）：
//   engine                由 daemon 使用：page（默认，本文件）/ cdp（扩展侧网络层拦截）
//   match / method        命中条件（URL 片段或正则；method 支持 *）
//   requestHeaders        追加/覆盖请求头
//   requestBody           替换请求体（字符串或对象）
//   status / body / headers / abort   命中即伪造响应（不再真的发出去）
//   response              放行真实请求后改写响应：{status, body, headers}
//
// 重要：本文件每次命令都会重新注入，因此状态必须挂在 window 上，
// 包装函数只在首次注入时安装，避免 fetch 被套多层。
//
// ---- 可检测性约束（改这段前先读完）----
//
// 1) 状态槽位：不再用 window.__abcNetState（名字显眼，且能被
//    Object.getOwnPropertyNames(window) 一眼认出）。改成
//      window['__atb_net_<random>'] = state
//    —— 不可枚举、不可写、可配置。**stealth scrub 依赖 "__atb_" 前缀**去定位
//    这些槽位（扫 own property names 取前缀匹配项）；改前缀或换成别的位置会让
//    scrub 漏掉残留。多次注入时先按前缀找回已有槽位，找不到才新建随机后缀，
//    这样同一页面里所有注入共享同一份状态，不会重复包装 fetch。
//
//    configurable 必须是 true：reset / scrub 要能把它 delete 掉。若设成 false，
//    这个属性会一直留在 window 上，getOwnPropertyNames 永远列得出来，删也删不掉
//    —— 比它想藏的东西更显眼。所以这里刻意不用 configurable:false。
//
// 2) 包装函数：见下面的 disguise()。站点最常见的检测是
//    fn.toString().includes('[native code]')，其次是 fn.name / fn.length。
//
// 3) 旧版本注入可能在页面上留了 window.__abcNetState：迁移到新槽位后 delete。
//
// 4) 入口定义：整个文件包在 IIFE 里，末尾用 defineProperty 把 __abcNet 挂到 window，
//    而不是写成顶层 `function __abcNet(){}`。原因有两条，都实测过：
//    a. 顶层函数声明在 classic script（扩展 executeScript({files}) 路径）里创建的是
//       **non-configurable** 全局绑定 —— 这个位是 V8 内部的 DONT_DELETE，
//       getOwnPropertyDescriptor 上看不出来（configurable 仍报 true），但
//       `delete window.__abcNet` 恒返回 false。结果 scrub 永远清不掉这个入口。
//       （daemon 的 eval 路径没有这个问题，但两条路径必须都给对。）
//    b. 全局函数声明是 enumerable 的，站点一行 `Object.keys(window)` 就能列出来。
//    defineProperty 之后它是非枚举 + 可配置：for...in / Object.keys 看不到，
//    delete 又能成功。名字保持不变 —— daemon 的 `return __abcNet({})` 是裸标识符，
//    靠全局对象属性照样解析得到；扩展侧是 `window[name]`，更不受影响。
;(function () {
function __abcNet(opts) {
  opts = opts || {};
  const action = String(opts.action || 'fetch').toLowerCase();
  const now = () => (typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now());

  // 状态槽位名 = STATE_PREFIX + randomToken()，即 `__atb_net_` + 8 位 [a-z0-9]。
  //
  // **这是一条跨文件契约**（读槽位的代码不在本文件里）：
  //   * assets/page_stealth.js 的 netStateKey()：audit 判定 + scrub 删除都靠它定位；
  //   * tests/live_xhr_wait.mjs：拿 xhrGuarded 判断访问器是否被接管。
  // 因此改前缀、改后缀长度或字符集，必须同步改这两处的匹配规则，否则 audit 会变瞎
  // （"页面干净"的假结论）。
  //
  // 读取侧必须**锚定整名**（/^__atb_net_[a-z0-9]{8}$/）而不是只比前缀：
  // 光比前缀的话，页面自己造一个 `__atb_net_fake` 就会被当成我们的槽位 —— scrub
  // 会删掉页面的属性、还报 'state-only'（读起来像"已还原"），既毁页面数据又误报。
  // 只按 enumerable === false 过滤不够：页面同样可以定义不可枚举属性，
  // 只有"名字形状 + 值结构（routes/log 数组、seq 数字、installed 布尔）"双检才稳。
  const STATE_PREFIX = '__atb_net_';
  const LEGACY_STATE_KEY = '__abcNetState';

  // wrapper 归属标记。
  ///
  /// 旧实现只靠 `state.installed` 防重复包装：状态槽被 scrub / reset 删掉后，
  /// 下一次注入会新建一份空 state，并把**已经是我们自己的 wrapper** 当成「原始 fetch」
  /// 再包一层 —— 一次会话可以套出好几层，日志重复、reset 也还原不回去。
  /// 现在给每个 wrapper 打两个不可枚举标记：
  ///   * `__abcNetOwn === true`：这个函数是我们装的（与扩展侧 __abcOwn 同一思路）；
  ///   * `[Symbol.for('atb.net.hookOwner')] = { state, key, originals }`：
  ///     包装时用的状态与原始函数。槽位丢失后再注入时直接**接管**这份状态，
  ///     既不二次包装，也不丢已捕获的日志/规则。
  /// 非枚举是硬要求：`Object.keys(fn)` / `for...in` 不能多出痕迹。
  const OWN_FETCH_MARK = '__abcNetOwn';
  const OWNER_SYMBOL = (function () {
    try { return Symbol.for('atb.net.hookOwner'); } catch (_) { return '__abcNetHookOwner'; }
  })();

  function markOwn(fn, info) {
    if (typeof fn !== 'function') return fn;
    try { Object.defineProperty(fn, OWN_FETCH_MARK, { value: true, enumerable: false, configurable: true }); } catch (_) {}
    if (info) {
      try { Object.defineProperty(fn, OWNER_SYMBOL, { value: info, enumerable: false, configurable: true }); } catch (_) {}
    }
    return fn;
  }

  function ownerOf(fn) {
    if (typeof fn !== 'function') return null;
    try { return fn[OWNER_SYMBOL] || null; } catch (_) { return null; }
  }

  function isOurWrapper(fn) {
    if (typeof fn !== 'function') return false;
    try { return fn[OWN_FETCH_MARK] === true; } catch (_) { return false; }
  }

  let state = null;
  let stateKey = null;

  /// 8 位 [a-z0-9]。**长度和字符集是契约的一部分**（见 STATE_PREFIX 注释里的
  /// 锚定正则），改这里就要同步改读取侧的匹配规则。
  function randomToken() {
    const alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789';
    const bytes = new Uint8Array(8);
    let filled = false;
    try {
      if (typeof crypto !== 'undefined' && crypto.getRandomValues) {
        crypto.getRandomValues(bytes);
        filled = true;
      }
    } catch (_) {}
    if (!filled) {
      for (let i = 0; i < bytes.length; i += 1) bytes[i] = Math.floor(Math.random() * 256);
    }
    let out = '';
    for (let i = 0; i < bytes.length; i += 1) out += alphabet[bytes[i] % alphabet.length];
    return out;
  }

  /// 槽位名的形状：`__atb_net_` + 8 位 [a-z0-9]（见 STATE_PREFIX 注释里的契约）。
  const STATE_SLOT_RE = /^__atb_net_[a-z0-9]{8}$/;

  /// 值结构校验：名字可以被页面撞（甚至伪造），字段结构才是我们独有的。
  function looksLikeNetState(value) {
    return !!value && typeof value === 'object'
      && Array.isArray(value.routes) && Array.isArray(value.log)
      && typeof value.seq === 'number' && typeof value.installed === 'boolean';
  }

  /// 只接受**数据属性**，一律通过描述符读取。
  /// 页面可以把 `__atb_net_xxxxxxxx` 定义成 getter：直接 `window[name]` 会在我们的
  /// 注入上下文里执行页面代码，等于白送它一个执行通道。访问器属性直接跳过、不触发。
  ///
  /// `hasOwnProperty(desc, 'value')` 判的是「描述符里有没有 value 这个槽位」，也就是
  /// **属性的种类**；别改写成 `desc.value !== undefined`（那是判值，语义不同：数据属性
  /// 完全可以有 undefined 值，访问器则根本没有 value 槽位），更别图省事改回
  /// `window[name]` —— 那会把上面这条执行通道重新打开。
  function dataSlot(name) {
    let desc = null;
    try { desc = Object.getOwnPropertyDescriptor(window, name); } catch (_) { return null; }
    if (!desc) return null;
    if (typeof desc.get === 'function' || typeof desc.set === 'function') return null;
    if (!Object.prototype.hasOwnProperty.call(desc, 'value')) return null;
    return desc;
  }

  /// 找已经存在的状态槽位（按名字形状扫，不靠固定名字），保证重复注入共享同一份状态。
  /// 三个条件同时满足才认：名字形状对、不可枚举（我们的写法）、值结构像 state ——
  /// 只比前缀会让页面自造的 `__atb_net_fake` 被当成我们的槽位，reset 时被删掉。
  function findStateKey() {
    let names = [];
    try { names = Object.getOwnPropertyNames(window) || []; } catch (_) { return null; }
    for (const name of names) {
      if (typeof name !== 'string' || !STATE_SLOT_RE.test(name)) continue;
      const desc = dataSlot(name);
      if (!desc || desc.enumerable !== false) continue;
      if (!looksLikeNetState(desc.value)) continue;
      return name;
    }
    return null;
  }

  function emptyState() {
    return {
      installed: false,
      seq: 0,
      log: [],
      routes: [],
      originals: null,
      maxLog: 200,
      captureBodies: false,
      // 响应体捕获的超时（毫秒）：0 = 用默认值。由 hook 的 bodyTimeoutMs 覆盖。
      bodyTimeoutMs: 0,
      // 我们装上去的 wrapper 引用：reset 只还原**自己的**包装，
      // 页面后来又替换过的 fetch 一律不碰。
      fetchWrapper: null,
      xhrOpenWrapper: null,
      xhrSendWrapper: null,
      xhrSetHeaderWrapper: null,
      xhrGuarded: false,
      xhrNative: null,
      xhrRestore: null
    };
  }

  function loadState() {
    const existingKey = findStateKey();
    if (existingKey) {
      const found = dataSlot(existingKey);
      if (found) return { key: existingKey, state: found.value };
    }

    const created = emptyState();
    // 旧版本（window.__abcNetState）留下的状态：接着用，然后把旧名字抹掉。
    // 同样只认数据属性 + 结构匹配：页面随手写个 window.__abcNetState = 'x'
    // 不该被我们当成自己的状态，更不该被 reset 删掉。
    const legacyDesc = dataSlot(LEGACY_STATE_KEY);
    if (legacyDesc && looksLikeNetState(legacyDesc.value)) {
      const legacy = legacyDesc.value;
      for (const key of Object.keys(created)) {
        if (legacy[key] !== undefined) created[key] = legacy[key];
      }
      try { delete window[LEGACY_STATE_KEY]; } catch (_) {}
    }

    const key = publishState(created, null);
    return { key: key, state: created };
  }

  /// 把状态对象挂回 window（不可枚举、可配置）。reset / scrub 会把它删掉，
  /// 接管旧 wrapper 时也要重新挂回去，否则下一次注入又找不到这份状态。
  function publishState(target, preferredKey) {
    const key = preferredKey || (STATE_PREFIX + randomToken());
    try {
      Object.defineProperty(window, key, {
        value: target,
        enumerable: false,
        configurable: true,
        writable: false
      });
    } catch (_) {
      // 页面把这个名字占成了不可配置属性：退回普通赋值至少让本次注入能用。
      try { window[key] = target; } catch (_) {}
    }
    return key;
  }

  /// 每次进入都确认 state 绑在 window 的真实槽位上：reset 会把槽位删掉，
  /// 之后的调用（同一份注入再被调用）需要重新建一份，否则会拿旧对象继续包装。
  function hydrate() {
    if (state && stateKey) {
      // 同样走描述符：页面若把槽位重定义成 getter，读值就等于执行它的代码。
      const alive = dataSlot(stateKey);
      if (alive && alive.value === state) return;
    }
    const slot = loadState();
    state = slot.state;
    stateKey = slot.key;
  }

  hydrate();

  // ---- 包装函数伪装 ------------------------------------------------------------
  //
  // 站点判断「fetch 被改过」最省事的办法就是 `window.fetch.toString()`：
  // 原生实现含 "[native code]"，我们的包装函数不含，一测就露。
  //
  // 这里不动 Function.prototype（改全局原型本身就是更强的痕迹，而且会拖慢
  // 整个页面、还可能被冻结原型的站点搞崩），只在 wrapper 自身上盖一个 toString，
  // 让 fn.toString() / String(fn) 都返回**原函数**的原生源码。同时对齐 name、
  // length 与原型链，覆盖另外两个廉价检测点。
  //
  // 幂等：wrapper 由 install() 在 installed 标志下只创建一次，重复注入不会二次包装。
  function disguise(wrapper, original) {
    if (typeof wrapper !== 'function') return wrapper;
    if (typeof original !== 'function') return wrapper;
    try {
      Object.defineProperty(wrapper, 'name', {
        value: original.name, configurable: true
      });
    } catch (_) {}
    try {
      Object.defineProperty(wrapper, 'length', {
        value: original.length, configurable: true
      });
    } catch (_) {}
    try {
      Object.setPrototypeOf(wrapper, Object.getPrototypeOf(original));
    } catch (_) {}
    try {
      const nativeSource = Function.prototype.toString.call(original);
      Object.defineProperty(wrapper, 'toString', {
        value: function toString() { return nativeSource; },
        enumerable: false,
        configurable: true,
        writable: true
      });
    } catch (_) {}
    return wrapper;
  }

  function pushLog(entry) {
    entry.id = ++state.seq;
    entry.at = Date.now();
    state.log.push(entry);
    if (state.log.length > state.maxLog) state.log.splice(0, state.log.length - state.maxLog);
    return entry;
  }

  function toHeaders(input) {
    const out = {};
    if (!input) return out;
    if (typeof Headers !== 'undefined' && input instanceof Headers) {
      input.forEach(function (value, key) { out[key] = value; });
      return out;
    }
    if (Array.isArray(input)) {
      for (const pair of input) {
        if (pair && pair.length >= 2) out[String(pair[0])] = String(pair[1]);
      }
      return out;
    }
    if (typeof input === 'object') {
      for (const key of Object.keys(input)) out[key] = String(input[key]);
      return out;
    }
    return out;
  }

  /// 只认**自有**属性：页面污染 Object.prototype（`Object.prototype.status = 200`）
  /// 之后，`rule.status !== undefined` 会对所有规则恒真 → isMock 恒真 → 所有命中规则的
  // 请求都被伪造。规则对象来自 JSON，走 hasOwnProperty 即可。
  function own(obj, key) {
    return !!obj && Object.prototype.hasOwnProperty.call(obj, key);
  }

  // 相对 URL 先解析成绝对 URL 再参与匹配。
  /// 页面写 `fetch('/api/user')` 时匹配目标只有 "/api/user"，用户按直觉配的
  /// `match:"example.com/api"` 永远不命中，而 `net route` 自己返回 ok、日志记「未命中」
  /// —— 静默失配，极难排查。日志里仍保留页面原本写的相对 URL。
  function absoluteUrl(url) {
    const raw = String(url || '');
    try { return new URL(raw, location.href).href; } catch (_) { return raw; }
  }

  /// 规则匹配：默认按**子串**匹配；`regex:true` 才把 match 当正则。
  ///
  /// 以前是「子串不中就用 new RegExp(match) 再试一次」：字面量里的 `.` `?` `+`
  /// 在正则分支意外获得通配语义（`app.js` 会命中 `appXjs`），用户无法表达
  // 「只按字面匹配」，而且每个请求每条规则都要重编译一次正则。
  /// 为了不打断已经在用的正则写法，字符串里带明显正则元字符时仍走回退分支；
  /// 想强制关掉回退可以写 `regex:false`。
  const REGEX_LOOKING = /[\^$*+?()[\]{}|\\]/;
  const reCache = new Map();

  function regexFor(pattern) {
    if (reCache.has(pattern)) return reCache.get(pattern);
    let compiled = null;
    try { compiled = new RegExp(pattern); } catch (_) { compiled = null; }
    // 上限 128：正常情况下规则只有 MAX_ROUTES（64）条，页面反复改规则时也不会无限增长。
    if (reCache.size >= 128) reCache.clear();
    reCache.set(pattern, compiled);
    return compiled;
  }

  function matchRule(url, method) {
    const target = String(url || '');
    const absolute = absoluteUrl(target);
    const targets = absolute === target ? [target] : [target, absolute];
    const verb = String(method || 'GET').toUpperCase();
    for (const rule of state.routes) {
      if (own(rule, 'method') && rule.method && rule.method !== '*'
          && String(rule.method).toUpperCase() !== verb) continue;
      if (!own(rule, 'match') || !rule.match) continue;
      const pattern = String(rule.match);
      let hit = targets.some(candidate => candidate.indexOf(pattern) >= 0);
      if (!hit && rule.regex !== false) {
        const useRegex = rule.regex === true || REGEX_LOOKING.test(pattern);
        const compiled = useRegex ? regexFor(pattern) : null;
        if (compiled) hit = targets.some(candidate => compiled.test(candidate));
      }
      if (hit) return rule;
    }
    return null;
  }

  function ruleBody(rule, fallback) {
    const body = own(rule, 'body') ? rule.body : fallback;
    if (body === undefined || body === null) return '';
    return typeof body === 'string' ? body : JSON.stringify(body);
  }

  function ruleHeaders(rule) {
    return Object.assign({ 'content-type': 'application/json' }, rule.headers || {});
  }

  function isMock(rule) {
    if (!rule) return false;
    return rule.abort === true
      || (own(rule, 'status') && rule.status !== undefined)
      || (own(rule, 'body') && rule.body !== undefined);
  }

  // ---- XHR 响应改写：接管访问器，而不是在事件回调里改实例 ----------------
  //
  // 事件监听器按「注册顺序」触发。页面通常在建好 XHR 后立刻 xhr.onload = ...
  // 再 send()，我们的 loadend 监听器只能注册在它们之后；即便在回调里改写
  // xhr.responseText，页面也早就把原始响应读走了。所以这里接管
  // XMLHttpRequest.prototype 上的 responseText / response / status / statusText
  // 以及两个 header 方法：读取时只要实例命中改写规则就返回改写值，
  // 与谁先注册监听器、读得多早都无关。规则在 send() 里挂到实例上，
  // 一定早于任何响应事件。
  const XHR_ACCESSORS = ['responseText', 'response', 'status', 'statusText'];

  function findDescriptor(name) {
    let cursor = XMLHttpRequest.prototype;
    while (cursor) {
      const found = Object.getOwnPropertyDescriptor(cursor, name);
      if (found) return found;
      cursor = Object.getPrototypeOf(cursor);
    }
    return null;
  }

  function captureXhrNative() {
    const props = {};
    for (const name of XHR_ACCESSORS) {
      const descriptor = findDescriptor(name);
      if (descriptor && descriptor.get) props[name] = descriptor;
    }
    return {
      props: props,
      allHeaders: XMLHttpRequest.prototype.getAllResponseHeaders,
      oneHeader: XMLHttpRequest.prototype.getResponseHeader
    };
  }

  function parseHeaderLines(raw) {
    const map = {};
    String(raw || '').split(/\r?\n/).forEach(function (line) {
      const at = line.indexOf(':');
      if (at <= 0) return;
      map[line.slice(0, at).trim().toLowerCase()] = line.slice(at + 1).trim();
    });
    return map;
  }

  /// 伪造 readyState=4（mock / abort 都要用）。
  ///
  /// 只在实例上还没有 own readyState 时补一个，并留下 `__abcNetOwnReady` 标记：
  /// 复用同一个 xhr 再发真实请求时，open() 靠这个标记**只删我们自己打的补丁**，
  // 不会误删页面/框架原本就在实例上设的值。
  function fakeReadyState(xhr) {
    if (Object.getOwnPropertyDescriptor(xhr, 'readyState')) return;
    try {
      Object.defineProperty(xhr, 'readyState', { value: 4, configurable: true });
      xhr.__abcNetOwnReady = true;
    } catch (_) {}
  }

  function buildRewrite(xhr, spec, out) {
    if (spec.mock) {
      out.text = spec.mock.text === undefined || spec.mock.text === null ? '' : String(spec.mock.text);
      if (spec.mock.status !== undefined) out.status = Number(spec.mock.status);
      if (spec.mock.statusText !== undefined) out.statusText = String(spec.mock.statusText);
      if (spec.mock.headers) out.headers = spec.mock.headers;
    }
    const rewrite = spec.response;
    if (!rewrite) return;
    // 只改 status/headers 时**不读 body**：旧实现无条件 rawXhrText(xhr)，
    // 于是「只想把 404 改成 200」也要把整个响应体解码成字符串 —— arraybuffer
    // 还要多走一趟 TextDecoder、blob 直接读不出来被误判成 body_unreadable，
    // 整条改写作废。现在没有 body 规则就把 text 留成 null，让 responseText /
    // response 原样透传（访问器见 rewriteValue：null 即放行原生值）。
    if (rewrite.body === undefined) {
      if (rewrite.status !== undefined) out.status = Number(rewrite.status);
      if (rewrite.statusText !== undefined) out.statusText = String(rewrite.statusText);
      // rule.headers 是「响应头」的统一入口（mock 与改写都认），
      // response.headers 只对改写生效且优先级更高。
      if (spec.headers || rewrite.headers) {
        out.headers = Object.assign({}, spec.headers || {}, rewrite.headers || {});
      }
      return;
    }
    const text = typeof rewrite.body === 'string' ? rewrite.body : JSON.stringify(rewrite.body);
    if (typeof text !== 'string') {
      // JSON.stringify 对函数 / Symbol / 循环引用返回 undefined 或抛错；不能拿
      // 空串顶替（会吞掉真实响应），整条改写作废并记原因。
      out.unsupported = 'body_unserializable';
      return;
    }
    out.text = text;
    if (rewrite.status !== undefined) out.status = Number(rewrite.status);
    if (rewrite.statusText !== undefined) out.statusText = String(rewrite.statusText);
    // rule.headers 是「响应头」的统一入口（mock 与改写都认），
    // response.headers 只对改写生效且优先级更高。
    if (spec.headers || rewrite.headers) {
      out.headers = Object.assign({}, spec.headers || {}, rewrite.headers || {});
    }
  }

  /// 惰性构建 + 缓存：第一次被读时算一次，之后直接复用。
  function rewriteOf(xhr) {
    const spec = xhr.__abcNetRespSpec;
    if (!spec) return null;
    if (xhr.readyState !== 4) return null;
    if (xhr.__abcNetRespBuilt) return xhr.__abcNetRespBuilt;
    const out = { text: null, status: null, statusText: null, headers: null, unsupported: null };
    try {
      buildRewrite(xhr, spec, out);
    } catch (e) {
      out.unsupported = (e && e.message) || String(e);
    }
    try {
      Object.defineProperty(xhr, '__abcNetRespBuilt', { value: out, configurable: true });
    } catch (_) {}
    return out;
  }

  function rewriteValue(xhr, out) {
    // out.text === null 表示「响应体没有真的被改写」（读不出来，或规则只改了
    // status/headers）。此时必须放行原生响应：否则下面 json 分支的
    // JSON.parse(null) 会得到 null，页面拿到的不是原响应而是一个空值。
    if (!out || out.text === null) return undefined;
    const type = xhr.responseType || '';
    if (type === '' || type === 'text') return out.text;
    if (type === 'json') {
      try { return JSON.parse(out.text); } catch (_) { return null; }
    }
    if (type === 'arraybuffer') {
      if (typeof TextEncoder === 'undefined') return undefined;
      const bytes = new TextEncoder().encode(out.text);
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    }
    if (type === 'document') {
      try { return new DOMParser().parseFromString(out.text, 'text/html'); } catch (_) { return undefined; }
    }
    if (type === 'blob') {
      // 原始 blob 无法同步读出，所以只有规则显式给了 body 才能构造新 blob。
      // 想改写「原始二进制体」需要用网络层拦截（net mock --engine cdp）。
      if (typeof Blob === 'undefined' || out.text === null) return undefined;
      const headers = out.headers || {};
      let contentType = null;
      for (const key of Object.keys(headers)) {
        if (String(key).toLowerCase() === 'content-type') contentType = String(headers[key]);
      }
      if (!contentType) {
        // 规则没改 content-type 就沿用原始响应头，别把 JSON blob 变成二进制流。
        try {
          const native = state.xhrNative && state.xhrNative.oneHeader;
          if (native) contentType = native.call(xhr, 'content-type');
        } catch (_) {}
      }
      return new Blob([out.text], { type: contentType || 'application/octet-stream' });
    }
    return undefined;
  }

  // ---- 响应体捕获与响应改写：都不能阻塞页面自己的 fetch ----------------------
  //
  // `clone.text()` 要等整个 body 结束。对 text/event-stream 这类**永不结束**的流，
  // 页面自己的 fetch Promise 就永远不 resolve —— 聊天流、进度推送、视频分片全挂死
  //。所以：
  //   * 捕获走旁路（不 await）：日志条目先落盘、preview 后补；
  //   * 改写只动 status/headers 时透传 response.body 流，不缓冲；
  //   * 命中流式/超大响应且规则没给 body 时跳过改写，并标明原因。
  const MAX_CAPTURE_BYTES = 4 * 1024 * 1024;
  // 捕获超时：旧实现只按 content-length 判「太大就跳过」，
  // 没有长度的 chunked 响应于是走 clone().text() 一直等下去 —— 页面自己的 fetch
  // 不受影响（捕获是旁路），但这条日志永远停在 body_capture_pending，body 永远补不上。
  // 现在：超时硬上限 + reader.cancel()，并由 content-type 白名单挡掉二进制。
  const DEFAULT_BODY_TIMEOUT_MS = 1500;
  const MAX_BODY_TIMEOUT_MS = 10000;
  // 规则条数上限：与 daemon 的 `ATB_NET_RULE_MAX_RULES`（默认 64）同口径。
  ///
  /// 两边必须是**同一个数字**：daemon 在新增规则时按它报 `too_many_rules`，页面在
  /// 「持久注入重放」时按它裁剪。历史实现这里是 50、daemon 是 64，于是 51~64 条规则
  /// 在刷新后会被页面静默裁掉 —— `net routes` 显示全量、浏览器实际只生效前 50 条，
  /// first-match 的原位替换语义也随之失真。只写一处、只改一处。
  const MAX_ROUTES = 64;
  const TEXT_LIKE_TYPES = [
    'text/',
    'application/json',
    'application/x-www-form-urlencoded',
    'application/javascript',
    'application/xml',
    'application/xhtml+xml',
    'application/ld+json',
    'application/problem+json',
    'application/graphql'
  ];
  const NULL_BODY_STATUS = new Set([101, 204, 205, 304]);

  function captureTimeoutMs() {
    const raw = Number(state.bodyTimeoutMs);
    if (!Number.isFinite(raw) || raw <= 0) return DEFAULT_BODY_TIMEOUT_MS;
    return Math.min(MAX_BODY_TIMEOUT_MS, Math.max(100, Math.floor(raw)));
  }

  function captureSkipReason(response) {
    let type = '';
    let length = 0;
    try { type = String(response.headers.get('content-type') || '').toLowerCase(); } catch (_) {}
    try { length = Number(response.headers.get('content-length') || 0); } catch (_) {}
    if (type.indexOf('text/event-stream') >= 0) return 'streaming_response (text/event-stream)';
    if (length > MAX_CAPTURE_BYTES) return 'content_length ' + length + ' > ' + MAX_CAPTURE_BYTES;
    // 二进制（图片/字体/视频分片）不做捕获：clone().text() 会把它按 UTF-8 解码，
    // 体积膨胀几十倍、内容全是乱码，对诊断没有价值。
    // 没有 content-type 时不跳过（宁可多读一次），没有 content-length 也不再跳过 ——
    // chunked 流由超时兜住。
    if (type && !TEXT_LIKE_TYPES.some((prefix) => type.indexOf(prefix) >= 0)) {
      return 'non_text_content_type (' + type.slice(0, 60) + ')';
    }
    return null;
  }

  /// 旁路读取响应体：不 await（页面自己的 fetch 不等这条），但**必须有上限**。
  /// 优先用 reader 逐块读：超时能 cancel；`clone().text()` 没有取消入口，
  // 遇到永不结束的 chunked 流就是永久挂起。
  function captureBody(response, entry) {
    const limitMs = captureTimeoutMs();
    let reader = null;
    let finished = false;
    let timer = null;
    function finish(fn) {
      if (finished) return;
      finished = true;
      if (timer) { try { clearTimeout(timer); } catch (_) {} timer = null; }
      try { fn(); } finally { try { delete entry.body_capture_pending; } catch (_) {} }
    }
    timer = setTimeout(function () {
      try { if (reader) reader.cancel(); } catch (_) {}
      finish(function () {
        entry.body_capture_skipped = 'timeout ' + limitMs + 'ms';
        entry.body_capture_error = 'body_capture_timeout';
      });
    }, limitMs);
    Promise.resolve().then(async function () {
      let text = '';
      try {
        const clone = response.clone();
        if (!clone || typeof clone.text !== 'function') throw new Error('response.clone() unavailable');
        if (clone.body && typeof clone.body.getReader === 'function'
            && typeof TextDecoder !== 'undefined') {
          reader = clone.body.getReader();
          const decoder = new TextDecoder('utf-8');
          let bytes = 0;
          for (;;) {
            const step = await reader.read();
            if (step.done) break;
            if (!step.value) continue;
            bytes += step.value.byteLength || 0;
            // 内容被压缩/服务端没给长度时，字节数只能边读边算：超了立刻停。
            if (bytes > MAX_CAPTURE_BYTES) throw new Error('body > ' + MAX_CAPTURE_BYTES + ' bytes');
            text += decoder.decode(step.value, { stream: true });
          }
          text += decoder.decode();
        } else {
          text = await clone.text();
        }
      } catch (e) {
        finish(function () {
          entry.body_capture_error = (e && e.message) || String(e);
        });
        return;
      }
      finish(function () {
        entry.response_preview = text.slice(0, 2000);
        entry.response_length = text.length;
      });
    });
  }

  /// `new Response(...)` 会丢掉 url / type / redirected（url 变空串、type 变 'default'），
  /// 依赖这些字段的页面会误判。能补就补，补不上留给日志标 url_lost。
  function preserveMeta(next, original) {
    const lost = [];
    for (const key of ['url', 'type', 'redirected']) {
      try {
        const value = original[key];
        if (value === undefined) continue;
        Object.defineProperty(next, key, { value: value, configurable: true, enumerable: false });
      } catch (_) { lost.push(key); }
    }
    return lost;
  }

  /// 构造 mock 响应。以前这段在 try **之外**：规则写 `status:204`（无内容响应是很
  /// 常见的 mock 需求）同时带 body 时 `new Response` 抛 TypeError，异常从包装器同步
  /// 抛出 → 页面 fetch 直接 reject，且一条日志都没有，`net log` 里查不到这次请求
  // 。现在：无内容状态码强制 body=null，任何构造失败都记
  /// `mock_invalid` 并放行真实请求。
  function buildMockResponse(rule, url, method, started) {
    const status = rule.status === undefined ? 200 : Number(rule.status);
    const valid = Number.isInteger(status) && status >= 200 && status <= 599;
    const body = NULL_BODY_STATUS.has(status) ? null : ruleBody(rule, '');
    try {
      if (!valid) throw new TypeError('invalid status: ' + String(rule.status));
      const response = new Response(body, {
        status: status,
        statusText: rule.statusText || 'OK',
        headers: ruleHeaders(rule)
      });
      pushLog({
        kind: 'fetch', url, method, mocked: true, status: status,
        ok: true, elapsed_ms: Math.round(now() - started)
      });
      return response;
    } catch (e) {
      pushLog({
        kind: 'fetch', url, method, mocked: true, ok: false,
        mock_invalid: (e && e.message) || String(e), status: status,
        elapsed_ms: Math.round(now() - started)
      });
      return null;
    }
  }

  function guardXhr() {
    if (state.xhrGuarded) return;
    const native = captureXhrNative();
    if (!native.props.responseText || !native.props.response) return;
    state.xhrNative = native;
    const proto = XMLHttpRequest.prototype;
    const restore = [];

    function define(name, descriptor) {
      // 浏览器实现里拿不到原生 getter 就干脆不接管，调用方仍走原生路径。
      if (!native.props[name]) return;
      const nativeDescriptor = native.props[name];
      const guarded = Object.assign({}, descriptor);
      // 接管的 getter 也是包装函数：toString 要跟原生的一致，
      // 否则 Object.getOwnPropertyDescriptor(proto, 'responseText').get 就成了铁证。
      if (typeof guarded.get === 'function' && nativeDescriptor.get) {
        guarded.get = disguise(guarded.get, nativeDescriptor.get);
      }
      try {
        Object.defineProperty(proto, name, guarded);
        restore.push({ name: name, descriptor: nativeDescriptor });
      } catch (_) {}
    }

    define('responseText', {
      configurable: true,
      enumerable: true,
      get: function () {
        const out = rewriteOf(this);
        // responseType 非 text 时原生访问器本身就该抛错，保持原语义。
        if (!out || out.text === null || this.responseType) {
          return native.props.responseText.get.call(this);
        }
        return out.text;
      }
    });

    define('response', {
      configurable: true,
      enumerable: true,
      get: function () {
        const out = rewriteOf(this);
        if (!out) return native.props.response.get.call(this);
        const value = rewriteValue(this, out);
        return value === undefined ? native.props.response.get.call(this) : value;
      }
    });

    define('status', {
      configurable: true,
      enumerable: true,
      get: function () {
        const out = rewriteOf(this);
        if (!out || out.status === null) return native.props.status.get.call(this);
        return out.status;
      }
    });

    define('statusText', {
      configurable: true,
      enumerable: true,
      get: function () {
        const out = rewriteOf(this);
        if (!out || out.statusText === null) return native.props.statusText.get.call(this);
        return out.statusText;
      }
    });

    proto.getAllResponseHeaders = disguise(function () {
      const raw = native.allHeaders.call(this);
      const out = rewriteOf(this);
      if (!out || !out.headers) return raw;
      const map = parseHeaderLines(raw);
      for (const key of Object.keys(out.headers)) {
        map[String(key).toLowerCase()] = String(out.headers[key]);
      }
      // 体被换掉后长度/编码必然对不上，直接去掉，免得页面按旧长度截断。
      if (out.text !== null) {
        delete map['content-length'];
        delete map['content-encoding'];
      }
      const keys = Object.keys(map);
      if (!keys.length) return '';
      return keys.map(function (key) { return key + ': ' + map[key]; }).join('\r\n') + '\r\n';
    }, native.allHeaders);

    proto.getResponseHeader = disguise(function (name) {
      const key = String(name || '').toLowerCase();
      const out = rewriteOf(this);
      if (out && out.headers) {
        for (const header of Object.keys(out.headers)) {
          if (String(header).toLowerCase() === key) return String(out.headers[header]);
        }
        if (out.text !== null && (key === 'content-length' || key === 'content-encoding')) return null;
      }
      return native.oneHeader.call(this, name);
    }, native.oneHeader);

    state.xhrRestore = restore;
    state.xhrGuarded = true;
  }

  // 把 originals 的 XHR 基线对齐到**当前**原型方法。
  /// 当前方法是我们自己的 wrapper 时保持原基线，否则新 wrapper 会调自己 → 无限递归。
  function rebaseXhrOriginals(originals) {
    const proto = XMLHttpRequest.prototype;
    const pairs = [
      ['xhrOpen', 'open', 'xhrOpenWrapper'],
      ['xhrSend', 'send', 'xhrSendWrapper'],
      ['xhrSetHeader', 'setRequestHeader', 'xhrSetHeaderWrapper']
    ];
    for (const pair of pairs) {
      const key = pair[0];
      const name = pair[1];
      const wrapperKey = pair[2];
      const current = proto[name];
      if (typeof current !== 'function') continue;
      if (current === state[wrapperKey] || isOurWrapper(current)) continue;
      if (originals[key] === current) continue;
      if (typeof originals[key] === 'function') state.xhr_replaced = true;
      originals[key] = current;
    }
  }

  function install() {
    // ① 已经装好且仍在位：直接复用（重复 hook 的最常见路径）。
    if (state.installed && state.fetchWrapper && window.fetch === state.fetchWrapper) {
      return state.originals;
    }
    const currentFetch = window.fetch;

    // ② 页面上的 fetch 带我们的标记，但不是当前 state 装的 —— 说明状态槽被
    //    reset / scrub 删过，这是上一次注入留下的 wrapper。接管它的状态
    //    （已捕获的日志、规则都还在），绝不二次包装。
    if (isOurWrapper(currentFetch)) {
      const owner = ownerOf(currentFetch);
      if (owner && owner.state && owner.state !== state && looksLikeNetState(owner.state)) {
        state = owner.state;
        state.installed = true;
        state.fetchWrapper = currentFetch;
        if (owner.originals) state.originals = owner.originals;
        // 槽位可能已被删掉：重新挂回去，让 scrub / 下一次注入仍能定位同一份状态。
        stateKey = publishState(state, stateKey);
        return state.originals;
      }
      if (owner && owner.state === state) {
        state.installed = true;
        state.fetchWrapper = currentFetch;
        stateKey = publishState(state, stateKey);
        return state.originals;
      }
      // 有标记但没有归属信息（极旧版本留下的 wrapper / 被页面整体复制过）：
      // 不覆盖它，本次只做观测，并明确标注冲突。
      state.installed = true;
      state.fetchWrapper = currentFetch;
      state.fetch_conflict = 'own_wrapper_without_owner';
      if (!state.originals) state.originals = { fetch: currentFetch };
      return state.originals;
    }

    // ③ 别家（页面自己）替换过 fetch，或我们在装好之后被页面顶掉：
    //    当前值原样保存成 originals（reset 时还原它），包装在它**外面**。
    //    旧实现无条件把 window.fetch 覆盖掉且 reset 直接写回旧引用，
    //    既不认页面后来装的 wrapper，也可能把页面的 fetch 抹成未定义。
    let originals = state.originals;
    if (originals && typeof originals === 'object') {
      // 基线是**共享对象**：install #1 装上去的 wrapper 在调用时才读
      // originals.fetch / originals.xhrOpen。就地改写等于让老 wrapper 调到「页面
      // 当前的实现」，而页面的实现通常正好包着老 wrapper —— pageWrapper →
      // 老 wrapper → pageWrapper 直接无限递归。所以先浅拷贝：新 wrapper 用新基线，
      // 老 wrapper 继续用老基线，页面自己的补丁留在链上。
      const next = Object.assign({}, originals);
      if (next.fetch !== currentFetch) {
        // 记录「装好之后被页面换过」：回包给调用方，排查起来不用猜。
        if (state.fetchWrapper) state.fetch_replaced = true;
        next.fetch = currentFetch;
      }
      // ：fetch 之外，XHR 三个方法也必须重定基线。
      //
      // 为什么会走到这里：`window.fetch !== state.fetchWrapper`，也就是「页面在我们
      // 装好之后换过 fetch，或把我们的 wrapper 顶掉了」。这时要装新 wrapper，而新
      // wrapper 的闭包用的是 originals —— 如果页面监控 SDK（Sentry / 各种 RUM）在
      // 这期间替换过 XHR 方法，沿用第一次保存的 originals 会把页面自己的补丁从调用
      // 链上**整体剪掉**：请求照发，那层 SDK 完全不执行，而且没有任何标记可查。
      // 只重定「当前值不是我们 wrapper」的项，否则会把基线改成自己（无限递归）。
      rebaseXhrOriginals(next);
      originals = next;
    } else {
      originals = {
        fetch: currentFetch,
        xhrOpen: XMLHttpRequest.prototype.open,
        xhrSend: XMLHttpRequest.prototype.send,
        xhrSetHeader: XMLHttpRequest.prototype.setRequestHeader
      };
    }
    state.originals = originals;
    // XHR 的响应改写靠访问器接管，必须和 fetch 一起装上。
    guardXhr();

    const fetchWrapper = disguise(async function (input, init) {
      const url = typeof input === 'string' ? input : (input && input.url) || '';
      const method = String((init && init.method) || (input && input.method) || 'GET').toUpperCase();
      const rule = matchRule(url, method);
      const started = now();

      if (rule && rule.abort) {
        pushLog({ kind: 'fetch', url, method, mocked: true, aborted: true, ok: false });
        throw new TypeError('Failed to fetch (blocked by atb rule)');
      }

      if (rule && isMock(rule)) {
        const mocked = buildMockResponse(rule, url, method, started);
        // 构造失败（非法 status / 204 带 body）已记进日志，这里放行真实请求，
        // 而不是把 TypeError 抛给页面。
        if (mocked) return mocked;
      }

      // 改写请求：只在规则确实带 request* 字段时重建 init，避免影响正常调用。
      let callInit = init;
      let rewritten = false;
      if (rule && (rule.requestHeaders || rule.requestBody !== undefined)) {
        const headers = Object.assign(
          toHeaders(input && input.headers),
          toHeaders(init && init.headers),
          toHeaders(rule.requestHeaders)
        );
        const next = Object.assign({}, init || {}, { headers: headers });
        // GET/HEAD 不能带 body，带规则的 body 只在其它方法上生效。
        if (rule.requestBody !== undefined && method !== 'GET' && method !== 'HEAD') {
          next.body = typeof rule.requestBody === 'string'
            ? rule.requestBody
            : JSON.stringify(rule.requestBody);
        }
        callInit = next;
        rewritten = true;
      }

      try {
        const response = await originals.fetch.call(this, input, callInit);
        const entry = {
          kind: 'fetch', url, method, status: response.status, ok: response.ok,
          elapsed_ms: Math.round(now() - started), mocked: false,
          request_rewritten: rewritten || undefined
        };
        if (state.captureBodies) {
          // 不 await：见 captureBody 的注释。流式响应直接跳过，避免挂死页面。
          const skip = captureSkipReason(response);
          if (skip) {
            entry.body_capture_skipped = skip;
          } else {
            entry.body_capture_pending = true;
            captureBody(response, entry);
          }
        }
        pushLog(entry);

        // 放行后改写响应。
        if (rule && rule.response) {
          const spec = rule.response;
          const skip = captureSkipReason(response);
          const status = spec.status || response.status;
          const statusText = spec.statusText || response.statusText;
          const nextHeaders = Object.assign(
            toHeaders(response.headers),
            toHeaders(rule.headers),
            toHeaders(spec.headers)
          );
          // 目标状态码是「无内容」时 body 必须置空：`new Response(stream, {status:204})`
          // 会直接抛 TypeError，页面自己的 fetch 就变成 reject。
          if (spec.body === undefined && NULL_BODY_STATUS.has(status)) {
            const noContent = new Response(null, {
              status: status,
              statusText: statusText,
              headers: nextHeaders
            });
            const lost = preserveMeta(noContent, response);
            pushLog({
              kind: 'fetch', url, method, response_modified: true, body_dropped: 'null_body_status',
              status: noContent.status, ok: true, url_lost: lost.length ? lost : undefined
            });
            return noContent;
          }
          // 只改 status / headers：透传原始 body 流，不读、不缓冲。
          // 流式响应也能走这条路 —— 见下面「拿不到流才退回缓冲」的分支。
          if (spec.body === undefined && response.body) {
            const passed = new Response(response.body, {
              status: status,
              statusText: statusText,
              headers: nextHeaders
            });
            const lost = preserveMeta(passed, response);
            pushLog({
              kind: 'fetch', url, method, response_modified: true, body_stream_passthrough: true,
              status: passed.status, ok: true, url_lost: lost.length ? lost : undefined
            });
            return passed;
          }
          // 拿不到 body 流（不透明响应 / 已被消费）且是流式或超大响应：不缓冲，
          // 放行原响应并记下原因 —— 否则 SSE 上整段缓冲等于永不返回。
          if (spec.body === undefined && skip) {
            pushLog({
              kind: 'fetch', url, method, response_modified: false,
              response_rewrite_skipped: skip,
              elapsed_ms: Math.round(now() - started)
            });
            return response;
          }
          let nextBody;
          if (spec.body !== undefined) {
            // 规则自带 body：原始响应体马上就要被丢弃，**绝不能**去读它。
            // 以前这里无条件 `await response.clone().text()`，遇到 text/event-stream
            // 这种永不结束的响应，页面自己的 fetch 会永久挂死。
            // 顺手把原始流取消掉，别让连接一直占着。
            try { if (response.body) response.body.cancel(); } catch (_) {}
            nextBody = ruleBody({ body: spec.body }, '');
          } else {
            try {
              nextBody = await response.clone().text();
            } catch (_) {
              nextBody = null;
            }
            // 读不到原始响应体（body 已被消费 / 不透明响应）且规则没显式给 body：
            // 空串会让页面拿到「200 + 空 body」，日志却写着改写成功——真实数据静默丢失。
            if (nextBody === null) {
              pushLog({
                kind: 'fetch', url, method, response_modified: false,
                response_rewrite_skipped: 'body_unreadable',
                elapsed_ms: Math.round(now() - started)
              });
              return response;
            }
          }
          const modified = new Response(nextBody, {
            status: status,
            statusText: statusText,
            headers: nextHeaders
          });
          preserveMeta(modified, response);
          pushLog({
            kind: 'fetch', url, method, response_modified: true,
            status: modified.status, ok: true
          });
          return modified;
        }
        return response;
      } catch (e) {
        pushLog({ kind: 'fetch', url, method, ok: false, error: e.message, elapsed_ms: Math.round(now() - started) });
        throw e;
      }
    }, originals.fetch);
    // 打归属标记（不可枚举）：下一次注入据此认出「这是我们的 wrapper」，
    // 状态槽丢失时接管而不是二次包装。
    markOwn(fetchWrapper, { state: state, originals: originals });
    state.fetchWrapper = fetchWrapper;
    window.fetch = fetchWrapper;

    XMLHttpRequest.prototype.open = disguise(function (method, url) {
      this.__abcNet = {
        method: String(method || 'GET').toUpperCase(),
        url: String(url || ''),
        // 请求头先缓冲，send 时统一下发：原生 setRequestHeader 对同名头是「追加」，
        // 页头已经设过 x-token 时，规则再设一次会变成 "origin, rewritten"。
        headerPairs: []
      };
      // 复用同一个 xhr 时清掉上一次的改写状态，否则会拿旧响应体。
      this.__abcNetRespSpec = null;
      try { delete this.__abcNetRespBuilt; } catch (_) {}
      // 同一个 XHR 反复 open/send 时，上一轮 send 里注册的 loadend 记账监听器
      // 还挂在实例上 —— 每轮多一条日志、多一次 rewriteOf，长轮询页面上会线性膨胀
      //。这里按保存的引用精确摘掉自己注册的那一个，不用 removeAll 误伤页面。
      if (this.__abcNetLoadend) {
        try { this.removeEventListener('loadend', this.__abcNetLoadend); } catch (_) {}
        this.__abcNetLoadend = null;
      }
      // mock / abort 会在实例上打一个 own 的 readyState=4（遮蔽原型的原生访问器）。
      // 复用同一个 xhr 再发真实请求时必须删掉它，否则 readyState 永远读作 4，
      // onreadystatechange / progress 全被遮蔽，等效挂死该请求。
      // 只删**我们自己打的**那个（靠 __abcNetOwnReady 标记）：页面/测试桩如果本来
      // 就有 own 的 readyState，不该被我们顺手删掉。
      if (this.__abcNetOwnReady) {
        try { delete this.readyState; } catch (_) {}
        try { delete this.__abcNetOwnReady; } catch (_) {}
      }
      return originals.xhrOpen.apply(this, arguments);
    }, originals.xhrOpen);

    XMLHttpRequest.prototype.setRequestHeader = disguise(function (name, value) {
      if (!this.__abcNet) return originals.xhrSetHeader.apply(this, arguments);
      // 非 OPENED 状态保持原生行为（抛 InvalidStateError）。
      if (this.readyState !== 1) return originals.xhrSetHeader.apply(this, arguments);
      this.__abcNet.headerPairs.push([String(name), String(value)]);
    }, originals.xhrSetHeader);

    XMLHttpRequest.prototype.send = disguise(function (body) {
      const meta = this.__abcNet || { method: 'GET', url: '', headers: {} };
      meta.started = now();
      if (typeof body === 'string') meta.request_preview = body.slice(0, 1000);
      const rule = matchRule(meta.url, meta.method);
      const xhr = this;

      if (rule && rule.abort) {
        setTimeout(function () {
          try {
            // 真实网络错误时浏览器会：readyState → 4、status → 0，并派发
            // **ProgressEvent**（不是裸 Event）。以前只 dispatch 一个普通 error，
            // readyState 停在 1，于是任何「在 onerror 里读 readyState/status」的库
            // 与重试逻辑都会行为错乱。
            fakeReadyState(xhr);
            xhr.__abcNetRespSpec = {
              mock: { text: '', status: 0, statusText: '', headers: null },
              response: null
            };
            if (typeof ProgressEvent === 'function') {
              xhr.dispatchEvent(new ProgressEvent('error'));
            } else {
              xhr.dispatchEvent(new Event('error'));
            }
            xhr.dispatchEvent(new Event('loadend'));
          } catch (_) {}
        }, 0);
        pushLog({ kind: 'xhr', url: meta.url, method: meta.method, mocked: true, aborted: true, ok: false });
        return;
      }

      if (rule && isMock(rule)) {
        const status = rule.status === undefined ? 200 : Number(rule.status);
        // 204/304 不能带 body；这里只是伪造文本，统一置空以免页面解析出脏数据。
        const text = NULL_BODY_STATUS.has(status) ? '' : ruleBody(rule, '');
        // 只伪造 readyState，其余字段交给访问器守卫统一返回，
        // 这样 status / responseText / getResponseHeader 在任何时刻读都一致。
        xhr.__abcNetRespSpec = {
          mock: {
            text: text,
            status: status,
            statusText: rule.statusText || 'OK',
            headers: ruleHeaders(rule)
          },
          response: null
        };
        setTimeout(function () {
          // 事件只派发一次：旧实现先手动 `xhr.onreadystatechange()` 再
          // `dispatchEvent(new Event('readystatechange'))`，凡是 onreadystatechange
          // 与 addEventListener('readystatechange') 都注册的页面会收到两次回调
          // （fetch 封装 / 重试队列里会重复解析、重复 push）。dispatchEvent 本身
          // 就会同步调用 onreadystatechange 属性上的处理器，手动调用是多余的。
          // 每一段各自兜住异常：onreadystatechange 里抛错不能把 load/loadend 吞掉。
          try { fakeReadyState(xhr); } catch (_) {}
          try { xhr.dispatchEvent(new Event('readystatechange')); } catch (_) {}
          try { xhr.dispatchEvent(new Event('load')); } catch (_) {}
          try { xhr.dispatchEvent(new Event('loadend')); } catch (_) {}
        }, 0);
        pushLog({
          kind: 'xhr', url: meta.url, method: meta.method, mocked: true,
          status: status, ok: true
        });
        return;
      }

      // 改写请求头 / 请求体（XHR 在 send 之前仍可 setRequestHeader）。
      let nextBody = body;
      const overrideHeaders = rule && rule.requestHeaders ? toHeaders(rule.requestHeaders) : null;
      if (overrideHeaders || (meta.headerPairs && meta.headerPairs.length)) {
        const pairs = meta.headerPairs || [];
        const overridden = overrideHeaders
          ? Object.keys(overrideHeaders).map(function (key) { return key.toLowerCase(); })
          : [];
        const finalPairs = pairs.filter(function (pair) {
          return overridden.indexOf(pair[0].toLowerCase()) < 0;
        });
        if (overrideHeaders) {
          for (const key of Object.keys(overrideHeaders)) {
            finalPairs.push([key, String(overrideHeaders[key])]);
          }
        }
        for (const pair of finalPairs) {
          try { originals.xhrSetHeader.call(this, pair[0], pair[1]); } catch (_) {}
        }
      }
      if (rule && rule.requestBody !== undefined
          && meta.method !== 'GET' && meta.method !== 'HEAD') {
        nextBody = typeof rule.requestBody === 'string'
          ? rule.requestBody
          : JSON.stringify(rule.requestBody);
      }

      // 响应改写：必须在 send 之前挂上，页面监听器无论何时注册、读多早都拿到改写值。
      if (rule && rule.response) {
        xhr.__abcNetRespSpec = {
          mock: null,
          response: rule.response,
          headers: rule.headers || null
        };
      }

      // 记账监听器要留引用：open() 复用同一个 XHR 时要精确摘掉。
      const onLoadend = function () {
        const computed = rule && rule.response ? rewriteOf(xhr) : null;
        const entry = {
          kind: 'xhr', url: meta.url, method: meta.method,
          status: computed && computed.status !== null ? computed.status : xhr.status,
          ok: true, mocked: false,
          request_rewritten: (rule && (rule.requestHeaders || rule.requestBody !== undefined)) || undefined,
          elapsed_ms: Math.round(now() - (meta.started || now()))
        };
        entry.ok = entry.status >= 200 && entry.status < 400;
        if (computed) {
          // status / statusText / headers 被改也算「改写生效」，不能只看 text：
          // 只改状态码的规则以前会记成 response_modified:false，日志与页面实际看到的不符。
          entry.response_modified = !computed.unsupported && (
            computed.text !== null || computed.status !== null ||
            computed.statusText !== null || computed.headers !== null
          );
          if (computed.unsupported) entry.response_rewrite_skipped = computed.unsupported;
        }
        pushLog(entry);
      };
      xhr.__abcNetLoadend = onLoadend;
      xhr.addEventListener('loadend', onLoadend);
      return originals.xhrSend.call(this, nextBody);
    }, originals.xhrSend);

    // XHR 三个包装函数同样打标记并留引用：reset 只还原**自己的**包装，
    // 页面后来替换过的方法一律不碰。
    markOwn(XMLHttpRequest.prototype.open, { state: state, originals: originals });
    markOwn(XMLHttpRequest.prototype.send, { state: state, originals: originals });
    markOwn(XMLHttpRequest.prototype.setRequestHeader, { state: state, originals: originals });
    state.xhrOpenWrapper = XMLHttpRequest.prototype.open;
    state.xhrSendWrapper = XMLHttpRequest.prototype.send;
    state.xhrSetHeaderWrapper = XMLHttpRequest.prototype.setRequestHeader;

    state.installed = true;
    return originals;
  }

  async function readBody(response, maxChars, responseType) {
    const contentType = response.headers.get('content-type') || '';
    if (responseType === 'base64') {
      const buffer = await response.arrayBuffer();
      const bytes = new Uint8Array(buffer);
      // E1-10：base64 相对原始字节的膨胀比是 4/3，要拿到 maxChars 个字符
      // 只需要读约 0.75 * maxChars 字节；先截断再编码，避免把整页 base64 塞进内存。
      const maxBytes = Math.max(1, Math.ceil(maxChars * 0.75));
      const limit = Math.min(bytes.length, maxBytes);
      // 分块 String.fromCharCode.apply：逐字节 `binary +=` 在大页面上是 O(n²) 字符串
      // 拼接，会卡死页面（E1-10）。0x8000 一块也远低于各引擎的 apply 参数上限。
      const CHUNK = 0x8000;
      let binary = '';
      for (let i = 0; i < limit; i += CHUNK) {
        binary += String.fromCharCode.apply(null, bytes.subarray(i, Math.min(i + CHUNK, limit)));
      }
      return {
        body: btoa(binary),
        encoding: 'base64',
        truncated: bytes.length > limit,
        total_bytes: bytes.length,
        content_type: contentType
      };
    }
    // 文本分支：直接用 reader 流式读，累计到 maxChars 就 cancel，避免先
    // `response.text()` 全量缓冲再 slice（E1-10）。没有 getReader 的老实现 /
    // 测试桩（mock Response）退回原来的 text() 路径。
    if (response.body && typeof response.body.getReader === 'function'
        && typeof TextDecoder !== 'undefined') {
      const reader = response.body.getReader();
      const decoder = new TextDecoder('utf-8');
      let text = '';
      let truncated = false;
      try {
        for (;;) {
          const step = await reader.read();
          if (step.done) break;
          text += decoder.decode(step.value, { stream: true });
          if (text.length >= maxChars) { truncated = true; break; }
        }
        if (!truncated) text += decoder.decode();
      } finally {
        if (truncated) { try { await reader.cancel(); } catch (_) {} }
      }
      return {
        body: truncated ? text.slice(0, maxChars) : text,
        encoding: 'text',
        truncated,
        // 提前 cancel 后拿不到真实总长：显式给 null，别把「已读长度」冒充总数。
        total_chars: truncated ? null : text.length,
        content_type: contentType
      };
    }
    const text = await response.text();
    const truncated = text.length > maxChars;
    return {
      body: truncated ? text.slice(0, maxChars) : text,
      encoding: 'text',
      truncated,
      total_chars: text.length,
      content_type: contentType
    };
  }

  async function doFetch() {
    const url = String(opts.url || '').trim();
    if (!url) return { ok: false, error: 'net fetch: url is required' };
    const method = String(opts.method || 'GET').toUpperCase();
    const headers = Object.assign({}, opts.headers || {});
    const init = {
      method,
      headers,
      credentials: opts.credentials || 'include',
      redirect: 'follow'
    };
    if (opts.body !== undefined && opts.body !== null && method !== 'GET' && method !== 'HEAD') {
      init.body = typeof opts.body === 'string' ? opts.body : JSON.stringify(opts.body);
      if (!Object.keys(headers).some((key) => key.toLowerCase() === 'content-type')) {
        headers['content-type'] = typeof opts.body === 'string'
          ? 'text/plain;charset=UTF-8'
          : 'application/json';
      }
    }
    const started = now();
    // 用 hook **之前**的原始 fetch：`net fetch` 是诊断手段，用来验证「mock 是否生效」。
    // 走被包装过的全局 fetch 会命中自己写的规则 → 拿到伪造响应还当是服务器返回，
    // 而且每次诊断自己都进一条 log，形成自证循环。
    const upstream = state.installed && state.originals
      && typeof state.originals.fetch === 'function'
      ? state.originals.fetch
      : fetch;
    const bypassedHooks = upstream !== fetch;
    let response;
    try {
      // 不绑 this：原生 fetch 不依赖调用方，绑到 window 反而会在某些 vm/测试桩里
      // 让 shim 的 this 变成空对象（mock 扩展自检就踩过）。
      response = await upstream(url, init);
    } catch (e) {
      return {
        ok: false,
        url,
        method,
        error: 'fetch failed: ' + (e && e.message ? e.message : String(e)),
        elapsed_ms: Math.round(now() - started)
      };
    }
    const responseHeaders = {};
    response.headers.forEach(function (value, key) { responseHeaders[key] = value; });
    const maxChars = opts.maxChars > 0 ? Math.floor(opts.maxChars) : 200000;
    const payload = await readBody(response, maxChars, opts.responseType);
    let json = null;
    if (opts.parseJson !== false && /json/i.test(payload.content_type)) {
      try { json = JSON.parse(payload.body); } catch (_) {}
    }
    return {
      // `ok` 表示「这次 fetch 动作执行成功」，不是 HTTP 状态：以前直接取
      // `response.ok`，于是 404 会被 daemon 判成 "page script failed"，调用方
      // 拿不到状态码、body 和 headers。HTTP 成败放在 http_ok 里。
      ok: true,
      http_ok: response.ok,
      url: response.url || url,
      method,
      status: response.status,
      status_text: response.statusText,
      headers: responseHeaders,
      body: payload.body,
      encoding: payload.encoding,
      truncated: payload.truncated,
      total_chars: payload.total_chars,
      total_bytes: payload.total_bytes,
      json,
      bypassed_hooks: bypassedHooks || undefined,
      elapsed_ms: Math.round(now() - started)
    };
  }

  if (action === 'fetch') return doFetch();

  if (action === 'hook' || action === 'install') {
    // 只有显式给出才覆盖。旧实现无条件 `= opts.captureBodies === true`，
    // 于是第二次 `net hook --persist`（只想刷新 matches）会把已持久化的
    // captureBodies 静默压回 false，用户看到的是「body 抓不到了」却找不到原因。
    // daemon 侧已改成「None 不下发」，页面侧必须同样把「未传」当成「保持现状」。
    if (opts.captureBodies !== undefined) {
      state.captureBodies = opts.captureBodies === true;
    }
    // 响应体捕获超时（毫秒）：未传 = 保持现状；<0/0/非数字 = 回默认。
    if (opts.bodyTimeoutMs !== undefined) {
      const raw = Math.floor(Number(opts.bodyTimeoutMs));
      state.bodyTimeoutMs = Number.isFinite(raw) && raw > 0
        ? Math.min(MAX_BODY_TIMEOUT_MS, Math.max(100, raw))
        : 0;
    }
    // E1-11：maxLog 之前没有上限，`--max-log 100000000` 会把整个进程的内存吃光。
    // 下限 10 保证还能看到最近的几条；上限 20000 条足够诊断，再大就该用 match 过滤。
    if (opts.maxLog > 0) {
      state.maxLog = Math.min(20000, Math.max(10, Math.floor(opts.maxLog)));
      // 收紧上限时立刻裁剪：否则要等下一条日志进来才生效，刚 `--max-log 10`
      // 就 `net log` 仍会看到旧的几千条。
      if (state.log.length > state.maxLog) state.log.splice(0, state.log.length - state.maxLog);
    }
    // 持久注入重放时会带上已有规则，避免刷新后规则丢失。
    // 超过 MAX_ROUTES 的部分会被截断，这里必须把条数回传给调用方：
    // 以前静默截断，第 N+1 条刷新后就消失且毫无提示。
    let routesDropped = 0;
    if (Array.isArray(opts.routes)) {
      const accepted = opts.routes.filter((rule) => rule && typeof rule === 'object');
      routesDropped = Math.max(0, accepted.length - MAX_ROUTES);
      state.routes = accepted.slice(0, MAX_ROUTES);
    }
    install();
    return {
      ok: true,
      action: 'hook',
      installed: true,
      capture_bodies: state.captureBodies,
      // 捕获超时的实际生效值：调用方据此判断「body 没抓到」是不是超时导致。
      body_timeout_ms: captureTimeoutMs(),
      // 冲突/顶替如实回报：旧实现只在内部静默覆盖，用户永远查不到
      // 「为什么 hook 装了却不生效」。
      fetch_replaced_by_page: state.fetch_replaced || undefined,
      // XHR 三个方法被页面在后装之后替换过（基线已重定，页面补丁不再被剪掉）。
      xhr_replaced_by_page: state.xhr_replaced || undefined,
      fetch_conflict: state.fetch_conflict || undefined,
      // routes 历史上是「规则条数」（数字），保留给老调用方；
      // 新调用方（daemon 持久化后重放）必须读 routes_list —— 把数字当数组存下来，
      // 刷新后原样下发会被上面的 Array.isArray(opts.routes) 跳过，规则就静默丢了。
      routes: state.routes.length,
      count: state.routes.length,
      // 本条实际生效的重放上限（与 `routes_dropped` 同级、number）。daemon 用它把
      // 「只重放前 N 条」的告警文案说准，缺字段时它按 64 回落。值来自上面的
      // `MAX_ROUTES` 单一常量，因此永远与真正的裁剪口径一致 —— 不要在 daemon 里
      // 另写一份常量（那正是 50 vs 64 那类漂移）。
      routes_cap: MAX_ROUTES,
      routes_dropped: routesDropped || undefined,
      routes_list: state.routes
    };
  }

  if (action === 'log') {
    const limit = opts.limit > 0 ? Math.floor(opts.limit) : 50;
    const needle = opts.match ? String(opts.match).toLowerCase() : '';
    let items = state.log;
    if (needle) {
      items = items.filter(function (entry) {
        return String(entry.url || '').toLowerCase().includes(needle);
      });
    }
    const sliced = items.slice(-limit);
    const out = {
      ok: true, action: 'log', installed: state.installed,
      count: items.length, items: sliced
    };
    if (opts.clear === true) state.log = [];
    return out;
  }

  if (action === 'route' || action === 'mock') {
    const rule = Object.assign({}, opts.rule || {});
    if (!rule.match && opts.url) rule.match = String(opts.url);
    if (!rule.match) return { ok: false, error: 'net route: 需要 match / url' };
    if (rule.method) rule.method = String(rule.method).toUpperCase();
    // 同 (match, method) 的规则**原位替换**，不能「先删再 push 到队尾」：
    // matchRule 按数组顺序取第一个命中，队尾意味着它从「具体规则在前」变成
    // 「排在更宽规则之后」—— 更新一条规则会静默改变生效顺序。
    const existing = state.routes.findIndex(function (item) {
      return own(item, 'match') && item.match === rule.match
        && String(item.method || '*') === String(rule.method || '*');
    });
    let replaced = false;
    if (existing >= 0) {
      state.routes[existing] = rule;
      replaced = true;
    } else {
      state.routes.push(rule);
    }
    if (!state.installed) install();
    return {
      ok: true, action: 'route', routes: state.routes.length, replaced: replaced,
      rule: rule, all: state.routes
    };
  }

  if (action === 'unroute') {
    const needle = String(opts.match || opts.url || '');
    if (!needle) return { ok: false, error: 'net unroute: 需要 match' };
    const before = state.routes.length;
    state.routes = state.routes.filter(function (item) { return item.match !== needle; });
    return {
      ok: true, action: 'unroute', removed: before - state.routes.length,
      routes: state.routes.length, all: state.routes
    };
  }

  if (action === 'routes') {
    // routes_list 与 hook 分支同名同义（真实数组），持久化逻辑只认这一个字段。
    return { ok: true, action: 'routes', count: state.routes.length, routes: state.routes, routes_list: state.routes };
  }

  if (action === 'reset' || action === 'unhook') {
    // 只还原**我们自己装的**包装：
    // 旧实现无条件 `window.fetch = state.originals.fetch`，页面在自己包装之后
    // 又把 fetch 换成别的东西时，reset 会把页面当前的实现整个抹掉 ——
    // 站点自己的拦截器、SDK 的 fetch 补丁全没了，而且没有任何提示。
    const skipped = [];
    if (state.originals) {
      const currentFetch = window.fetch;
      if (!state.fetchWrapper || currentFetch === state.fetchWrapper
          || (isOurWrapper(currentFetch) && (ownerOf(currentFetch) || {}).state === state)) {
        window.fetch = state.originals.fetch;
      } else if (currentFetch !== state.originals.fetch) {
        // 页面在我们之后换过 fetch：保留它，并如实回报「没还原」。
        skipped.push('fetch');
      }
      const proto = XMLHttpRequest.prototype;
      const restoreFn = (name, key, wrapperKey) => {
        if (!state.originals[key]) return;
        const current = proto[name];
        if (state[wrapperKey] && current === state[wrapperKey]) {
          proto[name] = state.originals[key];
        } else if (current !== state.originals[key]) {
          skipped.push('xhr.' + name);
        }
      };
      restoreFn('open', 'xhrOpen', 'xhrOpenWrapper');
      restoreFn('send', 'xhrSend', 'xhrSendWrapper');
      restoreFn('setRequestHeader', 'xhrSetHeader', 'xhrSetHeaderWrapper');
    }
    // 还原被接管的访问器，避免 hook 关掉后页面还留着我们的 getter。
    if (state.xhrGuarded && state.xhrNative) {
      for (const item of state.xhrRestore || []) {
        try { Object.defineProperty(XMLHttpRequest.prototype, item.name, item.descriptor); } catch (_) {}
      }
      XMLHttpRequest.prototype.getAllResponseHeaders = state.xhrNative.allHeaders;
      XMLHttpRequest.prototype.getResponseHeader = state.xhrNative.oneHeader;
      state.xhrGuarded = false;
      state.xhrNative = null;
      state.xhrRestore = null;
    }
    const cleared = state.log.length;
    state.routes = [];
    state.log = [];
    state.installed = false;
    state.originals = null;
    state.fetchWrapper = null;
    state.xhrOpenWrapper = null;
    state.xhrSendWrapper = null;
    state.xhrSetHeaderWrapper = null;
    state.fetch_replaced = false;
    state.xhr_replaced = false;
    state.fetch_conflict = null;
    // 状态槽位本身也是痕迹：不可枚举，但仍能被 getOwnPropertyNames 列出来，
    // 所以 reset 要连它一起删掉（configurable:true 就是为了这一步）。
    // 旧版本留下的 window.__abcNetState 也一并清掉。
    try { delete window[stateKey]; } catch (_) {}
    try { delete window[LEGACY_STATE_KEY]; } catch (_) {}
    // 槽位删掉了，但闭包里的 state 对象先留着：reset 之后可能还有在飞的请求
    // 会回调到 pushLog，直接置 null 会让页面里的请求抛 TypeError。
    // stateKey 置 null 后，下一次进入 __abcNet 由 hydrate() 重建槽位。
    stateKey = null;
    return {
      ok: true,
      action: 'reset',
      cleared_log: cleared,
      cleared: true,
      // 非空表示「这些包装不是我们的 / 已被页面换掉，没有还原」。
      restore_skipped: skipped.length ? skipped : undefined
    };
  }

  return { ok: false, error: 'net: 未知 action ' + action };
}

// 实现标记：MAIN world 里页面可以抢先把 window.__abcNet 定义成
// 自己的函数，扩展侧的「已加载就跳过注入」探测就会通过 → 执行页面控制的实现。
// 扩展现在每次都注入本文件，并校验这个不可枚举标记，抢不到就明确报错。
// 不可枚举：不在 Object.keys(window) / for...in 里多留一条痕迹。
try { Object.defineProperty(__abcNet, '__abcOwn', { value: true }); } catch (_) {}

// 入口挂载（见文件头第 4 条）：可配置 —— scrub 的 delete 才能真的成功；
// 不可枚举 —— 不出现在 Object.keys(window) / for...in 里。
try {
  Object.defineProperty(window, '__abcNet', {
    value: __abcNet,
    writable: true,
    enumerable: false,
    configurable: true
  });
} catch (_) {
  // 页面上已有旧版本留下的不可配置绑定：属性描述改不动，但值是 writable 的，
  // 赋值能把实现换成本版本（仍然是不可删，这一点只能等页面重载）。
  try { window.__abcNet = __abcNet; } catch (_) {}
}
})();
