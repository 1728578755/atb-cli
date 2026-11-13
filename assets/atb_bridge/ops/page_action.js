// 单往返页面动作：定位 → 滚动 → 校验 → 执行 → 返回命中信息。
// 与 CDP 路径相比少了 2~3 次往返，且在没有 debugger 权限时仍可用。
//
// 命名约定：本文件的辅助函数一律带 `__abcAct` 前缀（`__abcActVisible` 等）。
// 与 page_find.js 的同名逻辑**不能共用同一个全局名**：两份脚本都注入同一个
// world，后注入者会静默覆盖先注入者，于是「只改了其中一份」就变成行为漂移
//。语义相同也要各留一份、各叫各的名字。
//
// ---- 入口挂载----
// 扩展侧每次调用前都重新注入本文件（background.js 的 handlePageOp），同一 world
// 里会执行多次。classic script 顶层的 const/let 会进入该 world 的全局词法环境并
// 跨脚本持久：第二次注入就是「Identifier __ABC_TEXT_INNER_LIMIT has already been
// declared」早期错误 → 页面内的 click / fill 第一次可用、第二次起整个 pageOp
// reject，直到页面导航。`let __abcActNodeCount` 同理（它还带状态，更不能跨脚本）。
//
// 所以整个文件包在 IIFE 里（与 page_net.js 同一套），入口用 defineProperty 挂：
// 不可枚举（Object.keys(window) / for...in 看不到）+ 可配置（stealth scrub 的
// delete 能成功；顶层 function 声明在 classic script 里是不可删的全局绑定）。
// 名字不变：扩展侧读 `window.__abcAct`，daemon 侧是裸标识符。
;(function () {
function __abcActVisible(el) {
  if (!el || el.nodeType !== 1) return false;
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return false;
  const style = getComputedStyle(el);
  return style.visibility !== 'hidden' && style.display !== 'none' && Number(style.opacity) !== 0;
}

/// 口令类字段判定：与 page_find.js 的 `__abcSecretField` 同语义，但**必须换名**。
/// 两份脚本注入同一个 world，同名全局会被后注入者覆盖（本文件头部的命名约定），
/// 语义相同也要各留一份、各叫各的名字。
function __abcActSecretField(el) {
  if (!el || el.nodeType !== 1) return false;
  if (String(el.type || '').toLowerCase() === 'password') return true;
  const autocomplete = String((el.getAttribute && el.getAttribute('autocomplete')) || '').toLowerCase();
  return /current-password|new-password|cc-number|cc-csc|one-time-code/.test(autocomplete);
}

/// 元素总数只量一次：`text=` 定位要在「innerText 精确但会触发布局」和
/// 「textContent 快但口径宽」之间选一个。大页面上对每个候选读 innerText
// 会反复强制整页布局，几十万节点能卡到秒级。
/// 阈值与 page_wait.js 的 TEXT_INNER_LIMIT 对齐。
const __ABC_TEXT_INNER_LIMIT = 20000;
let __abcActNodeCount = null;
function __abcActTextMode() {
  if (__abcActNodeCount === null) {
    try { __abcActNodeCount = document.getElementsByTagName('*').length; } catch (_) { __abcActNodeCount = 0; }
  }
  return __abcActNodeCount <= __ABC_TEXT_INNER_LIMIT ? 'innerText' : 'textContent';
}

// ---- shadow DOM 定位----
/// find / snapshot 现在会把 open shadow root 里的元素也返回，选择器写成
/// `"<host 文档路径> >>> <shadow 内相对路径>"`（pierce:true）。动作侧必须认这种
/// 写法，否则「找到了却点不到」，工具链等于只修了一半。
/// `>>>` 不是合法 CSS：主文档 querySelectorAll 会抛错，所以先按分隔符解析。
const __ABC_ACT_SHADOW_MAX_ROOTS = 200;

function __abcActShadowScopes() {
  const out = [document];
  const queue = [{ root: document, depth: 0 }];
  while (queue.length) {
    const item = queue.shift();
    let hosts = [];
    try { hosts = item.root.querySelectorAll('*'); } catch (_) { hosts = []; }
    for (const el of hosts) {
      let shadow = null;
      try { shadow = el.shadowRoot; } catch (_) { shadow = null; }
      if (!shadow) continue;
      if (item.depth >= 8 || out.length > __ABC_ACT_SHADOW_MAX_ROOTS) continue;
      out.push(shadow);
      queue.push({ root: shadow, depth: item.depth + 1 });
    }
  }
  return out;
}

/// `host >>> inner` 逐层进 open shadow root。任一层缺失返回 null（目标不在）。
/// closed shadow root（host.shadowRoot === null）无法从页面 JS 进入，直接返回 null。
function __abcActResolvePierce(raw) {
  const parts = String(raw).split('>>>').map((item) => item.trim()).filter(Boolean);
  if (!parts.length) return null;
  let el = document.querySelector(parts[0]);
  for (let i = 1; i < parts.length; i += 1) {
    if (!el) return null;
    let shadow = null;
    try { shadow = el.shadowRoot; } catch (_) { shadow = null; }
    if (!shadow) return null;
    el = shadow.querySelector(parts[i]);
  }
  return el;
}

function __abcResolve(target, index) {
  if (!target) return null;
  if (typeof target === 'object' && target.selector) target = target.selector;
  if (typeof target === 'object' && target.text) target = 'text=' + target.text;
  const raw = String(target);
  let nodes = [];
  if (raw.startsWith('text=')) {
    const needle = raw.slice(5).trim().toLowerCase();
    const all = [];
    // 文本匹配同样要进 shadow root：页面主体用 Web Components 时，
    // 只在主文档里搜会把「看得见的按钮」判成不存在。
    for (const scope of __abcActShadowScopes()) {
      let found = [];
      try {
        // 主文档只扫 body 子树（保持历史口径：head/title 不是可点目标）；
        // shadow root 没有 body 概念，直接扫全部。
        found = scope === document
          ? document.querySelectorAll('body *')
          : scope.querySelectorAll('*');
      } catch (_) { found = []; }
      for (const el of found) all.push(el);
    }
    const mode = __abcActTextMode();
    for (const el of all) {
      if (el.children.length > 3) continue;
      const text = mode === 'innerText' ? (el.innerText || el.textContent || '') : (el.textContent || '');
      const t = String(text).trim().toLowerCase();
      if (t && t.includes(needle)) nodes.push(el);
      if (nodes.length > 200) break;
    }
  } else if (raw.startsWith('xpath=')) {
    const snapshot = document.evaluate(raw.slice(6), document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
    for (let i = 0; i < snapshot.snapshotLength; i += 1) nodes.push(snapshot.snapshotItem(i));
  } else if (raw.indexOf('>>>') >= 0) {
    const el = __abcActResolvePierce(raw);
    if (el) nodes = [el];
  } else {
    nodes = Array.from(document.querySelectorAll(raw));
  }
  // 默认取第一个可见节点；显式 index 时按原始顺序取。
  if (Number.isInteger(index)) return nodes[index] || null;
  return nodes.find(__abcActVisible) || nodes[0] || null;
}

function __abcDispatch(el, type, init) {
  const opts = Object.assign({ bubbles: true, cancelable: true, composed: true }, init || {});
  let event;
  if (type.startsWith('pointer') && typeof PointerEvent === 'function') {
    event = new PointerEvent(type, opts);
  } else if (type.startsWith('mouse') && typeof MouseEvent === 'function') {
    event = new MouseEvent(type, opts);
  } else {
    event = new Event(type, opts);
  }
  return el.dispatchEvent(event);
}

function __abcSetNativeValue(el, value) {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
    : (el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype);
  const setter = Object.getOwnPropertyDescriptor(proto, 'value')?.set;
  if (setter) setter.call(el, value);
  else el.value = value;
}

function __abcAct(op, target, params) {
  params = params || {};
  const index = params.index;
  // 目标解析要在**本函数内**包住：非法 selector / xpath 会抛 DOMException，
  // 冒到通道层后调用方只看到一句 "pageOp failed"，分不清「选择器写错了」和
  // 「注入失败了」。page_read.js / page_wait.js 已有 `invalid selector:` 这层保护。
  let el = null;
  try {
    el = __abcResolve(target, index);
  } catch (e) {
    const raw = (target && typeof target === 'object')
      ? String(target.selector || (target.text ? ('text=' + target.text) : ''))
      : String(target || '');
    const kind = raw.indexOf('xpath=') === 0 ? 'xpath' : 'selector';
    return {
      ok: false,
      op,
      target: raw,
      error: 'invalid ' + kind + ': ' + raw + ' (' + ((e && e.message) || e) + ')',
      error_code: 'invalid_' + kind
    };
  }

  if (op === 'scroll' && !el) {
    const dy = Number(params.dy || 0);
    const dx = Number(params.dx || 0);
    const to = params.to;
    if (to === 'top') window.scrollTo({ top: 0, behavior: 'instant' });
    // document_start 阶段 / XML 文档里 document.body 可能还不存在，退回 documentElement。
    else if (to === 'bottom') {
      const root = document.body || document.documentElement;
      window.scrollTo({ top: root ? root.scrollHeight : 0, behavior: 'instant' });
    }
    else window.scrollBy({ top: dy, left: dx, behavior: 'instant' });
    return { ok: true, op, target: null, scrollY: Math.round(window.scrollY), scrollX: Math.round(window.scrollX) };
  }

  if (!el) return { ok: false, op, error: 'target not found: ' + String(target) };

  el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const rect = el.getBoundingClientRect();
  // 0×0 的元素没有可命中的盒；elementFromPoint 只对「视口内且确实有盒」的坐标
  // 才有意义。这两种情况下它都返回 null，旧实现据此算出 matches:false +
  // covered_by:null ⇒ 一条 warning 都没有，而 el.click() 照发并报 ok:true，
  // 与「中心点被遮住必须报出来」的设计正好相反。
  const zeroSize = rect.width <= 0 || rect.height <= 0;
  const cx = Math.round(rect.x + rect.width / 2);
  const cy = Math.round(rect.y + rect.height / 2);
  const hitEl = zeroSize ? null : document.elementFromPoint(cx, cy);
  const hitMissReason = zeroSize ? 'zero_size' : (hitEl ? null : 'center_out_of_view');
  const hit = {
    tag: hitEl ? hitEl.tagName.toLowerCase() : null,
    matches: !!hitEl && (hitEl === el || el.contains(hitEl) || hitEl.contains(el)),
    covered_by: hitEl && hitEl !== el && !el.contains(hitEl) ? (hitEl.tagName.toLowerCase() + (hitEl.id ? '#' + hitEl.id : '')) : null,
    // 无命中点的原因要机器可读：zero_size / center_out_of_view。
    reason: hitMissReason
  };

  const rectInfo = { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.width), h: Math.round(rect.height) };
  const base = { op, tag: el.tagName.toLowerCase(), text: (el.innerText || '').trim().slice(0, 120), rect: rectInfo, hit };
  // 命中点被别的元素盖住时，真实鼠标点到的是遮罩层（cookie 弹窗 / 登录浮层 /
  // 全屏 click-catcher），而合成事件仍然直接派发到目标本身 —— 于是「页面上
  // 点不到」和「命令返回 ok」并存。以前这个信息只藏在 hit.matches 里，
  // daemon 与 CLI 都不消费，等于假成功。
  // 现在显式标 blocked / covered_by，由 daemon 提升为顶层 warnings；动作照旧派发
  // （不回退行为，避免调用方看到失败后重试造成二次点击）。
  // 中心点取不到（zero_size / center_out_of_view）同样算「真实鼠标点不到」，
  // 只是把可解释原因放进了 covered_by。
  const blockedBy = (!hit.matches && (hit.covered_by || hitMissReason)) || null;
  if (blockedBy && params.force !== true) {
    base.blocked = true;
    base.covered_by = blockedBy;
    base.hint = blockedBy === 'zero_size'
      ? '目标尺寸为 0（不可见）：真实鼠标点不到它。动作已派发到目标；传 force:true 可跳过此提示。'
      : blockedBy === 'center_out_of_view'
        ? '目标中心点不在视口内（elementFromPoint 返回 null）：真实鼠标点不到它。动作已派发到目标；传 force:true 可跳过此提示。'
        : '目标中心点被 ' + blockedBy + ' 遮住：真实鼠标会点到遮罩上。动作已派发到目标；传 force:true 可跳过此提示。';
  }
  const pointerInit = { clientX: cx, clientY: cy, pointerType: 'mouse', button: 0, buttons: 0 };

  try {
    if (op === 'hover') {
      __abcDispatch(el, 'pointerover', pointerInit);
      __abcDispatch(el, 'pointerenter', pointerInit);
      __abcDispatch(el, 'mouseover', pointerInit);
      __abcDispatch(el, 'mouseenter', pointerInit);
      __abcDispatch(el, 'pointermove', pointerInit);
      __abcDispatch(el, 'mousemove', pointerInit);
      return Object.assign(base, { ok: true });
    }

    if (op === 'click') {
      __abcDispatch(el, 'pointerover', pointerInit);
      __abcDispatch(el, 'mouseover', pointerInit);
      __abcDispatch(el, 'pointerdown', Object.assign({}, pointerInit, { buttons: 1 }));
      __abcDispatch(el, 'mousedown', Object.assign({}, pointerInit, { buttons: 1 }));
      if (typeof el.focus === 'function') { try { el.focus({ preventScroll: true }); } catch (_) {} }
      __abcDispatch(el, 'pointerup', Object.assign({}, pointerInit, { buttons: 0 }));
      __abcDispatch(el, 'mouseup', Object.assign({}, pointerInit, { buttons: 0 }));
      // 只让浏览器原生派发 click（el.click()）。这里再手动 dispatch 一次 click
      // 会让页面收到两次点击，提交类按钮会被重复触发。
      if (typeof el.click === 'function') {
        el.click();
      } else {
        __abcDispatch(el, 'click', Object.assign({}, pointerInit, { buttons: 0, detail: 1 }));
      }
      return Object.assign(base, { ok: true });
    }

    if (op === 'focus') {
      if (typeof el.focus === 'function') el.focus({ preventScroll: true });
      return Object.assign(base, { ok: true, active: document.activeElement === el });
    }

    if (op === 'select') {
      if (el.tagName.toLowerCase() !== 'select') return Object.assign(base, { ok: false, error: 'not a <select>' });
      const want = params.value != null ? String(params.value) : (params.label != null ? String(params.label) : '');
      let chosen = -1;
      for (let i = 0; i < el.options.length; i += 1) {
        const opt = el.options[i];
        if (String(opt.value) === want || (opt.text || '').trim() === want) { chosen = i; break; }
      }
      if (chosen < 0) {
        return Object.assign(base, {
          ok: false,
          error: 'option not found: ' + want,
          options: Array.from(el.options).slice(0, 40).map(o => ({ value: o.value, text: (o.text || '').trim() }))
        });
      }
      el.selectedIndex = chosen;
      __abcDispatch(el, 'input', {});
      __abcDispatch(el, 'change', {});
      return Object.assign(base, { ok: true, value: el.value });
    }

    if (op === 'check' || op === 'uncheck') {
      const want = op === 'check';
      // 必须走**原生 click**，不能直接 `el.checked = want`：
      // React 16+ 给 input 装了 value tracker，直接赋值会把 tracker 的 lastChecked
      // 一起同步，随后 dispatch 的 input/change 被判成「值没变」而丢弃；而且
      // React 的 onChange 对 checkbox/radio 挂的是 **click** 事件（ChangeEventPlugin
      // 的 shouldUseClickEvent），根本收不到我们手派的 change。结果：UI 勾上了、
      // 组件 state 没变，提交时表单值仍是旧的，命令却报 ok:true。
      // el.click() 走完整原生动作：翻转 checked → 派发 click → React 的 onChange 触发。
      let before = null;
      try { before = el.checked; } catch (_) {}
      if (typeof before !== 'boolean') {
        return Object.assign(base, { ok: false, error: 'not a checkbox/radio: ' + el.tagName.toLowerCase() });
      }
      if (before === want) {
        return Object.assign(base, { ok: true, checked: before, unchanged: true });
      }
      if (el.disabled) {
        return Object.assign(base, { ok: false, error: 'element is disabled', checked: before });
      }
      if (typeof el.click === 'function') {
        el.click();
      } else {
        const descriptor = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'checked');
        if (descriptor && typeof descriptor.set === 'function') descriptor.set.call(el, want);
        else el.checked = want;
        __abcDispatch(el, 'input', {});
        __abcDispatch(el, 'change', {});
      }
      const after = el.checked;
      if (after !== want) {
        // 点了但没生效（页面在 click 里 preventDefault / 立刻回滚）：不能报成功。
        return Object.assign(base, {
          ok: false, checked: after, via: 'click',
          error: 'click did not change checked state (page cancelled it)'
        });
      }
      return Object.assign(base, { ok: true, checked: after, changed: true, via: 'click' });
    }

    if (op === 'setValue') {
      const value = params.value == null ? '' : String(params.value);
      // append 的基准必须是**写入前**的当前值：
      // 旧实现先按默认 clear 清空再读 el.value，于是 `append:true` 实际拿到空串，
      // 行为退化成覆盖，调用方以为追加成功（回包 applied:true），输入框却被清空。
      // v11：这里曾经也叫 `base`，把外层的结果对象遮蔽成字符串，后面
      // `Object.assign(base, ...)` 只是在 String 包装对象上挂属性，序列化后
      // `ok/applied` 全丢 —— fill 明明写成功了，调用方却收到「没有返回结论」。
      const before = String(el.value == null ? '' : el.value);
      if (params.clear !== false) __abcSetNativeValue(el, '');
      // 语义：append=true 时「before + value」；否则就是 value（clear 与否都不拼接）。
      const want = params.append ? before + value : value;
      __abcSetNativeValue(el, want);
      __abcDispatch(el, 'input', {});
      __abcDispatch(el, 'change', {});
      // 事后校验：readOnly 输入框、在 input 里 preventDefault / 立刻
      // 回滚的受控组件，都会让我们写进去的值当场消失，旧实现照样回
      // `ok:true, value:<我们写的值>` —— 调用方以为填上了。check 分支早就是这么做的。
      const applied = String(el.value);
      // 敏感面：口令类字段的明文不能回显到 value/expected，也不能出现在
      // 错误文案里 —— 这条返回值会经 daemon → CLI → agent 上下文 → 日志落地。
      // 只保留 applied 布尔（写没写进去）与长度（排查口径不一致时够用）。
      const secret = __abcActSecretField(el);
      const spot = (s) => (secret ? 'len:' + s.length : JSON.stringify(s).slice(0, 120));
      const state = {
        value: secret ? '•••' : applied,
        expected: secret ? '•••' : want,
        applied: applied === want
      };
      if (typeof el.checked === 'boolean') state.checked = el.checked;
      if (applied !== want) {
        return Object.assign(base, state, {
          ok: false,
          error: 'value_not_applied: expected ' + spot(want) +
            ', got ' + spot(applied) +
            (el.readOnly ? ' (element is readOnly)' : '')
        });
      }
      return Object.assign(base, state, { ok: true });
    }

    if (op === 'scroll') {
      el.scrollIntoView({ block: params.block || 'center', inline: 'nearest', behavior: 'instant' });
      const after = el.getBoundingClientRect();
      return Object.assign(base, { ok: true, rect: { x: Math.round(after.x), y: Math.round(after.y), w: Math.round(after.width), h: Math.round(after.height) } });
    }
  } catch (e) {
    return Object.assign(base, { ok: false, error: e && e.message ? e.message : String(e) });
  }

  return Object.assign(base, { ok: false, error: 'unsupported op: ' + op });
}

// 实现标记（与 page_net / page_challenge 同一套）：给将来放开 world 留护栏。
try {
  Object.defineProperty(__abcAct, '__abcOwn', {
    value: true, enumerable: false, configurable: true
  });
} catch (_) {}

try {
  Object.defineProperty(window, '__abcAct', {
    value: __abcAct,
    writable: true,
    enumerable: false,
    configurable: true
  });
} catch (_) {
  try { window.__abcAct = __abcAct; } catch (_) {}
}
})();
