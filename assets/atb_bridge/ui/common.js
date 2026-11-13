/* popup 与 options 共用的工具。
 *
 * 只放「两边行为必须一致」的东西：错误文案、提示条、设置读写、主题/密度应用、剪贴板。
 * 页面特有的逻辑留在各自的 js 里。
 *
 * 依赖：ui/theme-boot.js 已先执行（提供 window.__atbSetTheme）。
 */
(function () {
  const DEFAULTS = {
    themeMode: 'auto',
    popupWidthMode: 'standard',
    uiDensity: 'standard',
    showBadge: false,
    clipboardTtlSec: 30,
    /** `inject --persist` 的默认过期时间（秒）；永久驻留必须由 CLI 显式确认。 */
    persistTtlSec: 7200,
  };

  // 下拉框只提供离散档位；storage 里出现非法值时回到默认值，不在 UI 上静默夹紧。
  const PERSIST_TTL_VALUES = [1800, 7200, 28800, 86400];

  const MSG_TTL_OK_MS = 4000;
  const msgTimers = new WeakMap();

  function normalizePersistTtl(value) {
    const n = Number(value);
    return PERSIST_TTL_VALUES.includes(n) ? n : DEFAULTS.persistTtlSec;
  }

  /**
   * 把底层异常映射成用户能行动的中文提示。
   * 直接展示 chrome 的 "Could not establish connection" 这类原文，用户无法自查。
   */
  function friendlyError(error) {
    const raw = String((error && error.message) || error || '');
    if (!raw) return '未知错误';
    if (/Could not establish connection|Receiving end does not exist/i.test(raw)) {
      return '扩展后台正在重启，请稍后重试，或点「重新加载扩展」';
    }
    if (/message port closed|The message port closed/i.test(raw)) {
      return '扩展后台没有响应，请稍后重试，或点「重新加载扩展」';
    }
    if (/Extension context invalidated|context invalidated/i.test(raw)) {
      return '扩展已更新，请关闭并重新打开这个页面';
    }
    if (/Cannot access contents of|chrome:\/\/|chrome-extension:\/\//i.test(raw)) {
      return '当前标签页不允许访问（chrome:// 或扩展页），请先切到普通网页';
    }
    if (/No tab with id|Invalid tab ID/i.test(raw)) {
      return '当前标签页已关闭，请重新打开一个网页';
    }
    if (/Clipboard|clipboard|Document is not focused/i.test(raw)) {
      return '剪贴板操作被浏览器拒绝（需要焦点与用户手势），请重试';
    }
    if (/^unknown$/i.test(raw.trim())) return '扩展后台返回未知错误';
    return raw;
  }

  /** 统一的背景消息调用：失败抛 Error，成功返回 data。 */
  async function send(message) {
    if (!globalThis.chrome || !chrome.runtime || !chrome.runtime.sendMessage) {
      throw new Error('扩展后台不可用');
    }
    const resp = await chrome.runtime.sendMessage(message);
    if (!resp) throw new Error('扩展后台没有返回结果');
    if (resp.ok === false) throw new Error(resp.error || resp.error_code || 'unknown');
    return resp.data === undefined ? {} : resp.data;
  }

  /** 提示条：成功 4s 自动淡出，错误保留（用户需要读完）。 */
  function setMsg(el, text, kind, ttlMs) {
    if (!el) return;
    const previous = msgTimers.get(el);
    if (previous) {
      clearTimeout(previous);
      msgTimers.delete(el);
    }
    el.textContent = text || '';
    el.className = 'msg' + (kind === 'err' ? ' err' : kind === 'ok' ? ' ok' : '');
    el.setAttribute('role', kind === 'err' ? 'alert' : 'status');
    el.setAttribute('aria-live', kind === 'err' ? 'assertive' : 'polite');
    if (text && kind !== 'err') {
      const ttl = ttlMs > 0 ? ttlMs : MSG_TTL_OK_MS;
      msgTimers.set(el, setTimeout(() => { el.textContent = ''; }, ttl));
    }
  }

  function clearMsg(el) {
    if (!el) return;
    const previous = msgTimers.get(el);
    if (previous) {
      clearTimeout(previous);
      msgTimers.delete(el);
    }
    el.textContent = '';
  }

  async function readSettings() {
    try {
      const got = await chrome.storage.local.get(DEFAULTS);
      return Object.assign({}, DEFAULTS, got || {});
    } catch (_) {
      return Object.assign({}, DEFAULTS);
    }
  }

  async function writeSetting(key, value) {
    try {
      await chrome.storage.local.set({ [key]: value });
      return true;
    } catch (_) {
      return false;
    }
  }

  function onSettingsChanged(callback) {
    try {
      chrome.storage.onChanged.addListener((changes, area) => {
        if (area && area !== 'local') return;
        const next = {};
        let touched = false;
        for (const key of Object.keys(changes)) {
          if (key in DEFAULTS) {
            next[key] = changes[key].newValue;
            touched = true;
          }
        }
        if (touched) callback(next);
      });
    } catch (_) {}
  }

  /** 把 themeMode / uiDensity 落到 <html>。popup 的宽度由调用方单独处理。 */
  function applyAppearance(settings) {
    const theme = settings.themeMode;
    if (typeof window.__atbSetTheme === 'function') {
      window.__atbSetTheme(theme === 'light' || theme === 'dark' ? theme : 'auto');
    }
    const density = settings.uiDensity === 'compact' ? 'compact' : 'standard';
    document.documentElement.dataset.density = density;
  }

  function fmtClock(ts) {
    const n = Number(ts);
    if (!n) return '-';
    try {
      return new Date(n).toLocaleTimeString('zh-CN', { hour12: false });
    } catch (_) {
      return new Date(n).toISOString().slice(11, 19);
    }
  }

  function fmtAgo(ts) {
    const n = Number(ts);
    if (!n) return '暂无';
    const diff = Date.now() - n;
    if (diff < 0) return '刚刚';
    if (diff < 5000) return '刚刚';
    if (diff < 60000) return Math.round(diff / 1000) + ' 秒前';
    if (diff < 3600000) return Math.round(diff / 60000) + ' 分钟前';
    if (diff < 86400000) return Math.round(diff / 3600000) + ' 小时前';
    return Math.round(diff / 86400000) + ' 天前';
  }

  function hostOf(url) {
    try {
      const u = new URL(String(url));
      return u.host || u.protocol;
    } catch (_) {
      return '-';
    }
  }

  function shorten(text, max) {
    const raw = String(text == null ? '' : text);
    const limit = max > 0 ? max : 18;
    return raw.length > limit ? raw.slice(0, limit - 1) + '…' : raw;
  }

  /**
   * 选一个「能读 cookie 的」标签页：http(s) / file，按 lastAccessed 取最近用过的。
   *
   * 为什么不能直接 query({active:true})：options 是标签页，popup 页面模式
   * （popup.html?page=1）也是标签页，而且它们自己就是当前窗口里 active 的那个。
   * 那样取到的 url 是 chrome-extension://，cookie 分区会把扩展 ID 当站点、
   * 查询永远为空。两处必须共用同一套选取口径。
   */
  async function pickCookieTab() {
    const tabs = await chrome.tabs.query({ currentWindow: true });
    const usable = (tabs || []).filter((tab) => tab && /^(https?|file):/i.test(String(tab.url || '')));
    usable.sort((a, b) => Number(b.lastAccessed || 0) - Number(a.lastAccessed || 0));
    return usable[0] || null;
  }

  /** 剪贴板写入；navigator.clipboard 在非聚焦页面会抛，退回到选区复制。 */
  async function copyText(text) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch (_) {
      try {
        const area = document.createElement('textarea');
        area.value = text;
        area.setAttribute('readonly', '');
        area.style.position = 'fixed';
        area.style.opacity = '0';
        document.body.appendChild(area);
        area.select();
        const ok = document.execCommand('copy');
        area.remove();
        return ok;
      } catch (_) {
        return false;
      }
    }
  }

  let toastHost = null;

  function toast(text, kind) {
    if (!toastHost) {
      toastHost = document.createElement('div');
      toastHost.className = 'toast-host';
      document.body.appendChild(toastHost);
    }
    const node = document.createElement('div');
    node.className = 'toast' + (kind === 'err' ? ' err' : kind === 'ok' ? ' ok' : '');
    node.textContent = text;
    toastHost.appendChild(node);
    setTimeout(() => {
      node.style.opacity = '0';
      node.style.transform = 'translateY(4px)';
      node.style.transition = 'opacity 160ms ease, transform 160ms ease';
      setTimeout(() => node.remove(), 200);
    }, 2600);
  }

  /** 按钮忙碌态：保持原文案宽度不变，避免布局跳动。 */
  async function withBusy(button, task) {
    if (!button) return task();
    const label = button.textContent;
    button.disabled = true;
    button.dataset.busy = '1';
    const original = button.style.minWidth;
    button.style.minWidth = button.offsetWidth + 'px';
    try {
      return await task();
    } finally {
      button.disabled = false;
      delete button.dataset.busy;
      button.style.minWidth = original;
      button.textContent = label;
    }
  }

  /**
   * 识别当前浏览器型号与版本。
   *
   * 扩展页面能拿到 navigator.userAgentData（Chromium 系）：brands 给主版本，
   * getHighEntropyValues(['fullVersionList']) 给完整版本（如 156.0.8078.79）。
   * 非 Chromium 或 API 不可用时退回 UA 正则。
   */
  let browserInfoCache = null;

  function syncBrowserInfo() {
    const ua = navigator.userAgent || '';
    const brands = (navigator.userAgentData && navigator.userAgentData.brands) || [];
    const findBrand = (re) => brands.find((b) => re.test(String((b && b.brand) || '')));
    const checks = [
      [/Microsoft Edge/i, 'Edge', /Edg\/([\d.]+)/],
      [/Google Chrome/i, 'Chrome', /Chrome\/([\d.]+)/],
      [/Chromium/i, 'Chromium', /Chrome\/([\d.]+)/],
      [/Brave/i, 'Brave', /Chrome\/([\d.]+)/],
    ];
    let name = '';
    let version = '';
    for (const [brandRe, label, uaRe] of checks) {
      const hit = findBrand(brandRe);
      const uaHit = ua.match(uaRe);
      if (hit || uaHit) {
        name = label;
        version = (hit && hit.version) || (uaHit && uaHit[1]) || '';
        break;
      }
    }
    if (!name) {
      const m = ua.match(/(OPR|Firefox)\/([\d.]+)/);
      if (m) {
        name = m[1] === 'OPR' ? 'Opera' : 'Firefox';
        version = m[2];
      }
    }
    const platform = (navigator.userAgentData && navigator.userAgentData.platform)
      || (/Windows/.test(ua) ? 'Windows'
        : /Mac OS X/.test(ua) ? 'macOS'
          : /Android/.test(ua) ? 'Android'
            : /Linux/.test(ua) ? 'Linux' : '');
    return { name: name || '未知浏览器', version, platform, ua };
  }

  async function detectBrowser() {
    if (browserInfoCache) return browserInfoCache;
    const base = syncBrowserInfo();
    let full = base.version;
    try {
      const uaData = navigator.userAgentData;
      if (uaData && typeof uaData.getHighEntropyValues === 'function') {
        const hi = await uaData.getHighEntropyValues(['fullVersionList', 'uaFullVersion', 'platformVersion']);
        const list = Array.isArray(hi.fullVersionList) ? hi.fullVersionList : [];
        const prefer = base.name === 'Edge' ? /Microsoft Edge/i
          : base.name === 'Chrome' ? /Google Chrome/i : null;
        const hit = prefer ? list.find((b) => prefer.test(String((b && b.brand) || ''))) : list[0];
        if (hit && hit.version) full = hit.version;
        else if (hi.uaFullVersion) full = hi.uaFullVersion;
      }
    } catch (_) {
      // 高熵值可能被策略禁用：保留主版本号即可，不影响展示。
    }
    browserInfoCache = Object.assign({}, base, { version: full || '' });
    return browserInfoCache;
  }

  window.ATB = {
    DEFAULTS,
    PERSIST_TTL_VALUES,
    normalizePersistTtl,
    friendlyError,
    send,
    setMsg,
    clearMsg,
    readSettings,
    writeSetting,
    onSettingsChanged,
    applyAppearance,
    fmtClock,
    fmtAgo,
    hostOf,
    shorten,
    pickCookieTab,
    copyText,
    detectBrowser,
    toast,
    withBusy,
  };
})();
