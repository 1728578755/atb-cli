;(function(){ // 只跳过 Streamlit 的内部端点页（/_stcore/*）。按标题匹配会误伤「标题里带 streamlit」
// 的普通网页（教程、博客），那些页面照样需要角标与状态。
if (/\/_stcore\//i.test(location.href)) return;
// 重注入时先把上一版留下的角标 + 配置监听撤掉，避免同一页面叠出两套 storage 回调。
if (window.__agentBrowserCliCleanup) window.__agentBrowserCliCleanup();
// 旧版本在这里装过一个追 TID 元素的 MutationObserver（每个 DOM 变更都跑），
// 现在这套通道已经废弃，这里保留一次清理调用，把旧注入留下的 observer 停掉。
if (window.__agentBrowserCliObserverCleanup) window.__agentBrowserCliObserverCleanup();

/**
 * 清理旧版本残留的角标节点。
 *
 * 只在 **top frame** 执行：manifest 里本脚本是 all_frames:true，
 * 旧实现无条件在每个 iframe 里跑 querySelectorAll('#atb-ind,#atb-style') 并 remove ——
 * 一是白付一次全文档查询（每个子框架一次），二是如果站点自己在 iframe 里用了
 * 同名 id（模板/第三方组件），会被我们删掉，属于越界改页面 DOM。
 * 角标本身只挂在 top frame（见下面 createBadge 的 window.self!==window.top 判断），
 * 子框架没有任何需要清理的东西。
 */
if (window.self === window.top) {
  try {
    document.querySelectorAll('#atb-ind,#atb-style').forEach(e => e.remove());
  } catch (_) {}
}

/**
 * 补做 persist_boot.js 没送达的持久注入。
 *
 * document_start 阶段 SW 常常还在启动（或刚被回收），persist-apply 消息会失败；
 * persist_boot 已本地重试 2 次，仍失败时把「URL → 条目名」写进
 * storage.local.persistBootPending。本脚本（document_idle）在这里按当前 URL 补发一次：
 *   * 条目名 → 从 persistInjections 取回代码（不在 pending 里存第二份代码）；
 *   * 成功（ok 且 applied 全部 ok）才删标记；
 *   * 超过 PERSIST_PENDING_TTL_MS 的标记一律清掉，避免长期堆积。
 * 每个 frame 只关心自己的 URL：persist_boot 注册时是 allFrames:false，只有 top frame
 * 会写这个标记，因此补发也只在 top frame 执行（否则每个 iframe 都要打一次 storage）。
 */
const PERSIST_PENDING_TTL_MS = 10 * 60 * 1000;

async function flushPersistPending() {
  try {
    const session = globalThis.chrome;
    if (!session?.storage?.local || !session?.runtime?.sendMessage) return;
    const store = await session.storage.local.get({ persistBootPending: {}, persistInjections: {} });
    const all = store.persistBootPending && typeof store.persistBootPending === 'object'
      ? store.persistBootPending
      : null;
    if (!all) return;
    const url = location.href;
    const now = Date.now();
    let dirty = false;
    for (const key of Object.keys(all)) {
      const item = all[key];
      if (!item || typeof item !== 'object' || !(now - Number(item.at || 0) <= PERSIST_PENDING_TTL_MS)) {
        delete all[key];
        dirty = true;
      }
    }
    const pending = all[url];
    const table = store.persistInjections && typeof store.persistInjections === 'object'
      ? store.persistInjections
      : {};
    const entries = pending && Array.isArray(pending.names)
      ? pending.names
        .map((name) => (table[name] && table[name].code
          ? { name, code: table[name].code, world: table[name].world || 'MAIN' }
          : null))
        .filter(Boolean)
      : [];
    if (!entries.length) {
      if (dirty) await session.storage.local.set({ persistBootPending: all });
      return;
    }
    const resp = await session.runtime.sendMessage({ type: 'persist-apply', entries });
    const applied = Array.isArray(resp?.applied) ? resp.applied : [];
    const failed = applied.filter((one) => one && one.ok !== true);
    if (resp?.ok === true && !failed.length) {
      delete all[url];
      dirty = true;
    }
    if (dirty) await session.storage.local.set({ persistBootPending: all });
  } catch (_) {
    // 补发失败不能影响页面本身；标记留着，下次导航继续尝试（TTL 到期自动清）。
  }
}

if (window.self === window.top) flushPersistPending();

/**
 * Render a right-side floating badge that reflects the real bridge connection status.
 *
 * 默认不渲染。历史版本无条件往每个页面插一个固定 id 的角标，
 * 站点只要 querySelector 那个 id 就能判定访客是自动化工具，
 * 这和「别被站点检测到自动化」直接冲突。现在分两层收紧：
 *   1) 只有用户在 popup 里显式勾选 showBadge 才挂载，关闭时连 style/定时器都不创建；
 *   2) 内部结构全部关在 closed shadow root 里，页面外面只剩一个中性 id 的空壳宿主。
 */
(function(){
  if(window.self!==window.top)return;

  // 宿主 id 用中性名字：旧 id 本身就是现成的自动化指纹，不再复用。
  const HOST_ID='atb-status-host';
  // 角标状态刷新策略（注释按实现写，不再宣称「纯事件驱动」）。
  //
  // 早期版本每个 top frame 每 5s 无条件发一条 runtime 消息，等于把 MV3 的 Service Worker
  // 钉在常驻状态（30s 无活动才允许回收），多标签用户既费电也让 SW 生命周期失去意义。
  //
  // 现在的行为：
  //   1) 轮询与**渲染**解耦：只要角标实例还在且没被用户手动关闭，就保留一个 30s 的
  //      低频状态刷新；是否真的显示由「最近 10s 内有没有命令」决定；
  //   2) 旧实现用「当前是否可见」控制是否轮询 —— 角标因 10s 无命令而隐藏后轮询就停了，
  //      下一次命令若没触发 visibility/focus（后台标签页、脚本直接下发），就再也没人
  //      把它点亮，只能等用户切标签页（自恢复死锁）；
  //   3) 「页面可见 / 窗口获得焦点 / bfcache 恢复」各立即探一次；
  //   4) 点关闭按钮后立刻停轮询（用户明确不想看），断开重连时重置 dismissed。
  //
  // 3) 为什么不能省：连接状态只有 background 侧知道，而 chrome.runtime.sendMessage 是
  // 「请求-响应」通道；目前唯一的反向通道是 popup 广播的 badgeConfig（只带配置、不带状态）。
  // 所以「ws 断开 / SW 被回收后重新唤起的角标」只能由页面自己按低频节奏问一次。
  // 30s 是这个取舍的上限：既能在 SW 被回收或 daemon 断开后几十秒内把角标切掉，
  // 又不会像 5s 常驻那样把 SW 一直钉住。
  const BADGE_POLL_MS=30000;

  // 当前挂载的角标实例（销毁函数）；null 表示页面上没有任何角标 DOM，也没有任何定时器。
  let inst=null;

  /**
   * 读 showBadge。缺省值必须当 false：开关不存在时宁可不显示，也不给站点留指纹。
   * 读失败（非扩展环境 / storage 不可用）同样按关闭处理。
   */
  async function readShowBadge(){
    try{
      const got=await globalThis.chrome?.storage?.local?.get({showBadge:false});
      return got?.showBadge===true;
    }catch(_){
      return false;
    }
  }

  function destroyBadge(){
    if(!inst)return;
    const teardown=inst;
    inst=null;
    try{ teardown(); }catch(_){}
  }

  function createBadge(){
    if(inst)return;
    // 宿主只是个空壳（自身不带任何视觉/布局痕迹），样式和结构全在 shadow root 里：
    // 页面 CSS 进不来，页面 JS 也 querySelector 不到内部节点。
    const host=document.createElement('div');
    host.id=HOST_ID;
    const root=host.attachShadow({mode:'closed'});
    const style=document.createElement('style');
    style.textContent = `:host{all:initial}
#atb-badge{position:fixed;top:40%;right:0;display:flex;align-items:center;gap:8px;
  width:200px;height:32px;box-sizing:border-box;padding:0 6px 0 10px;
  color:#e8eef7;border-radius:16px 0 0 16px;
  font:600 11.5px/32px "Segoe UI",system-ui,-apple-system,"PingFang SC","Microsoft YaHei UI",sans-serif;
  letter-spacing:.01em;z-index:99999;cursor:grab;white-space:nowrap;user-select:none;
  background:rgba(18,24,33,.94);border:1px solid rgba(255,255,255,.10);border-right:none;
  box-shadow:0 8px 22px rgba(8,12,20,.30);opacity:.85;overflow:visible;
  transform:translateX(172px);transition:transform .18s cubic-bezier(.2,.8,.2,1),opacity .18s ease,box-shadow .18s ease}
#atb-dot{flex:0 0 auto;width:7px;height:7px;border-radius:999px;background:#4ade80;box-shadow:0 0 0 3px rgba(74,222,128,.18)}
#atb-badge[data-connected="0"] #atb-dot{background:#f59e0b;box-shadow:0 0 0 3px rgba(245,158,11,.18)}
#atb-label{flex:1 1 auto;min-width:0;max-width:132px;overflow:hidden;text-overflow:ellipsis;font-size:11.5px;line-height:32px}
#atb-badge[data-expanded="1"]{opacity:.98;transform:translateX(0);box-shadow:0 10px 26px rgba(8,12,20,.38)}
#atb-badge[data-dragging="1"]{cursor:grabbing;transition:none}
#atb-close{position:relative;flex:0 0 auto;display:flex;align-items:center;justify-content:center;
  width:18px;height:18px;border-radius:999px;background:rgba(255,255,255,.10);color:#cbd5e1;
  font:600 12px/18px "Segoe UI",system-ui,sans-serif;text-align:center;opacity:0;pointer-events:none;cursor:pointer;
  transition:background .12s ease,color .12s ease}
#atb-badge[data-expanded="1"] #atb-close{opacity:1;pointer-events:auto}
#atb-close:hover{background:rgba(248,113,113,.22);color:#fecaca}
#atb-close::after{content:"本次隐藏";position:absolute;right:0;bottom:26px;padding:4px 8px;border-radius:6px;
  background:#0b1119;border:1px solid rgba(255,255,255,.10);color:#e8eef7;
  font:500 11px/16px "Segoe UI",system-ui,"Microsoft YaHei UI",sans-serif;white-space:nowrap;
  opacity:0;pointer-events:none;transform:translateY(4px);transition:opacity .12s ease,transform .12s ease}
#atb-close:hover::after{opacity:1;transform:translateY(0)}`;
    root.appendChild(style);
    const d=document.createElement('div');
    d.id='atb-badge';
    const dot=document.createElement('span');
    dot.id='atb-dot';
    const label=document.createElement('span');
    label.id='atb-label';
    label.textContent='ATB 已连接';
    const close=document.createElement('span');
    close.id='atb-close';
    close.textContent='×';
    d.appendChild(dot);
    d.appendChild(label);
    d.appendChild(close);
    root.appendChild(d);
    let collapseTimer = null;
    let dragging = false;
    let moved = false;
    let dragOffsetY = 0;
    let dismissed = false;
    let wasConnected = false;
    let lastSeenCommandAt = 0;
    let lastTop = Math.round(window.innerHeight * 0.4);
    d.style.top = lastTop + 'px';
    /**
     * 只更新状态与**渲染**，不碰定时器。
     * 显示条件：已连接 + 用户没手动关闭 + 最近 10s 内有命令。
     * 隐藏不等于「不轮询」—— 轮询由 ensurePolling 独立维持。
     */
    function setBadgeState(connected, detail, lastCommandAt) {
      // 手动关闭只在当前连接周期生效；断开后下次重新连接会再次显示。
      if (connected && !wasConnected) dismissed = false;
      wasConnected = connected;
      const commandAt = Number(lastCommandAt) || 0;
      if (commandAt > lastSeenCommandAt) lastSeenCommandAt = commandAt;
      const activeRecently = lastSeenCommandAt > 0 && Date.now() - lastSeenCommandAt <= 10000;
      d.style.display = connected && !dismissed && activeRecently ? 'flex' : 'none';
      label.textContent = connected ? 'ATB 已连接' : 'ATB 未连接';
      d.dataset.connected = connected ? '1' : '0';
      d.dataset.detail = detail || '';
      // 被手动关闭后不再重开轮询（setInterval 在 close 事件里已停掉）。
      if (!dismissed) ensurePolling();
    }
    function setExpanded(expanded) {
      d.dataset.expanded = expanded ? '1' : '0';
    }
    function clampTop(top) {
      return Math.max(8, Math.min(window.innerHeight - d.offsetHeight - 8, top));
    }
    /**
     * Poll background status and update the badge visibility.
     */
    async function refreshBadgeState() {
      try {
        const resp = await globalThis.chrome?.runtime?.sendMessage?.({ cmd: 'status' });
        const connected = !!resp?.ok && !!resp?.data?.wsConnected;
        setBadgeState(connected, resp?.data?.wsUrl || '', resp?.data?.lastCommandAt);
      } catch (e) {
        setBadgeState(false, (e && e.message) || '', 0);
      }
      // 注意：这里**不再**根据可见性启停轮询（见 ensurePolling 的注释）。
      ensurePolling();
    }
    d.addEventListener('mouseenter', () => {
      if (collapseTimer) clearTimeout(collapseTimer);
      setExpanded(true);
    });
    d.addEventListener('mouseleave', () => {
      if (collapseTimer) clearTimeout(collapseTimer);
      setExpanded(false);
    });
    d.addEventListener('pointerdown', (e) => {
      dragging = true;
      moved = false;
      dragOffsetY = e.clientY - d.getBoundingClientRect().top;
      d.dataset.dragging = '1';
      d.setPointerCapture(e.pointerId);
    });
    d.addEventListener('pointermove', (e) => {
      if (!dragging) return;
      const nextTop = clampTop(e.clientY - dragOffsetY);
      if (Math.abs(nextTop - lastTop) > 2) moved = true;
      lastTop = nextTop;
      d.style.top = lastTop + 'px';
    });
    d.addEventListener('pointerup', (e) => {
      dragging = false;
      d.dataset.dragging = '0';
      try { d.releasePointerCapture(e.pointerId); } catch (_) {}
    });
    close.addEventListener('pointerdown', (e) => { e.stopPropagation(); });
    close.addEventListener('click', (e) => {
      e.stopPropagation();
      dismissed = true;
      setExpanded(false);
      d.style.display = 'none';
      // 手动隐藏后立刻停轮询，不必等到下一个 tick。
      stopPolling();
    });
    const onResize = () => {
      lastTop = clampTop(lastTop);
      d.style.top = lastTop + 'px';
    };
    window.addEventListener('resize', onResize);
    (document.body||document.documentElement).appendChild(host);
    let refreshTimer = null;
    function stopPolling() {
      if (refreshTimer) { clearInterval(refreshTimer); refreshTimer = null; }
    }
    /**
     * 轮询与显示解耦：只要角标实例还在、且没被用户手动关闭，就一直保留
     * 30s 的低频状态刷新；**是否渲染**由 setBadgeState 里的 activeRecently 决定。
     * 旧实现按「当前是否可见」启停轮询：角标因 10s 无命令而隐藏后轮询就停了，
     * 下一次命令若没有触发 visibility/focus（后台标签页 / 脚本直接下发的命令），
     * 就再也没人把它点亮 —— 只能等用户切标签页，属于自恢复死锁。
     */
    function ensurePolling() {
      if (refreshTimer || dismissed) return;
      refreshTimer = setInterval(refreshBadgeState, BADGE_POLL_MS);
    }
    async function probeAndSync() {
      await refreshBadgeState();
    }
    function onVisibilityChange() {
      // 页面回到可见时立即刷新一次：后台期间的命令此刻才看得到。
      // 隐藏时不做事（轮询本身继续跑，浏览器会把 30s 定时器节流到分钟级）。
      if (document.hidden) return;
      probeAndSync();
    }
    setBadgeState(false, '');
    document.addEventListener('visibilitychange', onVisibilityChange);
    window.addEventListener('focus', probeAndSync);
    window.addEventListener('pageshow', probeAndSync);
    probeAndSync();

    inst = function teardownBadge() {
      if (collapseTimer) { clearTimeout(collapseTimer); collapseTimer = null; }
      stopPolling();
      document.removeEventListener('visibilitychange', onVisibilityChange);
      window.removeEventListener('focus', probeAndSync);
      window.removeEventListener('pageshow', probeAndSync);
      window.removeEventListener('resize', onResize);
      // 连宿主一起摘掉：关掉开关后页面上不该再剩下任何角标节点。
      host.remove();
    };
  }

  /**
   * storage 是主通道：popup 写 showBadge 后，已打开的标签页即时跟着开/关，不必等刷新。
   */
  const onStorageChanged = (changes, area) => {
    if (!changes || !('showBadge' in changes)) return;
    if (area && area !== 'local') return;
    if (changes.showBadge && changes.showBadge.newValue === true) createBadge();
    else destroyBadge();
  };

  /**
   * popup 额外广播的 badgeConfig：兜住「本页 content script 还没跑过 storage.get」的极短窗口。
   * 这里不信任消息里的布尔值，直接用 storage 的最新值对齐，避免两条通道打架。
   */
  const onRuntimeMessage = (msg) => {
    if (!msg || msg.cmd !== 'badgeConfig') return;
    readShowBadge().then((show) => {
      if (show) createBadge();
      else destroyBadge();
    });
  };

  try { globalThis.chrome?.storage?.onChanged?.addListener(onStorageChanged); } catch (_) {}
  try { globalThis.chrome?.runtime?.onMessage?.addListener(onRuntimeMessage); } catch (_) {}

  window.__agentBrowserCliCleanup = () => {
    destroyBadge();
    try { globalThis.chrome?.storage?.onChanged?.removeListener(onStorageChanged); } catch (_) {}
    try { globalThis.chrome?.runtime?.onMessage?.removeListener(onRuntimeMessage); } catch (_) {}
  };

  // 默认关闭：读到的不是明确的 true 就什么都不创建（无 DOM、无 style、无定时器）。
  readShowBadge().then((show) => { if (show) createBadge(); });
})();
})();
