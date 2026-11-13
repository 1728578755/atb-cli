// 页面结构化读取：一次注入返回正文、标题、元信息、表单与链接统计。
// 只做只读操作，不触发页面副作用。
//
// performance 约定（大页面）：
// - 文本、计数、标题、链接、表单字段都在一次遍历里完成，不做 5~6 次 querySelectorAll；
// - 元素数超过 visibleInnerTextLimit 时不再用 innerText（需要整页布局），
//   改用 TreeWalker 近似可见性，并在 text_mode 里标记 "approx"；
// - 全程受 budgetMs / maxNodes 约束，超限时返回 partial:true，而不是把页面卡死。
//
// ---- shadow DOM / iframe 边界----
// open shadow root 的正文与控件也算「页面内容」：旧实现只读 document，
// Web Components 站点会读出空正文并报 ok:true（静默假成功）。
// 现在 opts.shadowDom = auto（默认）/ off：
//   * 正文：主文档照旧走 innerText/TreeWalker，shadow root 的文本单独追加，
//     回包用 shadow_text_merged / shadow_text_truncated 标明；
//   * 控件统计/表单字段：逐个 root 各查一次；
//   * 选择器：`a >>> b` 写法会逐层进 open shadow root（与 page_action.js 一致）。
// closed shadow root 在页面 JS 里不可达，iframe 内容需要逐 frame 注入，
// 二者都在回包里显式标注（shadow_closed_unreachable / frame_count / scope）。
function __abcRead(opts) {
  opts = opts || {};
  const doc = document;
  const shadowMode = String((opts.shadowDom || 'auto')).toLowerCase() === 'off' ? 'off' : 'auto';

  /// 收集 open shadow root（嵌套也算），带「被跳过」计数。
  const __abcReadShadowRoots = () => {
    const out = [];
    let skipped = 0;
    if (shadowMode === 'off') {
      try {
        for (const el of doc.querySelectorAll('*')) if (el.shadowRoot) skipped += 1;
      } catch (_) {}
      return { roots: out, skipped: skipped };
    }
    const queue = [{ root: doc, depth: 0 }];
    while (queue.length) {
      const item = queue.shift();
      let hosts = [];
      try { hosts = item.root.querySelectorAll('*'); } catch (_) { hosts = []; }
      for (const el of hosts) {
        let shadow = null;
        try { shadow = el.shadowRoot; } catch (_) { shadow = null; }
        if (!shadow) continue;
        if (item.depth >= 8 || out.length >= 200) { skipped += 1; continue; }
        out.push(shadow);
        queue.push({ root: shadow, depth: item.depth + 1 });
      }
    }
    return { roots: out, skipped: skipped };
  };

  /// `host >>> inner` 选择器解析：逐层进 open shadow root。
  /// 返回 {ok, el} 或 {ok:false, invalid:true}。
  const __abcReadResolve = (selector) => {
    const raw = String(selector || '');
    const parts = raw.split('>>>').map((item) => item.trim());
    let scope = doc;
    let el = null;
    for (let i = 0; i < parts.length; i += 1) {
      const part = parts[i];
      if (!part) throw new Error('empty selector segment');
      if (i === 0) {
        el = scope.querySelector(part);
      } else {
        if (!el) return { ok: true, el: null };
        let shadow = null;
        try { shadow = el.shadowRoot; } catch (_) { shadow = null; }
        if (!shadow) return { ok: true, el: null, closed_shadow: true };
        el = shadow.querySelector(part);
      }
      if (!el) return { ok: true, el: null };
    }
    return { ok: true, el: el };
  };

  const shadow = __abcReadShadowRoots();
  // 非法选择器会让 querySelector 直接抛 DOMException；不包起来的话，调用方拿到的是
  // 通道级错误（pageOp failed: ...），分不清「选择器写错了」和「注入失败了」。
  // 与 page_wait.js 的 `invalid selector:` 口径对齐。
  let root = null;
  if (opts.selector) {
    try {
      const resolved = __abcReadResolve(opts.selector);
      root = resolved.el || null;
    } catch (e) {
      return { ok: false, error: 'invalid selector: ' + opts.selector + ' (' + ((e && e.message) || e) + ')' };
    }
  } else {
    // XML / SVG 文档没有 body：旧实现让 root 停在 null，下面报出
    // "selector not found: undefined" —— 既点名了一个不存在的选择器，又没说清
    // 真实原因是「这份文档没有 body」。退回 documentElement 仍能读正文。
    root = doc.body || doc.documentElement;
  }
  if (!root) {
    return {
      ok: false,
      error: opts.selector ? 'selector not found: ' + opts.selector : 'document has no body or documentElement',
      error_code: opts.selector ? 'selector_not_found' : 'no_document_body'
    };
  }

  const now = () => (typeof performance !== 'undefined' && performance.now ? performance.now() : Date.now());
  const started = now();
  // 参数约定：用户显式给的值一律照办，只有「缺省 / 非法 / <= 0」才落默认。
  // 以前 maxNodes / innerTextLimit 写的是「小于 1000 就当默认」，于是 --max-nodes 200
  // 反而拿到 60000 上限——用户想省 token，结果被静默放大，属于典型的「假成功」。
  const maxChars = opts.maxChars > 0 ? Math.floor(opts.maxChars) : 20000;
  // budgetMs 是唯一保留下限的：低于 10ms 这一趟基本读不到内容，等于返回空串。
  // 但下限不是静默的——被调整时在返回体里给 clamped / requested。
  const MIN_BUDGET_MS = 10;
  const requestedBudgetMs = Math.floor(Number(opts.budgetMs));
  const budgetClamped = requestedBudgetMs > 0 && requestedBudgetMs < MIN_BUDGET_MS;
  const budgetMs = requestedBudgetMs > 0 ? Math.max(requestedBudgetMs, MIN_BUDGET_MS) : 2000;
  const requestedMaxNodes = Math.floor(Number(opts.maxNodes));
  const maxNodes = requestedMaxNodes > 0 ? requestedMaxNodes : 60000;
  const requestedInnerTextLimit = Math.floor(Number(opts.innerTextLimit));
  const innerTextLimit = requestedInnerTextLimit > 0 ? requestedInnerTextLimit : 30000;
  const includeHidden = opts.includeHidden === true || opts.mode === 'all';
  const deadline = started + budgetMs;

  const BLOCK_TAGS = new Set([
    'ADDRESS', 'ARTICLE', 'ASIDE', 'BLOCKQUOTE', 'BR', 'DD', 'DETAILS', 'DIV', 'DL', 'DT',
    'FIELDSET', 'FIGCAPTION', 'FIGURE', 'FOOTER', 'FORM', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6',
    'HEADER', 'HR', 'LI', 'MAIN', 'NAV', 'OL', 'P', 'PRE', 'SECTION', 'SUMMARY', 'TABLE',
    'TBODY', 'TD', 'TFOOT', 'TH', 'THEAD', 'TR', 'UL'
  ]);
  const SKIP_TAGS = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK', 'TITLE']);

  function normalize(text) {
    return String(text || '')
      .replace(/\r\n?/g, '\n')
      .replace(/[ \t\f\v\u00a0]+/g, ' ')
      .replace(/ ?\n ?/g, '\n')
      .replace(/\n{3,}/g, '\n\n')
      .trim();
  }

  function clipped(text, max) {
    const value = String(text || '').replace(/\s+/g, ' ').trim();
    return value.length > max ? value.slice(0, max) : value;
  }

  // 口令/支付类字段的值不返回明文。
  // read 是 LLM 代理最常走的路径：fill 完登录表单再 read 一次确认，
  // 明文口令就会顺着「daemon 响应 → CLI 输出 → agent 上下文 → 日志」一路留痕。
  function secretField(el) {
    const type = String((el.type || '')).toLowerCase();
    if (type === 'password') return true;
    const autocomplete = String(el.getAttribute && el.getAttribute('autocomplete') || '').toLowerCase();
    return /current-password|new-password|cc-number|cc-csc|one-time-code/.test(autocomplete);
  }

  // 派生文本掩码：口径与 page_find.js 的 __abcMaskDerived
  /// 逐字对齐（三份文件各留一份、各叫各的名字，见 page_action.js 头部的命名约定）。
  ///
  /// 只掩码 placeholder 这类可能夹带半明文的**派生文本**：表单字段的 `name` 是站点
  /// 写的**结构标识**（agent 靠它定位字段），不是用户输入，不能一起掩掉。
  /// 空值统一返回 null（调用方据此区分「字段没写」与「写了但被掩码」）；
  /// secret 字段的非空值一律 '•••'，并在字段对象上置 redacted:true。
  function maskDerived(el, value) {
    const raw = value === undefined || value === null ? '' : String(value);
    if (!secretField(el)) return raw || null;
    return raw ? '•••' : null;
  }

  // 走一次 TreeWalker 收集文本；includeAll=false 时粗略跳过明显隐藏的子树。
  function collectText(includeAll, scopeRoot) {
    const chunks = [];
    let length = 0;
    let visited = 0;
    let truncated = false;
    const walker = doc.createTreeWalker(scopeRoot, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        if (node.nodeType === 3) return NodeFilter.FILTER_ACCEPT;
        const tag = node.tagName;
        if (SKIP_TAGS.has(tag)) return NodeFilter.FILTER_REJECT;
        if (!includeAll) {
          if (node.hidden === true) return NodeFilter.FILTER_REJECT;
          if (node.getAttribute('aria-hidden') === 'true') return NodeFilter.FILTER_REJECT;
          const inline = node.getAttribute('style');
          if (inline && /display\s*:\s*none|visibility\s*:\s*hidden/i.test(inline)) {
            return NodeFilter.FILTER_REJECT;
          }
        }
        return NodeFilter.FILTER_ACCEPT;
      }
    });
    let node = walker.nextNode();
    while (node) {
      visited += 1;
      if (visited > maxNodes) { truncated = true; break; }
      if ((visited & 511) === 0 && now() > deadline) { truncated = true; break; }
      if (node.nodeType === 1) {
        if (BLOCK_TAGS.has(node.tagName) && chunks.length) chunks.push('\n');
      } else {
        const raw = node.nodeValue;
        if (raw) {
          const value = raw.replace(/[ \t\f\v\u00a0]+/g, ' ');
          if (value.trim() || (chunks.length && !/\n$/.test(chunks[chunks.length - 1]))) {
            chunks.push(value);
            length += value.length;
          }
        }
      }
      if (length >= maxChars * 3) { truncated = true; break; }
      node = walker.nextNode();
    }
    return { text: normalize(chunks.join('')), truncated, visited };
  }

  const nodeCount = (root.getElementsByTagName ? root.getElementsByTagName('*').length : 0) + 1;
  let text = '';
  let textMode = includeHidden ? 'all' : 'visible';
  let textTruncated = false;
  let visited = 0;

  if (includeHidden) {
    const got = collectText(true, root);
    text = got.text;
    textTruncated = got.truncated;
    visited = got.visited;
  } else if (nodeCount <= innerTextLimit) {
    // 小页面：innerText 最准（会排除 CSS 隐藏内容），一次布局可以接受。
    text = normalize(root.innerText || root.textContent || '');
  } else {
    const got = collectText(false, root);
    text = got.text;
    textTruncated = got.truncated;
    visited = got.visited;
    textMode = 'visible-approx';
  }

  // shadow root 的正文：主文档的 innerText / TreeWalker 都不会穿进去，
  // 单独追加（只在没指定 selector 时才做，指定了就按调用方给的子树读）。
  let shadowTextMerged = false;
  let shadowTextTruncated = false;
  if (!opts.selector && shadow.roots.length) {
    const budget = maxChars * 3;
    const extra = [];
    for (const sr of shadow.roots) {
      if (now() > deadline) { shadowTextTruncated = true; break; }
      let chunk = '';
      try {
        chunk = includeHidden
          ? String(sr.textContent || '')
          : normalize(sr.textContent || '');
      } catch (_) { chunk = ''; }
      if (!chunk) continue;
      extra.push(chunk);
      shadowTextMerged = true;
      if (extra.join('\n').length >= budget) { shadowTextTruncated = true; break; }
    }
    if (extra.length) {
      text = normalize(text + '\n' + extra.join('\n'));
      if (text.length > budget) shadowTextTruncated = true;
    }
  }

  // 单次查询覆盖计数 / 标题 / 链接 / 表单，避免多次全量遍历。
  // shadow root 各查一次（auto 模式），否则组件化站点会读出 0 链接 0 表单。
  const SELECTOR = 'a[href],img,input,textarea,select,button,form,h1,h2,h3,[role="button"]';
  const counts = { links: 0, images: 0, inputs: 0, buttons: 0, forms: 0 };
  const headings = [];
  const wantLinks = opts.links === true;
  const wantForms = opts.forms !== false;
  const links = [];
  const forms = [];
  let scanned = 0;
  let partial = textTruncated || nodeCount > maxNodes;

  const elementScopes = opts.selector ? [root] : [root].concat(shadow.roots);
  for (const scope of elementScopes) {
    let elements = [];
    try { elements = scope.querySelectorAll(SELECTOR); } catch (_) { elements = []; }
    for (const el of elements) {
      scanned += 1;
      if (scanned > maxNodes) { partial = true; break; }
      if ((scanned & 255) === 0 && now() > deadline) { partial = true; break; }
      const tag = el.tagName;
      if (tag === 'A') {
        counts.links += 1;
        if (wantLinks && links.length < 100) {
          links.push({ text: clipped(el.textContent, 120), href: el.href });
        }
      } else if (tag === 'IMG') {
        counts.images += 1;
      } else if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
        counts.inputs += 1;
      } else if (tag === 'BUTTON' || el.getAttribute('role') === 'button') {
        counts.buttons += 1;
      } else if (tag === 'FORM') {
        counts.forms += 1;
        if (wantForms && forms.length < 20) {
          const fields = [];
          for (const f of el.querySelectorAll('input,textarea,select')) {
            if (fields.length >= 50) break;
            const secret = secretField(f);
            fields.push({
              name: f.name || '',
              type: (f.type || f.tagName).toLowerCase(),
              value: typeof f.value === 'string' ? (secret ? (f.value ? '•••' : '') : f.value.slice(0, 200)) : '',
              redacted: secret || undefined,
              // placeholder 常被站点写成「请输入密码/卡号后四位」，与 find 同口径掩码。
              placeholder: maskDerived(f, f.placeholder),
              required: !!f.required
            });
          }
          forms.push({ action: el.action || '', method: (el.method || 'get').toUpperCase(), fields });
        }
      } else if (tag === 'H1' || tag === 'H2' || tag === 'H3') {
        if (headings.length < 50) {
          const value = clipped(el.textContent, 200);
          if (value) headings.push({ tag: tag.toLowerCase(), text: value });
        }
      }
    }
    if (partial && scanned > maxNodes) break;
  }

  const meta = {};
  for (const m of doc.querySelectorAll('meta[name],meta[property]')) {
    const key = m.getAttribute('name') || m.getAttribute('property');
    const value = m.getAttribute('content');
    if (key && value && !(key in meta)) meta[key] = String(value).slice(0, 300);
  }

  const frameCount = (() => {
    try { return doc.querySelectorAll('iframe,frame').length; } catch (_) { return 0; }
  })();

  const out = {
    ok: true,
    url: location.href,
    title: doc.title,
    lang: doc.documentElement.lang || '',
    description: meta.description || meta['og:description'] || '',
    textLength: text.length,
    text: text.slice(0, maxChars),
    truncated: text.length > maxChars || textTruncated || shadowTextTruncated,
    text_mode: textMode,
    partial,
    // 扫描覆盖范围：iframe 内容不在本次结果里，必须让调用方看得见。
    scope: shadow.roots.length > 0 ? 'main-frame+shadow' : 'main-frame-only',
    frame_count: frameCount,
    shadow_mode: shadowMode,
    shadow_roots: shadow.roots.length,
    shadow_skipped: shadow.skipped,
    shadow_text_merged: shadowTextMerged || undefined,
    shadow_text_truncated: shadowTextTruncated || undefined,
    // 同上：只有页面确实用了 shadow DOM（见到 open root）时才提示 closed 可能不可达。
    shadow_closed_unreachable: shadowMode === 'auto' && shadow.roots.length > 0,
    headings,
    counts,
    stats: {
      node_count: nodeCount,
      scanned,
      visited,
      elapsed_ms: Math.round((now() - started) * 100) / 100,
      budget_ms: budgetMs,
      max_nodes: maxNodes
    }
  };

  if (wantLinks) out.links = links;
  if (wantForms) out.forms = forms;
  // 只在实际调整过用户显式值时才出现，避免「改了数还不说」。
  if (budgetClamped) {
    out.clamped = true;
    out.requested = { budget_ms: requestedBudgetMs };
  }
  return out;
}
