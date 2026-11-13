// popup 的 UI 逻辑（v5.4.0 视觉重构；安全语义与消息契约保持不变）。
//
// 安全前提：
//   * 打开 popup **不自动拉取并明文渲染 cookie**；只有点「刷新」才请求，
//     且默认只显示 name + [H]/[S]/[P] 标记，值一律 •••；
//   * 「显示值」需要二次点击确认；popup 关闭 / 页面隐藏时立即恢复掩码；
//   * 「复制 cookies」写入系统剪贴板后按 TTL（默认 30s，范围 30..300）自动清除，
//     清除前尽量比对剪贴板内容是否仍是刚写入的文本（无读取权限时退化为盲清，
//     并在 UI 上如实说明）。
//
// 可访问性：状态区 role=status + aria-live=polite，错误区 role=alert；
// 底层 JS 异常先经 ATB.friendlyError() 映射成人能读懂的提示，不直接回显裸 message。
//
// 布局：顶部品牌栏 + 三段分区（状态 / 数据 / 设置），只有内容区滚动；
// 所有颜色与间距来自 ui/tokens.css，浅色深色同一套结构。

const COOKIE_TTL_MIN_SEC = 30;
const COOKIE_TTL_MAX_SEC = 300;
const COOKIE_TTL_DEFAULT_SEC = 30;
const STATUS_POLL_MS = 5000;

let lastCookieData = [];
let lastCookieText = '点「刷新」读取当前站点的 cookies。值默认打码，仅在本机显示。';
// 3：默认必须是掩码状态，任何「重新打开 / 重新读取」都要回到这里。
let cookieRevealed = false;
let revealArmedAt = 0;
let clipboardTimer = null;
let lastCopiedText = '';
let clipboardTtlSec = COOKIE_TTL_DEFAULT_SEC;
let filterText = '';
let activeTab = 'status';
let statusTimer = null;
let lastStatus = null;

const $ = (id) => document.getElementById(id);

// v11 的回归：本文件所有反馈都调 toastMsg(元素 id, 文案, 类型, ttl)，
// 而 ui/common.js 导出的是 ATB.setMsg(元素, ...)。缺这一层时每次反馈都会抛
// ReferenceError —— 调用点大多在 try 里，表现是「操作成功却报失败」且数据被清空
// （fetchCookies 的 catch 会把 lastCookieData 清掉），复制后的剪贴板定时清除也不会启动。
const toastMsg = (id, text, kind, ttlMs) => ATB.setMsg($(id), text, kind, ttlMs);

// 页面模式：只有显式带 ?page=1 时才启用（管理页的「打开弹窗页面预览」会带这个参数）。
// 不再用 chrome.tabs.getCurrent 猜上下文——这个 API 在部分版本/场景下会返回 tab，
// 把工具栏弹窗误判成页面，弹窗宽度反而被压窄。
const pageMode = new URLSearchParams(location.search).has('page');

function applyMode() {
  document.documentElement.dataset.mode = pageMode ? 'page' : 'popup';
  document.body.dataset.mode = pageMode ? 'page' : 'popup';
}

applyMode();

document.addEventListener('DOMContentLoaded', init);

async function init() {
  const settings = await ATB.readSettings();
  applyWidth(settings.popupWidthMode);
  ATB.applyAppearance(settings);
  clipboardTtlSec = clampTtl(settings.clipboardTtlSec);
  syncSettingsUI(settings);

  bindTabs(settings.popupActiveTab);
  bindHeader();
  bindConnectionActions();
  bindIdentityActions();
  bindAppearanceActions();
  bindDebugSettings();
  bindBadgeToggle();
  bindCookieTools();
  bindClipboardLifecycle();

  renderBuildInfo();
  fillBrowserInfo();
  refreshBridgeStatus();
  statusTimer = setInterval(() => {
    if (document.hidden) return;
    refreshBridgeStatus({ quiet: true });
  }, STATUS_POLL_MS);

  // 注意：这里**不**调用 fetchCookies —— 打开 popup 不产生任何 cookie 读取/渲染。
}

// ---------------------------------------------------------------------------
// 分区 / 外观
// ---------------------------------------------------------------------------

function bindTabs(initial) {
  activeTab = initial === 'data' || initial === 'settings' ? initial : 'status';
  const buttons = Array.from(document.querySelectorAll('.tab'));
  for (const button of buttons) {
    button.addEventListener('click', () => activateTab(button.dataset.tab, true));
    // 左右方向键在分区之间移动，符合 WAI-ARIA tablist 习惯。
    button.addEventListener('keydown', (event) => {
      if (event.key !== 'ArrowRight' && event.key !== 'ArrowLeft') return;
      const index = buttons.indexOf(button);
      const next = buttons[(index + (event.key === 'ArrowRight' ? 1 : buttons.length - 1)) % buttons.length];
      next.focus();
      activateTab(next.dataset.tab, true);
    });
  }
  activateTab(activeTab, false);
}

function activateTab(tab, persist) {
  const name = tab === 'data' || tab === 'settings' ? tab : 'status';
  activeTab = name;
  for (const button of document.querySelectorAll('.tab')) {
    const selected = button.dataset.tab === name;
    button.setAttribute('aria-selected', selected ? 'true' : 'false');
    button.tabIndex = selected ? 0 : -1;
  }
  for (const panel of document.querySelectorAll('.panel')) {
    panel.hidden = panel.id !== 'panel-' + name;
  }
  if (name === 'data') renderCookies();
  if (persist) ATB.writeSetting('popupActiveTab', name);
}

function applyWidth(mode) {
  const value = mode === 'compact' || mode === 'wide' ? mode : 'standard';
  // 页面模式（标签页里打开）按 900px 列渲染，紧凑/宽松只影响工具栏弹窗；
  // 下拉框仍显示用户保存的档位，避免「设置被悄悄改掉」的错觉。
  document.body.dataset.width = pageMode ? 'standard' : value;
  // 双保险：body 固定宽度之外，再给 <html> 一个 min-width。
  // 弹窗的窗口尺寸按文档首选宽度计算，任何“内容收缩”都不会把它压回窄条。
  const px = value === 'compact' ? 340 : value === 'wide' ? 560 : 420;
  document.documentElement.style.minWidth = pageMode ? '' : px + 'px';
  const select = $('widthMode');
  if (select) select.value = value;
}

/** 把 storage 里的设置同步到控件（popup 与 options 同时打开时保持一致）。 */
function syncSettingsUI(settings) {
  const theme = $('themeMode');
  if (theme) theme.value = settings.themeMode === 'light' || settings.themeMode === 'dark' ? settings.themeMode : 'auto';
  const density = $('densityMode');
  if (density) density.value = settings.uiDensity === 'compact' ? 'compact' : 'standard';
  if (settings.popupWidthMode) applyWidth(settings.popupWidthMode);
  const badge = $('showBadge');
  if (badge) badge.checked = settings.showBadge === true;
  const ttl = $('clipboardTtl');
  if (ttl) ttl.value = String(clampTtl(settings.clipboardTtlSec));
  const persistTtl = $('persistTtlSec');
  if (persistTtl) persistTtl.value = String(ATB.normalizePersistTtl(settings.persistTtlSec));
}

function bindAppearanceActions() {
  $('themeMode')?.addEventListener('change', async (event) => {
    const mode = event.target.value;
    ATB.applyAppearance({ themeMode: mode, uiDensity: document.documentElement.dataset.density });
    await ATB.writeSetting('themeMode', mode);
  });

  $('widthMode')?.addEventListener('change', async (event) => {
    applyWidth(event.target.value);
    await ATB.writeSetting('popupWidthMode', event.target.value);
  });

  $('densityMode')?.addEventListener('change', async (event) => {
    const density = event.target.value === 'compact' ? 'compact' : 'standard';
    document.documentElement.dataset.density = density;
    await ATB.writeSetting('uiDensity', density);
  });

  // 另一个页面改了设置：主题 / 宽度 / 间距 / 角标 / 剪贴板 TTL 都要跟着走。
  ATB.onSettingsChanged((changed) => {
    if (changed.themeMode || changed.uiDensity) {
      ATB.applyAppearance({
        themeMode: changed.themeMode || $('themeMode')?.value || 'auto',
        uiDensity: changed.uiDensity || document.documentElement.dataset.density || 'standard',
      });
    }
    if (changed.popupWidthMode) applyWidth(changed.popupWidthMode);
    if (changed.showBadge != null) {
      const badge = $('showBadge');
      if (badge) badge.checked = changed.showBadge === true;
    }
    if (changed.clipboardTtlSec != null) {
      clipboardTtlSec = clampTtl(changed.clipboardTtlSec);
      const ttl = $('clipboardTtl');
      if (ttl) ttl.value = String(clipboardTtlSec);
    }
    if (changed.persistTtlSec != null) {
      const persistTtl = $('persistTtlSec');
      if (persistTtl) persistTtl.value = String(ATB.normalizePersistTtl(changed.persistTtlSec));
    }
  });
}

/**
 * 调试注入默认值：persistTtlSec（daemon 未显式给 --ttl 时由扩展读取）。
 * 默认值由 ui/common.js 的 DEFAULTS 统一提供，popup 与 options 双向同步。
 */
function bindDebugSettings() {
  $('persistTtlSec')?.addEventListener('change', async (event) => {
    const value = ATB.normalizePersistTtl(event.target.value);
    event.target.value = String(value);
    const ok = await ATB.writeSetting('persistTtlSec', value);
    toastMsg('debugMsg', ok ? `持久注入默认过期时间已保存（${formatPersistTtl(value)}）` : '保存失败：扩展存储不可用', ok ? 'ok' : 'err');
  });
}

function formatPersistTtl(sec) {
  if (sec >= 3600 && sec % 3600 === 0) return `${sec / 3600} 小时`;
  return `${Math.round(sec / 60)} 分钟`;
}

function bindHeader() {
  const open = () => {
    try {
      chrome.runtime.openOptionsPage();
    } catch (_) {
      // 没有 openOptionsPage（极少见）时退回到直接打开管理页 URL。
      try { chrome.tabs.create({ url: chrome.runtime.getURL('options.html') }); } catch (_) {}
    }
  };
  $('openOptions')?.addEventListener('click', open);
  $('openOptionsBottom')?.addEventListener('click', open);
  // 弹窗太窄时的稳妥出口：同一份界面在标签页里按 860px 铺开。
  $('openPage')?.addEventListener('click', () => {
    try { chrome.tabs.create({ url: chrome.runtime.getURL('popup.html?page=1') }); } catch (_) {}
  });
}

async function refreshBridgeStatus(options) {
  const quiet = !!(options && options.quiet);
  const pill = $('connPill');
  try {
    const data = await ATB.send({ cmd: 'status' });
    lastStatus = data || {};
    renderStatus(lastStatus);
    return lastStatus;
  } catch (e) {
    const friendly = ATB.friendlyError(e);
    if (!quiet) {
      if (pill) {
        pill.className = 'pill err';
        $('connText').textContent = '读取失败';
      }
      const dot = $('statusDot');
      if (dot) dot.dataset.state = 'err';
      const title = $('statusTitle');
      if (title) title.textContent = '状态读取失败';
      const url = $('statusUrl');
      if (url) url.textContent = friendly;
      const detail = $('wsConnectError');
      if (detail) {
        detail.hidden = false;
        detail.textContent = '读取扩展状态失败：' + friendly;
      }
    }
    return null;
  }
}

function renderStatus(data) {
  const connected = !!data.wsConnected;
  // 三种状态与状态点同色：已连接(绿) / 连接失败(红) / 未连接(灰)。
  const failed = !connected && !!data.lastConnectError;
  const pill = $('connPill');
  if (pill) {
    pill.className = 'pill ' + (connected ? 'ok' : failed ? 'err' : '');
    $('connText').textContent = connected ? '已连接' : '未连接';
  }

  const dot = $('statusDot');
  if (dot) dot.dataset.state = connected ? 'ok' : failed ? 'err' : 'off';
  $('statusTitle').textContent = connected ? '已连接' : '未连接';
  $('statusUrl').textContent = data.wsUrl || '尚未建立 WebSocket 连接';

  $('profileLabelText').textContent = data.profileLabel || '未设置';
  const browserId = String(data.browserId || '');
  const browserNode = $('browserIdText');
  browserNode.textContent = browserId ? ATB.shorten(browserId, 18) : '-';
  browserNode.title = browserId;
  $('portText').textContent = data.wsPort ? String(data.wsPort) : '-';

  const trust = data.wsTrustMode === 'strict' ? '严格' : '兼容';
  $('trustText').textContent = data.wsTokenSet ? trust : `${trust}（未设 token）`;

  const fileNode = $('fileAccessStatus');
  fileNode.textContent = data.fileSchemeAccess ? '已启用' : '未启用';
  fileNode.className = data.fileSchemeAccess ? 'positive' : 'negative';

  const hb = data.heartbeat || {};
  $('rttText').textContent = Number.isFinite(hb.lastRttMs) && hb.lastRttMs >= 0
    ? `${Math.round(hb.lastRttMs)} ms`
    : '-';
  $('lastCommandText').textContent = ATB.fmtAgo(data.lastCommandAt);

  $('buildInfo').textContent = `v${getManifestVersion()}`;

  renderAuthWarning(data);
  renderConnectError(data);

  const labelInput = $('profileLabel');
  if (labelInput && document.activeElement !== labelInput) {
    labelInput.value = data.profileLabel || '';
  }
  const modeSelect = $('wsTrustMode');
  if (modeSelect && document.activeElement !== modeSelect) {
    modeSelect.value = data.wsTrustMode === 'strict' ? 'strict' : 'compat';
  }
}

// ---------------------------------------------------------------------------
// 角标
// ---------------------------------------------------------------------------

/**
 * 认证告警。
 * 兼容档（未配置 wsToken）下 daemon 运行期接受任何本机进程的连接，
 * 对方可以直接发命令接管本浏览器，不需要等端口空出来。
 */
function renderAuthWarning(data) {
  const box = $('authWarning');
  if (!box) return;
  if (!data || data.wsTokenSet) {
    box.hidden = true;
    box.textContent = '';
    return;
  }
  box.hidden = false;
  box.textContent = '当前为兼容档：任何本机进程都可能连入 daemon 并接管本浏览器。'
    + '建议执行 atb trust-extension --token 切到 token 档。';
}

function renderConnectError(data) {
  const box = $('wsConnectError');
  if (!box) return;
  const err = data && !data.wsConnected ? data.lastConnectError : null;
  if (!err) {
    box.hidden = true;
    box.textContent = '';
    return;
  }
  box.hidden = false;
  const when = err.at ? ATB.fmtClock(err.at) : '';
  const tokenNote = err.hadToken ? '已带 token' : '未带 token';
  const times = err.repeats ? `，重复 ${err.repeats} 次` : '';
  box.textContent = `连接失败（${when}${when ? '，' : ''}${tokenNote}${times}）：${err.message || '未知原因'}`;
}

// ---------------------------------------------------------------------------
// 连接 / 身份
// ---------------------------------------------------------------------------

function bindConnectionActions() {
  $('reconnect')?.addEventListener('click', async (event) => {
    const button = event.currentTarget;
    await ATB.withBusy(button, async () => {
      try {
        await ATB.send({ cmd: 'reconnect' });
        // 重建连接是异步的：连续探几次状态，别立刻报「还在重连」。
        let data = null;
        for (let i = 0; i < 4; i += 1) {
          await sleep(350);
          data = await refreshBridgeStatus({ quiet: true });
          if (data && data.wsConnected) break;
        }
        if (data && data.wsConnected) {
          ATB.toast('已重新连接', 'ok');
        } else {
          const box = $('wsConnectError');
          if (box && !box.hidden) ATB.toast('重连请求已发出，仍在连接中', '');
          else ATB.toast('重连请求已发出，仍在连接中', '');
        }
      } catch (e) {
        ATB.toast('重连失败：' + ATB.friendlyError(e), 'err');
      }
    });
  });

  $('port')?.addEventListener('input', () => ATB.clearMsg($('portMsg')));
  $('savePort')?.addEventListener('click', savePort);
  $('wsToken')?.addEventListener('input', () => ATB.clearMsg($('wsTokenMsg')));
  $('saveWsToken')?.addEventListener('click', (event) => applyWsToken(event.currentTarget, $('wsToken').value.trim()));
  $('clearWsToken')?.addEventListener('click', (event) => applyWsToken(event.currentTarget, ''));
  $('wsTrustMode')?.addEventListener('change', saveTrustMode);
  $('reloadExt')?.addEventListener('click', (event) => {
    const button = event.currentTarget;
    button.disabled = true;
    button.textContent = '正在重载…';
    try { chrome.runtime.reload(); } catch (_) {}
  });
}

function bindIdentityActions() {
  $('profileLabel')?.addEventListener('input', () => ATB.clearMsg($('profileMsg')));
  $('saveProfileLabel')?.addEventListener('click', () => setProfileLabel($('profileLabel').value));
  $('clearProfileLabel')?.addEventListener('click', () => {
    $('profileLabel').value = '';
    setProfileLabel(null);
  });
}

async function savePort() {
  const input = $('port');
  const button = $('savePort');
  await ATB.withBusy(button, async () => {
    try {
      const port = Number(input ? input.value : NaN);
      if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('端口必须是 1-65535');
      if (port === 18767) throw new Error('18767 是 atb API 端口，请换一个插件端口');
      await ATB.send({ cmd: 'setPort', port });
      const data = await refreshBridgeStatus({ quiet: true });
      const connected = !!(data && data.wsConnected);
      toastMsg('portMsg', `已保存端口 ${port}，${connected ? '已连接' : '正在重连（当前未连接）'}`, 'ok');
    } catch (e) {
      toastMsg('portMsg', '保存失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

async function applyWsToken(button, token) {
  await ATB.withBusy(button, async () => {
    try {
      const resp = await ATB.send({ cmd: 'setWsToken', token });
      // 不在 popup 里回显 token：保存后立刻清空输入框。
      const input = $('wsToken');
      if (input) input.value = '';
      await refreshBridgeStatus({ quiet: true });
      toastMsg('wsTokenMsg', resp.wsTokenSet ? '已保存 token，正在重连' : '已清空 token，正在重连', 'ok');
    } catch (e) {
      toastMsg('wsTokenMsg', '保存失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

async function saveTrustMode() {
  const select = $('wsTrustMode');
  const mode = select.value === 'strict' ? 'strict' : 'compat';
  try {
    const data = await ATB.send({ cmd: 'setWsTrustMode', mode });
    await refreshBridgeStatus({ quiet: true });
    if (data && data.effective === false) {
      toastMsg('trustMsg', '已切换为严格档，但还没设置 token：严格档要等 token 设置后才真正生效', 'err');
    } else {
      toastMsg('trustMsg', `已切换为${mode === 'strict' ? '严格' : '兼容'}档`, 'ok');
    }
  } catch (e) {
    toastMsg('trustMsg', '切换失败：' + ATB.friendlyError(e), 'err');
  }
}

async function setProfileLabel(label) {
  try {
    const data = await ATB.send({ cmd: 'setProfileLabel', label });
    await refreshBridgeStatus({ quiet: true });
    toastMsg('profileMsg', `Profile Label：${(data && data.profileLabel) || '已清空'}`, 'ok');
  } catch (e) {
    toastMsg('profileMsg', '保存失败：' + ATB.friendlyError(e), 'err');
  }
}

// ---------------------------------------------------------------------------
// 角标
// ---------------------------------------------------------------------------

/**
 * 「在页面上显示状态角标」开关（storage.local.showBadge，缺省 false）。
 * 默认关：角标是页面里可见的 DOM，站点 querySelector 一下就能判定访客是自动化工具。
 */
function bindBadgeToggle() {
  const box = $('showBadge');
  if (!box) return;

  box.addEventListener('change', async () => {
    const showBadge = box.checked;
    try {
      await ATB.writeSetting('showBadge', showBadge);
    } catch (e) {
      toastMsg('showBadgeMsg', '保存角标开关失败：' + ATB.friendlyError(e), 'err');
      return;
    }
    toastMsg('showBadgeMsg', showBadge ? '角标已开启（已打开的标签页即时生效）' : '角标已关闭', 'ok');
    // 拿不到活动标签页（chrome://、扩展页、无活动窗口）时直接忽略：
    // storage.onChanged 已经覆盖了绝大多数页面。
    try {
      const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
      if (tab && tab.id != null) await chrome.tabs.sendMessage(tab.id, { cmd: 'badgeConfig' });
    } catch (_) {}
  });
}

// ---------------------------------------------------------------------------
// Cookie 区
// ---------------------------------------------------------------------------

function bindCookieTools() {
  $('refresh')?.addEventListener('click', fetchCookies);
  $('copyCookies')?.addEventListener('click', copyCookies);
  $('toggleCookies')?.addEventListener('click', toggleCookieValues);

  const select = $('clipboardTtl');
  if (select) {
    select.value = String(clipboardTtlSec);
    select.addEventListener('change', async () => {
      clipboardTtlSec = clampTtl(Number(select.value));
      select.value = String(clipboardTtlSec);
      await ATB.writeSetting('clipboardTtlSec', clipboardTtlSec);
      // 已经复制过：按新的 TTL 重新计时，别让旧的长 TTL 继续生效。
      if (lastCopiedText) armClipboardClear();
    });
  }

  const filter = $('cookieFilter');
  if (filter) {
    filter.addEventListener('input', () => {
      filterText = filter.value.trim();
      renderCookies();
    });
  }
}

function clampTtl(value) {
  const raw = Math.floor(Number(value));
  if (!Number.isFinite(raw) || raw <= 0) return COOKIE_TTL_DEFAULT_SEC;
  return Math.min(COOKIE_TTL_MAX_SEC, Math.max(COOKIE_TTL_MIN_SEC, raw));
}

function flagsOf(cookie) {
  return (cookie.httpOnly ? 'H' : '') + (cookie.secure ? 'S' : '') + (cookie.partitionKey ? 'P' : '');
}

function visibleCookies() {
  if (!filterText) return lastCookieData;
  const q = filterText.toLowerCase();
  return lastCookieData.filter((c) => {
    const hay = `${c.name || ''} ${flagsOf(c)}`.toLowerCase();
    return hay.includes(q);
  });
}

/** 默认只渲染 name + 标记 + 条数，值用 •••。 */
function renderCookies() {
  const list = $('cookieList');
  const empty = $('cookieEmpty');
  if (!list || !empty) return;

  const count = $('cookieCount');
  if (count) {
    count.textContent = String(lastCookieData.length);
    count.hidden = lastCookieData.length === 0;
  }

  list.textContent = '';
  const rows = visibleCookies();

  if (!lastCookieData.length) {
    empty.hidden = false;
    empty.textContent = lastCookieText;
    return;
  }
  if (!rows.length) {
    empty.hidden = false;
    empty.textContent = `没有匹配「${filterText}」的 cookie`;
    return;
  }
  empty.hidden = true;

  const fragment = document.createDocumentFragment();
  for (const c of rows) {
    const item = document.createElement('li');
    item.className = 'cookie-item' + (c.httpOnly || c.secure ? ' is-secret' : '');

    const name = document.createElement('span');
    name.className = 'cookie-name';
    name.textContent = c.name || '(未命名)';
    name.title = c.name || '';

    const flags = document.createElement('span');
    flags.className = 'cookie-flags';
    if (c.httpOnly) flags.appendChild(makeFlag('H', 'httpOnly'));
    if (c.secure) flags.appendChild(makeFlag('S', 'secure'));
    if (c.partitionKey) flags.appendChild(makeFlag('P', '分区'));

    const value = document.createElement('span');
    value.className = 'cookie-value';
    // 3：掩码只由 cookieRevealed 决定，默认永远是 •••。
    const shown = cookieRevealed ? String(c.value == null ? '' : c.value) : '•••';
    value.textContent = shown;
    // 列宽固定 + 单行省略：完整值放 title，避免长 cookie 把卡片撑破。
    value.title = cookieRevealed ? String(c.value == null ? '' : c.value) : '点「显示值」并二次确认后查看';

    item.append(name, flags, value);
    fragment.appendChild(item);
  }
  list.appendChild(fragment);
}

function makeFlag(text, title) {
  const node = document.createElement('span');
  node.className = 'flag' + (text === 'H' ? ' h' : '');
  node.textContent = text;
  node.title = title;
  return node;
}

/** 「显示值」二次确认：第一次点击只是解锁 5 秒，第二次才真的展开。 */
function toggleCookieValues() {
  const button = $('toggleCookies');
  if (!lastCookieData.length) {
    toastMsg('cookieMsg', '先点「刷新」获取 cookies', '');
    return;
  }
  if (cookieRevealed) {
    cookieRevealed = false;
    revealArmedAt = 0;
    if (button) button.textContent = '显示值';
    renderCookies();
    toastMsg('cookieMsg', '已恢复打码显示', 'ok');
    return;
  }
  const now = Date.now();
  if (!revealArmedAt || now - revealArmedAt > 5000) {
    revealArmedAt = now;
    if (button) button.textContent = '再次点击确认';
    toastMsg('cookieMsg', '明文会把 cookie 值画到屏幕上（含 httpOnly/secure），5 秒内再点一次确认', '', 5000);
    return;
  }
  cookieRevealed = true;
  revealArmedAt = 0;
  if (button) button.textContent = '隐藏值';
  renderCookies();
  toastMsg('cookieMsg', '已显示明文；关闭 popup 会自动恢复打码', 'ok');
}

/** 关闭 popup / 页面隐藏时恢复掩码（不重新请求，避免多余 IO）。 */
function hideCookieValues() {
  const button = $('toggleCookies');
  // 同 options：待确认态也要复位，避免切分区回来还挂着「再次点击确认」。
  revealArmedAt = 0;
  if (!cookieRevealed) {
    if (button) button.textContent = '显示值';
    return;
  }
  cookieRevealed = false;
  if (button) button.textContent = '显示值';
  renderCookies();
}

async function fetchCookies() {
  const button = $('refresh');
  await ATB.withBusy(button, async () => {
    try {
      // 页面模式（popup.html?page=1）本身就是 active 的扩展标签页，
      // query({active:true}) 只会拿到自己 → Cookies 分区把扩展 ID 当站点、永远查不到。
      // 复用 options 的选取口径：按 lastAccessed 选最近的 http(s)/file 标签页。
      const tab = pageMode
        ? await ATB.pickCookieTab()
        : (await chrome.tabs.query({ active: true, currentWindow: true }))[0];
      if (!tab || !tab.url) {
        lastCookieData = [];
        lastCookieText = pageMode
          ? '当前窗口没有 http/https/file 标签页，先打开一个网页再试'
          : '当前没有可读取的标签页';
        $('siteHost').textContent = '-';
        renderCookies();
        toastMsg('cookieMsg', lastCookieText, 'err');
        return;
      }
      $('siteHost').textContent = ATB.hostOf(tab.url);
      $('siteHost').title = tab.url;
      const resp = await ATB.send({ cmd: 'cookies', url: tab.url });
      // 后台回包是数组；空数组与「没有 cookies」是两种不同文案，别混。
      const items = Array.isArray(resp) ? resp : (Array.isArray(resp && resp.data) ? resp.data : []);
      if (!items.length) {
        lastCookieData = [];
        lastCookieText = '该站点没有 cookies';
        renderCookies();
        toastMsg('cookieMsg', '该站点没有 cookies', '');
        return;
      }
      lastCookieData = items;
      cookieRevealed = false;
      revealArmedAt = 0;
      const toggle = $('toggleCookies');
      if (toggle) toggle.textContent = '显示值';
      renderCookies();
      toastMsg('cookieMsg', `已读取 ${items.length} 条（值默认打码）`, 'ok');
    } catch (e) {
      lastCookieData = [];
      lastCookieText = '读取失败：' + ATB.friendlyError(e);
      renderCookies();
      toastMsg('cookieMsg', '读取 cookies 失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

async function copyCookies() {
  const include = $('includeHttpOnly');
  const includeSensitive = !!(include && include.checked);
  if (!lastCookieData.length) {
    toastMsg('cookieMsg', '没有可复制的 cookies，先点「刷新」', '');
    return;
  }
  // 默认只复制普通 cookie；httpOnly/secure 必须显式勾选（二者都视作敏感条目）。
  const picked = lastCookieData.filter((c) => includeSensitive || !(c.httpOnly || c.secure));
  const skipped = lastCookieData.length - picked.length;
  if (!picked.length) {
    toastMsg('cookieMsg', `未复制：${skipped} 条全是 httpOnly/secure，请勾选「含 httpOnly/secure」`, 'err');
    return;
  }
  lastCopiedText = picked.map((c) => `${c.name}=${c.value}`).join('; ');
  const ok = await ATB.copyText(lastCopiedText);
  if (!ok) {
    lastCopiedText = '';
    toastMsg('cookieMsg', '复制失败：浏览器拒绝了剪贴板写入，请重试', 'err');
    return;
  }
  const note = `已复制 ${picked.length} 条${includeSensitive ? '（含 httpOnly/secure）' : ''}`
    + (skipped ? `，跳过 ${skipped} 条敏感项` : '')
    + `，${clipboardTtlSec} 秒后自动清除`;
  toastMsg('cookieMsg', note, 'ok', clipboardTtlSec * 1000);
  armClipboardClear();
}

/** 复制后启动定时清除。重复复制时重置计时。 */
function armClipboardClear() {
  if (clipboardTimer) {
    clearTimeout(clipboardTimer);
    clipboardTimer = null;
  }
  if (!lastCopiedText) return;
  clipboardTimer = setTimeout(() => {
    clipboardTimer = null;
    clearClipboard('timeout');
  }, clampTtl(clipboardTtlSec) * 1000);
}

/**
 * 清除剪贴板里的 cookie 副本。
 *
 * 先读一次做比对：内容已经变了（用户又复制了别的东西）就不再覆盖，避免把用户的
 * 新剪贴板内容清掉。没有 clipboardRead 权限时 readText 会抛错 —— 这种情况下
 * 只能盲清（安全优先），并在 UI 上说明「无法校验」。
 */
async function clearClipboard(reason) {
  if (!lastCopiedText) return;
  let verified = false;
  let same = true;
  try {
    if (navigator.clipboard && navigator.clipboard.readText) {
      const current = await navigator.clipboard.readText();
      verified = true;
      same = current === lastCopiedText;
    }
  } catch (_) {
    verified = false;
  }
  if (verified && !same) {
    lastCopiedText = '';
    toastMsg('cookieMsg', '剪贴板内容已被你替换，未清空（原副本已被覆盖）', '');
    return;
  }
  const cleared = await ATB.copyText('');
  if (cleared) {
    lastCopiedText = '';
    toastMsg('cookieMsg', verified
      ? '已自动清除剪贴板里的 cookies'
      : '已自动清除剪贴板（无法校验内容：缺少剪贴板读取权限）', 'ok');
  } else if (reason === 'timeout') {
    toastMsg('cookieMsg', '自动清除失败，请手动清空剪贴板', 'err');
  }
}

/** pagehide / beforeunload / 隐藏时立刻清一次，别等 TTL。 */
function bindClipboardLifecycle() {
  const flush = () => {
    if (lastCopiedText) {
      // unload 阶段异步 Promise 可能来不及完成，仍先发一次（能成功就成功）。
      try { navigator.clipboard.writeText(''); } catch (_) {}
      lastCopiedText = '';
    }
    if (clipboardTimer) {
      clearTimeout(clipboardTimer);
      clipboardTimer = null;
    }
  };
  window.addEventListener('pagehide', () => {
    flush();
    hideCookieValues();
    if (statusTimer) clearInterval(statusTimer);
  });
  window.addEventListener('beforeunload', flush);
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) hideCookieValues();
  });
}

// ---------------------------------------------------------------------------

function getManifestVersion() {
  try {
    return chrome.runtime.getManifest().version;
  } catch (_) {
    return '-';
  }
}

/**
 * 浏览器型号 + 版本：扩展页面能读到 navigator.userAgentData 的高熵版本，
 * 拿不到就退回 UA 主版本；完整 UA 放在 title 里，方便区分 Chrome / Edge / Chromium。
 */
function fillBrowserInfo() {
  ATB.detectBrowser().then((info) => {
    const node = $('browserInfoText');
    if (!node) return;
    const label = [info.name, info.version].filter(Boolean).join(' ');
    node.textContent = label || '-';
    node.title = [label, info.platform, info.ua].filter(Boolean).join('\n');
  }).catch(() => {});
}

function renderBuildInfo() {
  const version = getManifestVersion();
  const info = $('buildInfo');
  if (info) info.textContent = `v${version}`;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
