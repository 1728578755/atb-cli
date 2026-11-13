// 页面内等待：
//   selector / text —— MutationObserver 监听 DOM 变化，命中即返回；
//   js             —— 谓词只编译一次，之后在页面里反复求值：DOM 变化由
//                     MutationObserver 立即触发，非 DOM 条件（时间、全局变量、
//                     网络回调）由定时器兜底，daemon 只注入一次。
// 后台标签页里 MutationObserver 不会被节流，这是它相对 setTimeout 轮询的关键优势；
// 定时器仅作兜底，被节流到 1s 也不影响 DOM 类条件的响应速度。
function __abcWait(opts) {
  opts = opts || {};
  const now = () => (typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now());
  const started = now();
  const timeoutMs = opts.timeoutMs > 0 ? Math.floor(opts.timeoutMs) : 10000;
  const selector = opts.selector ? String(opts.selector) : '';
  const text = opts.text ? String(opts.text) : '';
  // 谓词来源有两条：
  //   1) opts.jsFunc 已经是函数 —— daemon / 扩展侧下发**已编译**的实现，
  //      走 executeScript({func}) 不触发 eval，也就不受页面 CSP 约束；
  //   2) opts.js 是字符串 —— 只能在页面里 new AsyncFunction 编译。
  //
  // 契约 §3.2：字符串路径被 CSP 拒绝时，本文件**不假装**有回退 ——
  // 它只负责把原因如实结构化（error_code=wait_js_eval_blocked），由 daemon 侧
  // `wait_for_js_condition_with_target` 据此改走 CDP Runtime.evaluate。页面内写
  // 「已回退 CDP」是假话（本文件没有任何 CDP 通道），别再加这类文案。
  const jsFunc = typeof opts.jsFunc === 'function' ? opts.jsFunc : null;
  const jsCode = !jsFunc && opts.js ? String(opts.js) : '';
  const visible = opts.visible === true;
  const minIntervalMs = opts.minIntervalMs >= 0 ? Math.floor(opts.minIntervalMs) : 50;
  // 两种「页面内观察器」模式的静默窗口（毫秒）。0 表示未启用；
  // daemon 已把缺省值（ATB_WAIT_DOM_STABLE_MS / ATB_SCREENSHOT_NETWORK_IDLE_MS）
  // 换算成具体毫秒，这里不再做二次默认。
  const domStableMs = opts.domStableMs > 0 ? Math.floor(opts.domStableMs) : 0;
  const networkIdleMs = opts.networkIdleMs > 0 ? Math.floor(opts.networkIdleMs) : 0;

  let observer = null;
  let timer = null;
  let ticker = null;
  // 节流窗口内被推迟的那次巡检：存在就说明「还有一次变更没查」。
  let trailing = null;
  let done = false;
  let checks = 0;
  let lastCheckAt = 0;

  // `--visible` 的判定口径全文件只此一份：rect 非零 + 不是 display:none /
  // visibility:hidden。text 分支复用同一个函数，避免 selector 与 text 两条路径
  // 对同一个 --visible 给出不同答案。
  function isShown(el) {
    if (!el || el.nodeType !== 1) return false;
    const rect = el.getBoundingClientRect();
    if (!(rect.width > 0 && rect.height > 0)) return false;
    let style = null;
    try {
      style = getComputedStyle(el);
    } catch (_) {}
    return !style || (style.visibility !== 'hidden' && style.display !== 'none');
  }

  function checkSelector() {
    let nodes = null;
    try {
      nodes = document.querySelectorAll(selector);
    } catch (e) {
      return { error: 'invalid selector: ' + selector };
    }
    if (!nodes || !nodes.length) return { matched: false };
    if (!visible) return { matched: true, node: nodes[0] };
    // --selector --visible 不能只看第一个匹配：页面上同名 class 的第一个
    // 元素常是隐藏的模板 / 骨架屏 / aria 占位（display:none），真正要等的那个在
    // 第 2、3 个上 —— 旧实现会一路超时。这里按文档序遍历，找第一个真正可见的；
    // 上限 200 个，避免几千条同选择器时每轮巡检都全量算盒（每次 getBoundingClientRect
    // 都可能强制布局）。
    const limit = Math.min(nodes.length, 200);
    for (let i = 0; i < limit; i += 1) {
      const el = nodes[i];
      if (isShown(el)) return { matched: true, node: el, visible: true };
    }
    return { matched: false, node: nodes[0], visible: false };
  }

  // --text 的取文本口径：
  //   innerText    —— 尊重可见性，语义最准，但读取会**强制整页布局**；
  //   textContent  —— 不触发布局，但会把 display:none 与 <script>/<style> 里的
  //                   文本也算进来，口径更宽。
  // 26 万节点的 DOM 上单次 innerText 可达秒级，而命中不了时每 minIntervalMs
  // （默认 50ms）就要巡检一次，超时时间会被布局开销整个吃光，表现成「等很久还是
  // 超时」。所以只在超大 DOM 降级为 textContent，并把实际口径回传（text_match_mode）。
  // 阈值取 20000：常见内容站点的元素总数只在数千量级；2 万以上基本都是长列表 /
  // 大表格这类「整页布局本身就很贵」的页面，此时放宽口径的代价远小于布局开销。
  const TEXT_INNER_LIMIT = 20000;
  // 逐文本节点判可见时的遍历上限：文本节点数量级远大于元素数，超大 DOM 上
  // 无上限遍历会把一次巡检拖成秒级。超出即判「没找到可见命中」——
  // 比「不判可见、直接命中隐藏文案」安全。
  const TEXT_VISIBLE_NODE_LIMIT = 40000;
  let domNodeCount = null;
  let textMatchMode = null;
  // 最近一次 text 可见性判定（仅在 --visible 时非 null），供超时结果回传。
  let textVisibleState = null;

  /// 只量一次：阈值仅用于选口径，一次 wait 期间页面规模基本稳定；
  /// 每次巡检都遍历一遍 DOM 反而固定给大页面再加一笔开销。
  function countNodes() {
    if (domNodeCount !== null) return domNodeCount;
    try {
      domNodeCount = document.getElementsByTagName('*').length;
    } catch (_) {
      domNodeCount = 0;
    }
    return domNodeCount;
  }

  /// 单个文本节点是否「可见渲染」：有非零盒 + 父元素不是 display:none /
  /// visibility:hidden（visibility 是继承属性，computed 值已经带上祖先的结果；
  /// 祖先 display:none 时盒子根本不存在，range 自然拿不到 rect）。
  function textNodeVisible(node) {
    const el = node.parentElement;
    if (!el) return false;
    let boxed = false;
    try {
      const range = document.createRange();
      range.selectNodeContents(node);
      const rects = range.getClientRects();
      for (let i = 0; i < rects.length; i += 1) {
        if (rects[i].width > 0 && rects[i].height > 0) {
          boxed = true;
          break;
        }
      }
      range.detach && range.detach();
    } catch (_) {
      return false;
    }
    if (!boxed) return false;
    try {
      const style = getComputedStyle(el);
      if (style.display === 'none' || style.visibility === 'hidden') return false;
    } catch (_) {}
    return true;
  }

  /// 逐文本节点判可见，而不是整段 innerText：超大 DOM 上 innerText 会整页布局，
  /// 而这里只为「真的含目标文本」的那几个节点求 rect。
  function textVisible(root) {
    let visited = 0;
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, null);
    let node = walker.nextNode();
    while (node) {
      visited += 1;
      if (visited > TEXT_VISIBLE_NODE_LIMIT) return false;
      const data = node.nodeValue;
      if (data && data.indexOf(text) >= 0 && textNodeVisible(node)) return true;
      node = walker.nextNode();
    }
    return false;
  }

  function checkText() {
    // XML / SVG 文档没有 body，退回 documentElement（旧实现在这里直接 matched:false）。
    const root = document.body || document.documentElement;
    if (!root) return { matched: false };
    const degraded = countNodes() > TEXT_INNER_LIMIT;
    textMatchMode = degraded ? 'textContent' : 'innerText';
    const content = degraded
      ? String(root.textContent || '')
      : String(root.innerText || root.textContent || '');
    if (content.indexOf(text) < 0) return { matched: false, visible: false };
    // --text 的 visible 以前被静默忽略（visible 只在 checkSelector 里用），
    // 隐藏文案同样判命中。现在只有可见文本才算命中。
    if (!visible) return { matched: true };
    const shown = textVisible(root);
    textVisibleState = shown;
    return { matched: shown, visible: shown };
  }

  // `--dom-stable`：等 DOM 连续 `domStableMs` 没有任何变更。
  ///
  /// MutationObserver 只负责「重置静默计时」，真正的判定用 setTimeout：
  /// 后台标签页的定时器会被 Chrome 节流，但那只让判定**偏晚**，不会把仍在
  /// 变化的页面误判成稳定（每次变更都会把旧计时器清掉）。
  function waitDomStable() {
    return new Promise(function (resolve) {
      let mutations = 0;
      let lastMutationAt = 0;
      let observer = null;
      let quietTimer = null;
      let timeoutTimer = null;
      let finished = false;

      function elapsed() {
        return Math.round((now() - started) * 10) / 10;
      }

      function cleanup() {
        if (observer) {
          try { observer.disconnect(); } catch (_) {}
          observer = null;
        }
        if (quietTimer) {
          clearTimeout(quietTimer);
          quietTimer = null;
        }
        if (timeoutTimer) {
          clearTimeout(timeoutTimer);
          timeoutTimer = null;
        }
      }

      function succeed() {
        if (finished) return;
        finished = true;
        cleanup();
        resolve({
          ok: true,
          matched: true,
          mode: 'dom-stable',
          mode_used: 'dom-stable',
          stable_ms: domStableMs,
          mutations: mutations,
          elapsed_ms: elapsed()
        });
      }

      function armQuiet() {
        if (finished) return;
        if (quietTimer) clearTimeout(quietTimer);
        quietTimer = setTimeout(succeed, domStableMs);
      }

      try {
        observer = new MutationObserver(function (records) {
          if (finished) return;
          mutations += (records && records.length) || 1;
          lastMutationAt = Math.round(now() - started);
          armQuiet();
        });
        observer.observe(document.documentElement || document, {
          childList: true,
          subtree: true,
          attributes: true,
          characterData: true
        });
      } catch (_) {
        // 观察器装不起来时仍按「从此刻起静默」判定：最坏情况是把一个不再
        // 变化的页面判为稳定，而不是永远等不到。
        observer = null;
      }

      armQuiet();
      timeoutTimer = setTimeout(function () {
        if (finished) return;
        finished = true;
        cleanup();
        resolve({
          ok: false,
          matched: false,
          mode: 'dom-stable',
          mode_used: 'dom-stable',
          reason: 'timeout',
          mutations: mutations,
          last_mutation_ms: lastMutationAt,
          elapsed_ms: elapsed()
        });
      }, timeoutMs);
    });
  }

  // `--network-idle`：页面内 hook `fetch` / `XMLHttpRequest`，统计在途请求，
  /// 归零并持续 `networkIdleMs` 视为空闲。
  ///
  /// hook 挂在 `window.__abcNetIdleHook`（不可枚举），重复注入复用同一份计数，
  /// 不会层层包装。**不 hook WebSocket / EventSource**：它们没有天然的「完成」
  /// 语义，长连接会让空闲永远判不出来；这两类流量不计入 inflight。
  function waitNetworkIdle() {
    const note = '仅统计等待开始后的 fetch/XHR 请求';
    return new Promise(function (resolve) {
      let finished = false;
      let tickTimer = null;
      let timeoutTimer = null;
      let hookInstalled = false;

      function elapsed() {
        return Math.round((now() - started) * 10) / 10;
      }

      function cleanup() {
        if (tickTimer) {
          clearTimeout(tickTimer);
          tickTimer = null;
        }
        if (timeoutTimer) {
          clearTimeout(timeoutTimer);
          timeoutTimer = null;
        }
      }

      function finish(ok, extra) {
        if (finished) return;
        finished = true;
        cleanup();
        const payload = Object.assign({
          ok: ok,
          matched: ok,
          mode: 'network-idle',
          mode_used: 'network-idle',
          note: note,
          elapsed_ms: elapsed()
        }, extra || {});
        resolve(payload);
      }

      function installHook() {
        let state = null;
        try {
          state = window.__abcNetIdleHook;
        } catch (_) {
          state = null;
        }
        if (state && typeof state.inflight === 'number') {
          hookInstalled = true;
          return state;
        }
        state = { inflight: 0, last: now(), installed: true };
        const track = function () {
          state.inflight += 1;
          state.last = now();
        };
        const done = function () {
          state.inflight = Math.max(0, state.inflight - 1);
          state.last = now();
        };
        try {
          const originalFetch = window.fetch;
          if (typeof originalFetch === 'function' && !originalFetch.__abcNetIdleWrapped) {
            const wrappedFetch = function () {
              track();
              let pending = null;
              try {
                pending = originalFetch.apply(this, arguments);
              } catch (error) {
                done();
                throw error;
              }
              if (pending && typeof pending.finally === 'function') {
                return pending.finally(done);
              }
              done();
              return pending;
            };
            try {
              Object.defineProperty(wrappedFetch, '__abcNetIdleWrapped', {
                value: true,
                configurable: true,
                enumerable: false
              });
            } catch (_) {
              wrappedFetch.__abcNetIdleWrapped = true;
            }
            window.fetch = wrappedFetch;
          }
        } catch (_) {}
        try {
          const proto = XMLHttpRequest.prototype;
          const originalSend = proto.send;
          if (typeof originalSend === 'function' && !originalSend.__abcNetIdleWrapped) {
            const wrappedSend = function () {
              track();
              let settled = false;
              const settle = function () {
                if (settled) return;
                settled = true;
                done();
              };
              try {
                this.addEventListener('loadend', settle, { once: true });
              } catch (_) {}
              try {
                return originalSend.apply(this, arguments);
              } catch (error) {
                settle();
                throw error;
              }
            };
            try {
              Object.defineProperty(wrappedSend, '__abcNetIdleWrapped', {
                value: true,
                configurable: true,
                enumerable: false
              });
            } catch (_) {
              wrappedSend.__abcNetIdleWrapped = true;
            }
            proto.send = wrappedSend;
          }
        } catch (_) {}
        try {
          Object.defineProperty(window, '__abcNetIdleHook', {
            value: state,
            configurable: true,
            enumerable: false
          });
        } catch (_) {}
        hookInstalled = true;
        return state;
      }

      const state = installHook();
      if (!state) {
        finish(false, {
          reason: 'hook_unavailable',
          error: 'network_idle_hook_unavailable',
          inflight: null,
          hook_installed: false
        });
        return;
      }

      function tick() {
        if (finished) return;
        if (state.inflight <= 0 && now() - state.last >= networkIdleMs) {
          finish(true, {
            inflight: 0,
            idle_ms: networkIdleMs,
            hook_installed: hookInstalled
          });
          return;
        }
        tickTimer = setTimeout(tick, 50);
      }

      tickTimer = setTimeout(tick, Math.min(50, networkIdleMs));
      timeoutTimer = setTimeout(function () {
        finish(false, {
          reason: 'timeout',
          inflight: state.inflight,
          hook_installed: hookInstalled
        });
      }, timeoutMs);
    });
  }

  function evaluate() {
    if (selector) return checkSelector();
    if (text) return checkText();
    return { matched: false, error: 'wait: 需要 selector / text / js / domStableMs / networkIdleMs' };
  }

  // 谓词编译：三种写法都要支持——语句级 return、裸表达式、多语句 + await。
  // 编译只做一次。**在页面里 new AsyncFunction 等于 eval**：页面 CSP 禁
  // unsafe-eval 时会同步抛错。这里不掩饰这一点，只把失败如实交回上层。
  function compilePredicate(code) {
    const trimmed = String(code || '').trim();
    if (!trimmed) throw new Error('wait: js 为空');
    const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
    if (/^return\b/.test(trimmed)) return new AsyncFunction(trimmed);
    try {
      return new AsyncFunction('return (' + trimmed + '\n);');
    } catch (_) {}
    return new AsyncFunction(trimmed);
  }

  /// 失败是不是「被页面 CSP 拦下」而不是「谓词本身写错」。
  ///
  /// Chrome 被 CSP 拦时抛 EvalError（"Refused to evaluate a string as JavaScript
  /// because 'unsafe-eval' is not an allowed source..."），个别版本/注入通道会退化成
  /// 普通 Error 只带同一句文案，所以两条都要认：只看 name 会漏，只看 message 会误判
  /// 用户谓词里恰好出现的同名文本（那种情况不是 CSP，交回 CDP 也救不回来）。
  function isEvalBlocked(error) {
    const name = String((error && error.name) || '');
    if (name === 'EvalError') return true;
    const detail = String((error && error.message) || error || '');
    return /unsafe-eval|Refused to evaluate|Content Security Policy/i.test(detail);
  }

  // 模式优先级：js > selector > text > dom-stable > network-idle。
  /// 新模式只在没有任何旧条件时启用；同时给多个时旧条件优先，结果里的
  /// `mode_used` 说明本次实际用的是哪一种。
  const hasPredicateRequest = typeof jsFunc === 'function' || !!jsCode;
  if (!hasPredicateRequest && !selector && !text) {
    if (domStableMs > 0) return waitDomStable();
    if (networkIdleMs > 0) return waitNetworkIdle();
  }

  let predicate = jsFunc;
  if (!predicate && jsCode) {
    let compileError = null;
    try {
      predicate = compilePredicate(jsCode);
    } catch (e) {
      compileError = e;
    }
    if (compileError) {
      const detail = (compileError && compileError.message) || String(compileError);
      // 契约 §3.2：CSP 拦截必须给独立错误码，daemon 据此改走 CDP Runtime.evaluate
      // 轮询；其余编译失败（谓词语法错、空谓词）换通道也救不回来，保持原码。
      if (isEvalBlocked(compileError)) {
        return {
          ok: false,
          matched: false,
          mode: 'js-observer',
          mode_used: 'js',
          error: 'wait_js_eval_blocked: ' + detail,
          error_code: 'wait_js_eval_blocked',
          hint: '页面 CSP 禁止 unsafe-eval：字符串谓词无法在页面内编译。'
            + '由调用方改走 CDP Runtime.evaluate（daemon 侧按 error_code 自动切换），'
            + '或把谓词编译成函数后再下发（opts.jsFunc）',
          polls: 0,
          checks: 0,
          elapsed_ms: 0
        };
      }
      return {
        ok: false,
        matched: false,
        mode: 'js-observer',
        mode_used: 'js',
        error: 'js_predicate_compile_failed: ' + detail,
        error_code: 'js_predicate_compile_failed',
        // 语法错误：与 CSP 无关，换执行通道也一样失败。
        hint: '字符串谓词在页面内编译失败（谓词本身语法错误）：修正谓词，'
          + '或改用 --selector/--text 条件',
        polls: 0,
        checks: 0,
        elapsed_ms: 0
      };
    }
  }
  const hasPredicate = typeof predicate === 'function';
  const mode = hasPredicate ? 'js-observer' : 'observer';

  return new Promise((resolve) => {
    let lastError = null;
    let lastValue = null;
    let running = false;
    // `wait --js` 超时时回传最近 5 次求值（值或错误）。元素节点用 describeValue
    // 摘要，其它值统一做 JSON 安全摘要 —— 函数 / Symbol / 循环引用直接序列化会让
    // 扩展通道丢包或抛错。
    const RECENT_LIMIT = 5;
    const recent = [];
    const modeUsed = hasPredicate ? 'js' : (selector ? 'selector' : (text ? 'text' : 'none'));

    function cleanup() {
      if (observer) {
        try { observer.disconnect(); } catch (_) {}
        observer = null;
      }
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
      if (ticker) {
        clearInterval(ticker);
        ticker = null;
      }
      if (trailing) {
        clearTimeout(trailing);
        trailing = null;
      }
      if (hasPredicate) {
        try { document.removeEventListener('visibilitychange', onMutation, true); } catch (_) {}
      }
    }

    function finish(matched, extra) {
      if (done) return;
      done = true;
      cleanup();
      const payload = Object.assign({
        ok: matched,
        matched: matched,
        polls: checks,
        checks: checks,
        mode: mode,
        mode_used: modeUsed,
        elapsed_ms: Math.round((now() - started) * 10) / 10
      }, extra || {});
      // 仅 --text 条件会填：本次命中判定实际用的是 innerText 还是（超大 DOM 降级后的）
      // textContent。只新增字段，mode / checks 等既有字段保持原样。
      if (textMatchMode) payload.text_match_mode = textMatchMode;
      resolve(payload);
    }

    function describeValue(value) {
      try {
        if (value && value.nodeType === 1) {
          const rect = value.getBoundingClientRect();
          return {
            tag: String(value.tagName || '').toLowerCase(),
            id: value.id || undefined,
            text: String(value.textContent || '').trim().slice(0, 200),
            rect: {
              x: Math.round(rect.x),
              y: Math.round(rect.y),
              w: Math.round(rect.width),
              h: Math.round(rect.height)
            }
          };
        }
      } catch (_) {}
      return value;
    }

    /// JSON 安全摘要：只给调用方看「最近几次求值大概是什么」，
    /// 不承诺完整还原（函数 / 节点 / 循环引用都只留可读标记）。
    function summarizeValue(value, depth) {
      const level = depth || 0;
      try {
        if (value === null) return null;
        const kind = typeof value;
        if (kind === 'undefined') return '[undefined]';
        if (kind === 'string') {
          return value.length > 200 ? value.slice(0, 200) + '…' : value;
        }
        if (kind === 'boolean') return value;
        if (kind === 'number') return isFinite(value) ? value : String(value);
        if (kind === 'bigint') return String(value) + 'n';
        if (kind === 'symbol') return String(value);
        if (kind === 'function') return '[Function ' + (value.name || 'anonymous') + ']';
        if (value.nodeType === 1) return describeValue(value);
        if (value.nodeType) return '[Node ' + (value.nodeName || value.nodeType) + ']';
        if (level >= 2) return Array.isArray(value) ? '[Array]' : '[Object]';
        if (Array.isArray(value)) {
          const items = [];
          const limit = Math.min(value.length, 10);
          for (let i = 0; i < limit; i += 1) {
            items.push(summarizeValue(value[i], level + 1));
          }
          if (value.length > limit) items.push('…' + (value.length - limit) + ' more');
          return items;
        }
        const out = {};
        const keys = Object.keys(value).slice(0, 10);
        for (let i = 0; i < keys.length; i += 1) {
          out[keys[i]] = summarizeValue(value[keys[i]], level + 1);
        }
        if (Object.keys(value).length > keys.length) out.__truncated__ = true;
        return out;
      } catch (_) {
        return '[Unserializable]';
      }
    }

    function recordRecent(entry) {
      entry.at_ms = Math.round((now() - started) * 10) / 10;
      recent.push(entry);
      if (recent.length > RECENT_LIMIT) recent.splice(0, recent.length - RECENT_LIMIT);
    }

    function runPredicate() {
      if (done || running) return;
      running = true;
      lastCheckAt = now();
      checks += 1;
      let pending;
      try {
        pending = predicate();
      } catch (e) {
        running = false;
        lastError = (e && e.message) || String(e);
        recordRecent({ error: lastError });
        return;
      }
      Promise.resolve(pending).then(function (value) {
        running = false;
        lastValue = value;
        lastError = null;
        recordRecent({ value: summarizeValue(value) });
        if (value) finish(true, { value: describeValue(value) });
      }, function (err) {
        running = false;
        lastError = (err && err.message) || String(err);
        recordRecent({ error: lastError });
      });
    }

    // 观察器可能在一次 DOM 变更里连发几十个回调：用微任务合并同一批变更。
    // 这里不能用 setTimeout 做节流——后台标签页会把它节流到 1s，
    // 明明 DOM 已经变了却要晚一秒才求值。
    let pendingMicro = false;
    function onMutation() {
      if (done || running) return;
      const gap = minIntervalMs - (now() - lastCheckAt);
      if (gap <= 0) {
        runPredicate();
        return;
      }
      if (pendingMicro) return;
      pendingMicro = true;
      queueMicrotask(function () {
        pendingMicro = false;
        if (done) return;
        runPredicate();
      });
    }

    // 非 JS 条件（selector / text）的变更入口。节流窗口内的变更不能直接丢
    // 旧实现 `tick(false)` 在窗口内直接 return，恰好落在窗口里的那次
    // 「目标出现」就永久漏报，只能等超时。现在窗口内的变更加一个 trailing，
    // 窗口结束（minIntervalMs 后）必须再查一次。
    function onDomChange() {
      if (done) return;
      const gap = minIntervalMs - (now() - lastCheckAt);
      if (gap <= 0) {
        tick(true);
        return;
      }
      if (trailing) return;
      trailing = setTimeout(function () {
        trailing = null;
        tick(true);
      }, gap);
    }

    function tick(force) {
      if (done) return;
      const current = now();
      if (!force && minIntervalMs > 0 && current - lastCheckAt < minIntervalMs) return;
      lastCheckAt = current;
      checks += 1;
      const result = evaluate();
      if (result.error) return finish(false, { error: result.error });
      if (!result.matched) return;
      const extra = { value: true };
      // 把可见性判定结果一起交回去：调用方要能区分「命中了但不可见」（不会进入
      // matched 分支）与「压根没命中」，否则 --text --visible 又变成一个黑盒。
      if (result.visible !== undefined) extra.visible = result.visible;
      if (result.node) {
        try {
          const rect = result.node.getBoundingClientRect();
          extra.value = {
            tag: String(result.node.tagName || '').toLowerCase(),
            rect: {
              x: Math.round(rect.x),
              y: Math.round(rect.y),
              w: Math.round(rect.width),
              h: Math.round(rect.height)
            }
          };
        } catch (_) {}
      }
      return finish(true, extra);
    }

    if (hasPredicate) {
      runPredicate();
    } else {
      tick(true);
    }
    if (done) return;

    const root = document.documentElement || document;
    try {
      observer = new MutationObserver(hasPredicate ? onMutation : onDomChange);
      observer.observe(root, {
        childList: true,
        subtree: true,
        attributes: true,
        characterData: true
      });
    } catch (_) {
      // 极端情况下（document 尚未就绪）退回定时检查，仍比 daemon 轮询便宜。
      observer = null;
    }

    if (hasPredicate) {
      // 非 DOM 条件兜底：定时器 + 标签页可见性变化。
      ticker = setInterval(runPredicate, Math.max(minIntervalMs, 16));
      try {
        document.addEventListener('visibilitychange', onMutation, true);
      } catch (_) {}
    } else {
      // 轻量 ticker 兜底：MutationObserver 可能漏事件（文档未就绪时
      // observe 失败、被页面脚本 disconnect、或被嵌入页面的沙箱限制），
      // 100ms 起步的巡检既不像 minInterval(50ms) 那样长等待里持续耗电，
      // 又能把漏掉的 trailing 补回来。tick(false) 内部仍按 minIntervalMs 自节流。
      ticker = setInterval(function () { tick(false); }, Math.max(minIntervalMs, 100));
    }

    // 兜底定时器：只用于「一直不满足」时按时返回，不参与命中判断。
    timer = setTimeout(function () {
      const payload = { error: lastError || 'timeout', reason: 'timeout', value: lastValue };
      // 把最近 5 次求值（值或错误）一起交回；现有 `value` 字段保持旧语义。
      if (recent.length) payload.last_values = recent.slice();
      // 超时也要能回答 --text --visible 的可见性结论：文本存在但不可见时
      // matched 一直是 false，若不回传就看不出「命中了但被隐藏」。
      if (textVisibleState !== null) payload.visible = textVisibleState;
      finish(false, payload);
    }, timeoutMs);
  });
}
