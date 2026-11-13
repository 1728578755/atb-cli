// 管理页面（options.html）逻辑。
//
// 与 popup 共用同一套语义与安全约束：
//   * 打开页面不自动读取 cookie；
//   * 值默认打码，展示明文需要 5 秒内二次确认，切分区/离开页面立刻恢复；
//   * 复制写入剪贴板后按 TTL 自动清除，并在关闭页面时立刻清。
//
// 与 popup 的差异：
//   * 这里可以读到最近使用的普通网页标签页（popup 只能读当前活动标签页），
//     所以「当前站点」指的是本窗口最近访问的 http(s)/file 标签页；
//   * 多了自检、诊断、权限清单等专业信息，默认折叠在「诊断 / 关于」。

const SECTIONS = {
  overview: { title: '概览', sub: '连接状态与本机环境' },
  connection: { title: '连接', sub: 'daemon 端口、token 与信任档位' },
  identity: { title: '身份', sub: 'Profile 名称与只读标识' },
  cookies: { title: 'Cookies', sub: '读取、筛选与复制当前站点的 cookie' },
  appearance: { title: '外观', sub: '主题、弹窗宽度、间距与页面角标' },
  diagnostics: { title: '诊断', sub: '链路细节、自检与错误定位' },
  about: { title: '关于', sub: '版本、权限与数据边界' },
};

const COOKIE_TTL_MIN_SEC = 30;
const COOKIE_TTL_MAX_SEC = 300;
const SETTINGS_POLL_MS = 5000;

const $ = (id) => document.getElementById(id);

let lastStatus = null;
let lastError = null;
let cookieRows = [];
let cookieSourceUrl = '';
let revealed = false;
let revealArmedAt = 0;
let clipboardTimer = null;
let lastCopied = '';
let ttlSec = 30;
let filterText = '';
let activeSection = 'overview';
let pollTimer = null;

document.addEventListener('DOMContentLoaded', init);

async function init() {
  const settings = await ATB.readSettings();
  ATB.applyAppearance(settings);
  ttlSec = clampTtl(settings.clipboardTtlSec);
  syncAppearanceControls(settings);

  bindNav();
  bindTopActions();
  bindConnection();
  bindIdentity();
  bindCookies();
  bindAppearance();
  bindDebugSettings();
  bindAdvancedPermissions();
  bindDiagnostics();
  bindCopyButtons();
  bindLifecycle();

  renderAbout();
  fillBrowserInfo();
  void fillBuildStamp();
  await refreshStatus();
  await refreshManagementPermission();
  if (activeSection === 'cookies') renderCookieTable();
  pollTimer = setInterval(() => {
    if (document.hidden) return;
    refreshStatus({ quiet: true });
  }, SETTINGS_POLL_MS);
}

// ---------------------------------------------------------------------------
// 分区
// ---------------------------------------------------------------------------

function bindNav() {
  for (const button of document.querySelectorAll('.nav-item')) {
    button.addEventListener('click', () => activateSection(button.dataset.section, true));
  }
  const initial = (location.hash || '').replace('#', '');
  activateSection(SECTIONS[initial] ? initial : 'overview', false);
  window.addEventListener('hashchange', () => {
    const name = (location.hash || '').replace('#', '');
    if (SECTIONS[name]) activateSection(name, false);
  });
}

function activateSection(name, pushHash) {
  const key = SECTIONS[name] ? name : 'overview';
  activeSection = key;

  for (const button of document.querySelectorAll('.nav-item')) {
    if (button.dataset.section === key) button.setAttribute('aria-current', 'page');
    else button.removeAttribute('aria-current');
  }
  for (const section of document.querySelectorAll('.section')) {
    section.hidden = section.dataset.section !== key;
  }
  const meta = SECTIONS[key];
  $('pageTitle').textContent = meta.title;
  $('pageSub').textContent = meta.sub;
  document.title = `ATB · ${meta.title}`;

  if (pushHash) {
    const next = '#' + key;
    if (location.hash !== next) history.replaceState(null, '', next);
  }
  if (key !== 'cookies') hideCookieValues();
  if (key === 'cookies') renderCookieTable();
  window.scrollTo({ top: 0, behavior: 'auto' });
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

function bindTopActions() {
  $('btnRefresh')?.addEventListener('click', async (event) => {
    await ATB.withBusy(event.currentTarget, async () => {
      const data = await refreshStatus();
      ATB.toast(data ? '状态已刷新' : '状态读取失败', data ? 'ok' : 'err');
    });
  });
  $('btnReconnect')?.addEventListener('click', () => reconnect());
  $('ovReload')?.addEventListener('click', reloadExtension);
  // 这个按钮在本文件里长期零引用（外观分区另有一个 btnPreviewPopup），
  // 是已发布 UI 上的死按钮。与 btnPreviewPopup 走同一个地址，点击给出明确反馈。
  $('ovOpenPopup')?.addEventListener('click', () => {
    try {
      chrome.tabs.create({ url: chrome.runtime.getURL('popup.html?page=1') });
      ATB.toast('已在新标签页打开弹窗页面', 'ok');
    } catch (e) {
      ATB.toast('打开弹窗页面失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

async function reconnect() {
  const button = $('btnReconnect');
  await ATB.withBusy(button, async () => {
    try {
      await ATB.send({ cmd: 'reconnect' });
      let data = null;
      for (let i = 0; i < 5; i += 1) {
        await sleep(350);
        data = await refreshStatus({ quiet: true });
        if (data && data.wsConnected) break;
      }
      ATB.toast(data && data.wsConnected ? '已重新连接' : '重连请求已发出，仍在连接中', data && data.wsConnected ? 'ok' : '');
    } catch (e) {
      ATB.toast('重连失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

function reloadExtension() {
  if (!$('ovReload')) return;
  $('ovReload').disabled = true;
  $('ovReload').textContent = '正在重载…';
  try { chrome.runtime.reload(); } catch (_) {}
}

async function refreshStatus(options) {
  const quiet = !!(options && options.quiet);
  try {
    const data = await ATB.send({ cmd: 'status' });
    lastStatus = data || {};
    lastError = null;
    applyStatus(lastStatus);
    if (!quiet) renderDiagnostics(lastStatus);
    return lastStatus;
  } catch (e) {
    lastError = ATB.friendlyError(e);
    if (quiet) return null;
    applyStatus(null, lastError);
    return null;
  }
}

function applyStatus(data, errorText) {
  if (!data) {
    setSidePill('err', '读取失败');
    const state = $('statConnState');
    if (state) {
      state.textContent = '读取失败';
      state.className = 'stat-value err';
    }
    $('statConnSub').textContent = errorText || '无法读取扩展状态';
    return;
  }

  const connected = !!data.wsConnected;
  const failed = !connected && !!data.lastConnectError;
  setSidePill(connected ? 'ok' : failed ? 'err' : 'off', connected ? '已连接' : '未连接');

  const state = $('statConnState');
  state.textContent = connected ? '已连接' : '未连接';
  state.className = 'stat-value ' + (connected ? 'ok' : 'warn');
  $('statConnSub').textContent = data.wsUrl || '-';

  $('statPort').textContent = data.wsPort ? String(data.wsPort) : '-';
  $('statPortSub').textContent = connected ? 'daemon 已握手' : '等待扩展连接';

  const strict = data.wsTrustMode === 'strict';
  $('statTrust').textContent = strict ? '严格' : '兼容';
  $('statTrustSub').textContent = data.wsTokenSet ? 'token 已设置' : '未设置 token';

  const hb = data.heartbeat || {};
  const rtt = Number(hb.lastRttMs);
  $('statRtt').textContent = Number.isFinite(rtt) && rtt >= 0 ? `${Math.round(rtt)} ms` : '-';
  $('statRttSub').textContent = Number.isFinite(Number(hb.lastPongAgoMs))
    ? '最近 pong ' + ATB.fmtAgo(Date.now() - Number(hb.lastPongAgoMs))
    : '最近一次 ping/pong';

  $('ovProfile').textContent = data.profileLabel || '未设置';
  $('ovExtVersion').textContent = data.extensionVersion || '-';
  const fileNode = $('ovFileAccess');
  fileNode.textContent = data.fileSchemeAccess ? '已启用' : '未启用';
  fileNode.className = data.fileSchemeAccess ? 'positive' : 'negative';
  $('ovLastCommand').textContent = ATB.fmtAgo(data.lastCommandAt);
  $('ovWsUrl').textContent = data.wsUrl || '-';

  const portInput = $('portInput');
  if (portInput && document.activeElement !== portInput && data.wsPort) portInput.value = String(data.wsPort);
  const trustSelect = $('trustSelect');
  if (trustSelect && document.activeElement !== trustSelect) trustSelect.value = strict ? 'strict' : 'compat';
  const connUrl = $('connUrl');
  if (connUrl) connUrl.textContent = data.wsUrl || '-';

  $('idBrowser').textContent = data.browserId || '-';
  $('idProfile').textContent = data.profileId || '-';
  $('idExtension').textContent = data.extensionId || '-';

  const labelInput = $('labelInput');
  if (labelInput && document.activeElement !== labelInput) labelInput.value = data.profileLabel || '';

  $('aboutVersion').textContent = data.extensionVersion || '-';
  $('aboutId').textContent = data.extensionId || '-';
  $('optVersion').textContent = `v${getManifestVersion()}`;

  renderAuthWarning(data);
  renderConnectError(data);
  renderDiagnostics(data);
  // 没有 permissions API 时，用 status 里的 management_permission 兜底。
  if (!(chrome.permissions && typeof chrome.permissions.contains === 'function')) {
    renderManagementPermission(!!data.management_permission, true);
  }
}

function setSidePill(kind, text) {
  const pill = $('sideConnPill');
  if (!pill) return;
  pill.className = 'pill ' + (kind === 'ok' ? 'ok' : kind === 'err' ? 'err' : '');
  $('sideConnText').textContent = text;
}

function renderAuthWarning(data) {
  const box = $('ovAuthWarning');
  if (!box) return;
  if (!data || data.wsTokenSet) {
    box.hidden = true;
    box.textContent = '';
    return;
  }
  box.hidden = false;
  box.textContent = '当前为兼容档：本机任何进程都能连入 daemon 并接管浏览器。'
    + '到「连接」页设置 token，并执行 atb trust-extension --token 切到 token 档。';
}

function renderConnectError(data) {
  const box = $('ovConnectError');
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
// 连接
// ---------------------------------------------------------------------------

function bindConnection() {
  $('portInput')?.addEventListener('input', () => ATB.clearMsg($('portMsg')));
  $('tokenInput')?.addEventListener('input', () => ATB.clearMsg($('tokenMsg')));
  $('btnSavePort')?.addEventListener('click', savePort);
  $('btnSaveToken')?.addEventListener('click', (event) => applyToken(event.currentTarget, $('tokenInput').value.trim()));
  $('btnClearToken')?.addEventListener('click', (event) => applyToken(event.currentTarget, ''));
  $('trustSelect')?.addEventListener('change', saveTrust);
}

async function savePort() {
  await ATB.withBusy($('btnSavePort'), async () => {
    try {
      const port = Number($('portInput').value);
      if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('端口必须是 1-65535');
      if (port === 18767) throw new Error('18767 是 atb API 端口，请换一个插件端口');
      await ATB.send({ cmd: 'setPort', port });
      const data = await refreshStatus({ quiet: true });
      const connected = !!(data && data.wsConnected);
      ATB.setMsg($('portMsg'), `已保存端口 ${port}，${connected ? '已连接' : '正在重连（当前未连接）'}`, 'ok');
    } catch (e) {
      ATB.setMsg($('portMsg'), '保存失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

async function applyToken(button, token) {
  await ATB.withBusy(button, async () => {
    try {
      const data = await ATB.send({ cmd: 'setWsToken', token });
      $('tokenInput').value = '';
      await refreshStatus({ quiet: true });
      ATB.setMsg($('tokenMsg'), data.wsTokenSet ? '已保存 token，正在重连' : '已清空 token，正在重连', 'ok');
    } catch (e) {
      ATB.setMsg($('tokenMsg'), '保存失败：' + ATB.friendlyError(e), 'err');
    }
  });
}

async function saveTrust() {
  const mode = $('trustSelect').value === 'strict' ? 'strict' : 'compat';
  try {
    const data = await ATB.send({ cmd: 'setWsTrustMode', mode });
    await refreshStatus({ quiet: true });
    if (data && data.effective === false) {
      ATB.setMsg($('trustMsg'), '已切到严格档，但尚未设置 token：严格档要等 token 设置后才真正生效', 'err');
    } else {
      ATB.setMsg($('trustMsg'), `已切换为${mode === 'strict' ? '严格' : '兼容'}档`, 'ok');
    }
  } catch (e) {
    ATB.setMsg($('trustMsg'), '切换失败：' + ATB.friendlyError(e), 'err');
  }
}

// ---------------------------------------------------------------------------
// 身份
// ---------------------------------------------------------------------------

function bindIdentity() {
  $('labelInput')?.addEventListener('input', () => ATB.clearMsg($('labelMsg')));
  $('btnSaveLabel')?.addEventListener('click', () => {
    const value = $('labelInput').value.trim();
    if (!value) {
      ATB.setMsg($('labelMsg'), '名称不能为空；要清空请点「清空」', 'err');
      return;
    }
    setLabel(value);
  });
  $('btnClearLabel')?.addEventListener('click', () => {
    $('labelInput').value = '';
    setLabel(null);
  });
}

async function setLabel(label) {
  try {
    const data = await ATB.send({ cmd: 'setProfileLabel', label });
    await refreshStatus({ quiet: true });
    ATB.setMsg($('labelMsg'), `Profile Label：${(data && data.profileLabel) || '已清空'}`, 'ok');
  } catch (e) {
    ATB.setMsg($('labelMsg'), '保存失败：' + ATB.friendlyError(e), 'err');
  }
}

function bindCopyButtons() {
  for (const button of document.querySelectorAll('[data-copy]')) {
    button.addEventListener('click', async () => {
      const node = $(button.dataset.copy);
      const text = node ? node.textContent.trim() : '';
      if (!text || text === '-') {
        ATB.toast('没有可复制的值', 'err');
        return;
      }
      const ok = await ATB.copyText(text);
      ATB.toast(ok ? '已复制' : '复制失败', ok ? 'ok' : 'err');
    });
  }
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

function bindCookies() {
  $('btnLoadCookies')?.addEventListener('click', () => loadCookies(true));
  $('btnRevealCookies')?.addEventListener('click', toggleReveal);
  $('btnCopyCookies')?.addEventListener('click', copyAll);
  $('cookieSearch')?.addEventListener('input', () => {
    filterText = $('cookieSearch').value.trim();
    renderCookieTable();
  });

  const ttl = $('cookieTtl');
  if (ttl) {
    ttl.value = String(clampTtl(ttlSec));
    ttl.addEventListener('change', async () => {
      ttlSec = clampTtl(Number(ttl.value));
      ttl.value = String(ttlSec);
      await ATB.writeSetting('clipboardTtlSec', ttlSec);
      if (lastCopied) armClipboardClear();
    });
  }

  // 打开页面不读 cookie；切到 Cookies 分区时才提示用户点按钮。
}

// 选 cookie 来源标签页的口径与 popup 页面模式共用 ui/common.js：
// options 自己也是 chrome-extension:// 标签页，不能靠 query({active:true}) 取站点。

async function loadCookies(announce) {
  const button = $('btnLoadCookies');
  const run = async () => {
    try {
      const tab = await ATB.pickCookieTab();
      if (!tab) {
        cookieRows = [];
        cookieSourceUrl = '';
        $('cookieSite').textContent = '没有可读取的网页标签页';
        renderCookieTable();
        ATB.setMsg($('cookieMsg'), '当前窗口没有 http/https/file 标签页；先打开一个网页再试', 'err');
        return;
      }
      cookieSourceUrl = tab.url;
      $('cookieSite').textContent = ATB.hostOf(tab.url);
      $('cookieSite').title = tab.url;
      const resp = await ATB.send({ cmd: 'cookies', url: tab.url });
      const items = Array.isArray(resp) ? resp : (Array.isArray(resp && resp.data) ? resp.data : []);
      cookieRows = items;
      revealed = false;
      revealArmedAt = 0;
      const reveal = $('btnRevealCookies');
      if (reveal) reveal.textContent = '显示值';
      renderCookieTable();
      if (announce) {
        ATB.setMsg($('cookieMsg'), items.length ? `已读取 ${items.length} 条（值默认打码）` : '该站点没有 cookies', 'ok');
      }
    } catch (e) {
      cookieRows = [];
      renderCookieTable();
      ATB.setMsg($('cookieMsg'), '读取 cookies 失败：' + ATB.friendlyError(e), 'err');
    }
  };
  if (button) await ATB.withBusy(button, run);
  else await run();
}

function flagsOf(cookie) {
  return (cookie.httpOnly ? 'H' : '') + (cookie.secure ? 'S' : '') + (cookie.partitionKey ? 'P' : '');
}

function visibleRows() {
  if (!filterText) return cookieRows;
  const q = filterText.toLowerCase();
  return cookieRows.filter((c) => `${c.name || ''} ${flagsOf(c)}`.toLowerCase().includes(q));
}

function renderCookieTable() {
  const body = $('cookieRows');
  const wrap = $('cookieTableWrap');
  const empty = $('cookieEmpty');
  if (!body || !wrap || !empty) return;

  const rows = visibleRows();
  $('cookieCountLabel').textContent = cookieRows.length
    ? `${rows.length} / ${cookieRows.length} 条`
    : '';

  body.textContent = '';

  if (!cookieRows.length) {
    wrap.hidden = true;
    empty.hidden = false;
    empty.textContent = '还没有读取 cookie。点「读取当前站点」；值默认打码，二次确认后才显示明文。';
    return;
  }
  if (!rows.length) {
    wrap.hidden = true;
    empty.hidden = false;
    empty.textContent = `没有匹配「${filterText}」的 cookie`;
    return;
  }

  wrap.hidden = false;
  empty.hidden = true;

  const fragment = document.createDocumentFragment();
  for (const cookie of rows) {
    const tr = document.createElement('tr');

    const nameTd = document.createElement('td');
    nameTd.className = 'cell-name';
    nameTd.textContent = cookie.name || '(未命名)';
    nameTd.title = cookie.name || '';

    const valueTd = document.createElement('td');
    valueTd.className = 'cell-value' + (revealed ? '' : ' masked');
    const raw = cookie.value == null ? '' : String(cookie.value);
    valueTd.textContent = revealed ? raw : '•••';
    // 表格列宽固定，值单行省略；title 保留完整内容，便于核对。
    valueTd.title = revealed ? raw : '点「显示值」并二次确认后查看';

    const flagsTd = document.createElement('td');
    const flags = document.createElement('div');
    flags.className = 'cell-flags';
    if (cookie.httpOnly) flags.appendChild(makeFlag('H', 'httpOnly'));
    if (cookie.secure) flags.appendChild(makeFlag('S', 'secure'));
    if (cookie.partitionKey) flags.appendChild(makeFlag('P', '分区'));
    flagsTd.appendChild(flags);

    const actionTd = document.createElement('td');
    actionTd.className = 'cell-action';
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'row-copy';
    copy.textContent = '复制';
    copy.addEventListener('click', async () => {
      const ok = await ATB.copyText(`${cookie.name}=${cookie.value}`);
      if (ok) {
        lastCopied = `${cookie.name}=${cookie.value}`;
        armClipboardClear();
        ATB.toast(`已复制 ${cookie.name}，${ttlSec} 秒后清除`, 'ok');
      } else {
        ATB.toast('复制失败', 'err');
      }
    });
    actionTd.appendChild(copy);

    tr.append(nameTd, valueTd, flagsTd, actionTd);
    fragment.appendChild(tr);
  }
  body.appendChild(fragment);
}

function makeFlag(text, title) {
  const node = document.createElement('span');
  node.className = 'flag' + (text === 'H' ? ' h' : '');
  node.textContent = text;
  node.title = title;
  return node;
}

function toggleReveal() {
  const button = $('btnRevealCookies');
  if (!cookieRows.length) {
    ATB.setMsg($('cookieMsg'), '先点「读取当前站点」', '');
    return;
  }
  if (revealed) {
    revealed = false;
    revealArmedAt = 0;
    button.textContent = '显示值';
    renderCookieTable();
    ATB.setMsg($('cookieMsg'), '已恢复打码显示', 'ok');
    return;
  }
  const now = Date.now();
  if (!revealArmedAt || now - revealArmedAt > 5000) {
    revealArmedAt = now;
    button.textContent = '再次点击确认';
    ATB.setMsg($('cookieMsg'), '明文会把 cookie 值画到屏幕上（含 httpOnly/secure），5 秒内再点一次确认', '', 5000);
    return;
  }
  revealed = true;
  revealArmedAt = 0;
  button.textContent = '隐藏值';
  renderCookieTable();
  ATB.setMsg($('cookieMsg'), '已显示明文；切换分区或离开页面会自动恢复打码', 'ok');
}

function hideCookieValues() {
  const button = $('btnRevealCookies');
  // 即使还停在「再次点击确认」的待确认态，也要把按钮文案和计时复位，
  // 否则切回分区时会看到一个莫名其妙的「再次点击确认」。
  revealArmedAt = 0;
  if (!revealed) {
    if (button) button.textContent = '显示值';
    return;
  }
  revealed = false;
  if (button) button.textContent = '显示值';
  renderCookieTable();
}

async function copyAll() {
  if (!cookieRows.length) {
    ATB.setMsg($('cookieMsg'), '没有可复制的 cookies，先读取', '');
    return;
  }
  const includeSensitive = !!$('includeSensitive').checked;
  const picked = cookieRows.filter((c) => includeSensitive || !(c.httpOnly || c.secure));
  const skipped = cookieRows.length - picked.length;
  if (!picked.length) {
    ATB.setMsg($('cookieMsg'), `未复制：${skipped} 条全是 httpOnly/secure，请勾选「复制时包含 httpOnly/secure」`, 'err');
    return;
  }
  lastCopied = picked.map((c) => `${c.name}=${c.value}`).join('; ');
  const ok = await ATB.copyText(lastCopied);
  if (!ok) {
    lastCopied = '';
    ATB.setMsg($('cookieMsg'), '复制失败：浏览器拒绝了剪贴板写入，请重试', 'err');
    return;
  }
  ATB.setMsg($('cookieMsg'), `已复制 ${picked.length} 条${includeSensitive ? '（含敏感项）' : ''}`
    + (skipped ? `，跳过 ${skipped} 条敏感项` : '')
    + `，${ttlSec} 秒后自动清除`, 'ok', ttlSec * 1000);
  armClipboardClear();
}

function armClipboardClear() {
  if (clipboardTimer) {
    clearTimeout(clipboardTimer);
    clipboardTimer = null;
  }
  if (!lastCopied) return;
  clipboardTimer = setTimeout(() => {
    clipboardTimer = null;
    clearClipboard('timeout');
  }, clampTtl(ttlSec) * 1000);
}

async function clearClipboard(reason) {
  if (!lastCopied) return;
  let verified = false;
  let same = true;
  try {
    if (navigator.clipboard && navigator.clipboard.readText) {
      const current = await navigator.clipboard.readText();
      verified = true;
      same = current === lastCopied;
    }
  } catch (_) {
    verified = false;
  }
  if (verified && !same) {
    lastCopied = '';
    ATB.setMsg($('cookieMsg'), '剪贴板内容已被你替换，未清空（原副本已被覆盖）', '');
    return;
  }
  const cleared = await ATB.copyText('');
  if (cleared) {
    lastCopied = '';
    ATB.setMsg($('cookieMsg'), verified
      ? '已自动清除剪贴板里的 cookies'
      : '已自动清除剪贴板（无法校验内容：缺少剪贴板读取权限）', 'ok');
  } else if (reason === 'timeout') {
    ATB.setMsg($('cookieMsg'), '自动清除失败，请手动清空剪贴板', 'err');
  }
}

function bindLifecycle() {
  const flush = () => {
    if (lastCopied) {
      try { navigator.clipboard.writeText(''); } catch (_) {}
      lastCopied = '';
    }
    if (clipboardTimer) {
      clearTimeout(clipboardTimer);
      clipboardTimer = null;
    }
  };
  window.addEventListener('pagehide', () => {
    flush();
    hideCookieValues();
    if (pollTimer) clearInterval(pollTimer);
  });
  window.addEventListener('beforeunload', flush);
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) hideCookieValues();
  });
}

// ---------------------------------------------------------------------------
// 外观
// ---------------------------------------------------------------------------

function syncAppearanceControls(settings) {
  const theme = settings.themeMode === 'light' || settings.themeMode === 'dark' ? settings.themeMode : 'auto';
  for (const radio of document.querySelectorAll('input[name="theme"]')) {
    radio.checked = radio.value === theme;
  }
  $('widthSelect').value = settings.popupWidthMode === 'compact' || settings.popupWidthMode === 'wide'
    ? settings.popupWidthMode
    : 'standard';
  $('densitySelect').value = settings.uiDensity === 'compact' ? 'compact' : 'standard';
  $('badgeToggle').checked = settings.showBadge === true;
  $('persistTtlSelect').value = String(ATB.normalizePersistTtl(settings.persistTtlSec));
}

function bindAppearance() {
  for (const radio of document.querySelectorAll('input[name="theme"]')) {
    radio.addEventListener('change', async () => {
      if (!radio.checked) return;
      ATB.applyAppearance({ themeMode: radio.value, uiDensity: document.documentElement.dataset.density });
      await ATB.writeSetting('themeMode', radio.value);
      ATB.setMsg($('themeMsg'), `主题已切换为${radio.value === 'auto' ? '跟随系统' : radio.value === 'light' ? '浅色' : '深色'}`, 'ok');
    });
  }

  $('widthSelect')?.addEventListener('change', async (event) => {
    await ATB.writeSetting('popupWidthMode', event.target.value);
    ATB.setMsg($('appearanceMsg'), '弹窗宽度已保存', 'ok');
  });

  $('densitySelect')?.addEventListener('change', async (event) => {
    const density = event.target.value === 'compact' ? 'compact' : 'standard';
    document.documentElement.dataset.density = density;
    await ATB.writeSetting('uiDensity', density);
    ATB.setMsg($('appearanceMsg'), '间距已保存', 'ok');
  });

  $('badgeToggle')?.addEventListener('change', async (event) => {
    const show = event.currentTarget.checked;
    await ATB.writeSetting('showBadge', show);
    ATB.setMsg($('appearanceMsg'), show ? '角标已开启（已打开的标签页即时生效）' : '角标已关闭', 'ok');
    try {
      const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
      if (tab && tab.id != null) await chrome.tabs.sendMessage(tab.id, { cmd: 'badgeConfig' });
    } catch (_) {}
  });

  $('btnPreviewPopup')?.addEventListener('click', () => {
    try {
      chrome.tabs.create({ url: chrome.runtime.getURL('popup.html?page=1') });
    } catch (_) {}
  });

  // 另一个入口（popup）改了设置时保持同步。
  ATB.onSettingsChanged((changed) => {
    if (changed.themeMode || changed.uiDensity) {
      ATB.applyAppearance({
        themeMode: changed.themeMode || 'auto',
        uiDensity: changed.uiDensity || document.documentElement.dataset.density || 'standard',
      });
    }
    if (changed.themeMode) {
      for (const radio of document.querySelectorAll('input[name="theme"]')) {
        radio.checked = radio.value === changed.themeMode;
      }
    }
    if (changed.popupWidthMode) $('widthSelect').value = changed.popupWidthMode;
    if (changed.uiDensity) $('densitySelect').value = changed.uiDensity === 'compact' ? 'compact' : 'standard';
    if (changed.showBadge != null) $('badgeToggle').checked = changed.showBadge === true;
    if (changed.clipboardTtlSec != null) {
      ttlSec = clampTtl(changed.clipboardTtlSec);
      if ($('cookieTtl')) $('cookieTtl').value = String(ttlSec);
    }
    if (changed.persistTtlSec != null) {
      $('persistTtlSelect').value = String(ATB.normalizePersistTtl(changed.persistTtlSec));
    }
  });
}

/** 调试注入默认值：与 popup「设置 → 调试注入」共用 storage.persistTtlSec。 */
function bindDebugSettings() {
  $('persistTtlSelect')?.addEventListener('change', async (event) => {
    const value = ATB.normalizePersistTtl(event.target.value);
    event.target.value = String(value);
    const ok = await ATB.writeSetting('persistTtlSec', value);
    ATB.setMsg($('debugMsg'), ok ? `持久注入默认过期时间已保存（${formatPersistTtl(value)}）` : '保存失败：扩展存储不可用', ok ? 'ok' : 'err');
  });
}

function formatPersistTtl(sec) {
  if (sec >= 3600 && sec % 3600 === 0) return `${sec / 3600} 小时`;
  return `${Math.round(sec / 60)} 分钟`;
}

// ---------------------------------------------------------------------------
// 可选权限：management（只用于列出扩展）
// ---------------------------------------------------------------------------

function bindAdvancedPermissions() {
  $('btnGrantManagement')?.addEventListener('click', grantManagementPermission);
  // 用户也可能在 chrome://extensions 里撤销/重新授予；两个方向都刷新状态。
  try {
    chrome.permissions?.onAdded?.addListener(() => { void refreshManagementPermission(); });
    chrome.permissions?.onRemoved?.addListener(() => { void refreshManagementPermission(); });
  } catch (_) {}
}

/**
 * 查询 management 授权状态。正常走 chrome.permissions.contains；
 * 没有 permissions API 时退回 status 的 management_permission 字段。
 */
async function refreshManagementPermission() {
  let known = false;
  let granted = false;
  try {
    if (chrome.permissions && typeof chrome.permissions.contains === 'function') {
      granted = await chrome.permissions.contains({ permissions: ['management'] });
      known = true;
    }
  } catch (_) {
    known = false;
  }
  if (!known && lastStatus) {
    granted = !!lastStatus.management_permission;
    known = true;
  }
  renderManagementPermission(granted, known);
  return granted;
}

function renderManagementPermission(granted, known) {
  const pill = $('mgmtPermPill');
  const text = $('mgmtPermText');
  const button = $('btnGrantManagement');
  if (!pill || !text) return;
  if (!known) {
    pill.className = 'pill';
    text.textContent = '检测中';
    if (button) {
      button.disabled = false;
      button.textContent = '授权读取扩展列表';
    }
    return;
  }
  if (granted) {
    pill.className = 'pill ok';
    text.textContent = '已授权';
    if (button) {
      button.disabled = true;
      button.textContent = '已授权';
    }
    return;
  }
  pill.className = 'pill warn';
  text.textContent = '未授权';
  if (button) {
    button.disabled = false;
    button.textContent = '授权读取扩展列表';
  }
}

async function grantManagementPermission() {
  const button = $('btnGrantManagement');
  if (!button || button.disabled) return;
  if (!chrome.permissions || typeof chrome.permissions.request !== 'function') {
    ATB.setMsg($('mgmtPermMsg'), '当前浏览器不支持 chrome.permissions，无法在页面里授权；请在 chrome://extensions 的扩展详情里手动开启 management 权限。', 'err');
    return;
  }
  button.disabled = true;
  button.textContent = '等待授权…';
  try {
    // 必须由用户点击同步触发；成功返回 true，用户拒绝返回 false。
    const granted = await chrome.permissions.request({ permissions: ['management'] });
    if (granted) {
      renderManagementPermission(true, true);
      // MV3 的 Service Worker 在权限授予前已经缓存了「management 不可用」；
      // 不重载扩展时后台的 getAll 仍会报权限错误（实测）。
      // 这里自动重载一次，让 atb extensions 立刻可用。
      ATB.setMsg(
        $('mgmtPermMsg'),
        '已授权，正在重新加载扩展以启用 management API；页面会自动刷新，随后可运行 atb extensions。',
        'ok',
      );
      ATB.toast('已授权 management，正在重载扩展', 'ok');
      setTimeout(() => {
        try { chrome.runtime.reload(); } catch (_) {}
      }, 900);
    } else {
      renderManagementPermission(false, true);
      ATB.setMsg($('mgmtPermMsg'), '未授权：你拒绝了授权请求，或企业策略禁止 management 权限。可以稍后重试。', 'err');
    }
  } catch (e) {
    renderManagementPermission(false, true);
    ATB.setMsg($('mgmtPermMsg'), '授权失败：' + ATB.friendlyError(e) + '（常见原因是用户拒绝或企业策略限制）', 'err');
  }
}

// ---------------------------------------------------------------------------
// 诊断 / 关于
// ---------------------------------------------------------------------------

function bindDiagnostics() {
  $('btnSelfCheck')?.addEventListener('click', async (event) => {
    const out = $('selfCheckOut');
    await ATB.withBusy(event.currentTarget, async () => {
      try {
        const data = await ATB.send({ cmd: 'selfCheck' });
        out.hidden = false;
        out.textContent = JSON.stringify(data, null, 2);
        ATB.toast(data.degraded ? '自检完成，有告警' : '自检完成', data.degraded ? 'err' : 'ok');
      } catch (e) {
        out.hidden = false;
        out.textContent = '自检失败：' + ATB.friendlyError(e);
        ATB.toast('自检失败', 'err');
      }
    });
  });

  // 重置浏览器标识：第一步只变成「再次点击确认」，5 秒内不点就复位。
  let resetArmedAt = 0;
  $('btnResetIdentity')?.addEventListener('click', async (event) => {
    const button = event.currentTarget;
    const msg = $('resetIdentityMsg');
    const now = Date.now();
    if (now - resetArmedAt > 5000) {
      resetArmedAt = now;
      button.textContent = '再次点击确认';
      if (msg) {
        msg.className = 'msg';
        msg.textContent = '重置后浏览器会以新标识重新连入，旧的默认浏览器与 @e 引用会失效。5 秒内再点一次确认。';
      }
      setTimeout(() => {
        if (Date.now() - resetArmedAt >= 5000) {
          button.textContent = '重置';
          if (msg && msg.textContent.startsWith('重置后浏览器')) msg.textContent = '';
        }
      }, 5200);
      return;
    }
    resetArmedAt = 0;
    button.textContent = '重置浏览器标识';
    try {
      await chrome.runtime.sendMessage({ cmd: 'resetIdentity' });
      if (msg) {
        msg.className = 'msg msg-ok';
        msg.textContent = '已清除旧标识，扩展正在重载；刷新本页后可在「标识」里看到新的 browserId。';
      }
      ATB.toast('已重置，扩展正在重载', 'ok');
    } catch (e) {
      if (msg) {
        msg.className = 'msg msg-err';
        msg.textContent = '重置失败：' + ATB.friendlyError(e);
      }
      ATB.toast('重置失败', 'err');
    }
  });

  $('ovCopyDiag')?.addEventListener('click', copyDiagnostics);
  $('btnCopyDiag2')?.addEventListener('click', copyDiagnostics);
}

function renderDiagnostics(data) {
  if (!data) return;
  const hb = data.heartbeat || {};
  $('diagUrl').textContent = data.wsUrl || '-';
  $('diagPing').textContent = Number.isFinite(Number(hb.lastPingAgoMs)) ? ATB.fmtAgo(Date.now() - Number(hb.lastPingAgoMs)) : '-';
  $('diagPong').textContent = Number.isFinite(Number(hb.lastPongAgoMs)) ? ATB.fmtAgo(Date.now() - Number(hb.lastPongAgoMs)) : '-';
  $('diagRtt').textContent = Number.isFinite(Number(hb.lastRttMs)) ? `${Math.round(Number(hb.lastRttMs))} ms` : '-';
  $('diagLastCommand').textContent = ATB.fmtAgo(data.lastCommandAt);
  const err = data.lastConnectError;
  // 短值（无错误）右对齐，长错误左对齐换行——否则「无」会孤零零贴在左边。
  $('diagLastError').className = err ? 'wrap' : '';
  $('diagLastError').textContent = err
    ? `${err.message || '未知原因'}（${err.at ? ATB.fmtClock(err.at) : '-'}，${err.hadToken ? '已带 token' : '未带 token'}${err.repeats ? `，重复 ${err.repeats} 次` : ''}）`
    : '无';
  const fileNode = $('diagFileAccess');
  fileNode.textContent = data.fileSchemeAccess ? '已启用' : '未启用';
  fileNode.className = data.fileSchemeAccess ? 'positive' : 'negative';

  const warnings = Array.isArray(data.warnings) ? data.warnings : [];
  $('diagDegraded').textContent = data.degraded ? `降级：${warnings.length} 项告警` : '正常';
  $('diagDegraded').style.color = data.degraded ? 'var(--err)' : 'var(--text-3)';
}

async function copyDiagnostics() {
  const payload = {
    at: new Date().toISOString(),
    browser: browserInfo,
    status: lastStatus,
    lastError,
    manifest: getManifestBase(),
  };
  const ok = await ATB.copyText(JSON.stringify(payload, null, 2));
  ATB.toast(ok ? '诊断信息已复制' : '复制失败', ok ? 'ok' : 'err');
}

let browserInfo = null;

/// 磁盘上的构建指纹（部署脚本写 .atb-build.json）。
/// 只代表「磁盘上这份是何时打的包」，运行中的版本看「扩展版本」那一行。
async function loadBuildStamp() {
  try {
    const url = chrome.runtime.getURL('.atb-build.json') + '?ts=' + Date.now();
    const res = await fetch(url, { cache: 'no-store' });
    if (!res.ok) return null;
    const data = await res.json();
    return data && typeof data === 'object' ? data : null;
  } catch (_) {
    return null;
  }
}

async function fillBuildStamp() {
  const stamp = await loadBuildStamp();
  const node = $('aboutBuild');
  if (!node) return;
  if (!stamp || !stamp.builtAt) {
    node.textContent = '未部署指纹';
    node.title = '扩展目录里没有 .atb-build.json（老部署或手动拷贝），自检会退回 background.js 哈希';
    return;
  }
  const at = new Date(stamp.builtAt);
  const time = Number.isNaN(at.getTime())
    ? stamp.builtAt
    : at.toLocaleString('zh-CN', { hour12: false, month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' });
  node.textContent = stamp.version ? `${time} · ${stamp.version}` : time;
  node.title = `digest ${String(stamp.digest || '').slice(0, 16)}… · ${stamp.files || 0} 个文件\n部署指纹变了扩展会自动重载（60 秒内）`;
}

/** 浏览器型号 + 版本（当前页面所在浏览器），与 daemon 侧的 browser_label 互补。 */
async function fillBrowserInfo() {
  try {
    browserInfo = await ATB.detectBrowser();
    const label = [browserInfo.name, browserInfo.version].filter(Boolean).join(' ');
    const set = (id, text, title) => {
      const node = $(id);
      if (!node) return;
      node.textContent = text || '-';
      if (title) node.title = title;
    };
    set('ovBrowser', label, browserInfo.ua);
    set('aboutBrowser', label, browserInfo.ua);
    set('aboutPlatform', browserInfo.platform || '-');
    set('aboutUa', browserInfo.ua || '-');
  } catch (_) {
    // 识别失败不影响其它信息。
  }
}

function renderAbout() {
  const manifest = getManifestBase();
  $('aboutVersion').textContent = manifest.version || '-';
  $('aboutProtocol').textContent = '5 / 4';
  $('optVersion').textContent = `v${manifest.version || '-'}`;

  const list = $('permList');
  if (!list) return;
  list.textContent = '';
  const required = Array.from(new Set([...(manifest.permissions || []), ...(manifest.host_permissions || [])]));
  const optional = Array.from(new Set(manifest.optional_permissions || []));
  const perms = [...required.map((perm) => ({ perm, optional: false })), ...optional.map((perm) => ({ perm, optional: true }))];
  if (!perms.length) {
    const li = document.createElement('li');
    li.textContent = 'manifest 没有返回权限清单';
    list.appendChild(li);
    return;
  }
  for (const { perm, optional: isOptional } of perms) {
    const li = document.createElement('li');
    li.textContent = isOptional ? `${perm}（可选）` : perm;
    if (isOptional) li.title = '可选权限：由用户在「高级权限」里手动授权，未授权时不影响其它功能';
    list.appendChild(li);
  }
}

// ---------------------------------------------------------------------------

function getManifestBase() {
  try {
    return chrome.runtime.getManifest() || {};
  } catch (_) {
    return {};
  }
}

function getManifestVersion() {
  return getManifestBase().version || '-';
}

function clampTtl(value) {
  const raw = Math.floor(Number(value));
  if (!Number.isFinite(raw) || raw <= 0) return 30;
  return Math.min(COOKIE_TTL_MAX_SEC, Math.max(COOKIE_TTL_MIN_SEC, raw));
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
