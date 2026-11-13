// DOM 版快照：CDP 不可用（其他扩展 iframe 挡住 debugger）时，用一次页面注入
// 生成与 AX 快照同形的可操作元素列表，每个元素带可直接复用的 CSS 选择器。
// @e 引用在 daemon 侧会映射到这个选择器，后续动作走页面内执行，不再依赖 CDP。
//
// ---- shadow DOM / iframe 边界----
// 与 page_find.js 同一套口径：open shadow root 一起遍历（opts.shadowDom = auto|off），
// 元素带 scope:'shadow' + pierce 选择器（"<host 路径> >>> <shadow 内相对路径>"），
// page_action.js 的 __abcResolve 认这种写法。closed shadow root 页面 JS 不可达，
// 只能靠 CDP pierce；iframe 的内容同样扫不到，两者都在回包里显式标注，
// 不再用 ok:true + 空 items 假装「页面上没有可操作元素」。
//
// 选择器必须**唯一**：长度上限 + 只在撞上 id 时锚定，得到的是
// 相对路径，在重复结构（卡片/列表/表格行）里指向第一个同构位置 —— @e 引用于是
// 静默点错元素且返回 ok:true。这里逐层验唯一，不唯一就继续往上加祖先。
// 唯一性只在**元素所属的 root** 内判定（shadow 里的同名路径在主文档可能重复）。
//
// 已知边界（协议级限制，本轮不修）：CDP 引擎只给可操作角色发 `@e`
// （ref_scheme=operable-only），本脚本给所有「有名字」的结构节点发 `@e`
// （ref_scheme=all-named），非可操作节点（heading / img / label / generic 角色等）
// 也会占号。两个引擎的 `@eN` **不保证指向同一元素**，跨模式复用 `@e` 前必须重新
// snapshot；daemon 用回包里的 ref_scheme 区分口径。

// ---- 入口挂载----
// 扩展侧每次调用前都重新注入本文件（background.js 的 handlePageOp），同一 world
// 里会执行多次。classic script 顶层的 const/let 会跨脚本持久，第二次注入直接
// 「Identifier ... has already been declared」早期错误 → 整个 pageOp reject
// （snapshot 第一次可用、第二次起必坏），直到页面导航才恢复。
//
// 因此整个文件包在 IIFE 里（与 page_net.js 同一套），入口用 defineProperty 挂：
// 不可枚举（Object.keys 看不到）+ 可配置（stealth scrub 的 delete 能成功）。
// 名字不变：daemon 用裸标识符 `return __abcSnapshot({...})` 解析全局属性。
;(function () {
const __ABC_SNAP_SHADOW_MAX_ROOTS = 200;
const __ABC_SNAP_SHADOW_MAX_DEPTH = 8;

function __abcSnapShadowMode(opts) {
  const raw = String((opts && opts.shadowDom) || '').toLowerCase();
  return raw === 'off' ? 'off' : 'auto';
}

function __abcSnapRootOf(el) {
  try {
    const root = el && el.getRootNode ? el.getRootNode() : null;
    return root && (root.nodeType === 9 || root.nodeType === 11) ? root : document;
  } catch (_) {
    return document;
  }
}

/// 收集 open shadow root（含嵌套），并统计被跳过的数量。
function __abcSnapShadowRoots(mode) {
  const out = [];
  let skipped = 0;
  if (mode === 'off') {
    try {
      for (const el of document.querySelectorAll('*')) {
        if (el.shadowRoot) skipped += 1;
      }
    } catch (_) {}
    return { roots: out, skipped: skipped };
  }
  const queue = [{ root: document, depth: 0 }];
  while (queue.length) {
    const item = queue.shift();
    let hosts = [];
    try { hosts = item.root.querySelectorAll('*'); } catch (_) { hosts = []; }
    for (const el of hosts) {
      let shadow = null;
      try { shadow = el.shadowRoot; } catch (_) { shadow = null; }
      if (!shadow) continue;
      if (item.depth >= __ABC_SNAP_SHADOW_MAX_DEPTH || out.length >= __ABC_SNAP_SHADOW_MAX_ROOTS) {
        skipped += 1;
        continue;
      }
      out.push(shadow);
      queue.push({ root: shadow, depth: item.depth + 1 });
    }
  }
  return { roots: out, skipped: skipped };
}

function __abcSnapFrameCount() {
  try { return document.querySelectorAll('iframe,frame').length; } catch (_) { return 0; }
}

/// 在指定 root（Document / ShadowRoot）内生成唯一路径。
function __abcSnapCssPathIn(el, root) {
  if (!el || el.nodeType !== 1) return '';
  if (!root || typeof root.querySelectorAll !== 'function') root = document;
  const unique = (path) => {
    try { return root.querySelectorAll(path).length === 1; } catch (_) { return false; }
  };
  if (el.id) {
    const byId = '#' + CSS.escape(el.id);
    if (unique(byId)) return byId;
  }
  const parts = [];
  let node = el;
  let guard = 0;
  while (node && node.nodeType === 1 && guard < 64) {
    guard += 1;
    let part = node.tagName.toLowerCase();
    // 类名只保留在目标自身这层：祖先层用 nth-of-type 定位，不会因 class 变动失效。
    if (parts.length === 0) {
      const cls = (typeof node.className === 'string' ? node.className : '')
        .trim().split(/\s+/).filter(Boolean).slice(0, 2);
      if (cls.length) part += '.' + cls.map(c => CSS.escape(c)).join('.');
    }
    const parent = node.parentElement;
    if (parent) {
      const sameTag = Array.from(parent.children).filter(c => c.tagName === node.tagName);
      if (sameTag.length > 1) part += ':nth-of-type(' + (sameTag.indexOf(node) + 1) + ')';
    }
    parts.unshift(part);
    const path = parts.join(' > ');
    if (unique(path)) return path;
    node = parent;
    if (node && node.nodeType === 1 && node.id) {
      const anchored = '#' + CSS.escape(node.id) + ' > ' + parts.join(' > ');
      if (unique(anchored)) return anchored;
    }
  }
  return parts.join(' > ');
}

/// 最近可 pierce 锚点 + 相对路径。主文档元素退化为单段路径（与旧行为一致）。
function __abcSnapPath(el) {
  if (!el || el.nodeType !== 1) return { selector: '', scope: 'main' };
  const chain = [];
  let node = el;
  let guard = 0;
  while (node && node.nodeType === 1 && guard < __ABC_SNAP_SHADOW_MAX_DEPTH) {
    guard += 1;
    const root = __abcSnapRootOf(node);
    chain.unshift(__abcSnapCssPathIn(node, root));
    if (!root || root.nodeType !== 11 || !root.host) break;
    node = root.host;
  }
  if (chain.length <= 1) return { selector: chain[0] || '', scope: 'main' };
  return {
    selector: chain.join(' >>> '),
    host_selector: chain[0],
    shadow_path: chain.slice(1),
    scope: 'shadow',
    pierce: true
  };
}

/// 兼容旧名字（测试/其它脚本按字符串路径使用）。
function __abcSnapCssPath(el) {
  return __abcSnapPath(el).selector;
}

/// 口令类字段的值不返回明文：快照会进 CLI 输出 / agent 上下文。
function __abcSnapSecretField(el) {
  if (!el || el.nodeType !== 1) return false;
  const type = String(el.type || '').toLowerCase();
  if (type === 'password') return true;
  const autocomplete = String(el.getAttribute && el.getAttribute('autocomplete') || '').toLowerCase();
  return /current-password|new-password|cc-number|cc-csc|one-time-code/.test(autocomplete);
}

// 派生文本掩码：口径与 page_find.js 的 __abcMaskDerived
/// **逐字对齐**，两份文件语义相同但各留一份、各叫各的名字（见 page_action.js
/// 头部的命名约定）。快照的 name 是**无障碍名**（可能来自 placeholder / label
/// 正文），secret 字段上它常常就是半个明文（"请输入密码"、"卡号后四位"），必须一起掩码。
///
/// 空值统一返回 null 而不是 ''：调用方据此区分「字段没写」与「写了但被掩码」，
/// 这也是 find 侧的口径（`raw || null` / `raw ? '•••' : null`）。
/// 只掩码**派生文本**，不碰 type/name 这类结构标识 —— agent 靠它们定位字段。
function __abcSnapMaskDerived(el, value) {
  const raw = value === undefined || value === null ? '' : String(value);
  if (!__abcSnapSecretField(el)) return raw || null;
  return raw ? '•••' : null;
}

function __abcSnapRole(el) {
  const explicit = (el.getAttribute('role') || '').toLowerCase();
  if (explicit) return explicit;
  const tag = el.tagName.toLowerCase();
  if (tag === 'a') return el.hasAttribute('href') ? 'link' : 'generic';
  if (tag === 'button') return 'button';
  if (tag === 'select') return 'combobox';
  if (tag === 'textarea') return 'textbox';
  if (tag === 'img') return 'img';
  if (/^h[1-6]$/.test(tag)) return 'heading';
  if (tag === 'input') {
    const type = (el.type || 'text').toLowerCase();
    if (type === 'checkbox') return 'checkbox';
    if (type === 'radio') return 'radio';
    if (type === 'submit' || type === 'button' || type === 'reset') return 'button';
    if (type === 'search') return 'searchbox';
    if (type === 'file') return 'file';
    return 'textbox';
  }
  if (tag === 'label') return 'label';
  return 'generic';
}

function __abcSnapName(el) {
  const attr = (name) => (el.getAttribute(name) || '').trim();
  const aria = attr('aria-label');
  if (aria) return __abcSnapMaskDerived(el, aria);
  const tag = el.tagName.toLowerCase();
  if (tag === 'input' || tag === 'textarea' || tag === 'select') {
    const placeholder = attr('placeholder');
    if (placeholder) return __abcSnapMaskDerived(el, placeholder);
    if (el.type === 'submit' || el.type === 'button') return el.value || '';
    // label[for] 的 id 必须先判空再拼：`String(undefined)` 会得到 "undefined" 这个
    // 看似合法的 id，把 label 匹配到错误元素上。
    // 属性值选择器里的引号/反斜杠必须转义（CSS.escape 转的是**标识符**，
    // 直接拼进 "..."/'...' 会让含空格/引号的 id 变成非法选择器并抛错）。
    let labelled = null;
    const id = el.id === undefined || el.id === null ? '' : String(el.id);
    if (id) {
      const escaped = id.replace(/["\\]/g, '\\$&');
      try { labelled = el.ownerDocument.querySelector('label[for="' + escaped + '"]'); } catch (_) { labelled = null; }
    }
    const labelText = labelled ? (labelled.innerText || '').trim() : '';
    if (labelText) return __abcSnapMaskDerived(el, labelText);
    const wrapped = el.closest('label');
    if (wrapped) return __abcSnapMaskDerived(el, (wrapped.innerText || '').trim());
    return attr('name');
  }
  if (tag === 'img') return attr('alt') || attr('title');
  const text = (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ');
  return text.slice(0, 160);
}

// 可见性判断顺带把 rect 返回给调用方：回包里要用同一个 rect，以前在产出阶段
// 又调一次 getBoundingClientRect（同一元素每轮只读一次布局）。
function __abcSnapRect(el) {
  const rect = el.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return null;
  const style = getComputedStyle(el);
  if (style.visibility === 'hidden' || style.display === 'none' || Number(style.opacity) === 0) return null;
  return rect;
}

function __abcSnapshot(opts) {
  opts = opts || {};
  const maxItems = Math.max(1, Math.min(Number(opts.limit) || 400, 1000));
  // `offset`：跳过前 N 个可操作元素，从第 N+1 个开始返回（0 基，与 CLI 的 --offset 同义）。
  // 旧实现完全忽略 offset、永远从文档开头取 limit 条：daemon 侧只能本地切片，
  // offset≥1000 时页面只给前 1000 条，切完一条不剩（深分页）。
  const offset = Math.max(0, Math.floor(Number(opts.offset) || 0));
  const skipGeneric = opts.skipGeneric !== false;
  const interactiveOnly = opts.interactiveOnly === true;
  const shadowMode = __abcSnapShadowMode(opts);
  const shadow = __abcSnapShadowRoots(shadowMode);
  // XML / SVG 文档没有 body：旧实现拿到空列表后照样返回 ok:true + 0 条，
  // 调用方于是以为「页面上没有可操作元素」，实际是这份脚本根本没看到内容。
  // 结构不可用必须明确报错，而不是用一个空成功掩盖。
  if (!document.body) {
    return {
      ok: false,
      error: 'document has no body',
      error_code: 'no_document_body',
      url: location.href,
      title: document.title,
      mode: 'js',
      // JS 侧编号口径，见文件头「已知边界」。
      ref_scheme: 'all-named',
      total: 0,
      truncated: false,
      items: []
    };
  }
  // 与 Rust `is_operable_role` 对齐的 18 个角色（此前只有 11 个，
  // interactiveOnly 会漏掉 listbox / menuitem* / option / spinbutton / treeitem）。
  const OPERABLE = new Set([
    'button', 'link', 'textbox', 'searchbox', 'checkbox', 'radio', 'combobox',
    'listbox', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option',
    'slider', 'spinbutton', 'switch', 'tab', 'treeitem', 'file'
  ]);
  const items = [];
  // index 是**全页序号**：分页时也必须连续，`@e` 引用才能跨页稳定、不重号。
  let index = 0;
  let hasMore = false;
  const scopes = [document].concat(shadow.roots);
  for (const scope of scopes) {
    // document 用 body 作根（保持历史行为：不扫 head）；shadow root 直接扫全部。
    const root = scope === document ? document.body : scope;
    if (!root) continue;
    const all = root.querySelectorAll('*');
    for (const el of all) {
      const tag = el.tagName.toLowerCase();
      // E1-14：便宜的过滤（tag / role / skipGeneric / interactiveOnly / img-alt）先做，
      // 深分页时为前 offset 条省掉 CSS 选择器与回包对象的构造（name 仍要算，
      // 见下面的「占号 == 产出」约束）。
      if (tag === 'script' || tag === 'style' || tag === 'noscript' || tag === 'svg') continue;
      const role = __abcSnapRole(el);
      if (skipGeneric && role === 'generic') continue;
      if (interactiveOnly && !OPERABLE.has(role)) continue;
      if (role === 'img' && !el.getAttribute('alt')) continue;
      // 序号口径是「只有可见元素占号」，而 @e 的全页编号就靠这个计数。
      // 因此可见性判断不能延后到 offset 之后：前缀里只要有一个隐藏元素
      // （input[type=hidden]、折叠菜单里的 a[href] 都是），盲计数就会让后续
      // @e 整体偏移、跨页复用点错元素。这里省掉的是产出开销（selector/对象构造），
      // 不是序号正确性。
      const rect = __abcSnapRect(el);
      if (!rect) continue;
      // E1-14b：序号只给「会产出」的元素 —— 必须在 index += 1 之前先过 name 判断，
      // 否则「可见但无 name 且非 input」的元素占号却不进 items，index 与 items 数量
      // 脱钩，next_offset = offset + items.length 就会小于真实覆盖位置，翻页会重复
      // 返回同一个 @e（指向同一元素，但调用方会误以为还有新内容，深分页可能空转）。
      // input 例外：即使没有 name 也要占号并产出（保持历史行为，文件型/隐藏控件依赖它）。
      const secret = __abcSnapSecretField(el);
      const name = __abcSnapName(el);
      if (!name && tag !== 'input') continue;
      index += 1;
      // 先按 offset 跳过（序号照常累加），再判断本页是否已满。
      if (index <= offset) continue;
      // 本页已满但仍有下一个命中项：说明确实还有下一批（不是「刚好取完」）。
      if (items.length >= maxItems) { hasMore = true; break; }
      const path = __abcSnapPath(el);
      const item = {
        ref: '@e' + index,
        role,
        name: String(name || '').slice(0, 160),
        tag,
        selector: path.selector,
        scope: path.scope,
        rect: {
          x: Math.round(rect.x), y: Math.round(rect.y),
          w: Math.round(rect.width), h: Math.round(rect.height)
        }
      };
      if (path.pierce) {
        item.pierce = true;
        item.host_selector = path.host_selector;
        item.shadow_path = path.shadow_path;
      }
      if (secret) item.redacted = true;
      if (el.disabled) item.disabled = true;
      if (typeof el.checked === 'boolean' && (el.type === 'checkbox' || el.type === 'radio')) item.checked = el.checked;
      if (typeof el.value === 'string' && el.value && (tag === 'input' || tag === 'textarea' || tag === 'select')) {
        // 口令类字段只回传占位符：快照会被写进 CLI 输出与 agent 对话上下文。
        item.value = secret ? '•••' : el.value.slice(0, 160);
      }
      if (tag === 'select') {
        item.options = Array.from(el.options).slice(0, 20).map(o => ({ value: o.value, text: (o.text || '').trim() }));
      }
      if (/^h[1-6]$/.test(tag)) item.level = Number(tag.slice(1));
      if (tag === 'a' && el.href) item.href = el.href;
      items.push(item);
    }
    if (hasMore) break;
  }
  return {
    ok: true,
    url: location.href,
    title: document.title,
    mode: 'js',
    // 与 CDP 引擎（operable-only）区分编号口径。
    ref_scheme: 'all-named',
    // 扫描覆盖范围：iframe 内容扫不到，shadow 的 open/closed 也要说清。
    scope: shadow.roots.length > 0 ? 'main-frame+shadow' : 'main-frame-only',
    frame_count: __abcSnapFrameCount(),
    shadow_mode: shadowMode,
    shadow_roots: shadow.roots.length,
    shadow_skipped: shadow.skipped,
    // 只有真的见到 open shadow root 才提示「可能有 closed root」：页面完全不用
    // shadow DOM 时无条件置 true，会让每一份 js 快照都被判成「覆盖不完整」。
    shadow_closed_unreachable: shadowMode === 'auto' && shadow.roots.length > 0,
    // 显式回传分页窗口：daemon 侧据此确认「这一批的起点」而不是靠猜。
    offset: offset,
    limit: maxItems,
    returned: items.length,
    // 命中总数只在本批就是最后一批时才是确定值，因此单独用 total_known 标注。
    total: hasMore ? null : index,
    total_known: !hasMore,
    has_more: hasMore,
    // next_offset 与「已消费候选数」严格对齐：序号只在通过全部过滤
    // （含 name 判定，E1-14b）之后才 +1，所以 offset+1 .. offset+items.length 这段
    // 一定是连续产出、没有空洞 —— 下一次从 next_offset+1 开始既不重复也不跳号。
    // 最后一批时 total === offset + items.length，与 items.length 自洽；还有下一批
    // 时 total 为 null（未知），不存在「total 与 items 不一致」的情形，因此不需要
    // 改成 offset+index：index 已是全页绝对序号，再叠 offset 会双重计算。
    next_offset: hasMore ? offset + items.length : null,
    truncated: hasMore,
    items
  };
}

// 实现标记（与 page_net / page_challenge 同一套）：给将来放开 world 留护栏。
try {
  Object.defineProperty(__abcSnapshot, '__abcOwn', {
    value: true, enumerable: false, configurable: true
  });
} catch (_) {}

try {
  Object.defineProperty(window, '__abcSnapshot', {
    value: __abcSnapshot,
    writable: true,
    enumerable: false,
    configurable: true
  });
} catch (_) {
  try { window.__abcSnapshot = __abcSnapshot; } catch (_) {}
}
})();
