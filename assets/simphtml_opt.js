// 页面侧递归克隆必须和 Rust 侧同阈值（html.rs 的 MAX_DOM_DEPTH = 512）。
// 构造成本极低的超深嵌套 DOM 会让 cloneNode 抛 RangeError: Maximum call stack
// size exceeded，而 optHTML 没有任何兜底 → scan 直接报错。
// 另外整个函数套一层 try/catch，异常统一变成 `__ABC_SCAN_ERROR__<msg>` 哨兵，
// 由 daemon 转成结构化错误，而不是一句通道级 "pageOp failed"。
//
// ---- 入口挂载----
// 本文件以前把 optHTML / __abcOptSecret / analyzeNode… 全写成顶层声明：在 classic
// script 里那是**可枚举**的全局（`Object.keys(window)` 一行就能列出来），正是
// page_net.js 头部批判过的指纹形态；而 stealth 的 OWN_GLOBALS 只列了 8 个 `__abc*`，
// 这几个完全漏网 —— audit 报不到（假干净）、scrub 也不去删。
//
// 现在整个文件包进 IIFE，只有「daemon 必须按裸标识符调用」的入口才挂到 window：
// 不可枚举 + 可配置（scrub 能删），且**用完即撤**（见 publishTempGlobal）——
// 调用方在同一段注入脚本里同步 `return optHTML(...)`，之后没有任何消费者，
// 留在页面上纯属多一个可枚举指纹（旧路径下它其实只是函数作用域的局部声明）。
;(function () {
const __ABC_MAX_DOM_DEPTH = 512;

/// 口令类字段的值不写进 scan 结果：scan 的 HTML 会进 CLI 输出
/// 与 agent 上下文，autofill 出来的口令同样是明文。
function __abcOptSecret(el) {
  if (!el || el.nodeType !== 1) return false;
  const type = String(el.type || '').toLowerCase();
  if (type === 'password') return true;
  const autocomplete = String(el.getAttribute && el.getAttribute('autocomplete') || '').toLowerCase();
  return /current-password|new-password|cc-number|cc-csc|one-time-code/.test(autocomplete);
}

function optHTML(text_only = false, opts) {
  try {
    return __abcOptHTMLBody(text_only, opts);
  } catch (e) {
    return '__ABC_SCAN_ERROR__' + ((e && e.message) ? e.message : String(e));
  }
}

function __abcOptHTMLBody(text_only=false, opts) {
  opts = opts || {};
  // 调试日志默认关闭：这些 console.log 打在**第三方页面**的控制台里，
  // 既污染用户页面，也把「有人在用自动化工具」写在了明处；而且 console 会持有
  // 节点引用导致不可回收。需要时传 {debug:true}。
  const __debug = opts.debug === true;
  let __depthCapHit = false;
  // 超大页面上「克隆 + 逐节点 getComputedStyle/getBoundingClientRect」会到秒级，
  // 这里加一个硬预算：超过后不再深入，返回带截断标记的部分结果。
  const __nodeBudget = opts.nodeBudget > 0 ? Math.floor(opts.nodeBudget) : 0;
  let __visited = 0;
  let __budgetHit = false;
  // 十万级节点页面上，第一次 getBoundingClientRect / getComputedStyle 会触发
  // 数秒的整页布局。超过阈值时跳过布局读数，用属性判断隐藏，并在结果里注明。
  const __totalElements = document.getElementsByTagName('*').length;
  // 十万级页面上，把整棵 DOM 序列化成 HTML 再跨进程传回来要 1.5s 以上，
  // 而调用方真正要的是「页面里有什么」。超过阈值直接让 daemon 改走文本路径。
  const __tooLarge = opts.tooLarge > 0 ? Math.floor(opts.tooLarge) : 0;
  if (__tooLarge && __totalElements > __tooLarge) {
    return '__ABC_TOO_LARGE__' + __totalElements;
  }
  const __layoutLimit = opts.layoutNodeLimit > 0 ? Math.floor(opts.layoutNodeLimit) : 50000;
  const __skipLayout = opts.skipLayout === true ||
    (opts.skipLayout !== false && __totalElements > __layoutLimit);
function createEnhancedDOMCopy() {
  const nodeInfo = new WeakMap();
  const ignoreTags = ['SCRIPT', 'STYLE', 'NOSCRIPT', 'META', 'LINK', 'COLGROUP', 'COL', 'TEMPLATE', 'PARAM', 'SOURCE'];
  // 扩展状态角标是纯 UI，不该出现在 scan 的正文里。
  // atb-status-host 是 5.0 起的 Shadow DOM 宿主，atb-ind 是历史版本留下的节点。
  const ignoreIds = ['atb-ind', 'atb-status-host'];
  function cloneNode(sourceNode, keep=false, depth=0) {
    if (depth > __ABC_MAX_DOM_DEPTH) { __depthCapHit = true; return null; }
    if (__nodeBudget) {
      __visited += 1;
      if (__visited > __nodeBudget) { __budgetHit = true; return null; }
    }
    if (sourceNode.nodeType === 8 ||
        (sourceNode.nodeType === 1 && (
          ignoreTags.includes(sourceNode.tagName) ||
          (sourceNode.id && ignoreIds.includes(sourceNode.id))
        ))) {
      return null;
    }
    if (sourceNode.nodeType === 3) return sourceNode.cloneNode(false);
    const clone = sourceNode.cloneNode(false);
    // 派生文本与默认属性同样算敏感面：secret 字段**无条件**改写 value。
    // 旧写法 `&& sourceNode.value` 有两处漏：① cloneNode(false) 会把源节点上的
    // value="明文" 属性一起复制，value 为空/未定义时这层保护整个不生效；
    // ② 未聚焦时 property 可能还没同步过来。所以 secret 判断优先、不看有没有值。
    const __secretField = __abcOptSecret(sourceNode);
    if (sourceNode.tagName === 'INPUT' || sourceNode.tagName === 'TEXTAREA') {
      if (__secretField) clone.setAttribute('value', '•••');
      else if (sourceNode.value) clone.setAttribute('value', sourceNode.value);
    }
    if (sourceNode.tagName === 'INPUT' && (sourceNode.type === 'radio' || sourceNode.type === 'checkbox') && sourceNode.checked) clone.setAttribute('checked', '');
    else if (sourceNode.tagName === 'SELECT' && sourceNode.value) clone.setAttribute('data-selected', sourceNode.value);
    try { if (sourceNode.matches && sourceNode.matches(':-webkit-autofill')) { clone.setAttribute('data-autofilled', 'true'); if (!sourceNode.value) clone.setAttribute('value', '⚠️受保护-读tmwebdriver_sop的autofill章节提取'); } } catch(e) {}

    const isDropdown = sourceNode.classList?.contains('dropdown-menu') ||
             /dropdown|menu/i.test(sourceNode.className) || sourceNode.getAttribute('role') === 'menu';
    const _ddItems = isDropdown ? sourceNode.querySelectorAll('a, button, [role="menuitem"], li').length : 0;
    const isSmallDropdown = _ddItems > 0 && _ddItems <= 7 && sourceNode.textContent.length < 500;

    const childNodes = [];
    // secret 的 TEXTAREA 正文就是明文：value 属性改了也没用，子文本节点会照样
    // 序列化进 <textarea>…</textarea>。这类节点的子节点整段跳过。
    if (!(__secretField && sourceNode.tagName === 'TEXTAREA')) {
      for (const child of sourceNode.childNodes) {
        const childClone = cloneNode(child, keep || isSmallDropdown, depth + 1);
        if (childClone) childNodes.push(childClone);
      }
    }
    let iframeWrapper = null;
    if (sourceNode.tagName === 'IFRAME') {
      try {
        const iDoc = sourceNode.contentDocument || sourceNode.contentWindow?.document;
        if (iDoc && iDoc.body && iDoc.body.children.length > 0) {
          const wrapper = document.createElement('div');
          wrapper.setAttribute('data-iframe-content', sourceNode.src || '');
          for (const ch of iDoc.body.childNodes) {
            const c = cloneNode(ch, keep, depth + 1);
            if (c) wrapper.appendChild(c);
          }
          if (wrapper.childNodes.length) {
            childNodes.push(wrapper);
            iframeWrapper = wrapper;
          }
        }
      } catch(e) {}
    }
    if (sourceNode.shadowRoot) {
      for (const shadowChild of sourceNode.shadowRoot.childNodes) {
        const shadowClone = cloneNode(shadowChild, keep, depth + 1);
        if (shadowClone) childNodes.push(shadowClone);
      }
    }

    let rect;
    let style;
    let __looksHidden = false;
    if (__skipLayout) {
      const inline = sourceNode.getAttribute ? (sourceNode.getAttribute('style') || '') : '';
      __looksHidden = (sourceNode.hidden === true) ||
        (sourceNode.getAttribute && sourceNode.getAttribute('aria-hidden') === 'true') ||
        /display\s*:\s*none|visibility\s*:\s*hidden/i.test(inline);
      rect = { left: 0, top: 0, right: 1, bottom: 1, width: 1, height: 1 };
      style = {
        display: __looksHidden ? 'none' : 'block', visibility: 'visible', opacity: '1',
        position: 'static', backgroundColor: 'transparent', backgroundImage: 'none',
        backdropFilter: 'none', boxShadow: 'none', zIndex: 'auto'
      };
    } else {
      rect = sourceNode.getBoundingClientRect();
      style = window.getComputedStyle(sourceNode);
    }
    const area = __skipLayout
      ? (__looksHidden ? 0 : 1)
      : ((style.display === 'none' || style.visibility === 'hidden' || parseFloat(style.opacity) <= 0) ? 0 : rect.width * rect.height);
    const isVisible = __skipLayout ? !__looksHidden : (rect.width > 1 && rect.height > 1 &&
                  style.display !== 'none' && style.visibility !== 'hidden' &&
                  parseFloat(style.opacity) > 0 &&
                  Math.abs(rect.left) < 5000 && Math.abs(rect.top) < 5000)
                  || isSmallDropdown;
    const zIndex = style.position !== 'static' ? (parseInt(style.zIndex) || 0) : 0;

    let info = {
          rect, area, isVisible, isSmallDropdown, zIndex,
          style: {
            display: style.display, visibility: style.visibility,
            opacity: style.opacity, position: style.position
          }};

    const nonTextChildren = childNodes.filter(child => child.nodeType !== 3);
    const hasValidChildren = nonTextChildren.length > 0;

    if (hasValidChildren) {
      const childrenInfos = nonTextChildren.map(c => nodeInfo.get(c)).filter(i => i && i.rect && i.rect.width > 0 && i.rect.height > 0);
      const bgAlpha = (() => {
        const c = style.backgroundColor;
        if (!c || c === 'transparent') return 0;
        const m = c.match(/rgba?\([^)]+,\s*([\d.]+)\)/);
        return m ? parseFloat(m[1]) : 1;
      })();
      const hasVisualBg = bgAlpha > 0.1 || style.backgroundImage !== 'none' || (style.backdropFilter && style.backdropFilter !== 'none') || style.boxShadow !== 'none';

      if (!hasVisualBg && childrenInfos.length > 0) {
        // Skip fixed/absolute children when computing parent's merged rect (they're out of flow)
        const flowChildren = childrenInfos.filter(cInfo => cInfo.style && cInfo.style.position !== 'fixed' && cInfo.style.position !== 'absolute');
        if (flowChildren.length > 0) {
          let minL = Infinity, minT = Infinity, maxR = -Infinity, maxB = -Infinity;
          for (const cInfo of flowChildren) {
            minL = Math.min(minL, cInfo.rect.left);
            minT = Math.min(minT, cInfo.rect.top);
            maxR = Math.max(maxR, cInfo.rect.right);
            maxB = Math.max(maxB, cInfo.rect.bottom);
          }
          info.rect = { left: minL, top: minT, right: maxR, bottom: maxB, width: maxR - minL, height: maxB - minT };
          info.area = info.rect.width * info.rect.height;
        } else {
          const maxC = childrenInfos.filter(i => i.isVisible).sort((a, b) => b.area - a.area)[0];
          if (maxC && maxC.area > 10000 && (!isVisible || maxC.area > info.area * 5)) info = maxC;
        }
      }
    }

    // **关键**：把 info 真正写回 WeakMap。以前只算不写（全文件没有 nodeInfo.set），
    // 于是 analyzeNode 的入口守卫 `!getNodeInfo(node)` 永远命中 → K:main / R:covered /
    // R:floatingAd 一个都不打，`[data-mark^="R:"]` 删不掉任何节点，顶层 fixed 对话框
    // 提升也全被 `!info?.rect` 过滤掉 —— scan 的「剔除被遮挡内容 / 浮层广告」整条
    // 链路是死代码，每次都白做分析还照样输出广告层。
    nodeInfo.set(clone, info);
    if (iframeWrapper) nodeInfo.set(iframeWrapper, info);

    if (sourceNode.nodeType === 1 && sourceNode.tagName === 'DIV') {
      if (!hasValidChildren && !sourceNode.textContent.trim()) return null;
    }
    // aria-hidden + not visible = truly hidden (e.g. mobile menus), remove even if has children
    if (sourceNode.getAttribute && sourceNode.getAttribute('aria-hidden') === 'true' && !info.isVisible) {
      return null;
    }
    if (info.isVisible || hasValidChildren || keep) {
      childNodes.forEach(child => clone.appendChild(child));
      return clone;
    }
    return null;
  }

  return {
    domCopy: cloneNode(document.body),
    getNodeInfo: node => nodeInfo.get(node),
    isVisible: node => {
      const info = nodeInfo.get(node);
      return info && info.isVisible;
    }
  };
}
const { domCopy, getNodeInfo, isVisible } = createEnhancedDOMCopy();
if (text_only) {
  const blocks = new Set(['DIV','P','H1','H2','H3','H4','H5','H6','LI','TR','SECTION','ARTICLE','HEADER','FOOTER','NAV','BLOCKQUOTE','PRE','HR','BR','DT','DD','FIGCAPTION','DETAILS','SUMMARY']);
  domCopy.querySelectorAll('*').forEach(el => {
    if (blocks.has(el.tagName)) el.insertAdjacentText('beforebegin', '\n');
  });
  domCopy.querySelectorAll('input:not([type=hidden]),textarea,select').forEach(el=>{
    const p=[el.tagName,el.id&&'#'+el.id,el.getAttribute('name')&&'name='+el.getAttribute('name'),el.tagName==='INPUT'&&'type='+(el.getAttribute('type')||'text'),el.getAttribute('placeholder')&&'"'+el.getAttribute('placeholder')+'"',el.getAttribute('data-autofilled')&&'autofilled',el.disabled&&'disabled',el.tagName==='SELECT'&&el.getAttribute('data-selected')&&'="'+el.getAttribute('data-selected')+'"'].filter(Boolean).join(' ');
    el.insertAdjacentText('beforebegin','\n['+p+']\n');
  });
  domCopy.querySelectorAll('button[disabled]').forEach(el=>el.insertAdjacentText('beforebegin','[DISABLED] '));
  const __text = domCopy.textContent;
  return __budgetHit
    ? __text + '\n…[truncated: 已扫描 ' + __visited + ' 个节点，超过预算 ' + __nodeBudget + ']'
    : __text;
}
const viewportArea = window.innerWidth * window.innerHeight;

function analyzeNode(node, pPathType='main', depth=0) {
    if (depth > 64) return;
    // 处理非元素节点和叶节点
    if (node.nodeType !== 1 || !node.children.length) {
      node.nodeType === 1 && (node.dataset.mark = 'K:leaf');
      return;
    }
    const pathType = (node.dataset.mark === 'K:secondary') ? 'second' : pPathType;
    const nodeInfoData = getNodeInfo(node);
    if (!nodeInfoData || !nodeInfoData.rect) return;
    const rectn = nodeInfoData.rect;
    if (rectn.width < window.innerWidth * 0.8 && rectn.height < window.innerHeight * 0.8) return node;
    if (node.tagName === 'TABLE') return;
    const children = Array.from(node.children);
    if (children.length === 1) {
      node.dataset.mark = 'K:container';
      return analyzeNode(children[0], pathType, depth + 1);
    }
    if (children.length > 10) return;

    // 获取子元素信息并排序
    const childrenInfo = children.map(child => {
      const info = getNodeInfo(child) || { rect: {}, style: {} };
      return { node: child, rect: info.rect, style: info.style,
          area: info.area, zIndex: (info.zIndex || 0), isVisible: info.isVisible };
    });
    childrenInfo.sort((a, b) => b.area - a.area);

    // 检测是划分还是覆盖
    const isOverlay = hasOverlap(childrenInfo);
    node.dataset.mark = isOverlay ? 'K:overlayParent' : 'K:partitionParent';

    if (isOverlay) handleOverlayContainer(childrenInfo, pathType);
    else handlePartitionContainer(childrenInfo, pathType);

    if (__debug) {
      console.log(`${isOverlay ? '覆盖' : '划分'}容器:`, node, `子元素数量: ${children.length}`);
      console.log('子元素及标记:', children.map(child => ({
        element: child,
        mark: child.dataset.mark || '无',
        info: getNodeInfo ? getNodeInfo(child) : undefined
      })));
    }
    for (const child of children)
      if (!child.dataset.mark || child.dataset.mark[0] !== 'R') analyzeNode(child, pathType, depth + 1);
  }

  // 处理划分容器
  function handlePartitionContainer(childrenInfo, pathType) {
    childrenInfo.sort((a, b) => b.area - a.area);
    const totalArea = childrenInfo.reduce((sum, item) => sum + item.area, 0);
    // 遗留调试语句（原 console.log(childrenInfo[0].area / totalArea)）已删：
    // 它每处理一个划分容器就往**被访页面**的控制台打一行，属可见污染。
    const hasMainElement = childrenInfo.length >= 1 &&
                          (childrenInfo[0].area / totalArea > 0.5) &&
                          (childrenInfo.length === 1 || childrenInfo[0].area > childrenInfo[1].area * 2);
    if (hasMainElement) {
      childrenInfo[0].node.dataset.mark = 'K:main';
      for (let i = 1; i < childrenInfo.length; i++) {
        const child = childrenInfo[i];
        let className = (child.node.getAttribute('class') || '').toLowerCase();
        let isSecondary = containsButton(child.node);
        if (className.includes('nav')) isSecondary = true;
        if (className.includes('breadcrumbs')) isSecondary = true;
        if (className.includes('header') && className.includes('table')) isSecondary = true;
        if (child.node.innerHTML.trim().replace(/\s+/g, '').length < 500) isSecondary = true;
        if (child.node.textContent.trim().length > 200) isSecondary = true;  // P3: 有实质文本内容则保留
        if (child.style.visibility === 'hidden') isSecondary = false;
        if (isSecondary) child.node.dataset.mark = 'K:secondary';
        else child.node.dataset.mark = 'K:nonEssential';
      }
    } else {
      // relaxed 模式刻意跳过 equalmany 去重，避免误删结构不同的容器；
      // token 预算由后续的列表截断兜底。
      return;
    }
  }

  function containsButton(container) {
    const hasStandardButton = container.querySelector('button, input[type="button"], input[type="submit"], [role="button"]') !== null;
    if (hasStandardButton) return true;
    const hasClassButton = container.querySelector('[class*="-btn"], [class*="-button"], .button, .btn, [class*="btn-"]') !== null;
    return hasClassButton;
  }

  function handleOverlayContainer(childrenInfo, pathType) {
    // elementFromPoint ground truth: 让浏览器告诉我们谁在视觉最上层
    const _efp = document.elementFromPoint(window.innerWidth/2, window.innerHeight/2);
    if (_efp) { let _el = _efp; while (_el) { const _h = childrenInfo.find(c => c.node.id && c.node.id === _el.id); if (_h) { _h.zIndex = 9999; break; } _el = _el.parentElement; } }
    const sorted = [...childrenInfo].sort((a, b) => b.zIndex - a.zIndex);
    if (__debug) console.log('排序后的子元素:', sorted);
    if (sorted.length === 0) return;

    const top = sorted[0];
    const rect = top.rect;
    const topNode = top.node;
    const isComplex = top.node.querySelectorAll('input, select, textarea, button, a, [role="button"]').length >= 1;

    const textContent = topNode.textContent?.trim() || '';
    const textLength = textContent.length;
    const hasLinks = topNode.querySelectorAll('a').length > 0;
    const isMostlyText = textLength > 7 && !hasLinks;

    const centerDiff = Math.abs((rect.left + rect.width/2) - window.innerWidth/2) / window.innerWidth;
    const minDimensionRatio = Math.min(rect.width / window.innerWidth, rect.height / window.innerHeight);
    const maxDimensionRatio = Math.max(rect.width / window.innerWidth, rect.height / window.innerHeight);
    const isNearTop = rect.top < 50;
    const isDialog = (top.node.querySelector('iframe') || top.node.querySelector('button') || top.node.querySelector('input')) && centerDiff < 0.3;

    if (isComplex && centerDiff < 0.2 &&
        ((minDimensionRatio > 0.2 && rect.width/window.innerWidth < 0.98) || minDimensionRatio > 0.95)) {
      top.node.dataset.mark = 'K:mainInteractive';
       sorted.slice(1).forEach(e => {
          if ((parseInt(e.zIndex)||0) <= (parseInt(sorted[0].zIndex)||0)) {
              e.node.dataset.mark = 'R:covered';
          } else {
              e.node.dataset.mark = 'K:noncovered';
          }
      });
    } else {
      if (isComplex && isNearTop && maxDimensionRatio > 0.4 && top.isVisible) {
        top.node.dataset.mark = 'K:topBar';
      } else if (isMostlyText || isComplex || isDialog) {
        topNode.dataset.mark = 'K:messageContent';
      } else {
        topNode.dataset.mark = 'R:floatingAd';
      }
      const rest = sorted.slice(1);
      rest.length && (!hasOverlap(rest) ? handlePartitionContainer(rest, pathType) : handleOverlayContainer(rest, pathType));
    }
  }

  function hasOverlap(items) {
    return items.some((a, i) =>
      items.slice(i+1).some(b => {
        const r1 = a.rect, r2 = b.rect;
        if (!r1.width || !r2.width || !r1.height || !r2.height) {return false;}
        const epsilon = 1;
        const x1 = r1.x !== undefined ? r1.x : r1.left;
        const y1 = r1.y !== undefined ? r1.y : r1.top;
        const x2 = r2.x !== undefined ? r2.x : r2.left;
        const y2 = r2.y !== undefined ? r2.y : r2.top;
        return !(x1 + r1.width <= x2 + epsilon || x1 >= x2 + r2.width - epsilon ||
            y1 + r1.height <= y2 + epsilon || y1 >= y2 + r2.height - epsilon
        );
      })
    );
}

// Hoist top 1-2 deep fixed dialogs to body level for overlay detection
const _fc = [...domCopy.querySelectorAll('*')].filter(el => {
  if (el.parentNode === domCopy) return false;
  const info = getNodeInfo(el);
  if (!info?.rect || info.style.position !== 'fixed') return false;
  const r = info.rect, cover = (r.width * r.height) / viewportArea;
  const cd = Math.abs((r.left + r.width/2) - window.innerWidth/2) / window.innerWidth;
  return cover > 0.15 && cover < 1.0 && cd < 0.3 && el.querySelector('button, input, a, [role="button"], iframe');
}).filter((el, _, arr) => !arr.some(o => o !== el && o.contains(el)))
  .sort((a, b) => (getNodeInfo(b).rect.width * getNodeInfo(b).rect.height) - (getNodeInfo(a).rect.width * getNodeInfo(a).rect.height))
  .slice(0, 2);
_fc.forEach(el => {
  const r = getNodeInfo(el).rect;
  if (__debug) {
    console.log(
      '[simphtml] Hoisted fixed dialog:',
      el.tagName + (el.id ? '#' + el.id : '') + (el.className ? '.' + String(el.className).split(' ')[0] : ''),
      Math.round(r.width) + 'x' + Math.round(r.height),
      Math.round(100 * r.width * r.height / viewportArea) + '%'
    );
  }
  el.parentNode.removeChild(el);
  domCopy.appendChild(el);
});
const result = analyzeNode(domCopy);
domCopy.querySelectorAll('[data-mark^="R:"]').forEach(el=>el.parentNode?.removeChild(el));
let root = domCopy;
while (root.children.length === 1) {
  root = root.children[0];
}
for (let ii = 0; ii < 3; ii++) {
  root.querySelectorAll('div').forEach(div => (!div.textContent.trim() && div.children.length === 0) && div.remove());
}
root.querySelectorAll('[data-mark]').forEach(e => e.removeAttribute('data-mark'));
root.removeAttribute('data-mark');
root.querySelectorAll('iframe').forEach(f => {
  if (f.children.length) {
    const d = document.createElement('div');
    for (const a of f.attributes) d.setAttribute(a.name, a.value);
    d.setAttribute('data-tag', 'iframe');
    while (f.firstChild) d.appendChild(f.firstChild);
    f.parentNode.replaceChild(d, f);
  }
});
const __html = root.outerHTML;
const __notes = [];
if (__skipLayout) __notes.push('layout skipped: ' + __totalElements + ' elements > ' + __layoutLimit + ' (visibility by attributes only; use read/scan --fast for text)');
if (__budgetHit) __notes.push('truncated: scanned ' + __visited + ' nodes (budget ' + __nodeBudget + ')');
if (__depthCapHit) __notes.push('truncated: DOM deeper than ' + __ABC_MAX_DOM_DEPTH + ' levels');
return __notes.length ? '<!-- scan: ' + __notes.join(' | ') + ' -->\n' + __html : __html;
    }

// 这里**不要**再写一个裸的 `optHTML()` 调用。历史上文件末尾就多这一句，
// 于是每次把这段脚本注入页面（哪怕只是准备调用 optHTML）都会先对整页做一遍
// 全量克隆：多付一次秒级开销，还会在页面上留下一次多余的布局读取。

// 临时全局入口。
///
/// 为什么需要它：daemon 的注入脚本是 `<本文件>\nreturn optHTML(false, opts)`，
/// 两段拼在**同一个脚本**里，所以 optHTML 必须是能被裸标识符解析到的绑定。
/// IIFE 之后它已经不在脚本作用域里，只能挂到 window 上（全局对象属性同样参与
/// 标识符解析，非枚举属性也一样）。
///
/// 三条不变量：
///   * 不可枚举 —— Object.keys(window) / for...in 看不到；
///   * 可配置   —— stealth scrub 的 delete 能成功；
///   * 本次任务结束即还原 —— 页面之前若占用了同名全局，按快照写回去；没有占用就删掉。
///     调用方的 `return optHTML(...)` 是同步的，setTimeout(0) 一定在其后执行。
function publishTempGlobal(name, value) {
  let previous = null;
  let had = false;
  try {
    had = Object.prototype.hasOwnProperty.call(window, name);
    previous = had ? Object.getOwnPropertyDescriptor(window, name) : null;
  } catch (_) {}
  try {
    Object.defineProperty(window, name, {
      value: value, writable: true, enumerable: false, configurable: true
    });
  } catch (_) {
    // 页面上已有不可配置的同名绑定：属性描述改不动，赋值至少让本次调用能解析到。
    try { window[name] = value; } catch (_) {}
  }
  try {
    setTimeout(function () {
      if (had && previous) {
        try { Object.defineProperty(window, name, previous); return; } catch (_) {}
      }
      try { if (window[name] === value) delete window[name]; } catch (_) {}
    }, 0);
  } catch (_) {}
}

publishTempGlobal('optHTML', optHTML);
})();
