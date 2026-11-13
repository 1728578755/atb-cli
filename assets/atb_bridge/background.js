// background.js - Cookie + CDP Bridge
//
// manifest.json 的 minimum_chrome_version = 120 的依据（manifest 是纯 JSON，不能写注释，
// 故把推导过程记在这里）：
//   - chrome.scripting.executeScript 的 world:'MAIN'（runUserCodeInTab / persist 注入）需要 111+；
//   - chrome.storage.session（Fetch 拦截状态的冷启动恢复）需要 102+；
//   - chrome.alarms 的 30s 最小粒度 clamp 语义按 120+ 生效（scheduleProbe / scheduleKeepalive
//     的节奏依赖这个下限）；
//   - 另外 chrome.cookies.getAll({partitionKey}) 需要 119+，已被 120 覆盖。
// 取三者（111 / 102 / 120）的最大值 = 120。
/// 与 daemon 的消息契约版本（daemon 侧 src/protocol.rs EXTENSION_PROTOCOL_VERSION）。
/// 改动 ext_ready / tabs_update / pageOp 的字段语义时 +1，daemon 会据此判断是否配套。
const EXTENSION_PROTOCOL = 4;

const LEGACY_CSP_RULE_ID = 9999;

/// 异步回调的统一兜底：把 rejection 收敛成一条 warn 日志，
/// 不让「顺手做的事」把 SW / 监听器整个带下去。
const bgTask = (label, fn) => Promise.resolve().then(fn).catch((e) => {
  console.warn('[TMWD] ' + label, (e && e.message) || e);
});

/// 结构化失败（契约第 8 条）：新增失败路径必须带 error_code / hint。
function structuredError(code, message, hint) {
  const err = new Error(message);
  err.code = code;
  if (hint) err.hint = hint;
  return err;
}

async function removeLegacyCspRule() {
  try {
    // Dynamic rules survive extension upgrades, so explicitly migrate existing installs.
    // declarativeNetRequest 权限**保留**，唯一用途就是这条一次性迁移（删除
    // 旧版本留下的 CSP 规则）；不得在此之上新增任何请求改写能力。
    await chrome.declarativeNetRequest.updateDynamicRules({ removeRuleIds: [LEGACY_CSP_RULE_ID] });
  } catch (e) {
    console.warn('[TMWD] Failed to remove legacy CSP rule:', e.message);
  }
}

// 顶层调用必须把 rejection 吃掉：Service Worker 求值阶段的未处理拒绝，
// 在部分 Edge 版本上会被记成「启动失败」，表现就是扩展页那条笼统的
// Service worker registration failed。
removeLegacyCspRule().catch(() => {});
chrome.runtime.onInstalled.addListener(() => {
  console.log('CDP Bridge installed');
  removeLegacyCspRule();
});

let lastCommandAt = 0;
const DEFAULT_WS_PORT = 18765;
const CLI_API_PORT = 18767;
let wsPort = DEFAULT_WS_PORT;
// 可选的 WebSocket 鉴权 token（对应 daemon 的 <用户配置目录>/ws-trust.json）。
// 空串 = 不启用：此时连接 URL 与历史版本一字不差，绝不改变默认行为。
let wsToken = '';
// 信任档位：compat（默认，与历史行为完全一致）| strict。
// strict 下「无 token 的连接」只放行只读命令，高危命令（exec / cdp / cookies / inject…）
// 返回结构化拒绝 —— 端口被本机其它进程抢占时，最多只能看到浏览器状态，拿不到会话与任意脚本执行。
// 注意：真正的对端身份校验必须由 daemon 侧完成（core-ws 的 ATB_WS_TRUST_MODE），
// 因为扩展是被连接方，无法凭 token 证明「绑在这个端口上的是真 daemon」。
const WS_TRUST_MODES = new Set(['compat', 'strict']);
let wsTrustMode = 'compat';
// 最后一次连接失败的原因（同时落 chrome.storage.local 的 lastConnectError，popup 读它做诊断）。
// 只在内存里做去重指纹，避免 probe 每秒往 storage 写一次。
let lastConnectError = null;
let wsConnectErrorHydrated = false;
// browserId 必须落 chrome.storage.local：MV3 的 Service Worker 随时会被回收，
// 每次冷启动都会重新求值本模块。以前它是模块级 const，重启后就变成新 UUID，
// 于是 session_key（browser_id:profile_id:tab_id）漂移，daemon 侧的 @e 引用、
// net 配置、--tab 寻址全部失联。现在与 profileId 一样从 storage 读取/生成后落盘。
let browserId = null;
const extensionVersion = (() => {
  try {
    return chrome.runtime.getManifest().version;
  } catch (_) {
    return '0.0.0';
  }
})();
let profileId = null;
let profileLabel = null;
/// 用户给这台浏览器起的名字（popup 可写，落 storage.local）。多浏览器/多 profile
/// 同时连到同一个 daemon 时，UUID 看不出谁是谁，可读标签才是人能用上的东西。
let browserLabelCustom = null;
// 单飞：并发调用只跑一次 loadClientIdentity，避免两个首次启动路径各自生成 id 后再互相覆盖。
let clientIdentityPromise = null;

/// 浏览器品牌：UA 里没有可靠的 `brands`（Service Worker 里没有 userAgentData），
/// 只能按 UA 里的引擎标记判断。只用于展示，不参与任何判定。
function detectBrowserBrand() {
  const ua = String(navigator.userAgent || '');
  if (/\bEdg\//.test(ua)) return 'Edge';
  if (/\bOPR\/|\bOpera\b/.test(ua)) return 'Opera';
  if (/\bVivaldi\b/.test(ua)) return 'Vivaldi';
  if (/\bFirefox\//.test(ua)) return 'Firefox';
  if (/\bChrome\//.test(ua)) return 'Chrome';
  return 'Browser';
}

function detectOsName() {
  const ua = String(navigator.userAgent || '');
  if (/Windows NT 10/.test(ua)) return 'Windows';
  if (/Windows NT/.test(ua)) return 'Windows';
  if (/Mac OS X/.test(ua)) return 'macOS';
  if (/Android/.test(ua)) return 'Android';
  if (/Linux/.test(ua)) return 'Linux';
  return 'Unknown';
}

/// 浏览器元信息：品牌 + 完整版本 + 平台。
///
/// Service Worker 里可能没有 userAgentData（不同版本不一样），所以先用 UA 解析兜底，
/// 有 UA-CH 再用 fullVersionList 换成完整版本（如 156.0.8078.79）。
let browserMeta = null;
let browserMetaPromise = null;

function parseBrowserFromUA() {
  const ua = String(navigator.userAgent || '');
  const name = detectBrowserBrand();
  const patterns = name === 'Edge' ? [/Edg\/([\d.]+)/, /Chrome\/([\d.]+)/]
    : name === 'Opera' ? [/OPR\/([\d.]+)/, /Chrome\/([\d.]+)/]
      : name === 'Firefox' ? [/Firefox\/([\d.]+)/]
        : [/Chrome\/([\d.]+)/];
  let version = '';
  for (const re of patterns) {
    const m = ua.match(re);
    if (m) { version = m[1]; break; }
  }
  return { name, version, platform: detectOsName() };
}

async function detectBrowserMeta() {
  const base = parseBrowserFromUA();
  try {
    const uaData = navigator.userAgentData;
    if (uaData && typeof uaData.getHighEntropyValues === 'function') {
      const hi = await uaData.getHighEntropyValues(['fullVersionList', 'platform']);
      const list = Array.isArray(hi.fullVersionList) ? hi.fullVersionList : [];
      const prefer = base.name === 'Edge' ? /Microsoft Edge/i
        : base.name === 'Chrome' ? /Google Chrome/i
          : base.name === 'Chromium' ? /Chromium/i : null;
      const hit = prefer ? list.find((b) => prefer.test(String((b && b.brand) || ''))) : list[0];
      if (hit && hit.version) base.version = hit.version;
      if (hi.platform) base.platform = hi.platform;
    }
  } catch (_) {
    // 高熵值被策略禁用时保留 UA 版本。
  }
  return base;
}

/// 握手前等一次：保证 ext_ready 里带的标签已经是完整版本号。
function ensureBrowserMeta() {
  if (browserMeta) return Promise.resolve(browserMeta);
  if (!browserMetaPromise) {
    browserMetaPromise = detectBrowserMeta()
      .then((meta) => (browserMeta = meta))
      .catch(() => (browserMeta = parseBrowserFromUA()));
  }
  return browserMetaPromise;
}

/// 「Edge · Windows · 工作机 · #3f2a」——最后一个短码来自 browserId，
/// 保证同名浏览器也能区分（比如开了两个 Edge profile）。
function composeBrowserLabel() {
  const meta = browserMeta || parseBrowserFromUA();
  const brand = meta.version ? `${meta.name} ${meta.version}` : meta.name;
  const parts = [brand, meta.platform || detectOsName()];
  const custom = String(browserLabelCustom || '').trim();
  if (custom) parts.push(custom);
  const short = String(browserId || '').replace(/^browser-/, '').slice(0, 4);
  if (short) parts.push(`#${short}`);
  return parts.filter(Boolean).join(' · ');
}

async function loadClientIdentity() {
  const data = await chrome.storage.local.get({ browserId: null, profileId: null, profileLabel: null, browserLabel: null });
  const patch = {};
  browserId = data.browserId || `browser-${crypto.randomUUID()}`;
  profileId = data.profileId || `profile-${crypto.randomUUID()}`;
  profileLabel = data.profileLabel || null;
  browserLabelCustom = data.browserLabel || null;
  if (!data.browserId) patch.browserId = browserId;
  if (!data.profileId) patch.profileId = profileId;
  // 一次 set 写回，避免两次写之间被 SW 回收导致只剩一半落盘。
  if (Object.keys(patch).length) await chrome.storage.local.set(patch);
  return { browserId, profileId, profileLabel };
}

/// 所有需要身份的地方都等这个：保证 browserId/profileId 一定已就绪。
function ensureClientIdentity() {
  if (!clientIdentityPromise) {
    clientIdentityPromise = loadClientIdentity().catch((e) => {
      // 读失败不要永久缓存 reject，下一次调用重试。
      clientIdentityPromise = null;
      throw e;
    });
  }
  return clientIdentityPromise;
}

function withClientIdentity(payload) {
  return Object.assign({
    browser_id: browserId,
    // 可读标签：daemon 的 tabs / browsers / --browser 都用它区分「哪个浏览器」。
    browser_label: composeBrowserLabel(),
    profile_id: profileId || 'profile-pending',
    profile_label: profileLabel || undefined
  }, payload);
}

/**
 * 注入自检：分别验证「MAIN world 纯函数」「MAIN world eval」「ISOLATED world eval」。
 *
 * exec 走的是 MAIN world eval；失败后会自动回退 CDP，而 CDP 在 Chrome 156 上
 * 可能被别的扩展 iframe 挡住，最终报的是回退失败——真正的原因被掩盖。
 * 这个命令把三条路径的真实错误分别暴露出来。
 */
async function handleInjectProbe(msg, sender) {
  let tabId;
  try {
    tabId = resolveTabId(msg, sender);
  } catch (e) {
    return { ok: false, error: e.message };
  }
  const out = { tabId };
  const probe = async (label, options) => {
    try {
      const result = await chrome.scripting.executeScript(options);
      out[label] = { ok: true, value: result?.[0]?.result ?? null };
    } catch (e) {
      out[label] = { ok: false, error: e.message };
    }
  };
  await probe('mainPlain', { target: { tabId }, world: 'MAIN', func: () => 1 + 1 });
  await probe('mainEval', {
    target: { tabId },
    world: 'MAIN',
    func: async (s) => await eval(s),
    args: ['(async () => 2 + 2)()']
  });
  await probe('isolatedEval', {
    target: { tabId },
    func: async (s) => await eval(s),
    args: ['(async () => 3 + 3)()']
  });
  if (msg.code) {
    // 复现 exec 的完整注入路径（buildPageScript），用来定位返回值为什么是 null。
    await probe('execPath', {
      target: { tabId },
      world: 'MAIN',
      func: async (s) => await eval(s),
      args: [buildPageScript(String(msg.code))]
    });
    await probe('execPathPlain', {
      target: { tabId },
      world: 'MAIN',
      func: async (s) => await eval(s),
      args: [String(msg.code)]
    });
  }
  return { ok: true, data: out };
}

/// 把 tabId/frameId/allFrames 归一成一个 executeScript target。
function buildScriptTarget(tabId, msg) {
  const target = { tabId };
  if (msg.allFrames === true || msg.all_frames === true) {
    target.allFrames = true;
    return target;
  }
  const raw = msg.frameId !== undefined ? msg.frameId : msg.frame_id;
  if (raw !== undefined && raw !== null && raw !== '') {
    const frameId = Number(raw);
    if (Number.isInteger(frameId) && frameId >= 0) target.frameIds = [frameId];
  }
  return target;
}

// ============================================================================
// persist match pattern 口径 —— **必须与 persist_boot.js 逐字等价**
// ----------------------------------------------------------------------------
// 两处服务的是同一份 `persistInjections` 配置：
//   * persist_boot.js：document_start 在页面里（ISOLATED world）做第一次匹配；
//   * 本文件：动态注册 content script 失败的浏览器上，退回
//     webNavigation.onCommitted 之后重放。
// 只要两边口径不同，用户就会看到「有些页面 document_start 生效、有些页面要等导航后」，
// 而且很难自查。所以两份实现刻意写成同一份算法（含注释），改一处必须改另一处。
//
// 语义 = Chrome match pattern 的「scheme / host / path」三段匹配，但因为 match
// pattern 本身不允许写端口，而实际 URL 常带端口（本地服务、内网），所以这里：
//   1. `<all_urls>`          → 只放行 http/https（`file:` 必须显式声明）
//   2. `<scheme>://host/path`
//        - scheme 为 `*` 时仅放行 http/https；其余按字面相等比较（file/ftp…）
//        - host 只比 hostname（**天然忽略端口**）；写 `:端口` 也会被剥掉后忽略
//        - host 里的 `*` 匹配任意字符但不跨 `/`
//        - path 里的 `*` 匹配任意字符（含 `/`），整体 `^...$`
//   3. 不含 `://` 的写法（如 `*/api/*`）→ 退回「整条 URL 做 glob」
//   4. patterns 配置损坏（存在但不是非空字符串数组）→ **不注入**，只 console.warn；
//      只有 patterns 缺失（undefined/null）才退回 `<all_urls>`（见 normalizePersistPatterns）
//
// 与 CDP 侧（deriveUrlPattern / buildFetchPatterns）的关系见那里的大段注释：
// CDP 的 urlPattern 只负责**缩小暂停范围**，命中判定仍由 interceptRuleMatches 做，
// 所以两者允许不完全相同，但 CDP 侧必须至少覆盖本函数能命中的 URL。
// ============================================================================
function normalizePersistPatterns(patterns) {
  // 区分「没配」与「配了但坏了」：
  //   undefined / null        → 没配，沿用历史的 `<all_urls>`（兼容老条目）
  //   非空字符串数组           → 过滤掉非字符串项后使用
  //   其它（字符串 / 空数组 / 对象 / 全是非字符串项）→ 返回 null 表示「不注入」
  // 以前的写法是 `Array.isArray(patterns) && patterns.length ? … : ['<all_urls>']`，
  // 配置一坏就变成「全站注入」——对用户自建的持久注入属于 fail-open。
  if (patterns === undefined || patterns === null) return ['<all_urls>'];
  if (!Array.isArray(patterns)) return null;
  const list = patterns.filter((item) => typeof item === 'string' && item.trim() !== '');
  return list.length ? list : null;
}

function describePersistPatterns(patterns) {
  if (Array.isArray(patterns)) {
    const kinds = Array.from(new Set(patterns.map((item) => (item === null ? 'null' : typeof item))));
    return `array(length=${patterns.length}, itemTypes=${kinds.join('|') || 'none'})`;
  }
  return typeof patterns;
}

function matchesPersistPattern(patterns, url) {
  const list = normalizePersistPatterns(patterns);
  if (!list) {
    console.warn('[TMWD] 持久注入的 matches 配置损坏，已跳过注入（不会退回 <all_urls>）:', describePersistPatterns(patterns));
    return false;
  }
  const escape = (text) => text.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
  let parsed = null;
  try {
    parsed = new URL(url);
  } catch (_) {
    parsed = null;
  }
  return list.some((pattern) => {
    if (pattern === '<all_urls>') return /^https?:/i.test(url);
    if (!parsed) return false;
    const at = String(pattern).indexOf('://');
    if (at <= 0) {
      try {
        return new RegExp('^' + escape(pattern).replace(/\*/g, '.*') + '$', 'i').test(url);
      } catch (_) {
        return false;
      }
    }
    const scheme = pattern.slice(0, at).toLowerCase();
    const rest = pattern.slice(at + 3);
    const slash = rest.indexOf('/');
    // 端口写在 host 里时先剥掉：persist 只比 hostname，端口不该影响命中。
    const hostPattern = (slash >= 0 ? rest.slice(0, slash) : rest).toLowerCase().replace(/:\d+$/, '');
    const pathPattern = slash >= 0 ? rest.slice(slash) : '/';
    const protocol = String(parsed.protocol || '').replace(':', '').toLowerCase();
    if (scheme !== '*' && scheme !== protocol) return false;
    if (scheme === '*' && !/^https?$/.test(protocol)) return false;
    const hostRe = new RegExp('^' + escape(hostPattern).replace(/\*/g, '[^/]*') + '$', 'i');
    if (!hostRe.test(parsed.hostname)) return false;
    const pathRe = new RegExp('^' + escape(pathPattern).replace(/\*/g, '.*') + '$', 'i');
    return pathRe.test(parsed.pathname || '/');
  });
}

/// 无 eval 通道：页面 CSP 禁止 eval 时，用「注入文件 + 调用全局函数」执行固定操作。
/// 文件与 daemon 侧的 page_*.js 同源（tools/sync_extension 会一起复制）：
///   ops/page_read.js     -> window.__abcRead(opts)
///   ops/page_find.js     -> window.__abcFind(spec)
///   ops/page_snapshot.js -> window.__abcSnapshot(opts)
///   ops/page_action.js   -> window.__abcAct(op, target, params)
/// 注入到哪个 world 会直接影响可检测性：默认 ISOLATED（全局只在扩展的隔离世界里，
/// 站点读 `typeof window.__abcRead` 得到 undefined），只有必须操作页面原生对象的 op
/// （net / stealth）才用 MAIN —— 那时代价是站点能看见同名全局，属于必要之恶。
const PAGE_OP_FILES = {
  read: { file: 'ops/page_read.js', global: '__abcRead' },
  find: { file: 'ops/page_find.js', global: '__abcFind' },
  snapshot: { file: 'ops/page_snapshot.js', global: '__abcSnapshot' },
  action: { file: 'ops/page_action.js', global: '__abcAct' },
  wait: { file: 'ops/page_wait.js', global: '__abcWait' },
  // net / stealth 跑在 MAIN world，入口全局对页面可见、**可被页面抢占**；
  // requireOwn 让调用前校验实现上有没有我们打的 __abcOwn 标记。
  net: { file: 'ops/page_net.js', global: '__abcNet', requireOwn: true },
  challenge: { file: 'ops/page_challenge.js', global: '__abcChallenge', requireOwn: true },
  stealth: { file: 'ops/page_stealth.js', global: '__abcStealth', requireOwn: true }
};

/// 每个 op 应该在哪个 world 执行 —— 这张表就是「可检测性」的开关。
/// MAIN world 是页面自己的 window，注入的全局/包装过的原生对象站点全都看得见；
/// ISOLATED world 是扩展私有世界，`window.__abcRead` 之类的名字对站点不可见。
/// 所以只有**必须碰页面原生对象**的 op 才留在 MAIN：
///   * net       —— 要包装页面自己的 window.fetch / XMLHttpRequest 才能截到站点请求；
///   * stealth   —— 审计的就是「站点视角」，在 ISOLATED 里根本看不到自己在 MAIN 的残留；
///   * challenge —— 判定依赖站点自己挂在页面里的全局（window.turnstile、
///                  window._cf_chl_opt、hCaptcha 的 __hcaptcha_cfg 等）。ISOLATED world
///                  有独立的 window，读这些全局永远是 undefined → 回退路径一直报「无挑战」。
/// 其余 op 只是给 CSP 页面补一个执行入口，放 ISOLATED 即可（查表 miss 时也兜底 ISOLATED）。
const PAGE_OP_WORLD = {
  read: 'ISOLATED',
  find: 'ISOLATED',
  snapshot: 'ISOLATED',
  action: 'ISOLATED',
  wait: 'ISOLATED',
  challenge: 'MAIN',
  net: 'MAIN',
  stealth: 'MAIN'
};

/// world 归一化：daemon 侧把 world 当字符串透传，大小写并不稳定
/// （`isolated` / `Isolated`）。旧写法 `=== 'ISOLATED' ? 'ISOLATED' : 'MAIN'`
/// 会把小写 isolated 静默落到 MAIN —— 本该只活在扩展隔离世界的 `__abcRead`/`__abcExec`
/// 直接挂到页面 window 上，等于给站点一个「这是自动化」的判定依据。
/// 无法识别的值返回 ''，由调用方按自己的默认口径兜底（页面 op 有决策表，注入默认 MAIN）。
function normalizeWorld(world) {
  const value = String(world === undefined || world === null ? '' : world).trim().toUpperCase();
  return value === 'ISOLATED' || value === 'MAIN' ? value : '';
}

async function handlePageOp(msg) {
  const tabId = Number(msg.tabId);
  if (!Number.isInteger(tabId) || tabId <= 0) return { ok: false, error: 'tabId is required' };
  const op = String(msg.op || '');
  const entry = PAGE_OP_FILES[op];
  if (!entry) return { ok: false, error: 'unknown pageOp: ' + op };
  // world 解析三级：显式指定（daemon 有调用方依赖这个语义）> op 决策表 > 兜底 ISOLATED。
  // 以前的写法是 `=== 'ISOLATED' ? 'ISOLATED' : 'MAIN'`，等于默认全挂到 MAIN，
  // 把 __abcRead/__abcAct 这些名字直接暴露在页面 window 上 —— 一句话就能判定访客是自动化。
  const world = normalizeWorld(msg.world) || (PAGE_OP_WORLD[op] || 'ISOLATED');
  const target = buildScriptTarget(tabId, msg);
  const args = msg.args === undefined ? {} : msg.args;
  const invoke = async () => {
    const result = await chrome.scripting.executeScript({
      target,
      world,
      func: (name, payload, requireOwn) => {
        if (name === '__abcAct') {
          if (typeof window.__abcAct !== 'function') return { __needLoad: true };
          const p = payload || {};
          return window.__abcAct(p.op, p.target, p.params);
        }
        const fn = window[name];
        if (typeof fn !== 'function') return { __needLoad: true };
        // MAIN world 里页面可以抢先把同名全局定义成自己的函数：那样我们执行的
        // 就是**页面控制的代码**（read/find 返回伪造数据、net 伪造日志与规则、
        // stealth 报「很干净」）。只有带我们不可枚举标记 __abcOwn 的实现才执行。
        if (requireOwn && fn.__abcOwn !== true) return { __hijacked: true };
        return fn(payload || {});
      },
      args: [entry.global, args, entry.requireOwn === true]
    });
    return result && result[0] ? result[0].result : null;
  };
  try {
    // 每次调用都先注入文件，再执行。
    //
    // 以前是「先探测 window[name] 是不是函数，是就跳过注入」省一次往返。问题是
    // MAIN world 的页面可以预置同名函数，探测直接通过 → 整条 CSP 回退通道变成
    // 「执行页面提供的实现」。这几个文件本身是幂等的（状态挂在
    // __atb_net_* 槽位上，重复注入不会二次包装 fetch），代价只是多一次
    // executeScript（毫秒级），换来的是「执行的永远是扩展自己的代码」。
    await chrome.scripting.executeScript({ target, world, files: [entry.file] });
    const value = await invoke();
    if (value && typeof value === 'object' && value.__hijacked) {
      return {
        ok: false,
        error: 'pageOp: 页面已抢占 ' + entry.global + '，拒绝执行页面控制的实现（' + entry.file + '）'
      };
    }
    if (value && typeof value === 'object' && value.__needLoad) {
      return { ok: false, error: 'pageOp: 脚本注入被页面拒绝（' + entry.file + '）' };
    }
    return { ok: true, data: { op, value } };
  } catch (e) {
    return { ok: false, error: 'pageOp failed: ' + (e && e.message ? e.message : String(e)) };
  }
}

// 把页面脚本回包分类成「执行了/没执行/抛错了」。
///
/// `buildPageScript` 的成功分支固定返回 `{ok:true,data}`；失败分支返回
/// `{ok:false,error:{...},csp,executed}`。只有 `executed === true` 才说明用户代码
/// 已经跑起来（副作用可能已发生），`csp` / 编译期 SyntaxError 都表示一行没执行。
function classifyUserCodeResult(raw, started) {
  const duration_ms = Math.max(0, Date.now() - started);
  if (raw && typeof raw === 'object' && 'ok' in raw) {
    if (raw.ok === false) {
      const message = (raw.error && raw.error.message) || raw.error || 'inject failed';
      // __abcEvalStarted 是页面内的探针：eval 真正开始执行后才置 true。
      // 用户脚本自己抛带 "CSP" 字样的错误时 probe 已置位，不能按 csp 误判成没执行。
      const executed = raw.executed === undefined ? raw.csp !== true : raw.executed === true;
      return {
        ok: false,
        value: null,
        error: message,
        threw: executed ? message : null,
        executed,
        error_name: (raw.error && raw.error.name) || 'Error',
        csp: raw.csp === true,
        duration_ms
      };
    }
    return {
      ok: true,
      value: raw.data === undefined ? null : raw.data,
      error: null,
      threw: null,
      executed: true,
      duration_ms
    };
  }
  // executeScript 没抛错但也没有回包（frame 被移除 / 页面在注入瞬间导航）。
  // buildPageScript 永远返回对象，所以裸 null 不可能是用户的成功返回值。
  return {
    ok: false,
    value: null,
    error: 'inject: executeScript 没有返回结果（frame 已移除或页面在注入时导航）',
    threw: null,
    executed: false,
    error_name: 'InjectionError',
    csp: false,
    duration_ms
  };
}

/// 在指定目标里执行用户代码，语义与 exec 一致（支持 return / 裸表达式 / await）。
async function runUserCodeInTab(tabId, code, world, target) {
  const started = Date.now();
  let result;
  try {
    result = await chrome.scripting.executeScript({
      target: target || { tabId },
      // 小写 isolated 也必须进隔离世界；无法识别时按注入的历史默认 MAIN。
      world: normalizeWorld(world) || 'MAIN',
      func: async (s) => await eval(s),
      args: [buildPageScript(code)]
    });
  } catch (e) {
    // 注入本身失败（frame 被移除 / 无权限 / 页面已关闭）：脚本一行都没执行。
    return {
      ok: false,
      value: null,
      error: (e && e.message) || String(e),
      threw: null,
      executed: false,
      error_name: 'InjectionError',
      csp: false,
      duration_ms: Math.max(0, Date.now() - started)
    };
  }
  const raw = result && result[0] ? result[0].result : null;
  return classifyUserCodeResult(raw, started);
}

// ---------------------------------------------------------------------------
// 用户脚本模式（==UserScript== 元数据 + 最小 GM shim）
// ---------------------------------------------------------------------------

/// 解析油猴元数据块；没有识别到任何字段时返回 null（仍按普通脚本执行）。
function parseUserscriptMetadata(code) {
  const text = String(code || '');
  const block = text.match(/\/\/\s*==UserScript==([\s\S]*?)\/\/\s*==\/UserScript==/);
  if (!block) return null;
  const meta = { name: null, namespace: null, version: null, matches: [], grant: [], runAt: null };
  for (const line of block[1].split(/\r?\n/)) {
    const match = line.match(/^\s*\/\/\s*@([A-Za-z][A-Za-z-]*)\s+(.+?)\s*$/);
    if (!match) continue;
    const key = match[1].toLowerCase();
    const value = match[2];
    if (key === 'match' || key === 'include') meta.matches.push(value);
    else if (key === 'grant') meta.grant.push(value);
    else if (key === 'run-at') meta.runAt = value;
    else if (key === 'name') meta.name = value;
    else if (key === 'namespace') meta.namespace = value;
    else if (key === 'version') meta.version = value;
  }
  const recognised = !!(meta.name || meta.namespace || meta.version || meta.runAt
    || meta.matches.length || meta.grant.length);
  return recognised ? meta : null;
}

/// GM 存储命名空间：只保留 `[A-Za-z0-9._-]`，空了就回退默认值
/// （localStorage key 里出现冒号/空白时，前缀匹配与清理都不可靠）。
function sanitizeGmNamespace(raw) {
  const text = String(raw === undefined || raw === null ? '' : raw).trim();
  const safe = text.replace(/[^A-Za-z0-9._-]/g, '_');
  return safe || 'atb-userscript';
}

/// 生成「GM shim + 用户脚本」的 IIFE。用户代码原样嵌进内层 async 函数，
/// 因此 `return` / `await` 都可用；GM_* 是内层可见的词法变量（与油猴一致，
/// 不挂到 window 上，避免被页面脚本误用/覆盖）。
function buildUserscriptSource(code, meta, gmNamespace) {
  const info = {
    script: {
      name: meta ? meta.name : null,
      version: meta ? meta.version : null,
      namespace: meta ? meta.namespace : null,
      matches: meta ? meta.matches : [],
      grant: meta ? meta.grant : [],
      runAt: meta ? meta.runAt : null
    },
    scriptHandler: 'atb-userscript',
    version: extensionVersion
  };
  const prefix = '__atb_gm__:' + sanitizeGmNamespace(gmNamespace) + ':';
  return [
    '(function () {',
    "  'use strict';",
    '  var __atbPrefix = ' + JSON.stringify(prefix) + ';',
    '  var __atbMem = Object.create(null);',
    '  var __atbInfo = ' + JSON.stringify(info) + ';',
    '  var unsafeWindow = window;',
    '  var GM_info = __atbInfo;',
    '  function __atbGet(key, fallback) {',
    '    var name = __atbPrefix + String(key);',
    '    try {',
    '      var raw = localStorage.getItem(name);',
    '      if (raw === null || raw === undefined) {',
    '        return Object.prototype.hasOwnProperty.call(__atbMem, String(key)) ? __atbMem[String(key)] : fallback;',
    '      }',
    '      return JSON.parse(raw);',
    '    } catch (e) {',
    '      return Object.prototype.hasOwnProperty.call(__atbMem, String(key)) ? __atbMem[String(key)] : fallback;',
    '    }',
    '  }',
    '  function __atbSet(key, value) {',
    '    var name = __atbPrefix + String(key);',
    '    __atbMem[String(key)] = value;',
    '    try { localStorage.setItem(name, JSON.stringify(value)); } catch (_) {}',
    '  }',
    '  function __atbDelete(key) {',
    '    delete __atbMem[String(key)];',
    '    try { localStorage.removeItem(__atbPrefix + String(key)); } catch (_) {}',
    '  }',
    '  function __atbList() {',
    '    var out = [];',
    '    var seen = Object.create(null);',
    '    try {',
    '      for (var i = 0; i < localStorage.length; i += 1) {',
    '        var full = localStorage.key(i);',
    '        if (full && full.indexOf(__atbPrefix) === 0) {',
    '          var k = full.slice(__atbPrefix.length);',
    '          if (!seen[k]) { seen[k] = true; out.push(k); }',
    '        }',
    '      }',
    '    } catch (_) {}',
    '    for (var m in __atbMem) { if (!seen[m]) { seen[m] = true; out.push(m); } }',
    '    return out;',
    '  }',
    '  var GM_getValue = __atbGet;',
    '  var GM_setValue = __atbSet;',
    '  var GM_deleteValue = __atbDelete;',
    '  var GM_listValues = __atbList;',
    '  var GM_log = function () { try { console.log.apply(console, [\'[ATB GM]\'].concat(Array.prototype.slice.call(arguments))); } catch (_) {} };',
    '  var GM_addStyle = function (css) {',
    "    var style = document.createElement('style');",
    '    style.textContent = String(css || \'\');',
    '    (document.head || document.documentElement).appendChild(style);',
    '    return style;',
    '  };',
    '  var __atbMenuSeq = 0;',
    '  var GM_registerMenuCommand = function (name, fn) {',
    '    var id = __atbMenuSeq += 1;',
    '    window.__atbGmMenu = window.__atbGmMenu || [];',
    '    window.__atbGmMenu.push({ id: id, name: String(name || \'\'), fn: typeof fn === \'function\' ? fn : null });',
    '    try { console.log(\'[ATB GM] menu registered:\', name); } catch (_) {}',
    '    return id;',
    '  };',
    '  var GM_xmlhttpRequest = function () {',
    '    throw new Error(\'GM_xmlhttpRequest is not supported by atb userscript shim\');',
    '  };',
    '  return (async function () {',
    code,
    '  }).call(window);',
    '})()'
  ].join('\n');
}

/// 按注入选项准备最终代码：userscript=true 时解析元数据并包 GM shim。
function prepareInjectCode(code, options) {
  const opts = options || {};
  if (opts.userscript !== true) {
    return { code, meta: null, userscript: false };
  }
  const meta = parseUserscriptMetadata(code);
  const namespace = sanitizeGmNamespace(
    opts.gmNamespace || (meta && meta.namespace) || (meta && meta.name) || 'atb-userscript'
  );
  return {
    code: buildUserscriptSource(code, meta, namespace),
    meta,
    userscript: true,
    gmNamespace: namespace
  };
}

// 注入前清理旧宿主。选择器错误只记 errors，不中断主脚本。
async function runCleanupInTab(tabId, cleanupSelectors, cleanupCode, world, target) {
  const selectors = Array.isArray(cleanupSelectors)
    ? cleanupSelectors.map((item) => String(item || '').trim()).filter(Boolean)
    : [];
  const result = { selectors: [], code_ok: null, errors: [] };
  if (!selectors.length && !cleanupCode) return result;
  if (selectors.length) {
    try {
      const rows = await chrome.scripting.executeScript({
        target: target || { tabId },
        world: normalizeWorld(world) || 'MAIN',
        func: (list) => list.map((selector) => {
          try {
            const nodes = Array.from(document.querySelectorAll(selector));
            nodes.forEach((node) => node.remove());
            return { selector, removed: nodes.length };
          } catch (e) {
            return { selector, removed: 0, error: (e && e.message) || String(e) };
          }
        }),
        args: [selectors]
      });
      // allFrames 时每个 frame 各回一份数组；逐个展开，统计仍然按选择器分组。
      for (const frame of rows || []) {
        for (const row of Array.isArray(frame && frame.result) ? frame.result : []) {
          if (!row) continue;
          if (row.error) result.errors.push(row.selector + ': ' + row.error);
          const existing = result.selectors.find((item) => item.selector === row.selector);
          if (existing) existing.removed += Number(row.removed) || 0;
          else result.selectors.push({ selector: row.selector, removed: Number(row.removed) || 0 });
        }
      }
    } catch (e) {
      result.errors.push('cleanup selectors failed: ' + ((e && e.message) || String(e)));
    }
  }
  if (cleanupCode) {
    try {
      const outcome = await runUserCodeInTab(tabId, String(cleanupCode), world, target || { tabId });
      result.code_ok = outcome.ok === true;
      if (!outcome.ok) result.errors.push('cleanup code: ' + (outcome.error || 'failed'));
    } catch (e) {
      result.code_ok = false;
      result.errors.push('cleanup code: ' + ((e && e.message) || String(e)));
    }
  }
  return result;
}

/// persist 的条目在重放时也要按原选项执行（userscript / cleanup）。
async function applyPersistEntry(tabId, entry, target) {
  const world = entry.world || 'MAIN';
  const options = {
    userscript: entry.userscript === true,
    gmNamespace: entry.gmNamespace || null,
    cleanupSelectors: entry.cleanupSelectors || [],
    cleanupCode: entry.cleanupCode || null
  };
  const prepared = prepareInjectCode(entry.code, options);
  const cleanup = await runCleanupInTab(
    tabId,
    options.cleanupSelectors,
    options.cleanupCode,
    world,
    target || { tabId }
  );
  const outcome = await runUserCodeInTab(tabId, prepared.code, world, target || { tabId });
  return { outcome, cleanup, userscript_meta: prepared.meta };
}

/// 持久注入条目是否已过期。
/// `expiresAt` 缺失 = 旧条目（旧行为：永久）；`null` = 显式永久（--persist-forever）。
function persistEntryExpiresAt(entry) {
  const at = Number(entry && entry.expiresAt);
  return Number.isFinite(at) && at > 0 ? at : null;
}

function isPersistEntryExpired(entry, now) {
  const at = persistEntryExpiresAt(entry);
  return at !== null && at <= (now === undefined ? Date.now() : now);
}

// 返回值摘要。页面侧（buildExecScript）已经把 DOM / Window / Function /
/// Error 转成 `{__type:...}`；这里兜住「结构化克隆后残留的 Error/函数」并统一
/// 计算 value_type。普通对象 / 数组 / 原始值原样返回。
function describeValue(value) {
  if (value === undefined || value === null) return value;
  if (typeof value === 'function') return { __type: 'function', name: String(value.name || '') };
  if (typeof value !== 'object') return value;
  if (value.__type) return value;
  try { if (value instanceof Error) return { __type: 'error', name: String(value.name || 'Error'), message: String(value.message || '').slice(0, 500) }; } catch (_) {}
  try {
    if (typeof Window !== 'undefined' && value instanceof Window) {
      return { __type: 'window', url: String((value.location && value.location.href) || ''), title: String((value.document && value.document.title) || '') };
    }
  } catch (_) {}
  if (value.nodeType === 1 && value.tagName) {
    let rect = { x: 0, y: 0, width: 0, height: 0 };
    try { const r = value.getBoundingClientRect(); rect = { x: r.x, y: r.y, width: r.width, height: r.height }; } catch (_) {}
    return {
      __type: 'element',
      tag: String(value.tagName || '').toLowerCase(),
      id: String(value.id || ''),
      class: typeof value.className === 'string' ? value.className : String((value.getAttribute && value.getAttribute('class')) || ''),
      rect,
      text: String(value.innerText || value.textContent || '').trim().slice(0, 200)
    };
  }
  if (value.nodeType === 3) return { __type: 'text', text: String(value.textContent || '').slice(0, 200) };
  return value;
}

/// persistInjections 的读-改-写必须串行。
///
/// 之前每个 persist / remove 都是独立的 `get → 改 → set`：batch 里两条 persist，
/// 或 daemon 并发下发两条时，后写会把前写整份覆盖，条目静默丢失（用户只会发现
/// 「我明明加过，过一会就没了」）。这个 key 只有 background.js 自己写，所以一条
/// promise 链就够，不需要引入锁库。
let persistStoreQueue = Promise.resolve();

function withPersistStoreLock(task) {
  const run = persistStoreQueue.then(task, task);
  // 锁链条自身不能被 reject 卡死：业务错误照常抛给调用方，链条上只留已 settle 的尾巴。
  persistStoreQueue = run.then(() => null, () => null);
  return run;
}

async function readPersistInjections() {
  const store = await chrome.storage.local.get({ persistInjections: {} });
  const map = store.persistInjections;
  return map && typeof map === 'object' ? map : {};
}

/// 过期条目清理：与 persist / remove 共用同一条 promise 链，避免
/// 「读整份 map → 删除过期 → 写回」覆盖并发 persist 的新条目。
async function pruneExpiredPersistInjections(now) {
  return withPersistStoreLock(async () => {
    const map = await readPersistInjections();
    const removed = [];
    for (const [name, entry] of Object.entries(map)) {
      if (isPersistEntryExpired(entry, now === undefined ? Date.now() : now)) {
        delete map[name];
        removed.push(name);
      }
    }
    if (removed.length) await chrome.storage.local.set({ persistInjections: map });
    return { map, removed };
  });
}

/// 脚本注入：一次性执行（run）或按名字持久化（persist/remove/list）。
///
/// 持久化不是 registerContentScripts：MV3 只允许注册文件、不允许内联代码，
/// 所以改成「存代码 + 导航时自动重放」，代价是 document_start 的几毫秒窗口。
async function handleInject(msg) {
  const action = String(msg.action || 'run');
  // 大小写归一化后再落库，持久化条目里的 world 也必须是规范值。
  const world = normalizeWorld(msg.world) || 'MAIN';
  const cleanupSelectors = Array.isArray(msg.cleanupSelectors)
    ? msg.cleanupSelectors.map((item) => String(item || '').trim()).filter(Boolean)
    : [];
  const cleanupCode = msg.cleanupCode ? String(msg.cleanupCode) : '';
  const gmNamespace = msg.gmNamespace ? String(msg.gmNamespace) : null;

  if (action === 'list') {
    // 列出之前先清掉过期条目，并把旧条目（没有 expiresAt）迁移成
    // 「现在 + defaultTtlSec」。整段必须进锁：list 自身会写回 storage。
    const now = Date.now();
    const defaultTtlSec = Number(msg.defaultTtlSec) > 0 ? Number(msg.defaultTtlSec) : 7200;
    const snapshot = await withPersistStoreLock(async () => {
      const map = await readPersistInjections();
      let changed = false;
      for (const [name, entry] of Object.entries(map)) {
        if (isPersistEntryExpired(entry, now)) {
          delete map[name];
          changed = true;
          continue;
        }
        // 只迁移「缺少 expiresAt」的旧条目；显式 null 是 --persist-forever，不能动。
        if (entry && typeof entry === 'object' && entry.expiresAt === undefined) {
          entry.expiresAt = now + defaultTtlSec * 1000;
          entry.legacy = true;
          changed = true;
        }
      }
      if (changed) await chrome.storage.local.set({ persistInjections: map });
      return map;
    });
    const items = Object.entries(snapshot).map(([name, rawEntry]) => {
      const entry = rawEntry && typeof rawEntry === 'object' ? rawEntry : {};
      const expiresAt = persistEntryExpiresAt(entry);
      const expiresInSec = expiresAt === null ? null : Math.max(0, Math.ceil((expiresAt - now) / 1000));
      const item = {
        name,
        world: entry.world || 'MAIN',
        matches: entry.matches || ['<all_urls>'],
        code_length: String(entry.code || '').length,
        code_preview: String(entry.code || '').slice(0, 120),
        created_at: entry.createdAt || null,
        expires_at: expiresAt,
        expires_in_sec: expiresInSec,
        expired: false
      };
      if (expiresInSec !== null && expiresInSec > 0) item.warning = 'long-lived injection';
      if (entry.legacy === true) item.legacy = true;
      if (entry.userscript === true) item.userscript = true;
      return item;
    });
    const count = items.length;
    if (count === 0) await ensurePersistBoot(false);
    return {
      ok: true,
      data: {
        action,
        count,
        items,
        warnings: [`${count} 个持久注入会在每次导航后重放；--persist 默认 2 小时过期`]
      }
    };
  }

  if (action === 'remove') {
    const name = String(msg.name || '').trim();
    if (!name) return { ok: false, error: 'name is required' };
    const saved = await withPersistStoreLock(async () => {
      const map = await readPersistInjections();
      for (const [key, item] of Object.entries(map)) {
        if (isPersistEntryExpired(item)) delete map[key];
      }
      const existed = !!map[name];
      delete map[name];
      await chrome.storage.local.set({ persistInjections: map });
      return { existed, count: Object.keys(map).length };
    });
    if (saved.count === 0) await ensurePersistBoot(false);
    return { ok: true, data: { action, name, removed: saved.existed, count: saved.count, total_persist_count: saved.count } };
  }

  if (action === 'persist') {
    const name = String(msg.name || '').trim();
    if (!name) return { ok: false, error: 'name is required' };
    const code = String(msg.code || '');
    if (!code) return { ok: false, error: 'code is required' };
    // 只有显式传 null/undefined 才表示 `<all_urls>`。旧实现把「非数组/空数组」
    // 一律当没配 → 悄悄退回 `<all_urls>`，于是 matches:"*/api/*"（字符串）会变成全站注入，
    // 用户以为只注入了接口页，实际每个页面都被执行。这里改用与读回路径同一个校验函数，
    // 非法形状直接报错、不写 storage。
    const matches = normalizePersistPatterns(msg.matches);
    if (!matches) {
      return { ok: false, error: 'matches 必须是非空字符串数组（传 null 才表示 <all_urls>）' };
    }
    // TTL 口径。
    // - 旧 daemon 三个字段都没传 → 保持旧行为「永久」，升级扩展不能让旧 CLI 的注入突然失效；
    // - 新 daemon 显式给 --ttl → ttlSec=N 优先；
    // - 没给 --ttl 但给了 defaultTtlSec → 优先用户在本扩展设置页选的 persistTtlSec，
    //   没有设置再用 daemon 的默认值；
    // - persistForever=true → 永久。
    const hasTtlProtocol =
      msg.persistForever !== undefined ||
      msg.ttlSec !== undefined ||
      msg.defaultTtlSec !== undefined;
    let ttlSec = null;
    if (msg.persistForever === true) {
      ttlSec = null;
    } else if (hasTtlProtocol) {
      const explicit = Number(msg.ttlSec);
      if (Number.isFinite(explicit) && explicit > 0) {
        ttlSec = Math.max(1, Math.floor(explicit));
      } else {
        const fallbackRaw = Number(msg.defaultTtlSec);
        const fallback =
          Number.isFinite(fallbackRaw) && fallbackRaw > 0 ? Math.floor(fallbackRaw) : 7200;
        let stored = fallback;
        try {
          const settings = await chrome.storage.local.get({ persistTtlSec: fallback });
          const value = Number(settings.persistTtlSec);
          if (Number.isFinite(value) && value > 0) stored = Math.floor(value);
        } catch (_) {}
        ttlSec = stored;
      }
    }
    const createdAt = Date.now();
    const expiresAt = ttlSec === null ? null : createdAt + ttlSec * 1000;
    const userscript = msg.userscript === true;
    const prepared = prepareInjectCode(code, { userscript, gmNamespace });
    const entry = {
      code,
      world,
      matches,
      createdAt,
      expiresAt,
      userscript,
      gmNamespace: userscript ? (gmNamespace || prepared.gmNamespace || null) : null,
      cleanupSelectors,
      cleanupCode: cleanupCode || null
    };
    // 读-改-写整段进锁；下面的实际注入留在锁外，避免一条慢注入堵住其它 persist。
    // 顺手清掉过期条目，避免 total_persist_count 把死条目算进去。
    const saved = await withPersistStoreLock(async () => {
      const map = await readPersistInjections();
      for (const [key, item] of Object.entries(map)) {
        if (isPersistEntryExpired(item, createdAt)) delete map[key];
      }
      map[name] = entry;
      await chrome.storage.local.set({ persistInjections: map });
      return { count: Object.keys(map).length };
    });
    await ensurePersistBoot(true);
    const cleanup = { selectors: [], code_ok: null, errors: [] };
    const apply = { executed: false, threw: null };
    let applied = false;
    try {
      const tabId = Number(msg.tabId);
      if (Number.isInteger(tabId) && tabId > 0) {
        const result = await applyPersistEntry(tabId, entry, buildScriptTarget(tabId, msg));
        Object.assign(cleanup, result.cleanup);
        applied = result.outcome.ok === true;
        apply.executed = result.outcome.executed === true;
        apply.threw = result.outcome.threw || null;
        if (!result.outcome.ok) console.log('[TMWD] persist apply failed:', result.outcome.error);
      }
    } catch (e) {
      console.log('[TMWD] persist apply failed:', e.message);
    }
    const expiresInSec = expiresAt === null ? null : Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000));
    const data = {
      action,
      name,
      world,
      matches,
      applied,
      count: saved.count,
      total_persist_count: saved.count,
      expires_at: expiresAt,
      expires_in_sec: expiresInSec,
      persist_forever: expiresAt === null,
      warning: expiresAt === null
        ? '持久注入永久驻留：每次导航后重放，会与页面/其它用户脚本长期共存；用 --remove 清理'
        : `持久注入会在每次导航后重放，并在 ${expiresInSec} 秒后过期`,
      apply,
      cleanup
    };
    if (userscript) {
      data.userscript_meta = prepared.meta;
      if (!prepared.meta) {
        data.userscript_warning = '未找到 ==UserScript== 元数据';
        data.warning += '；未找到 ==UserScript== 元数据';
      }
    }
    return { ok: true, data };
  }

  // run：一次性执行
  const tabId = Number(msg.tabId);
  if (!Number.isInteger(tabId) || tabId <= 0) return { ok: false, error: 'tabId is required' };
  const code = String(msg.code || '');
  if (!code) return { ok: false, error: 'code is required' };
  const started = Date.now();
  const target = buildScriptTarget(tabId, msg);
  const userscript = msg.userscript === true;
  const prepared = prepareInjectCode(code, { userscript, gmNamespace });
  let cleanup;
  let outcome;
  try {
    cleanup = await runCleanupInTab(tabId, cleanupSelectors, cleanupCode, world, target);
    outcome = await runUserCodeInTab(tabId, prepared.code, world, target);
  } catch (e) {
    const message = (e && e.message) || String(e);
    const cleanupResult = cleanup || { selectors: [], code_ok: null, errors: [] };
    return {
      ok: false,
      error_code: 'inject_failed',
      error: message,
      action: 'run',
      world,
      executed: false,
      returned: null,
      threw: null,
      value: null,
      duration_ms: Date.now() - started,
      cleanup: cleanupResult,
      data: {
        ok: false,
        action: 'run',
        world,
        executed: false,
        returned: null,
        threw: null,
        value: null,
        error_code: 'inject_failed',
        error: message,
        duration_ms: Date.now() - started,
        cleanup: cleanupResult
      }
    };
  }
  const base = {
    action: 'run',
    world,
    duration_ms: Math.max(0, Date.now() - started),
    cleanup
  };
  if (userscript) {
    base.userscript_meta = prepared.meta;
    if (!prepared.meta) base.warning = '未找到 ==UserScript== 元数据';
  }
  if (!outcome.ok) {
    // executed=true 表示脚本已经开始执行后抛错（副作用可能已发生）；
    // false 表示注入/编译阶段失败，一行都没执行。
    const codeName = outcome.executed ? 'inject_script_threw' : 'inject_failed';
    const threw = outcome.executed ? (outcome.threw || outcome.error) : null;
    const data = Object.assign({}, base, {
      ok: false,
      executed: outcome.executed === true,
      returned: null,
      threw,
      value: null,
      error_code: codeName,
      error: outcome.error
    });
    // 顶层同样展开一份：daemon 走 data，popup / 其它直接调用方读顶层。
    return Object.assign({ ok: false, error_code: codeName, error: outcome.error, data }, data);
  }
  const value = describeValue(outcome.value);
  const data = Object.assign({}, base, {
    executed: true,
    returned: value,
    threw: null,
    value,
    value_type: typeof value
  });
  if (value === null) {
    data.note = '脚本执行完成，返回值为 null；如果脚本有重入 guard，可能提前 return';
  }
  return { ok: true, data };
}

const PERSIST_BOOT_ID = 'abc-persist-boot';
let persistBootReady = false;

// 把目标标签切到前台（可选同时聚焦窗口）。
/// `chrome.tabs.update({active:true})` 只切换窗口内 active tab；窗口最小化时
/// visibilityState 仍是 hidden，所以需要 `windows.update({focused:true})`。
async function handleFocusTab(msg) {
  const tabId = Number(msg.tabId);
  if (!Number.isInteger(tabId) || tabId <= 0) return { ok: false, error: 'tabId is required' };
  try {
    const tab = await chrome.tabs.update(tabId, { active: true });
    if (!tab) return { ok: false, error: 'tab not found: ' + tabId };
    const focusWindow = msg.focusWindow !== false && msg.focus_window !== false;
    let windowStarted = false;
    let windowError = null;
    if (focusWindow && tab.windowId) {
      try {
        // 最小化窗口只设 focused 在部分平台不会恢复窗口；先恢复 normal 再聚焦。
        const win = await chrome.windows.get(tab.windowId);
        if (win && win.state === 'minimized') {
          await chrome.windows.update(tab.windowId, { state: 'normal' });
        }
        await chrome.windows.update(tab.windowId, { focused: true });
        windowStarted = true;
      } catch (e) {
        windowError = (e && e.message) || String(e);
      }
    }
    return {
      ok: true,
      data: {
        active: true,
        tab_id: tab.id,
        url: tab.url || null,
        title: tab.title || null,
        window_id: tab.windowId || null,
        window_focused: windowStarted,
        window_error: windowError
      }
    };
  } catch (e) {
    return { ok: false, error: (e && e.message) || String(e) };
  }
}

// 列出已安装扩展。`management` 是 optional_permissions —— 用户没在设置页
/// 点授权时 `chrome.management` 不存在，必须给出可执行的 hint，而不是只报一句
/// "undefined is not a function"。
async function handleListExtensions(msg) {
  if (!chrome.management || typeof chrome.management.getAll !== 'function') {
    return {
      ok: false,
      error_code: 'permission_required',
      error: '扩展未获得 management 权限，无法列出已安装扩展',
      hint: '在扩展设置页点击「授权读取扩展列表」'
    };
  }
  try {
    const includeDisabled = msg.includeDisabled === true || msg.include_disabled === true;
    const all = await chrome.management.getAll();
    const items = (all || [])
      .filter((item) => item && (includeDisabled || item.enabled))
      .map((item) => ({
        id: item.id,
        name: item.name,
        version: item.version,
        enabled: item.enabled === true,
        type: item.type || null,
        install_type: item.installType || null,
        homepage_url: item.homepageUrl || null,
        description: item.description || ''
      }));
    return {
      ok: true,
      data: { permission_granted: true, count: items.length, items }
    };
  } catch (e) {
    const message = (e && e.message) || String(e);
    if (/permission|not available|undefined/i.test(message)) {
      return {
        ok: false,
        error_code: 'permission_required',
        error: '扩展未获得 management 权限：' + message,
        hint: '在扩展设置页点击「授权读取扩展列表」'
      };
    }
    return { ok: false, error: message };
  }
}

/// 让 persist_boot.js 在 document_start 运行，这样持久注入能更早落到页面里。
/// 注册失败（旧版 Chrome / 权限受限）时退回 webNavigation.onCommitted 重放。
async function ensurePersistBoot(enable) {
  try {
    const existing = await chrome.scripting.getRegisteredContentScripts({ ids: [PERSIST_BOOT_ID] });
    if (enable) {
      if (existing && existing.length) {
        persistBootReady = true;
        return true;
      }
      await chrome.scripting.registerContentScripts([{
        id: PERSIST_BOOT_ID,
        js: ['persist_boot.js'],
        matches: ['<all_urls>'],
        runAt: 'document_start',
        allFrames: false,
        world: 'ISOLATED',
        persistAcrossSessions: true
      }]);
      persistBootReady = true;
      return true;
    }
    if (existing && existing.length) {
      await chrome.scripting.unregisterContentScripts({ ids: [PERSIST_BOOT_ID] });
    }
    persistBootReady = false;
    return true;
  } catch (e) {
    console.log('[TMWD] persist boot toggle failed:', e.message);
    persistBootReady = false;
    return false;
  }
}

async function syncPersistBoot() {
  // 冷启动时顺带清过期条目：否则「只留一堆过期注入」也会让我们注册
  // persist_boot.js，每次导航都白跑一次 storage 读取。
  let count = 0;
  try {
    const { map } = await pruneExpiredPersistInjections();
    count = Object.keys(map).length;
  } catch (e) {
    const store = await chrome.storage.local.get({ persistInjections: {} });
    count = Object.keys(store.persistInjections || {}).length;
  }
  await ensurePersistBoot(count > 0);
}

// ---------------------------------------------------------------------------
// 持久注入「同一次页面导航只执行一次」的去重
//
// persistBootReady 是内存标志：SW 冷启动窗口内它是 false，于是 webNavigation.onCommitted
// 会重放一次，而 persist_boot.js 的 document_start 注入同时也在跑 → 用户自定义的
// persist 代码执行两遍（net hook 不受影响，page_net.js 有自己的幂等保护）。
//
// 这里只防「重复执行」，**不改变注入时机**：两条路径谁先到谁执行，另一条在 1.5s
// 窗口内被丢弃。键 = tabId + frameId + 去掉 hash 的 URL，所以「换一份配置后再注入
// 同一个页面」这种合法重复不会被误挡（它发生在窗口之外）。
//
// 优先存 chrome.storage.session：SW 被回收后去重表还在，正好覆盖「冷启动窗口内两条
// 注入路径都到齐」这个唯一场景；没有 session 命名空间时退回内存 Map（同一 SW 实例内
// 同样能挡住重复）。
// ---------------------------------------------------------------------------
const PERSIST_DEDUPE_KEY = 'persistInjectDedupe';
const PERSIST_DEDUPE_WINDOW_MS = 1500;
const PERSIST_DEDUPE_MAX_AGE_MS = 60 * 1000;
const persistDedupeMemory = new Map();

function persistDedupeArea() {
  return chrome.storage?.session || null;
}

function persistDedupeKeyOf(tabId, frameId, url) {
  const frame = Number(frameId);
  return `${tabId}|${Number.isInteger(frame) ? frame : 0}|${String(url || '').split('#')[0]}`;
}

/// 返回 true = 这次可以执行。存储不可用/读失败时一律放行：
/// 宁可多执行一次，也不能因为去重把用户的注入整条挡掉。
///
/// 并发串行化：实现体是「读去重表 → 判窗口 → 写回」，check-then-set 不是原子操作。
/// document_start 的 persist_boot 请求与 webNavigation.onCommitted 的重放几乎同时到达时，
/// 两条都会读到「没有记录」，于是同一次导航各执行一遍 —— 去重形同虚设。
/// 用一条 promise 链把整段（含 storage 不可用时的内存分支）串起来：后到的调用
/// 会在前一个写完之后再读，才能看到它刚写的时间戳。
let persistClaimChain = Promise.resolve();

function claimPersistInjection(tabId, frameId, url) {
  const run = () => claimPersistInjectionInner(tabId, frameId, url);
  const next = persistClaimChain.then(run, run);
  persistClaimChain = next.then(() => null, () => null);
  return next;
}

async function claimPersistInjectionInner(tabId, frameId, url) {
  if (!Number.isInteger(tabId)) return true;
  const key = persistDedupeKeyOf(tabId, frameId, url);
  const now = Date.now();
  const area = persistDedupeArea();
  if (!area) {
    for (const [k, ts] of persistDedupeMemory) {
      if (now - ts > PERSIST_DEDUPE_MAX_AGE_MS) persistDedupeMemory.delete(k);
    }
    const last = persistDedupeMemory.get(key);
    if (last !== undefined && now - last < PERSIST_DEDUPE_WINDOW_MS) return false;
    persistDedupeMemory.set(key, now);
    return true;
  }
  try {
    const stored = await area.get({ [PERSIST_DEDUPE_KEY]: {} });
    const raw = stored[PERSIST_DEDUPE_KEY];
    const map = raw && typeof raw === 'object' ? raw : {};
    const last = Number(map[key]);
    if (Number.isFinite(last) && now - last < PERSIST_DEDUPE_WINDOW_MS) return false;
    map[key] = now;
    for (const k of Object.keys(map)) {
      if (now - Number(map[k]) > PERSIST_DEDUPE_MAX_AGE_MS) delete map[k];
    }
    await area.set({ [PERSIST_DEDUPE_KEY]: map });
    return true;
  } catch (e) {
    console.warn('[TMWD] 持久注入去重表不可用，本次不加去重:', e && e.message);
    return true;
  }
}

/// document_start 的 boot 脚本发来的请求：立刻在 MAIN world 执行对应代码。
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.type !== 'persist-apply') return false;
  // 同款纵深防御：只接受本扩展、且来自真实标签页的消息。
  if (sender && sender.id && sender.id !== chrome.runtime?.id) return false;
  const tabId = sender?.tab?.id;
  if (!Number.isInteger(tabId)) return false;
  bgTask('persistApply', async () => {
    const applied = [];
    // 与 onCommitted 的重放共用同一个去重键：同一次导航最多执行一遍。
    if (!(await claimPersistInjection(tabId, sender.frameId, sender.url || sender.tab?.url))) {
      sendResponse({ ok: true, applied, deduped: true });
      return;
    }
    // 重放前先清过期条目；boot 传来的 entry 可能已经过期（document_start
    // 到 SW 收到消息之间隔了一次导航），以 storage 里的最新条目为准。
    let live = null;
    try {
      const pruned = await pruneExpiredPersistInjections();
      live = pruned.map;
    } catch (e) {
      console.warn('[TMWD] 持久注入过期清理失败，本次按消息内容重放:', e && e.message);
    }
    for (const requested of message.entries || []) {
      const entry = live ? live[requested.name] : requested;
      // 已删除/已过期不是「投递失败」：标 ok:true + skipped，避免 persist_boot
      // 的失败重试和 content.js 的 pending 补发反复追一条死配置。
      if (!entry) {
        applied.push({ name: requested.name, ok: true, skipped: true, error: 'removed' });
        continue;
      }
      if (isPersistEntryExpired(entry)) {
        applied.push({ name: requested.name, ok: true, skipped: true, expired: true, error: 'expired' });
        continue;
      }
      try {
        const result = await applyPersistEntry(tabId, entry, { tabId });
        // 执行后抛错也算「已送达」：再重试只会重复副作用，persist_boot 不该重发。
        const delivered = result.outcome.ok === true || result.outcome.executed === true;
        applied.push({
          name: requested.name,
          ok: delivered,
          executed: result.outcome.executed === true,
          threw: result.outcome.threw || null,
          error: result.outcome.error || null
        });
      } catch (e) {
        applied.push({ name: requested.name, ok: false, error: (e && e.message) || String(e) });
      }
    }
    sendResponse({ ok: true, applied });
  });
  return true;
});

/// persist_boot.js 发现过期条目时发来的「请清理」消息。
/// 内容脚本不能安全地直接写整份 map（会覆盖并发的 persist），必须交给 SW 的锁。
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.type !== 'persist-prune-expired') return false;
  if (sender && sender.id && sender.id !== chrome.runtime?.id) return false;
  bgTask('persistPrune', async () => {
    try {
      const { removed } = await pruneExpiredPersistInjections();
      sendResponse({ ok: true, removed: removed.length, names: removed });
    } catch (e) {
      sendResponse({ ok: false, error: (e && e.message) || String(e) });
    }
  });
  return true;
});

function summarizeDownload(item) {
  if (!item) return null;
  // startTime / endTime 是 ISO 字符串：daemon 侧排序与「下载耗时」都要毫秒数，
  // 让扩展直接算好，避免每个消费方各写一遍 Date.parse（且解析失败的处理各不相同）。
  const parseMs = (value) => {
    if (!value) return null;
    const ms = Date.parse(value);
    return Number.isFinite(ms) ? ms : null;
  };
  return {
    id: item.id,
    filename: item.filename,
    url: item.url,
    final_url: item.finalUrl,
    state: item.state,
    paused: !!item.paused,
    can_resume: !!item.canResume,
    bytes_received: item.bytesReceived,
    total_bytes: item.totalBytes,
    mime: item.mime,
    start_time: item.startTime,
    end_time: item.endTime,
    startTimeMs: parseMs(item.startTime),
    endTimeMs: parseMs(item.endTime),
    error: item.error || null,
    exists: item.exists
  };
}

/// 从 URL 末段推导下载文件名（去 query / hash，解百分号转义）。
/// 推导不出来就返回空串，让 Chrome 按 Content-Disposition 决定，
/// 而不是塞一个没有扩展名的字面量（例如 `download` 这种必然要用户手动改名的文件）。
function deriveDownloadFilename(url) {
  let parsed;
  try {
    parsed = new URL(String(url));
  } catch (_) {
    return '';
  }
  if (!/^(https?|ftp|file):$/i.test(parsed.protocol)) return '';
  const segments = String(parsed.pathname || '').split('/').filter(Boolean);
  if (!segments.length) return '';
  let name = segments[segments.length - 1];
  try { name = decodeURIComponent(name); } catch (_) { /* 保留原样 */ }
  // Chrome 会拒绝这些字符，换成下划线而不是让整条下载失败。
  name = name.replace(/[\\/:*?"<>|\x00-\x1f]/g, '_').trim();
  if (!name || name === '.' || name === '..') return '';
  // 末段没有扩展名（如 /api/download?id=1）时几乎都不是真正的文件名，交给 Chrome。
  if (!/\.[A-Za-z0-9]{1,8}$/.test(name)) return '';
  return name;
}

/// 下载监听：list / start / cancel。
/// wait 由 daemon 侧轮询，避免扩展 Service Worker 在长下载里被回收。
async function handleDownloads(msg) {
  try {
    if (!chrome.downloads) return { ok: false, error: 'downloads API unavailable' };
    const action = String(msg.action || 'list');
    if (action === 'start') {
      const url = String(msg.url || '').trim();
      if (!url) return { ok: false, error: 'url is required' };
      const options = { url, saveAs: false, conflictAction: 'uniquify' };
      // 文件名优先级：显式 --filename > 从 URL 末段推导。
      // 以前没给 filename 时直接写死 'download'，于是 `download <url> --dir sub` 会
      // 得到一个没有扩展名的字面量 download 文件，用户还得手动改名。
      const explicitName = msg.filename ? String(msg.filename) : '';
      const fileName = explicitName || deriveDownloadFilename(url);
      const dir = msg.dir ? String(msg.dir).replace(/^[\\/]+/, '').replace(/[\\/]+$/, '') : '';
      if (fileName) {
        // --dir 只影响目录前缀；目录为空时就是纯文件名。
        options.filename = dir ? `${dir}/${fileName}` : fileName;
      } else if (dir) {
        // 推导不出文件名就不能只传目录（Chrome 会拒绝以 '/' 结尾的 filename），
        // 这时宁可整条 filename 都不传，让 Chrome 按 Content-Disposition 决定。
        console.warn('[TMWD] downloads: 无法从 URL 推导文件名，已忽略 --dir，交由 Chrome 决定');
      }
      const id = await chrome.downloads.download(options);
      const [item] = await chrome.downloads.search({ id });
      return { ok: true, data: { action, id, item: summarizeDownload(item) } };
    }
    if (action === 'cancel') {
      const id = Number(msg.id);
      if (!Number.isInteger(id)) return { ok: false, error: 'id is required' };
      await chrome.downloads.cancel(id);
      return { ok: true, data: { action, id, cancelled: true } };
    }
    if (action === 'erase') {
      const hasId = msg.id !== undefined && msg.id !== null && msg.id !== '';
      const hasState = !!msg.state;
      // chrome.downloads.erase({}) 会清空浏览器的**全部**下载历史，这是不可撤销的破坏性操作。
      // 缺省 id/state 时必须由调用方显式声明 confirmAll:true，否则直接拒绝。
      if (!hasId && !hasState && msg.confirmAll !== true) {
        return {
          ok: false,
          error: 'erase 需要指定 id 或 state；如果要清空全部下载历史，请显式传 confirmAll:true'
        };
      }
      const query = {};
      if (hasId) query.id = Number(msg.id);
      if (hasState) query.state = String(msg.state);
      const erased = await chrome.downloads.erase(query);
      return { ok: true, data: { action, erased, all: !hasId && !hasState } };
    }
    const query = {};
    if (msg.id !== undefined && msg.id !== null && msg.id !== '') query.id = Number(msg.id);
    if (msg.state) query.state = String(msg.state);
    if (msg.url) query.url = String(msg.url);
    if (msg.query) query.filenameRegex = String(msg.query);
    const items = await chrome.downloads.search(query);
    items.sort((a, b) => String(b.startTime || '').localeCompare(String(a.startTime || '')));
    const limit = Math.max(1, Math.min(Number(msg.limit || 20), 200));
    return {
      ok: true,
      data: {
        action: 'list',
        count: items.length,
        limit,
        items: items.slice(0, limit).map(summarizeDownload)
      }
    };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleFrames(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    if (!chrome.webNavigation?.getAllFrames) {
      return { ok: false, error: 'webNavigation API 不可用（manifest 需要 "webNavigation" 权限）' };
    }
    const frames = await chrome.webNavigation.getAllFrames({ tabId });
    return {
      ok: true,
      data: {
        tabId,
        count: frames ? frames.length : 0,
        frames: (frames || []).map((frame) => ({
          frame_id: frame.frameId,
          parent_frame_id: frame.parentFrameId,
          url: frame.url
        }))
      }
    };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

/// 消息来源分级：
///   internal  = 模块内部 / WS daemon 通道（没有 tab、没有 url），能力最全；
///   extension = popup / 扩展页（sender.url 以 chrome-extension://<本扩展 id>/ 开头）；
///   content   = 内容脚本 / 页面里的 boot 脚本（sender.tab 存在），只允许只读低位命令。
function classifyExtSender(sender) {
  if (!sender || typeof sender !== 'object') return 'internal';
  if (sender.tab) return 'content';
  const url = String(sender.url || '');
  if (!url) return 'internal';
  const prefix = `chrome-extension://${chrome.runtime?.id}/`;
  return url.startsWith(prefix) ? 'extension' : 'content';
}

/// 内容脚本唯一允许触发的低位命令（都是「读状态 / 同步角标」用）。
const CONTENT_SCRIPT_ALLOWED_CMDS = new Set(['status', 'badgeConfig', 'persistApplied']);

async function handleExtMessage(msg, sender) {
  // 没有 sender 校验时，任何能往 runtime 发消息的上下文（含被注入的内容脚本）
  // 都能直接调 cookies / cdp / inject 这类高权限命令。先验扩展 id，再按来源降级能力。
  const senderId = sender && sender.id;
  if (senderId && senderId !== chrome.runtime?.id) {
    return { ok: false, error_code: 'untrusted_sender', error: '拒绝来自其它扩展的消息' };
  }
  if (!msg || typeof msg !== 'object') {
    return { ok: false, error_code: 'invalid_message', error: '消息必须是对象' };
  }
  if (classifyExtSender(sender) === 'content') {
    const cmd = String(msg.cmd || msg.type || '');
    if (!CONTENT_SCRIPT_ALLOWED_CMDS.has(cmd)) {
      return {
        ok: false,
        error_code: 'forbidden_sender',
        error: `内容脚本无权调用 ${cmd || '(empty)'}（仅 popup / 扩展页 / daemon 通道可下发高权限命令）`
      };
    }
  }
  if (msg.cmd === 'status') return await handleStatus();
  if (msg.cmd === 'setPort') return await handleSetPort(msg);
  // 用户从 popup / 管理页面点「重新连接」：与改端口、改 token 走同一条 reconnectWS 路径，
  // 不另写一套重连逻辑（历史上两套路径会导致「配置改了却不生效」）。
  if (msg.cmd === 'reconnect') return await handleReconnect();
  if (msg.cmd === 'setWsTrustMode') return await handleSetWsTrustMode(msg);
  if (msg.cmd === 'setProfileLabel') return await handleSetProfileLabel(msg);
  if (msg.cmd === 'resetIdentity') return await handleResetIdentity();
  if (msg.cmd === 'setWsToken') return await handleSetWsToken(msg);
  lastCommandAt = Date.now();
  if (msg.cmd === 'cookies') return await handleCookies(msg, sender);
  if (msg.cmd === 'cdp') return await handleCDP(msg, sender);
  if (msg.cmd === 'batch') return await handleBatch(msg, sender);
  if (msg.cmd === 'captureVisible') return await handleCaptureVisible(msg);
  if (msg.cmd === 'captureFullPage') return await handleCaptureFullPage(msg);
  if (msg.cmd === 'blockingExtensions') return await handleBlockingExtensions(msg);
  if (msg.cmd === 'openTab') return await handleOpenTab(msg);
  if (msg.cmd === 'closeTab') return await handleCloseTab(msg, sender);
  if (msg.cmd === 'networkStart') return await handleNetworkStart(msg, sender);
  if (msg.cmd === 'networkList') return await handleNetworkList(msg, sender);
  if (msg.cmd === 'networkDetail') return await handleNetworkDetail(msg, sender);
  if (msg.cmd === 'networkClear') return await handleNetworkClear(msg, sender);
  if (msg.cmd === 'networkStop') return await handleNetworkStop(msg, sender);
  if (msg.cmd === 'consoleStart') return await handleConsoleStart(msg, sender);
  if (msg.cmd === 'consoleList') return await handleConsoleList(msg, sender);
  if (msg.cmd === 'consoleClear') return await handleConsoleClear(msg, sender);
  if (msg.cmd === 'consoleStop') return await handleConsoleStop(msg, sender);
  if (msg.cmd === 'debugClearAll') return await handleDebugClearAll();
  if (msg.cmd === 'tabs') {
    try {
      if (msg.method === 'switch') {
        const tab = await chrome.tabs.update(msg.tabId, { active: true });
        // 窗口被最小化时，切 active tab 并不会让页面「可见」：visibilityState 仍是 hidden，
        // 后台定时器被节流到分钟级，CF 挑战脚本（RTT 上报 / 轮询放行）根本跑不完，
        // 表现就是 challenge wait 一路等到超时。这里把最小化窗口恢复成 normal ——
        // 只恢复、不抢焦点（focused 仍只在 allowFocus 时设置）。
        if (tab && tab.windowId) {
          try {
            const win = await chrome.windows.get(tab.windowId);
            if (win && win.state === 'minimized') {
              await chrome.windows.update(tab.windowId, { state: 'normal' });
            }
          } catch (_) {}
          // 默认只切换 Chrome 内部 active tab，不抢占系统前台窗口。
          if (msg.allowFocus === true) await chrome.windows.update(tab.windowId, { focused: true });
        }
        return { ok: true };
      } else {
        // daemon 靠这份列表建立会话表：必须和 tabs_update 用同一套字段，
        // 否则「拉取说有、推送说没有」，标签页会随机消失。
        const tabs = await queryScriptableTabs();
        const groupTitles = await queryTabGroupTitles();
        const data = tabs.map(t => ({
          id: t.id,
          window_id: t.windowId,
          url: t.url,
          title: t.title,
          active: t.active,
          group_title: tabGroupTitle(groupTitles, t),
        }));
        return { ok: true, data };
      }
    } catch (e) { return { ok: false, error: e.message }; }
  }
  if (msg.cmd === 'inject') {
    return await handleInject(msg);
  }
  // 新增命令只加不改。旧 daemon 不认识它们 → 永远走不到这里。
  if (msg.cmd === 'focusTab') {
    return await handleFocusTab(msg, sender);
  }
  if (msg.cmd === 'listExtensions') {
    return await handleListExtensions(msg, sender);
  }
  if (msg.cmd === 'frames') {
    return await handleFrames(msg, sender);
  }
  if (msg.cmd === 'downloads') {
    return await handleDownloads(msg);
  }
  if (msg.cmd === 'pageOp') {
    return await handlePageOp(msg);
  }
  if (msg.cmd === 'netIntercept') {
    return await handleNetIntercept(msg, sender);
  }
  if (msg.cmd === 'injectProbe') {
    return await handleInjectProbe(msg, sender);
  }
  if (msg.cmd === 'selfCheck') {
    return await handleSelfCheck();
  }
  // exec 取消帧（id 形如 "<exec_id>#cancel"）。必须在 Unknown cmd 之前，
  // 否则 daemon 会收到一条「未知命令」的迟到结果，污染等待中的取消请求。
  if (msg.cmd === 'cancel') {
    return await handleCancelCommand(msg);
  }
  // management / contentSettings 的处理器已删除（对应权限也从 manifest 移除）。
  // 这里保留显式失败而不是落到「Unknown cmd」，避免旧脚本把它当成拼写错误。
  if (msg.cmd === 'management' || msg.cmd === 'contentSettings') {
    return {
      ok: false,
      error_code: 'permission_removed',
      error: `${msg.cmd} 已停用：扩展不再声明 management / contentSettings 权限，既不枚举也不改扩展与站点设置`,
      hint: '改用 atb open/click/exec 等受控能力；确实需要时可自行构建带额外权限的扩展'
    };
  }
  return { ok: false, error: 'Unknown cmd: ' + msg.cmd };
}

async function handleStatus() {
  // status 可能在 WS 连上之前就被 popup / 角标调用，先把身份读出来，
  // 否则会返回 browserId: null（popup 里显示 '-'）。
  // 诊断路径不能把失败伪装成「成功且无问题」——读不到身份就显式降级。
  const warnings = [];
  try { await ensureClientIdentity(); } catch (e) {
    warnings.push('clientIdentity: ' + ((e && e.message) || e));
  }
  // token 只回「有没有配」，绝不回原文：status 会被 popup 与页面角标读到。
  const wsError = await hydrateWsConnectError();
  return {
    ok: true,
    data: {
      wsConnected: !!ws && ws.readyState === WebSocket.OPEN,
      wsUrl: getWsUrl(),
      wsPort,
      wsTokenSet: !!wsToken,
      wsTrustMode,
      lastConnectError: publicWsConnectError(wsError),
      lastCommandAt,
      browserId,
      profileId,
      profileLabel,
      extensionId: chrome.runtime?.id,
      extensionVersion,
      // 只报「有没有拿到 management 权限」，不触发任何授权弹窗。
      management_permission: !!chrome.management,
      fileSchemeAccess: await isAllowedFileSchemeAccess(),
      // 心跳可观测性：最近一次 ping / pong 的时间与 RTT，排障时不用猜链路状态。
      heartbeat: {
        lastPingAgoMs: lastPingAt ? Date.now() - lastPingAt : null,
        lastPongAgoMs: lastPongAt ? Date.now() - lastPongAt : null,
        lastRttMs: lastPongRttMs
      },
      warnings,
      degraded: warnings.length > 0
    }
  };
}

/// 设置/清空 WS token。落 storage 后由 storage.onChanged 统一触发重连（外部写入同理）；
/// 这里额外显式重连一次，避免「onChanged 事件丢了 → 存了 token 却不生效」这种最难查的状态。
async function handleSetWsToken(msg) {
  const raw = msg.token === undefined || msg.token === null ? '' : String(msg.token);
  const next = raw.trim();
  // token 会被拼进 URL 查询串：只接受可见 ASCII，避免控制字符/换行导致握手异常难查。
  if (next && !/^[\x21-\x7e]{1,512}$/.test(next)) {
    return { ok: false, error: 'token 只能是可见 ASCII 字符（1-512 个），不能含空格或换行' };
  }
  await chrome.storage.local.set({ wsToken: next });
  wsToken = next;
  reconnectWS();
  return { ok: true, data: { wsTokenSet: !!wsToken, wsUrl: getWsUrl() } };
}

async function handleSetPort(msg) {
  const port = normalizePort(msg.port);
  await saveWsPort(port);
  reconnectWS();
  return {
    ok: true,
    data: {
      wsPort,
      wsUrl: getWsUrl(),
      wsConnected: !!ws && ws.readyState === WebSocket.OPEN
    }
  };
}

/// 主动重连（popup / 管理页面的「重新连接」）。
/// 只负责发起：reconnectWS 是异步的，回包时 ws 刚被置空，wsConnected 一定是 false，
/// 所以这里不回连接态，由调用方轮询 status 判断，避免 UI 拿到误导性的「未连接」。
async function handleReconnect() {
  reconnectWS();
  return { ok: true, data: { requested: true, wsUrl: getWsUrl(), wsPort } };
}

// 信任档位：默认 compat 不改任何既有行为；strict 由用户显式开启。
async function handleSetWsTrustMode(msg) {
  const mode = String(msg.mode || '').trim().toLowerCase();
  if (!WS_TRUST_MODES.has(mode)) {
    return { ok: false, error_code: 'invalid_trust_mode', error: 'mode 只能是 compat / strict' };
  }
  wsTrustMode = mode;
  try {
    await chrome.storage.local.set({ wsTrustMode: mode });
  } catch (e) {
    return { ok: false, error_code: 'storage_failed', error: (e && e.message) || String(e) };
  }
  return { ok: true, data: { wsTrustMode, effective: !!(mode !== 'strict' || wsToken) } };
}

function normalizeProfileLabel(label) {
  const raw = String(label || '').trim().replace(/\s+/g, ' ');
  if (!raw) return null;
  if (raw.length > 40) throw new Error('Profile Label 长度不能超过 40 个字符');
  // label 是给人看的名字，不是标识符：只允许 [A-Za-z0-9_.-] 会让中文/带空格的名字
  // 在 popup 点「保存」时直接抛错（已确认触发路径），而 daemon 侧对 label
  // 没有字符集限制。这里放宽到「任意可打印字符（允许空格）」，只拒绝控制字符和
  // 零宽字符——它们会让两个看起来相同的 label 在比较时不相等，是最难查的一类脏数据。
  if (/[\u0000-\u001f\u007f\u200b-\u200f\u2028\u2029\ufeff]/.test(raw)) {
    throw new Error('Profile Label 不能包含控制字符或零宽字符');
  }
  return raw;
}

async function handleSetProfileLabel(msg) {
  const label = normalizeProfileLabel(msg.label);
  // 先确保身份已就绪，否则返回值里的 browserId/profileId 会是 null。
  try { await ensureClientIdentity(); } catch (_) {}
  profileLabel = label;
  await chrome.storage.local.set({ profileLabel });
  await sendTabsUpdate();
  return {
    ok: true,
    data: {
      browserId,
      profileId,
      profileLabel
    }
  };
}

/// 重置浏览器标识：清掉 browserId / profileId 后重载扩展，下次启动会生成新的 UUID。
///
/// 正常使用**不需要**它：标识必须跨重载稳定，否则 session_key（browser_id:profile_id:tab）
/// 会漂移，daemon 侧的 @e 引用、--tab 寻址、默认浏览器绑定全部失联。只有在
/// 「整份浏览器 profile 被克隆到另一台机器」这类两个实例撞同一个 id 的场景才用它。
async function handleResetIdentity() {
  await chrome.storage.local.remove(['browserId', 'profileId']);
  // 先把回包发出去再重载：chrome.runtime.reload() 会立刻切断消息通道，
  // 同步调用的话调用方只会看到 "message port closed"。
  setTimeout(() => {
    try { chrome.runtime.reload(); } catch (_) {}
  }, 150);
  return {
    ok: true,
    data: {
      reset: true,
      note: '扩展将重载，浏览器标识会重新生成',
      previous: { browserId, profileId },
    },
  };
}

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  handleExtMessage(msg, sender)
    .then(sendResponse)
    // handleExtMessage 里即使有 try/catch 也仍可能抛（例如 sendTabsUpdate 的异常），
    // 之前没有 .catch → sendResponse 永不执行，popup 点「保存」毫无反应、只等通道超时。
    .catch((err) => {
      const message = String((err && err.message) || err);
      console.error('[TMWD] handleExtMessage failed:', message);
      try { sendResponse({ ok: false, error: message }); } catch (_) { /* 通道已关闭 */ }
    });
  return true;
});

async function handleCookies(msg, sender) {
  try {
    let url = msg.url || sender.tab?.url;
    if (!url && msg.tabId) {
      const tab = await chrome.tabs.get(msg.tabId);
      url = tab.url;
    }
    // chrome://、about:blank、view-source:、扩展页、chrome-extension:// 都没有 cookie，
    // 也没有「origin」。以前直接 url.match(...)[0] → TypeError: Cannot read properties
    // of null，报错与真实原因（这个页面不支持）无关，用户只能猜。
    let parsed = null;
    try { parsed = new URL(String(url || '')); } catch (_) { parsed = null; }
    if (!parsed || !/^https?:$/i.test(parsed.protocol)) {
      return { ok: false, error: '该页面不支持读取 cookie（仅 http/https）' };
    }
    const origin = parsed.origin;
    // clear：反复测挑战页 / 切登录态时要能一键清干净。默认按 url 清（只影响这个站点），
    // 传 domain 则按域清（含子域）。chrome.cookies 权限已经够，不需要 browsingData。
    if (String(msg.action || '').toLowerCase() === 'clear') {
      const domain = String(msg.domain || '').trim().replace(/^\./, '').toLowerCase();
      const explicitUrl = String(msg.url || '').trim();

      // 清理范围必须**严格限定**在一个站点内。
      // 血泪教训：早期版本把 getAll({}) 的结果也并进候选集，然后删了 435 条 ——
      // 那一次把整个 profile 的 cookie 全清了。所以：
      //   1) 有 url 就只认 getAll({url})（它本身已按站点过滤，且包含分区 cookie）；
      //   2) 有 domain 只认后缀匹配；
      //   3) 最后再按 host 兜一层，并且**条数超限直接拒绝执行**，宁可报错也不误删。
      let targets = [];
      let scanned = 0;

      if (explicitUrl) {
        let host = '';
        let origin = '';
        try {
          const parsed = new URL(explicitUrl);
          host = parsed.hostname.toLowerCase();
          origin = parsed.origin;
        } catch (_) { host = ''; }
        const list = await chrome.cookies.getAll({ url: explicitUrl }).catch(() => []);
        // 分区 cookie（cf_clearance 就是）只在带 partitionKey 的查询里出现。
        // 列表接口一直这么取（list 分支），清理分支以前漏了这一步，于是「看得到、删不掉」。
        const part = origin
          ? await chrome.cookies
              .getAll({ url: explicitUrl, partitionKey: { topLevelSite: origin } })
              .catch(() => [])
          : [];
        const merged = [...(list || [])];
        for (const c of part || []) {
          if (!merged.some((x) => x.name === c.name && x.domain === c.domain && x.path === c.path)) {
            merged.push(c);
          }
        }
        scanned = merged.length;
        // 只按「cookie domain 匹配 host」这一个方向判，方向写反就会跨子域误删。
        // 正确：domain === host（本域），或 host 是 domain 的子域（host.endsWith('.'+domain)，
        // 这些是页面实际携带的父域 cookie）；反之 domain 是 host 的子域时**不能删** ——
        // 那不是这个页面会收到的 cookie，旧代码 `d.endsWith('.'+host)` 会把它一起清掉（越权）。
        targets = merged.filter((c) => {
          const d = String(c.domain || '').replace(/^\./, '').toLowerCase();
          if (!d) return false;
          if (!host) return true;
          return d === host || host.endsWith(`.${d}`);
        });
      } else if (domain) {
        const seen = new Map();
        const absorb = (list) => {
          for (const c of list || []) {
            const d = String(c.domain || '').replace(/^\./, '').toLowerCase();
            if (!(d === domain || d.endsWith(`.${domain}`))) continue;
            const key = [c.storeId, c.domain, c.path, c.name, JSON.stringify(c.partitionKey || null)].join('|');
            if (!seen.has(key)) seen.set(key, c);
          }
        };
        // 分区 cookie 只在「具体页面 URL」的查询里出现，按域查不到；
        // 用已打开的同域标签页补一轮（页面还开着时这条最关键）。
        try { absorb(await chrome.cookies.getAll({ domain })); } catch (_) {}
        try {
          const tabs = await chrome.tabs.query({});
          for (const tab of tabs) {
            const tabUrl = String(tab.url || '');
            if (!/^https?:/i.test(tabUrl)) continue;
            let host = '';
            try { host = new URL(tabUrl).hostname.toLowerCase(); } catch (_) { continue; }
            if (host === domain || host.endsWith(`.${domain}`)) {
              absorb(await chrome.cookies.getAll({ url: tabUrl }).catch(() => []));
            }
          }
        } catch (_) {}
        targets = [...seen.values()];
        scanned = targets.length;
      } else {
        return { ok: false, error: 'cookies clear: 需要 url 或 domain 之一（拒绝无范围清理）' };
      }

      // 硬保护：单个站点超过 200 条基本只可能是范围算错了。
      if (targets.length > 200) {
        return {
          ok: false,
          error: `cookies clear: 命中 ${targets.length} 条，超过单站点上限 200，已拒绝执行（疑似范围算错）`,
          data: { action: 'clear', cleared: 0, scanned, domain: domain || null, url: explicitUrl || url },
        };
      }

      let cleared = 0;
      const failed = [];
      for (const cookie of targets) {
        const scheme = cookie.secure ? 'https' : 'http';
        const host = String(cookie.domain || '').replace(/^\./, '');
        const spec = {
          url: `${scheme}://${host}${cookie.path || '/'}`,
          name: cookie.name,
          storeId: cookie.storeId,
        };
        if (cookie.partitionKey) spec.partitionKey = cookie.partitionKey;
        try {
          await chrome.cookies.remove(spec);
          cleared += 1;
        } catch (e) {
          failed.push({ name: cookie.name, domain: cookie.domain, error: e && e.message });
        }
      }
      return {
        ok: true,
        data: {
          action: 'clear', cleared, failed, scanned,
          domain: domain || null,
          url: explicitUrl || url,
          scope: explicitUrl ? 'url' : 'domain',
        },
      };
    }
    const all = await chrome.cookies.getAll({ url });
    const part = await chrome.cookies.getAll({ url, partitionKey: { topLevelSite: origin } }).catch(() => []);
    const merged = [...all];
    for (const c of part) {
      if (!merged.some(x => x.name === c.name && x.domain === c.domain)) merged.push(c);
    }
    return { ok: true, data: merged };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}


const debugSessions = new Map();

// 采集缓冲上限由 daemon 下发（max_entries，默认 5000，值域 100..100000）。
// 以前是写死的 1000，daemon 想在一轮诊断里多留一点就得改扩展源码。
const NET_CAPTURE_MAX_ENTRIES_DEFAULT = 5000;
const NET_CAPTURE_MAX_ENTRIES_MIN = 100;
const NET_CAPTURE_MAX_ENTRIES_MAX = 100000;

function normalizeMaxEntries(raw) {
  const value = Number(raw);
  if (!Number.isFinite(value)) return NET_CAPTURE_MAX_ENTRIES_DEFAULT;
  return Math.min(NET_CAPTURE_MAX_ENTRIES_MAX, Math.max(NET_CAPTURE_MAX_ENTRIES_MIN, Math.floor(value)));
}

/// 按上限丢最旧请求（连同 requests Map 里的条目一起删，否则只是列表变短、内存照旧）。
function trimRequestBuffer(session) {
  if (!session) return;
  const cap = normalizeMaxEntries(session.networkMaxEntries);
  while (session.requestOrder.length > cap) {
    const old = session.requestOrder.shift();
    session.requests.delete(old);
  }
}

function trimConsoleBuffer(session) {
  if (!session) return;
  const cap = normalizeMaxEntries(session.consoleMaxEntries);
  if (session.logs.length > cap) session.logs.splice(0, session.logs.length - cap);
}

// 按 tabId 串行化 attach/detach/enable。
// WS 的 onmessage 是 async 且不排队，两条命令并发进入时第二个 attach 会报
// "Another debugger is already attached"，而恢复路径的 detach+重 attach 会把先建立的
// Fetch.enable / Network.enable 一起打掉。所有会改 debugger 状态的入口都走这条链。
const debugOpChains = new Map();

/// tabId 唯一收敛点：消息里可能传 "12"（字符串）、12、或 false。
/// 不归一化就会以 "12" 和 12 作两个 Map key，同一标签页出现两份 debugSession，
/// 表现为重复 attach / 规则只生效一半 / detach 关不掉另一份。
function normalizeDebugTabId(tabId) {
  const id = Number(tabId);
  if (!Number.isInteger(id) || id <= 0) {
    throw structuredError('invalid_tab_id', `invalid_tab_id: ${String(tabId)}`, 'tabId 必须是正整数（标签页 id）');
  }
  return id;
}

/// 单个 tab 的操作串行锁（attach / Fetch.enable / detach / 捕获事件处理共用）。
///
/// **锁序**：持有状态锁（withDebugStateLock）时可以进入 tab 锁；
/// 持有 tab 锁时**禁止**等待状态锁 —— 即严禁在 withDebugTabLock 内部调用
/// persistDebugInterceptState() / ensureDebugSessionsRestored() 这类入口。
/// 反例就是 restoreDebugSessionsLocked：它持状态锁等某个 tab 的锁，若该 tab 同时
/// 持锁等状态锁，两条 promise 链都没有超时，会永久挂死（该 tab 后续所有 CDP 命令
/// 与全局状态持久化一起停摆）。需要落盘的入口一律在锁外统一调用。
function withDebugTabLock(tabId, task) {
  const id = normalizeDebugTabId(tabId);
  const prev = debugOpChains.get(id) || Promise.resolve();
  const run = prev.then(() => task(), () => task());
  const tail = run.then(() => undefined, () => undefined);
  debugOpChains.set(id, tail);
  // 链尾执行完且没有后来者时回收条目，避免长期运行时 Map 按标签页数量增长。
  tail.then(() => {
    if (debugOpChains.get(id) === tail) debugOpChains.delete(id);
  });
  return run;
}

// detach 延迟。attach 期间 Chrome 会在页面顶部挂一条「已开始调试此浏览器」的信息条，
// 它会把视口高度顶掉 ~40px（布局位移），也是站点侧最容易看到的自动化特征之一。
// 旧值 30s 意味着「点一下页面，横幅挂半分钟」；现在只留 1.2s：足够吸收
// 「unroute 后立刻 route」这类连续命令，又不至于让人一直看到横幅。
const DEBUG_DETACH_DELAY_MS = 1200;
const DEBUG_DETACH_ALARM_PREFIX = 'tmwd-debug-detach-';

function debugDetachAlarmName(tabId) {
  return `${DEBUG_DETACH_ALARM_PREFIX}${tabId}`;
}

/**
 * 「这个 tab 是否还有必须保持 debugger attach 的用途」——全扩展唯一的判定入口。
 *
 * attach 是 per-tab 的共享资源，被三种用途共用：
 *   1. network 监听（Network.enable）
 *   2. console 监听（Runtime/Log.enable）
 *   3. 正在生效的 Fetch 拦截（intercept.fetchEnabled）
 * 只要还有任何一个用途在线，就不能 detach；全部释放后才允许拆。
 *
 * 注意 `session.intercept` 本身**不算用途**：规则被删空后 Fetch 已经 disable，
 * 剩下的只有 rules/stats/log 这些数据（保留它们是为了 detach 之后 net log 还能读到）。
 * 把它算成用途会导致「规则删光但 debugger 永远 attach」——标签页顶部一直挂
 * 「正在调试」横条，还会让别的扩展无法 attach 该标签。
 */
function debugSessionInUse(session) {
  if (!session || !session.attached) return false;
  if (session.network || session.console) return true;
  return !!(session.intercept && session.intercept.fetchEnabled);
}

function scheduleDebugDetach(session, reason = 'idle') {
  // 未 attach，或还有用途（network/console/Fetch）→ 什么都不做。
  if (!session || !session.attached) return;
  // 入口同样归一化。这里不抛错（调用点常在 catch 里），非法 id 直接放弃排期。
  const tabId = Number(session.tabId);
  if (!Number.isInteger(tabId) || tabId <= 0) return;
  if (debugSessionInUse(session)) return;
  session.detachReason = reason;

  // 主路径用 setTimeout：alarms 的硬下限是 30s（Chrome 120+ 会把更小的值抬到 ≥30s），
  // 只靠它的话「已开始调试此浏览器」横幅必然挂满半分钟。
  // 【事实修正 】MV3 **不保证**「有未执行的 timer 时 SW 不被回收」，短 timer 只是
  // 尽力而为的最优路径；真正的保底是下面那条 alarm（跨 SW 回收存活）。注释不再承诺
  // 平台没有承诺的存活语义。
  if (session.detachTimer) return; // 已有更早的排期，别再叠加
  session.detachTimer = setTimeout(() => {
    session.detachTimer = null;
    if (!session.attached) return;
    if (debugSessionInUse(session)) return; // 期间又被 network/console 占用
    detachDebugSession(session, session.detachReason || 'idle').catch(() => {});
  }, DEBUG_DETACH_DELAY_MS);

  // 兜底：SW 被回收后 timer 会丢，alarm 仍能把「挂着的 attach」收掉（30s 粒度可以接受）。
  chrome.alarms?.create?.(debugDetachAlarmName(session.tabId), { delayInMinutes: ALARM_MIN_MINUTES });
}

function resolveTabId(msg, sender) {
  const tabId = Number(msg.tabId || sender.tab?.id);
  if (!Number.isInteger(tabId) || tabId <= 0) throw new Error('no tabId');
  return tabId;
}

function getDebugSession(tabId) {
  const id = normalizeDebugTabId(tabId);
  let session = debugSessions.get(id);
  if (!session) {
    session = {
      tabId: id,
      attached: false,
      network: false,
      console: false,
      requests: new Map(),
      requestOrder: [],
      logs: [],
      // 采集缓冲上限（daemon 可通过 networkStart/consoleStart 的 max_entries 调整）。
      networkMaxEntries: NET_CAPTURE_MAX_ENTRIES_DEFAULT,
      consoleMaxEntries: NET_CAPTURE_MAX_ENTRIES_DEFAULT,
      intercept: null,
      detachReason: null
    };
    debugSessions.set(id, session);
  }
  return session;
}

function isDebuggerAlreadyAttachedError(error) {
  return /Another debugger is already attached/i.test(String(error?.message || error || ''));
}

/// detach 时「本来就没 attach」的典型报错：这属于正常竞态，不该当失败记日志。
function isDebuggerNotAttachedError(error) {
  return /not attached|No target with given id|Cannot access a chrome/i.test(String(error?.message || error || ''));
}

/// 「这个 tab 是**本扩展**attached 的吗」（attach 恢复与 的 detach 闸门共用）。
/// getTargets 的 attached 对「被别的扩展 attach」的 tab 也是 true，直接拿去 detach 只会
/// 报 Another debugger is already attached，还会让我们误以为自己的会话已清理干净。
async function isOurDebuggerAttached(tabId) {
  try {
    const targets = await chrome.debugger.getTargets();
    const me = chrome.runtime?.id;
    const target = targets.find(t => t.tabId === tabId && t.attached && (!t.extensionId || !me || t.extensionId === me));
    return !!target;
  } catch (e) {
    console.log('[TMWD-DEBUG] getTargets failed:', e.message);
    return false;
  }
}

async function attachDebuggerWithRecovery(tabId) {
  try {
    await chrome.debugger.attach({ tabId }, '1.3');
    return;
  } catch (e) {
    // crbug.com/40753571：页面里只要嵌了「别的扩展」的 iframe，Chrome 就拒绝 attach，
    // 报错文案却完全看不出原因。这里补一句可执行的提示。
    if (/chrome-extension:\/\/ URL of different extension/i.test(String(e?.message || ''))) {
      throw new Error(`${e.message}（该页面内嵌了其他扩展的 iframe，Chrome 不允许同时 attach；请暂时关闭相关扩展，或改用 exec / click 等 DOM 操作）`);
    }
    if (!isDebuggerAlreadyAttachedError(e)) throw e;

    // daemon 非 graceful 退出时，Chrome 可能还保留本扩展的旧 attach。
    // 这里只在 Chrome 明确报告该 tab 已 attached **且确实是我们自己**的 attach 后做一次
    // detach+retry（别的扩展占着时我们无权 detach，应把原始错误抛出去）；
    // 这样也避免正常路径反复抖动。
    const attached = await isOurDebuggerAttached(tabId);
    if (!attached) throw e;
    try {
      await chrome.debugger.detach({ tabId });
    } catch (detachError) {
      console.log('[TMWD-DEBUG] stale detach failed:', tabId, detachError.message);
      throw e;
    }
    await chrome.debugger.attach({ tabId }, '1.3');
  }
}

/// attach 本体：**调用方必须已持有 tab 锁**。
/// 需要自行加锁的入口用下面的 ensureDebugAttached；已在锁内的组合操作直接调本函数，
/// 否则会在同一条 promise 链上等待自己而死锁。
async function ensureDebugAttachedLocked(session) {
  // chrome.debugger 缺失时不能静默崩在这里（报「Cannot read properties of undefined」），
  // 要给出「为什么不可用」的可见错误，同时让调用方降级到 DOM 通道。
  if (!chrome.debugger?.attach) {
    throw new Error('chrome.debugger API 不可用：请确认 manifest 声明了 "debugger" 权限并且扩展已正确加载；本命令需要降级到 DOM 通道');
  }
  await chrome.alarms.clear(debugDetachAlarmName(session.tabId)).catch(() => null);
  session.detachReason = null;
  // 上一轮释放排的 timer 必须撤销，否则刚 attach 就被它按旧理由拆掉。
  if (session.detachTimer) {
    try { clearTimeout(session.detachTimer); } catch (_) {}
    session.detachTimer = null;
  }
  // 注意这里**不** markDebugTabTouched：单纯 attach（exec 的 CDP 兜底 / cdp / batch）
  // 不产生任何持久化状态。若在这种路径上标脏，一次 detach 就会把「本 SW 还没恢复、
  // 但别处仍需要的规则条目」删掉（要防的正是这个）。
  if (session.attached) return;
  await attachDebuggerWithRecovery(session.tabId);
  session.attached = true;
  // 后台标签页会被 Chrome 降级/冻结，长任务容易中途失联。
  // 已 attach debugger 时显式声明页面仍处于 active 生命周期，保证脚本与事件继续运转。
  try {
    await chrome.debugger.sendCommand({ tabId: session.tabId }, 'Page.setWebLifecycleState', { state: 'active' });
  } catch (_) {}
}

function ensureDebugAttached(session) {
  return withDebugTabLock(session.tabId, () => ensureDebugAttachedLocked(session));
}

// 单个 tab 的 detach 本体：**调用方必须已持有 tab 锁**。
// 返回值 true = 需要调用方在**锁外**落盘。
async function detachDebugSessionLocked(session, reason = 'manual') {
  if (!session) return false;
  await chrome.alarms.clear(debugDetachAlarmName(session.tabId)).catch(() => null);
  // 有没有需要从快照里清掉的状态，必须在清标志**之前**判断。
  const hadPersistedState = !!(session.network || session.console || (session.intercept && session.intercept.fetchEnabled));
  // ws-close / keepalive-lost 是我方连接断了，规则本身仍然有效：这里 detach 只是
  // 释放浏览器侧的 CDP，快照条目必须留着，否则重建连时拿不回用户的拦截规则。
  const keepPersisted = reason === 'ws-close' || reason === 'keepalive-lost';
  // 不能只信内存里的 session.attached。冷启动恢复只对 fetchEnabled 的 tab 回填
  // attached，只开了 networkStart/consoleStart 的 tab 在 SW 重启后 attached=false，
  // 于是这里跳过 detach → 浏览器侧仍 attach、调试横幅常驻。
  // 因此：内存说没 attach 时再用 getTargets 复核一次浏览器侧的真实状态。
  let shouldDetach = !!session.attached;
  if (!shouldDetach) shouldDetach = await isOurDebuggerAttached(session.tabId);
  if (shouldDetach) {
    try {
      await chrome.debugger.detach({ tabId: session.tabId });
    } catch (e) {
      // 「本来就没 attach」是正常竞态（tab 已关闭 / 别人先 detach），不是错误；
      // 它不能阻止下面的状态清理，否则横幅和状态会一起残留。
      if (!isDebuggerNotAttachedError(e)) {
        console.log('[TMWD-DEBUG] detach failed:', session.tabId, reason, e && e.message);
      }
    }
  }
  session.attached = false;
  session.network = false;
  session.console = false;
  markInterceptDetached(session);
  session.detachReason = null;
  // detach 之后 Fetch/Network/Runtime 一定不可用：把快照里这个 tab 的条目删掉，
  // 否则 SW 重启后的冷启动恢复会对着一个已 detach 的 tab 白试一次。
  // 只有「本来就有状态」的会话才写：纯 attach 的 detach 不该动别的 tab 的条目。
  // 但断线（ws-close / keepalive-lost）不是「不要这些规则了」，只是连不上 daemon，
  // 此时必须保留条目 —— 旧实现无条件 persist，而内存里的标志刚被清空，
  // 于是每个 tab 的条目都被逐个删掉，把 的守卫从旁边绕过去了。
  if (hadPersistedState && !keepPersisted) {
    markDebugTabTouched(session.tabId);
    // 这里**不**落盘。本函数在 tab 锁内执行，而落盘要走状态锁；restore
    // 正持状态锁等本 tab 的锁，锁内 await 状态锁就是死锁。由 detachDebugSession
    // 在锁外补写，返回值只表达「需要写」。
    return true;
  }
  if (hadPersistedState) {
    console.log(`[TMWD-NET] detach(reason=${reason}) 保留 tab ${session.tabId} 的持久化拦截快照（${await countDebugStateEntries()} 个 tab 待恢复）`);
  }
  return false;
}

function detachDebugSession(session, reason = 'manual', options) {
  if (!session) return Promise.resolve();
  const skipPersist = !!(options && options.skipPersist);
  return withDebugTabLock(session.tabId, () => detachDebugSessionLocked(session, reason))
    .then(async (needsPersist) => {
      if (needsPersist && !skipPersist) await persistDebugInterceptState();
    });
}

/// detach 之后 Fetch 一定已经不可用，但 rules/stats/log 这些**数据**要留着：
/// 规则被删空自动 detach 后，`net log` / `status` 仍然要能读到刚才的拦截记录。
/// 只把 fetchEnabled 置 false（= 不再算作「用途」），不要整个清空。
function markInterceptDetached(session) {
  if (!session || !session.intercept) return;
  session.intercept.fetchEnabled = false;
}

async function detachDebugIfIdle(session) {
  // 判定逻辑全部收敛在 scheduleDebugDetach / debugSessionInUse 里，这里只表达「现在没在忙」。
  scheduleDebugDetach(session, 'idle');
}

async function detachAllDebugSessions(reason = 'cleanup') {
  const sessions = [...debugSessions.values()];
  for (const session of sessions) {
    session.requests.clear();
    session.requestOrder = [];
    session.logs = [];
    // 批量路径由函数末尾统一落盘一次：逐 tab 写会把 N 次读-改-写放大成 N 份
    // 配额开销，顺序上也没有额外收益（中间态本来就不该被观察到）。
    await detachDebugSession(session, reason, { skipPersist: true });
  }
  debugSessions.clear();
  // 只有「daemon 明确要求全部清掉」才抹持久化快照。
  // 'ws-close' / 'keepalive-lost' 是我方连接断了，规则本身仍然有效：上面逐个 detach 时
  // 已按 reason 跳过删除（见 detachDebugSessionLocked 的 keepPersisted），这里也不再整表重写。
  // 旧实现无条件调 persistDebugInterceptState()，而那时内存已经清空 → 写进去的是空表，
  // 于是每一次 daemon 重启都把 `net route --persist` 的规则永久抹掉。
  //
  // 【真实恢复入口】消费这份快照的只有本文件里的两处（都不经过 daemon）：
  //   1) SW 冷启动求值末尾的一次 ensureDebugSessionsRestored()；
  //   2) 每次 WebSocket onopen 发完 ext_ready 之后的 ensureDebugSessionsRestoredOnReconnect()。
  // 两者都只对 getTargets() 里 attached===true 的 tab 重新 enable，失败/未 attach 的条目会被清掉。
  if (reason === 'debugClearAll' || reason === 'cleanup') {
    await persistDebugInterceptState();
  } else {
    console.log(`[TMWD-NET] 连接断开（reason=${reason}），保留持久化拦截快照等重连后恢复`);
  }
  // 断线意味着「本 SW 已恢复过」这个结论作废 —— 浏览器侧 CDP 刚被拆掉。
  // 不置空的话，scheduleFastReconnect（300ms~5s）连回来的那次 onopen 会命中
  // 已经 settle 的旧 promise → 恢复被永久跳过，规则看起来还在却完全不生效。
  if (reason === 'ws-close' || reason === 'keepalive-lost') forgetDebugRestoreMemo();
}

// ---------------------------------------------------------------------------
// 纯 CDP 会话的残留清理
//
// exec 的 CDP 兜底 / cdp / batch 这类操作只 attach、不产生任何**持久化**状态
// （debugStateEntryOf 对它们返回 null，不进 DEBUG_INTERCEPT_STORE_KEY 快照），
// 所以 SW 被回收后：内存 Map 空了，浏览器侧却仍然 attached —— 标签页顶部
// 「正在调试此浏览器」横幅常驻，别的扩展也 attach 不了。恢复路径只覆盖
// 「有持久化状态」的 tab，管不到这一类；scheduleDebugDetach 的 alarm 也是按
// 内存里的 session 排的，同样随 SW 一起没了。
// 这里在每分钟的自检 alarm 里扫一遍 getTargets：attached、extensionId 是我们、
// 且**内存里根本没有 session** 的 tab，就是上一代 SW 的遗留，直接 detach。
// 只处理「无 session」的 tab 是刻意的：attach 过程中 getTargets 已经能看到
// attached，但 session.attached 还没置位 —— 只看内存有无 session 才不会误拆
// 正在进行的 attach。
// ---------------------------------------------------------------------------
async function sweepOrphanDebugSessions() {
  if (!chrome.debugger?.getTargets) return 0;
  let targets = [];
  try {
    targets = await chrome.debugger.getTargets();
  } catch (e) {
    console.log('[TMWD-DEBUG] sweep: getTargets 失败，跳过:', e && e.message);
    return 0;
  }
  const me = chrome.runtime?.id;
  let swept = 0;
  for (const target of targets || []) {
    if (!target || !target.attached) continue;
    // 别的扩展的 attach 不归我们管，也无权 detach。
    if (target.extensionId && me && target.extensionId !== me) continue;
    const tabId = Number(target.tabId);
    if (!Number.isInteger(tabId) || tabId <= 0) continue;
    if (debugSessions.has(tabId)) continue; // 本代 SW 的会话（含正在 attach 的）
    try {
      await chrome.debugger.detach({ tabId });
      swept += 1;
      console.log('[TMWD-DEBUG] sweep: 清理上一代 SW 遗留的 attach，tab', tabId);
    } catch (e) {
      if (!isDebuggerNotAttachedError(e)) {
        console.log('[TMWD-DEBUG] sweep: detach 失败 tab', tabId, e && e.message);
      }
    }
  }
  return swept;
}

// ---------------------------------------------------------------------------
// 调试状态的冷启动恢复
//
// debugSessions 是纯内存 Map，MV3 的 Service Worker 随时会被回收；但 attach 与
// Fetch.enable / Network.enable / Runtime.enable 是**浏览器侧**状态。SW 冷启动后新事件
// 照常送达，内存里却没有规则，旧代码直接 `return` → 被暂停的请求永远没人 continue →
// 页面永久转圈；network/console 事件则被逐条丢弃，列表返回空而 status 报 stopped。
//
// 【契约】恢复由**扩展自己**完成，不依赖 daemon 主动重新下发：ext_ready 里的 restartSeq
// 由 daemon 旁路解析（ws.rs note_extension_restart，置 cdpRebuildRequired，net.rs 下次
// route 整体重下发）；扩展侧仅上报，不据此自行动作。
// （历史注释曾声称「daemon 侧从未解析」，这是错的：src/server/ws.rs 一直在解析
// restartSeq 并触发 cdp 重建，勿再照抄旧说法。）
//
// 这里把「哪些 tab 正在拦截/采集」落到 storage，SW 启动时对仍然 attached 的 tab 重建
// session 并重新 enable。注意 chrome.storage.session 在 MV3 需要 "storage" 权限
// （manifest 已有）；老 Chrome（<102）没有该命名空间时退化到 storage.local。
// 取舍：local 会跨浏览器重启保留，理论上可能在「重启后 tab 还是 attached」的罕见
// 组合下复活过期规则；但恢复只对 getTargets() 里 attached===true 的 tab 执行，
// 且后续 daemon 命令会覆盖规则内容，所以影响可控。
// ---------------------------------------------------------------------------
const DEBUG_INTERCEPT_STORE_KEY = 'debugInterceptState';

function debugInterceptStoreArea() {
  return chrome.storage?.session || chrome.storage?.local || null;
}

// 本 SW 实例**真正动过**的 tab：只有这些 tab 的条目会被读-改-写覆盖或删除。
// 旧实现每次把内存全量写进同一个 key。冷启动时内存为空，任何在 restore 之前触发的写
// （onDebuggerDetach、netIntercept stop…）都会把快照覆盖成 {}，restore 随后读到空表直接
// return，其它 tab 的规则永久丢失，而浏览器侧 Fetch.enable 仍在拦 —— 请求被暂停却没人判定。
// 用 touched 集合做增量合并，就从根上消除了「空内存抹掉别人」的可能。
const debugStateTouchedTabs = new Set();

function markDebugTabTouched(tabId) {
  const id = Number(tabId);
  if (Number.isInteger(id) && id > 0) debugStateTouchedTabs.add(id);
}

// 状态读-改-写的单飞锁：restore 与所有写入必须串行，
// 否则「读 → 写」之间插进另一次写入，后写就会覆盖先读的结果。
//
// **锁序**：本锁内可以获取 tab 锁（restore 需要逐个 tab 重新 attach）；
// 反过来不行 —— 持 tab 锁等本锁会与 restore 形成环。因此所有「持 tab 锁的入口」
// 都必须在锁外调用 persistDebugInterceptState，而不是在锁内顺手落盘。
let debugStateQueue = Promise.resolve();

function withDebugStateLock(task) {
  // 前一个任务无论成功失败都继续：一次配额/权限失败不能把后续持久化永久卡死。
  const run = debugStateQueue.then(() => task(), () => task());
  debugStateQueue = run.then(() => undefined, () => undefined);
  return run;
}

/// 把一个会话压成可持久化的快照条目；null = 这个 tab 已经没有任何要恢复的状态。
/// 除 Fetch 拦截外还记 network/console：两者同样是浏览器侧 enable 状态，
/// SW 回收后需要扩展自己重建，而不是等 daemon 重下发 —— daemon 从不消费 restartSeq。
function debugStateEntryOf(session) {
  if (!session) return null;
  const state = session.intercept;
  const fetchEnabled = !!(state && state.fetchEnabled);
  const network = !!session.network;
  const consoleOn = !!session.console;
  if (!fetchEnabled && !network && !consoleOn) return null;
  const entry = { network, console: consoleOn };
  if (fetchEnabled) {
    entry.intercept = true;
    entry.fetchEnabled = true;
    entry.rules = state.rules;
    entry.responseStage = !!state.responseStage;
  }
  return entry;
}

/// 读-改-写快照：只覆盖本实例动过的 tab，其余条目原样保留。调用方必须已持锁。
async function writeDebugStateSnapshotLocked(dropTabIds) {
  const area = debugInterceptStoreArea();
  if (!area) return;
  let snapshot = {};
  try {
    const stored = await area.get({ [DEBUG_INTERCEPT_STORE_KEY]: {} });
    const raw = stored && stored[DEBUG_INTERCEPT_STORE_KEY];
    if (raw && typeof raw === 'object' && !Array.isArray(raw)) snapshot = raw;
  } catch (e) {
    // 读不到旧快照就**不写**：内存里没有其它 tab 的规则，写下去等于把它们删光。
    console.warn('[TMWD-FETCH] 读取拦截状态快照失败，本次跳过写入（避免覆盖其它 tab 的规则）:', e && e.message);
    return;
  }
  if (dropTabIds) {
    for (const tabId of dropTabIds) delete snapshot[String(tabId)];
  }
  for (const tabId of debugStateTouchedTabs) {
    const entry = debugStateEntryOf(debugSessions.get(tabId));
    if (entry) snapshot[String(tabId)] = entry;
    else delete snapshot[String(tabId)];
  }
  try {
    await area.set({ [DEBUG_INTERCEPT_STORE_KEY]: snapshot });
    // 写成功后清空脏标记。旧实现只增不删，长会话里每次持久化都要把
    // 历史上动过的所有 tab 重算一遍（O(N) 且 N 只增），SW 内存与配额都被慢慢吃掉。
    // 失败时保留标记，下一次写入仍会带上这些 tab。
    debugStateTouchedTabs.clear();
  } catch (e) {
    // 配额/权限问题只影响「重启后能否自愈」，不能影响正在进行的拦截。
    console.warn('[TMWD-FETCH] 持久化拦截状态失败:', e && e.message);
  }
}

/// 落盘入口：与 restore 共用同一把单飞锁，保证两者不会互相覆盖。
async function persistDebugInterceptState() {
  if (!debugInterceptStoreArea()) return;
  return withDebugStateLock(() => writeDebugStateSnapshotLocked());
}

/// 快照里现在有多少个 tab 的条目。只用于日志（断线保留条目时说明还剩几条待恢复），
/// 失败一律返回 0：日志不该成为 detach 路径上的新失败点。
async function countDebugStateEntries() {
  const area = debugInterceptStoreArea();
  if (!area) return 0;
  try {
    const stored = await area.get({ [DEBUG_INTERCEPT_STORE_KEY]: {} });
    const raw = stored && stored[DEBUG_INTERCEPT_STORE_KEY];
    return raw && typeof raw === 'object' ? Object.keys(raw).length : 0;
  } catch (_) {
    return 0;
  }
}

// 冷启动恢复的单飞 promise：onDebuggerEvent 里第一批 Fetch.requestPaused 会先 await 它，
// 恢复完成后再按规则处置，而不是直接当 orphan 透传（那样 mock/abort 会被静默跳过）。
const DEBUG_RESTORE_WAIT_MS = 3000;
let debugRestorePromise = null;

function ensureDebugSessionsRestored() {
  if (!debugRestorePromise) {
    debugRestorePromise = withDebugStateLock(() => restoreDebugSessionsLocked())
      .catch((e) => {
        // 失败不缓存：否则这一次 settle 成 rejected 之后，重连路径永远拿不到第二次机会。
        debugRestorePromise = null;
        throw e;
      });
  }
  return debugRestorePromise;
}

/// 连接建立后的「补恢复」入口（另一半）。
///
/// 恢复的消费方只有两处：SW 冷启动求值时的一次 ensureDebugSessionsRestored()，
/// 和每次 onopen 发完 ext_ready 之后的这里。两者共用同一个永久记忆的 promise，
/// 所以成功过就**不会**在每次 onopen 重跑（重跑会把「未 attach」的条目当脏数据清掉），
/// 但失败过的会在下一次 onopen 再试一次。restoreInFlight 只是把「同一次 onopen 的
/// 并发调用」收敛成一条，避免连发 ext_ready 时重复排队。
///
/// 断线（ws-close / keepalive-lost）时浏览器侧 CDP 已被拆掉，这份记忆必须
/// 显式作废，否则同一 SW 实例内的快速重连永远拿不到第二次恢复机会。
/// restoreDebugSessionsLocked 本身幂等（只按 touched 增量写、已 attach 的 tab 直接跳过），
/// 重跑安全。
let restoreInFlight = null;
function forgetDebugRestoreMemo() {
  debugRestorePromise = null;
  restoreInFlight = null;
}
function ensureDebugSessionsRestoredOnReconnect() {
  if (restoreInFlight) return restoreInFlight;
  restoreInFlight = ensureDebugSessionsRestored()
    .catch((e) => {
      console.warn('[TMWD-FETCH] 重连后恢复拦截状态失败:', e && e.message);
      return null;
    })
    .finally(() => { restoreInFlight = null; });
  return restoreInFlight;
}

// ---------------------------------------------------------------------------
// 「每次浏览器会话只做一次冷启动恢复」
//
// MV3 里 SW 会被每次 alarm 唤醒并**重新求值**整个模块，模块末尾的
// ensureDebugSessionsRestored() 因此会在每次自检 / 心跳 / probe 唤醒时重跑：
// 对仍然 attach 的 tab 反复 enable Fetch/Network/Runtime，既是白开销，也会在
// restore 与「内存里已有的规则」之间来回覆盖。
//
// 判据用 chrome.storage.session（同一个浏览器会话内跨 SW 存活，浏览器重启/扩展
// 重载时清空）+ 一个本会话固定下来的 bootId：
//   - 第一次求值：session 里没有 bootId → 写入本 SW 的候选值并跑恢复；
//   - 恢复成功且没有「留待下次」的条目 → 记 debugRestoreDoneForBootId；
//   - 之后任何 alarm 唤醒：bootId 与 done 值相同 → 直接跳过。
// 恢复失败或有 pending 条目不记标记，下一次唤醒会重试。
// ---------------------------------------------------------------------------
const DEBUG_BOOT_ID_KEY = 'debugBootId';
const DEBUG_RESTORE_BOOT_KEY = 'debugRestoreDoneForBootId';
let debugBootIdPromise = null;

function ensureDebugBootId() {
  if (!debugBootIdPromise) {
    debugBootIdPromise = (async () => {
      const origin = (typeof performance !== 'undefined' && performance.timeOrigin) || Date.now();
      const local = `${chrome.runtime?.id || 'ext'}:${Math.round(origin)}`;
      const area = chrome.storage?.session;
      if (!area) return local;
      try {
        const data = await area.get({ [DEBUG_BOOT_ID_KEY]: null });
        const stored = data && data[DEBUG_BOOT_ID_KEY];
        if (stored) return String(stored);
        await area.set({ [DEBUG_BOOT_ID_KEY]: local });
        return local;
      } catch (_) {
        return local;
      }
    })();
  }
  return debugBootIdPromise;
}

/// 同一代 SW 内的单飞：模块末尾与（可能的）重连路径并发调用时只跑一次。
let debugBootRestorePromise = null;
function ensureDebugSessionsRestoredOnBoot() {
  if (!debugBootRestorePromise) {
    debugBootRestorePromise = runDebugRestoreOnBoot().catch((e) => {
      debugBootRestorePromise = null; // 失败不缓存，下一次唤醒/调用再试
      throw e;
    });
  }
  return debugBootRestorePromise;
}

async function runDebugRestoreOnBoot() {
  const area = chrome.storage?.session;
  let bootId = '';
  try { bootId = await ensureDebugBootId(); } catch (_) {}
  if (area && bootId) {
    try {
      const done = await area.get({ [DEBUG_RESTORE_BOOT_KEY]: null });
      if (done && done[DEBUG_RESTORE_BOOT_KEY] === bootId) {
        console.log('[TMWD-FETCH] 本浏览器会话已恢复过拦截状态，跳过本次 SW 求值');
        return null;
      }
    } catch (_) {}
  }
  const outcome = await ensureDebugSessionsRestored();
  const pending = (outcome && Number(outcome.pending)) || 0;
  if (area && bootId && !pending) {
    try { await area.set({ [DEBUG_RESTORE_BOOT_KEY]: bootId }); } catch (_) {}
  }
  return outcome;
}

/// 恢复本体：**调用方必须已持有状态锁**（经 ensureDebugSessionsRestored 进入）。
/// 对仍 attached 的 tab 重建 session，并重新打开 Fetch / Network / Runtime。
/// 任何一步失败都只 warn，绝不能让 SW 启动路径 reject。
// 返回值 `{ pending }`：仍保留、等下一次恢复的条目数。
async function restoreDebugSessionsLocked() {
  const area = debugInterceptStoreArea();
  if (!area) return { pending: 0 };
  let snapshot = {};
  try {
    const stored = await area.get({ [DEBUG_INTERCEPT_STORE_KEY]: {} });
    const raw = stored && stored[DEBUG_INTERCEPT_STORE_KEY];
    if (raw && typeof raw === 'object' && !Array.isArray(raw)) snapshot = raw;
  } catch (e) {
    console.warn('[TMWD-FETCH] 读取持久化拦截状态失败:', e && e.message);
    return { pending: 0 };
  }
  const tabIds = Object.keys(snapshot)
    .map(Number)
    .filter((tabId) => Number.isInteger(tabId) && tabId > 0);
  if (!tabIds.length) return { pending: 0 };
  if (!chrome.debugger?.getTargets) {
    console.warn('[TMWD-FETCH] chrome.debugger.getTargets 不可用，跳过拦截状态恢复');
    return { pending: tabIds.length };
  }
  let attachedTabs;
  try {
    const targets = await chrome.debugger.getTargets();
    attachedTabs = new Set((targets || []).filter(t => t && t.attached).map(t => t.tabId));
  } catch (e) {
    console.warn('[TMWD-FETCH] getTargets 失败，跳过拦截状态恢复:', e && e.message);
    return { pending: tabIds.length };
  }
  const staleTabs = [];
  const pendingTabs = new Set();
  for (const tabId of tabIds) {
    const saved = snapshot[tabId];
    if (!saved || typeof saved !== 'object') { staleTabs.push(tabId); continue; }
    // 只有「浏览器侧还 attached」的 tab 才值得恢复；否则 Fetch/Network 已随 detach 失效。
    // ws-close / keepalive-lost 会主动 detach（释放浏览器侧 CDP），但快照里的
    // 规则是用户**显式要求持久化**的。重连 / 冷启动时必须重新 attach，否则这些条目
    // 永远恢复不了，只会被当成脏数据删掉 —— 的「保留快照」就白做了。
    let canRestore = attachedTabs.has(tabId);
    if (!canRestore) {
      try {
        const pending = getDebugSession(tabId);
        await withDebugTabLock(tabId, () => ensureDebugAttachedLocked(pending));
        canRestore = !!pending.attached;
        if (canRestore) console.log('[TMWD-FETCH] 已重新 attach tab', tabId, '以恢复持久化拦截规则');
      } catch (e) {
        console.log('[TMWD-FETCH] tab', tabId, '重新 attach 失败（DevTools 占用 / 页面已关）:', e && e.message);
      }
    }
    if (!canRestore) {
      // tab 已不存在 → 脏数据清掉；tab 还在但 attach 不了（DevTools 占用等）→ 保留条目，
      // 下次重连 / 冷启动再试。旧实现在这里一律当 stale 删除，等于「一次占用永久丢规则」。
      let exists = true;
      try {
        await chrome.tabs.get(tabId);
      } catch (_) {
        exists = false;
      }
      if (!exists) staleTabs.push(tabId);
      else {
        pendingTabs.add(tabId);
        console.log('[TMWD-FETCH] tab', tabId, '暂不可 attach，保留持久化规则等待下次恢复');
      }
      continue;
    }
    try {
      const session = getDebugSession(tabId);
      session.attached = true;
      markDebugTabTouched(tabId);
      let restoredAnything = false;
      // network / console 同样是浏览器侧状态：SW 换了一代，事件还会到达，但内存标志没了。
      // 不重建的话 status 会报 stopped，列表还会因为「无 session」被丢弃（另一半）。
      if (saved.network) {
        try {
          await chrome.debugger.sendCommand({ tabId }, 'Network.enable', {});
          session.network = true;
          restoredAnything = true;
        } catch (e) {
          console.warn('[TMWD-FETCH] 恢复 tab', tabId, '的 Network 采集失败:', e && e.message);
        }
      }
      if (saved.console) {
        try {
          await chrome.debugger.sendCommand({ tabId }, 'Runtime.enable', {});
          await chrome.debugger.sendCommand({ tabId }, 'Log.enable', {}).catch(() => null);
          session.console = true;
          restoredAnything = true;
        } catch (e) {
          console.warn('[TMWD-FETCH] 恢复 tab', tabId, '的 console 采集失败:', e && e.message);
        }
      }
      if (saved.fetchEnabled) {
        const state = interceptStateOf(session);
        state.rules = normalizeInterceptRules(saved.rules);
        state.responseStage = !!saved.responseStage;
        const patterns = buildFetchPatterns(state.rules, state.responseStage);
        if (!patterns.length) {
          state.fetchEnabled = false;
          state.patterns = [];
        } else {
          await chrome.debugger.sendCommand(
            { tabId },
            'Fetch.enable',
            { patterns: toFetchEnablePatterns(patterns) }
          );
          state.fetchEnabled = true;
          state.patterns = patterns;
          restoredAnything = true;
          console.log('[TMWD-FETCH] 已恢复 tab', tabId, '的拦截规则:', state.rules.length);
        }
      }
      // 一条都没恢复成功 → 这份条目是脏数据，下面按 stale 清掉，避免每次冷启动重试；
      // 同时排一次延迟 detach：此时是我们为恢复而重新 attach 的，
      // 规则恢复不出来就没有任何理由继续挂着「正在调试此浏览器」横幅。
      if (!restoredAnything) {
        staleTabs.push(tabId);
        scheduleDebugDetach(session, 'restore-no-rule');
      }
    } catch (e) {
      console.warn('[TMWD-FETCH] 恢复 tab', tabId, '失败:', e && e.message);
      staleTabs.push(tabId);
      // 失败也可能已经 attach 上了：同样不要留下无用途的 attach。
      try { scheduleDebugDetach(getDebugSession(tabId), 'restore-failed'); } catch (_) {}
    }
  }
  // 恢复失败的条目（或 tab 已不存在）顺手清掉，避免每次冷启动都重试同一份脏数据。
  // 注意这里必须调 *Locked 版本：外层已持锁，再调 persistDebugInterceptState 会等待自己而死锁。
  await writeDebugStateSnapshotLocked(staleTabs);
  return { pending: pendingTabs.size };
}

async function handleNetworkStart(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    const maxEntries = normalizeMaxEntries(msg.max_entries ?? msg.maxEntries);
    session.networkMaxEntries = maxEntries;
    // 幂等 start（daemon 重连 / 重试时会带 idempotent:true）：
    // 已在采集就只更新配置并裁剪，不清空已捕获条目、不重复 enable 监听器。
    // 必须同时要求 attached：SW 换代后 network 标志可能残留但浏览器侧已 detach，
    // 这时要落到下面的正常路径重新 attach + enable，而不是谎报 already_running。
    if (session.network && session.attached && msg.idempotent === true) {
      trimRequestBuffer(session);
      await persistDebugInterceptState();
      return {
        ok: true,
        status: 'started',
        already_running: true,
        tabId,
        max_entries: maxEntries,
        count: session.requestOrder.length
      };
    }
    // attach 与 Network.enable 放在同一把 tab 锁里，避免并发命令把刚建好的
    // attach 当成「旧 attach」detach 后重来（那样 Fetch.enable 也会一起丢）。
    await withDebugTabLock(tabId, async () => {
      await ensureDebugAttachedLocked(session);
      await chrome.debugger.sendCommand({ tabId }, 'Network.enable', {});
      session.network = true;
      markDebugTabTouched(tabId);
    });
    trimRequestBuffer(session);
    // 采集状态必须落盘：SW 重启后由扩展自己重建 Network.enable，不等 daemon 重下发。
    await persistDebugInterceptState();
    return { ok: true, status: 'started', tabId, max_entries: maxEntries, count: session.requestOrder.length };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleNetworkList(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    const filter = String(msg.filter || '').toLowerCase();
    const limit = Math.max(1, Math.min(Number(msg.limit || 100), 1000));
    let items = session.requestOrder.map(id => session.requests.get(id)).filter(Boolean);
    if (filter) {
      items = items.filter(item => [item.url, item.method, item.status, item.mimeType, item.resourceType].some(v => String(v || '').toLowerCase().includes(filter)));
    }
    items = items.slice(-limit).map(summarizeRequest);
    return { ok: true, status: session.network ? 'started' : 'stopped', tabId, count: items.length, requests: items };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleNetworkDetail(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const requestId = String(msg.requestId || '');
    const session = getDebugSession(tabId);
    const item = session.requests.get(requestId);
    if (!item) return { ok: false, error: 'unknown requestId: ' + requestId };
    const detail = Object.assign({}, item);
    // 区分「未采集」与「取不到」：以前未 attach 时两个字段都不出现，调用方无法判断
    // 到底是没开 debugger 还是响应体已经被丢弃。
    if (!session.attached) {
      detail.body = null;
      detail.bodyError = 'debugger 未 attach，未采集响应体';
    } else if (!item.completed) {
      detail.body = null;
      detail.bodyError = '请求尚未结束，未采集响应体';
    } else if (item.failed) {
      detail.body = null;
      detail.bodyError = '请求失败，没有响应体';
    } else {
      try {
        const body = await chrome.debugger.sendCommand({ tabId }, 'Network.getResponseBody', { requestId });
        // base64 是「每 4 字符一组」的编码：按任意长度 slice 会切出非法 base64，
        // 调用方一解码就失败。上限先向下取到 4 的倍数再截断。
        const raw = String(body.body || '');
        const isB64 = !!body.base64Encoded;
        let limit = 20000;
        if (isB64) limit -= limit % 4;
        detail.body = truncateText(raw, limit);
        detail.base64Encoded = isB64;
        detail.bodyTruncated = raw.length > limit;
        // 明确告诉调用方「这是被切断的 base64，不是完整文件」，别拿它去写盘。
        if (isB64 && detail.bodyTruncated) detail.bodyEncoding = 'base64-truncated';
      } catch (e) {
        detail.bodyError = e.message;
      }
    }
    return { ok: true, tabId, request: detail };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleNetworkClear(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    session.requests.clear();
    session.requestOrder = [];
    return { ok: true, status: 'cleared', tabId };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleNetworkStop(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    // 与 attach/Network.enable 同一把 tab 锁，避免「stop 的 disable」插进
    // 并发 start 的 attach+enable 之间，留下「enable 了但内存说 stopped」的错位。
    await withDebugTabLock(tabId, async () => {
      if (session.attached) {
        try { await chrome.debugger.sendCommand({ tabId }, 'Network.disable', {}); } catch (_) {}
      }
      session.network = false;
      markDebugTabTouched(tabId);
      session.requests.clear();
      session.requestOrder = [];
    });
    await persistDebugInterceptState();
    await detachDebugIfIdle(session);
    return { ok: true, status: 'stopped', cleared: true, tabId };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleConsoleStart(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    const maxEntries = normalizeMaxEntries(msg.max_entries ?? msg.maxEntries);
    session.consoleMaxEntries = maxEntries;
    // 同 networkStart 的幂等语义（已在采集只更新上限，不清日志、不重复 enable）。
    // 同样要求 attached，避免 SW 换代后「标志还在、浏览器侧已 detach」被误判为已在采集。
    if (session.console && session.attached && msg.idempotent === true) {
      trimConsoleBuffer(session);
      await persistDebugInterceptState();
      return {
        ok: true,
        status: 'started',
        already_running: true,
        tabId,
        max_entries: maxEntries,
        count: session.logs.length
      };
    }
    await withDebugTabLock(tabId, async () => {
      await ensureDebugAttachedLocked(session);
      await chrome.debugger.sendCommand({ tabId }, 'Runtime.enable', {});
      await chrome.debugger.sendCommand({ tabId }, 'Log.enable', {}).catch(() => null);
      session.console = true;
      markDebugTabTouched(tabId);
    });
    trimConsoleBuffer(session);
    // 同 networkStart：采集状态落盘，SW 重启后由扩展自己重建。
    await persistDebugInterceptState();
    return { ok: true, status: 'started', tabId, max_entries: maxEntries, count: session.logs.length };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleConsoleList(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    const level = String(msg.level || '').toLowerCase();
    const limit = Math.max(1, Math.min(Number(msg.limit || 100), 1000));
    let logs = session.logs;
    if (level) logs = logs.filter(item => String(item.level || '').toLowerCase() === level);
    return { ok: true, status: session.console ? 'started' : 'stopped', tabId, count: logs.length, logs: logs.slice(-limit) };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleConsoleClear(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    session.logs = [];
    return { ok: true, status: 'cleared', tabId };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleConsoleStop(msg, sender) {
  try {
    const tabId = resolveTabId(msg, sender);
    const session = getDebugSession(tabId);
    await withDebugTabLock(tabId, async () => {
      if (session.attached) {
        try { await chrome.debugger.sendCommand({ tabId }, 'Runtime.disable', {}); } catch (_) {}
        try { await chrome.debugger.sendCommand({ tabId }, 'Log.disable', {}); } catch (_) {}
      }
      session.console = false;
      markDebugTabTouched(tabId);
      session.logs = [];
    });
    await persistDebugInterceptState();
    await detachDebugIfIdle(session);
    return { ok: true, status: 'stopped', cleared: true, tabId };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

// --- 网络层拦截（CDP Fetch 域） ---
//
// 与 page_net.js 的页面级 hook 互补：那边包装 fetch/XHR，改不了 responseType=blob，
// 也拦不到 img/script 这类非 JS 发起的请求；这里在网络层出手。
//
// 规则字段（都可选）：
//   match / method                       命中条件（URL 片段或正则；method 支持 *）
//   abort                                请求直接失败
//   status / body / base64Body / headers 伪造响应（不真实发出请求）
//   requestHeaders / requestBody         放行但改请求
//   response                             放行并拿到真实响应后改写 {status, body, base64Body, headers}
function emptyInterceptStats() {
  return { matched: 0, mocked: 0, aborted: 0, rewritten: 0, continued: 0, failed: 0 };
}

/// CDP 的 body 一律是 base64。这里按 编码，不能用 btoa(unescape(encodeURIComponent))。
function textToBase64(text) {
  const bytes = new TextEncoder().encode(String(text === undefined || text === null ? '' : text));
  let binary = '';
  for (let i = 0; i < bytes.length; i += 1) binary += String.fromCharCode(bytes[i]);
  return btoa(binary);
}

/// 规则里的 body 允许字符串或对象，与 page_net 的 ruleBody 保持一致。
function interceptBodyText(body) {
  if (body === undefined || body === null) return '';
  return typeof body === 'string' ? body : JSON.stringify(body);
}

function headersToEntries(headers) {
  const out = [];
  if (!headers) return out;
  if (Array.isArray(headers)) {
    for (const item of headers) {
      if (!item) continue;
      if (Array.isArray(item)) out.push({ name: String(item[0]), value: String(item[1] === undefined ? '' : item[1]) });
      else if (item.name !== undefined) out.push({ name: String(item.name), value: String(item.value === undefined ? '' : item.value) });
    }
    return out;
  }
  if (typeof headers === 'object') {
    for (const key of Object.keys(headers)) out.push({ name: key, value: String(headers[key]) });
  }
  return out;
}

/// CDP 的 headers 是整体替换，所以必须「基于原头部合并覆盖」后再全量传，否则原有头会丢。
///
/// 同名头必须按**多值**保存。旧实现用 `Map<name, entry>` 去重，而 order 里
/// 仍留着重复的 key → 输出时把「第一条」写了两遍、第二条彻底丢失。最典型的就是
/// 两条 `Set-Cookie`：改一个 status 就会让其中一个 cookie 消失（甚至把错误的值重复下发）。
/// 现在 base 的每个同名项逐条保留；只有在规则**显式**给出同名头时才整组替换（覆盖语义），
/// 且规则自己给多条同名头时按顺序追加，不当成互相覆盖。
function mergeHeaderEntries(base, ...patches) {
  const order = [];
  const groups = new Map(); // lowerName -> [{ name, value }]
  const pushGroup = (key, entry) => {
    if (!groups.has(key)) {
      groups.set(key, []);
      order.push(key);
    }
    groups.get(key).push(entry);
  };
  for (const entry of headersToEntries(base)) pushGroup(entry.name.toLowerCase(), entry);
  const patched = new Set();
  for (const patch of patches) {
    for (const entry of headersToEntries(patch)) {
      const key = entry.name.toLowerCase();
      if (!patched.has(key)) {
        // 规则显式覆盖：整组换成规则给的值（保留规则里的原始大小写）。
        groups.set(key, [{ name: entry.name, value: entry.value }]);
        patched.add(key);
        if (!order.includes(key)) order.push(key);
      } else {
        groups.get(key).push({ name: entry.name, value: entry.value });
      }
    }
  }
  const out = [];
  for (const key of order) {
    const items = groups.get(key);
    if (items && items.length) out.push(...items);
  }
  return out;
}

function dropHeaderEntries(entries, names) {
  const banned = new Set(names.map(name => name.toLowerCase()));
  return entries.filter(entry => !banned.has(String(entry.name).toLowerCase()));
}

function interceptHasResponseStage(rules) {
  return (rules || []).some(rule => rule && rule.response && typeof rule.response === 'object');
}

/// 规则的 match 是「子串 / glob / 正则」三合一的宽松写法，而 CDP Fetch 的 urlPattern
/// 只认 `*`（和 `?`）通配。这里把 match 尽力收窄成 urlPattern，原则是
/// **宁可多拦（回退 `*`），绝不漏拦**：漏拦会让规则静默失效，多拦只是多一次 JS 判断。
///
/// 与 persist（matchesPersistPattern / persist_boot.js）的口径对齐：
///   * 无 scheme 的 match（`*/api/*`）→ 前置 `*` 之后 http 与 https 都命中，与 persist 一致；
///   * 端口：match 里写了 `host:port` 时**不能**把端口整段删掉 —— `localhost:8080/api`
///     删成 `localhost/api` 后会推出 `*localhost/api*`，永远不命中带端口的真实 URL，
///     规则静默失效。这里只把 `:` 换成 `*`，推导结果仍是原 match 的超集；
///   * `*` 是唯一的通配符，其余字符按字面量 —— 与 persist 的 glob 语义相同。
///
/// **逐条独立推导**：某条规则推导失败只让它自己退化成 `*`，不会把其它规则一起拖成 `*`。
/// 返回值里的 source 会原样进 `status.patterns`，让用户看得见「是这条正则逼得全量拦截」：
///   exact    —— match 不含 `*`，按子串收窄（`api/mock` → `*api/mock*`）
///   glob     —— match 含 `*`，按 glob 收窄（`*/api/*` → `*/api/*`）
///   fallback —— 无法安全推导，退化成 `*`（正则 / 元字符 / 完整 URL / 非法字符 / `#`）
function deriveUrlPattern(match) {
  const raw = String(match || '').trim();
  const fallback = { urlPattern: '*', source: 'fallback' };
  if (!raw || raw === '*') return fallback;
  // /.../flags 正则写法无法翻译
  if (/^\/.*\/[a-z]*$/i.test(raw)) return fallback;
  // 正则里常见的元字符（含 `.*` 这种 mix 写法）：CDP 会当字面量，翻译必然漏拦。
  if (/[\\^$+()[\]{}|]/.test(raw) || /\.\*/.test(raw)) return fallback;
  // 完整 URL 写法（带 scheme）不参与收窄：`*https://h/x*` 这种「前导通配 + scheme」
  // 在 CDP 里的语义没有明确保证，宁可退化成 `*`。
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(raw) || raw.includes('://')) return fallback;
  // `#` 不会出现在请求 URL 里，带上必然永不命中。
  if (raw.includes('#')) return fallback;
  // 只保留 URL 里本来就常见的字符，其余一律放弃收窄。
  if (!/^[\w\-.~:/?@!$&'%*+,;=]+$/.test(raw)) return fallback;
  // 显式写了端口（`host:8080/api`）：只把 `:` 换成通配符，**不删除端口**。
  // 旧实现 `replace(... ':\\d+' ...)` 把 `:8080` 整段吞掉，推出 `*localhost/api*` ——
  // 带端口的真实 URL 永远不命中。换成 `*` 后 pattern 仍是原 match 的超集：
  // `*localhost*8080/api*` 既能命中 `http://localhost:8080/api/x`，也不会误命中别的 host。
  const widened = raw.replace(/(^|\/)([A-Za-z0-9._-]+):(\d{1,5})(?=\/|$)/, '$1$2*$3');
  if (widened.includes('*')) {
    // glob：两侧补通配，保证「子串位置」也能命中（`api/*` 也应命中 https://h/api/x）。
    const head = widened.startsWith('*') ? '' : '*';
    const tail = widened.endsWith('*') ? '' : '*';
    return { urlPattern: head + widened + tail, source: 'glob' };
  }
  // 纯子串：两侧补通配
  return { urlPattern: '*' + widened + '*', source: 'exact' };
}

/// 按当前规则生成 Fetch 的 patterns（附带 source 供 status 展示）。
/// 返回空数组表示「没有任何请求需要拦」，调用方应当直接关掉 Fetch，
/// 这样「开着 CDP 引擎但没规则」时页面零开销（否则每个请求都要过一次 CDP 往返）。
///
/// 注意：这里返回的对象**不能**直接传给 Fetch.enable —— 多出来的 source/match
/// 字段会被 CDP 当成非法参数。enableFetchInterception 会先 strip 成
/// `{ urlPattern, requestStage }` 再发。
function buildFetchPatterns(rules, withResponseStage) {
  const list = Array.isArray(rules) ? rules : [];
  const seen = new Set();
  const patterns = [];
  const push = (rule, urlPattern, source, requestStage) => {
    const key = `${requestStage}|${urlPattern}`;
    if (seen.has(key)) return;
    seen.add(key);
    patterns.push({ urlPattern, requestStage, source, match: String(rule.match) });
  };
  for (const rule of list) {
    if (!rule || !rule.match) continue;
    const { urlPattern, source } = deriveUrlPattern(rule.match);
    // 只改响应体的规则不需要在请求阶段暂停，少一次往返。
    if (interceptRuleHasRequestEffect(rule)) push(rule, urlPattern, source, 'Request');
    // 响应阶段会让每个命中的请求多一次往返，只在确实有 response 规则时才开。
    if (withResponseStage && rule.response && typeof rule.response === 'object') {
      push(rule, urlPattern, source, 'Response');
    }
  }
  return patterns;
}

/// Fetch.enable 只接受 RequestPattern 的合法字段，多余字段会报 Invalid parameters。
function toFetchEnablePatterns(patterns) {
  return (patterns || []).map(item => ({ urlPattern: item.urlPattern, requestStage: item.requestStage }));
}

/// 哪些规则的 match 逼得链路退化成 `*`（正则 / 元字符等）——给 status 用。
/// 直接从规则算，而不是从 patterns 算：patterns 会按 (stage|urlPattern) 去重，
/// 去重后可能丢掉「某条正则也退化成了 *」这条信息。
function fallbackMatchesOf(rules) {
  const out = [];
  for (const rule of rules || []) {
    if (!rule || !rule.match) continue;
    const match = String(rule.match);
    if (deriveUrlPattern(match).source !== 'fallback') continue;
    if (!out.includes(match)) out.push(match);
  }
  return out;
}

function isInterceptMockRule(rule) {
  if (!rule) return false;
  if (rule.status !== undefined || rule.body !== undefined || rule.base64Body !== undefined) return true;
  // headers 单独出现就是「伪造响应头」；但如果同时有 response，headers 属于响应改写。
  return rule.headers !== undefined && !rule.response;
}

function interceptRuleHasRequestEffect(rule) {
  if (!rule) return false;
  return rule.abort === true
    || isInterceptMockRule(rule)
    || rule.requestHeaders !== undefined
    || rule.requestBody !== undefined;
}

/// glob → RegExp：只把 `*` 当通配，其余字符全部按字面量。与 persist 的匹配口径一致。
function globToRegExp(glob) {
  const escaped = String(glob).replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*');
  return new RegExp('^' + escaped + '$', 'i');
}

/// 单条规则的 match 判定，三种写法（与 persist 的 glob 口径统一）：
///   1. 含 `*` → 按 glob 整体匹配（`*/api/*` 这类写法终于能命中）；
///   2. 纯子串 → includes 命中（向下兼容老规则 `api/mock`）；
///   3. `/re/flags` 或裸正则 → RegExp 命中（保留历史行为，写错也不抛）。
function interceptRuleMatches(match, target) {
  const pattern = String(match || '');
  if (!pattern) return false;
  if (pattern.includes('*')) {
    try {
      if (globToRegExp(pattern).test(target)) return true;
    } catch (_) { /* 落到下面的字面量兜底 */ }
    const literal = pattern.replace(/\*/g, '');
    if (!literal) return true;
    return target.includes(literal);
  }
  if (target.includes(pattern)) return true;
  // /re/flags：先剥掉两侧斜杠，否则正则会去匹配字面量斜杠，等于永远不命中。
  if (pattern.length > 2 && pattern.startsWith('/') && /\/[a-z]*$/i.test(pattern)) {
    const end = pattern.lastIndexOf('/');
    try { return new RegExp(pattern.slice(1, end), pattern.slice(end + 1)).test(target); } catch (_) { return false; }
  }
  try { return new RegExp(pattern).test(target); } catch (_) { return false; }
}

function matchInterceptRule(rules, url, method) {
  const target = String(url || '');
  const verb = String(method || 'GET').toUpperCase();
  for (const rule of rules || []) {
    if (!rule || !rule.match) continue;
    if (rule.method && rule.method !== '*' && String(rule.method).toUpperCase() !== verb) continue;
    if (interceptRuleMatches(rule.match, target)) return rule;
  }
  return null;
}

function normalizeInterceptRules(rules) {
  const list = Array.isArray(rules) ? rules : [];
  return list
    .filter(rule => rule && typeof rule === 'object' && rule.match)
    .slice(0, 200)
    .map(rule => Object.assign({}, rule, { match: rule.match }));
}

function interceptStateOf(session) {
  if (!session.intercept) {
    session.intercept = {
      rules: [],
      patterns: [],
      fetchEnabled: false,
      responseStage: false,
      debugPass: INTERCEPT_LOG_PASS_DEFAULT,
      stats: emptyInterceptStats(),
      log: []
    };
  }
  return session.intercept;
}

const INTERCEPT_LOG_LIMIT = 200;
// status 只回最近 100 条，否则大日志会把消息通道拖慢。
const INTERCEPT_LOG_STATUS_LIMIT = 100;
// 放行（pass）是绝大多数请求，默认不记日志：200 条环形缓冲几秒就会被它挤满，
// 真正有价值的 mock/rewrite 记录反而全部丢失。排查时用 netIntercept 的 debugPass 打开。
const INTERCEPT_LOG_PASS_DEFAULT = false;

function interceptLogOf(state) {
  if (!state) return [];
  if (!Array.isArray(state.log)) state.log = [];
  return state.log;
}

/// 环形日志：每条记录一个真正被处理过的请求（mock/abort/continue/rewrite/pass）。
/// 字段固定，daemon 侧要和 page_net 的记录合并排序，缺字段会让两边的格式对不上。
function recordInterceptLog(state, entry) {
  if (!state || !entry) return;
  // pass 默认不落盘，避免把 mock/rewrite 记录挤出环形缓冲。
  if (entry.action === 'pass' && !state.debugPass) return;
  const log = interceptLogOf(state);
  const item = {
    at: Date.now(),
    requestId: entry.requestId,
    url: truncateText(entry.url, 300),
    method: entry.method,
    stage: entry.stage,
    action: entry.action
  };
  if (entry.status !== undefined && entry.status !== null) item.status = Number(entry.status);
  if (entry.match !== undefined && entry.match !== null) item.match = truncateText(entry.match, 100);
  if (entry.resourceType) item.resourceType = String(entry.resourceType);
  // 只留一小段预览：够看出「改成了什么」，又不至于把日志撑爆。
  if (entry.preview !== undefined && entry.preview !== null) item.preview = truncateText(entry.preview, 160);
  log.push(item);
  if (log.length > INTERCEPT_LOG_LIMIT) log.splice(0, log.length - INTERCEPT_LOG_LIMIT);
}

function bumpInterceptStat(session, key) {
  if (!session.intercept || !session.intercept.stats) return;
  if (session.intercept.stats[key] === undefined) session.intercept.stats[key] = 0;
  session.intercept.stats[key] += 1;
}

/// 所有 Fetch.* 调用都走这里：请求可能已经被处理/取消，报错只记 failed，不抛。
async function sendFetchCommand(session, method, params, statKey) {
  try {
    const result = await chrome.debugger.sendCommand({ tabId: session.tabId }, method, params);
    if (statKey) bumpInterceptStat(session, statKey);
    return { ok: true, result };
  } catch (e) {
    bumpInterceptStat(session, 'failed');
    return { ok: false, error: String((e && e.message) || e) };
  }
}

/// continueResponse 是较新的方法；老版本 Chrome 没有它，只能退回 continueRequest。
/// **无论 continueResponse 报什么错都必须再放行一次**：被 Fetch.enable 暂停的请求
/// 如果没有被 continue/fail/fulfill 显式处置，网络栈会一直挂着，页面表现为永久转圈且无报错。
/// 宁可重复发一次 continueRequest（第二次会被 CDP 忽略），也不要把请求挂死。
async function continueFetchResponse(session, requestId, guard) {
  const sent = await sendFetchCommand(session, 'Fetch.continueResponse', { requestId }, 'continued');
  if (sent.ok) {
    if (guard) guard.settled = true;
    return true;
  }
  if (!isFetchMethodMissingError(sent.error)) {
    console.warn('[TMWD-FETCH] Fetch.continueResponse 失败，回退 Fetch.continueRequest:', sent.error);
  }
  const fallback = await sendFetchCommand(session, 'Fetch.continueRequest', { requestId }, 'continued');
  if (fallback.ok && guard) guard.settled = true;
  return fallback.ok;
}

/// 老版本 Chrome 缺 continueResponse 时的典型报错（方法不存在），其余错误都算「意外」。
function isFetchMethodMissingError(message) {
  return /wasn't found|isn't found|not found|not supported|Unknown method/i.test(String(message || ''));
}

/// 一个被暂停的请求「已经被处置过」的标记。
/// 真正的释放动作由 handleFetchPaused 的 finally 兜底，任何漏掉的分支都会被补一次 continue。
function createFetchGuard() {
  return { settled: false };
}

/// 处置类命令（continueRequest / continueResponse / failRequest / fulfillRequest）专用：
/// 成功即视为该请求已放行，finally 里就不会再补一次。
async function settleFetch(session, guard, method, params, statKey) {
  const out = await sendFetchCommand(session, method, params, statKey);
  if (out.ok && guard) guard.settled = true;
  return out;
}

/// 兜底放行一个被暂停的请求：先按**事件所在阶段**选方法，失败再降级到另一个。
///
/// continueRequest 只对 Request 阶段有效、continueResponse 只对 Response 阶段有效，
/// 用错方法时 CDP 会拒绝（错误还常被吞进 stats.failed），于是请求永远挂着 —— 页面
/// 表现为白屏/转圈且没有任何报错。所以两条路径都要试，都失败才算失败。
///
/// 阶段判定依据与 handleFetchPaused 一致：Fetch.requestPaused 的 params 里只要带了
/// responseStatusCode / responseErrorReason 就是 Response 阶段（isFetchResponseStage）。
async function releasePausedRequest(session, requestId, params) {
  if (!session || !requestId) return false;
  const responseStage = isFetchResponseStage(params || {});
  const preferred = responseStage ? 'Fetch.continueResponse' : 'Fetch.continueRequest';
  const fallbackMethod = responseStage ? 'Fetch.continueRequest' : 'Fetch.continueResponse';
  const sent = await sendFetchCommand(session, preferred, { requestId });
  if (sent.ok) return true;
  if (!isFetchMethodMissingError(sent.error)) {
    console.warn('[TMWD-FETCH] 兜底放行首选失败，降级重试:', preferred, '→', fallbackMethod, sent.error);
  }
  const fallback = await sendFetchCommand(session, fallbackMethod, { requestId });
  if (fallback.ok) return true;
  // 两次都失败：请求很可能已经结束（Invalid InterceptionId），但绝不能静默。
  // 这两次失败已由 sendFetchCommand 计入 stats.failed，这里再打一条 error 留痕。
  console.error('[TMWD-FETCH] 兜底放行两次都失败，请求可能已挂起:', requestId,
    preferred, sent.error, '|', fallbackMethod, fallback.error);
  return false;
}

/// 开关 Fetch 拦截并维护内存状态。**调用方必须已持有 tab 锁**。
///
/// 这里**不得**调用 persistDebugInterceptState()：它要走状态锁，而 restore 持状态锁时
/// 会反过来要本 tab 的锁 —— 两条链互相等待就是永久挂死。落盘统一由调用方
/// 在 withDebugTabLock 之外完成（见 handleNetIntercept 的 start/rules 分支）。
async function enableFetchInterception(session, rules) {
  const state = interceptStateOf(session);
  // 只要动过这个 tab 的拦截状态，就必须让读-改-写快照覆盖它。
  markDebugTabTouched(session.tabId);
  const wantResponseStage = interceptHasResponseStage(rules);
  const patterns = buildFetchPatterns(rules, wantResponseStage);
  // 没有任何请求需要拦（规则为空 / 规则都不需要 CDP）→ 彻底关掉 Fetch，
  // 这样「开着 CDP 引擎但没规则」时页面零额外开销。
  if (!patterns.length) {
    if (state.fetchEnabled) {
      try { await chrome.debugger.sendCommand({ tabId: session.tabId }, 'Fetch.disable', {}); } catch (_) {}
      state.fetchEnabled = false;
    }
    state.responseStage = false;
    state.patterns = [];
    // Fetch 关了就不再算「用途」：如果 network/console 也没开，安排一次延迟 detach，
    // 免得标签页顶部一直挂「正在调试」横条、并挡住其它扩展 attach。
    await detachDebugIfIdle(session);
    return patterns;
  }
  if (state.fetchEnabled && sameFetchPatterns(state.patterns, patterns)) {
    state.patterns = patterns;
    state.responseStage = wantResponseStage;
    return patterns;
  }
  // patterns 不能热更新：阶段/范围变了只能 disable + enable。
  if (state.fetchEnabled) {
    try { await chrome.debugger.sendCommand({ tabId: session.tabId }, 'Fetch.disable', {}); } catch (_) {}
    state.fetchEnabled = false;
  }
  const fallbackMatches = fallbackMatchesOf(rules);
  if (fallbackMatches.length) {
    // 只影响「过滤范围」不影响命中：这些规则无法安全翻译，只能全量暂停后再用 JS 判。
    console.warn('[TMWD-FETCH] 以下规则无法收窄 urlPattern，已退化为 *（全量拦截）:', fallbackMatches.join(' | '));
  }
  // Fetch.enable 失败时旧规则已经随上面的 disable 一起丢掉了：绝不能保留
  // rules=N / enabled:false 这种自相矛盾的状态——调用方会以为「规则还在，只是没启用」，
  // 实际拦截已经全部失效。这里把状态清成诚实的「什么都没有」，并把错误抛给上层。
  try {
    await chrome.debugger.sendCommand({ tabId: session.tabId }, 'Fetch.enable', { patterns: toFetchEnablePatterns(patterns) });
  } catch (e) {
    state.fetchEnabled = false;
    state.responseStage = false;
    state.patterns = [];
    state.rules = [];
    console.error('[TMWD-FETCH] Fetch.enable 失败，拦截已完全关闭:', e && e.message);
    await detachDebugIfIdle(session);
    throw new Error('fetch-enable-failed: ' + ((e && e.message) || String(e)));
  }
  state.fetchEnabled = true;
  state.responseStage = wantResponseStage;
  state.patterns = patterns;
  return patterns;
}

function sameFetchPatterns(a, b) {
  const left = Array.isArray(a) ? a : [];
  const right = Array.isArray(b) ? b : [];
  if (left.length !== right.length) return false;
  const key = (item) => `${item.requestStage} ${item.urlPattern}`;
  const set = new Set(left.map(key));
  return right.every((item) => set.has(key(item)));
}

async function handleNetIntercept(msg, sender) {
  let tabId;
  try {
    tabId = resolveTabId(msg, sender);
  } catch (e) {
    return { ok: false, error: e.message };
  }
  const action = String(msg.action || 'status').toLowerCase();
  const session = getDebugSession(tabId);

  if (action === 'status') {
    const state = session.intercept;
    const log = interceptLogOf(state);
    // patterns 每项形如 { urlPattern, requestStage, source, match }，
    // source 直接告诉用户这条 pattern 是精确推导、glob 推导，还是被迫退化成 `*`。
    const fallbackMatches = state ? fallbackMatchesOf(state.rules) : [];
    return {
      ok: true,
      action: 'status',
      tabId,
      enabled: !!(state && state.fetchEnabled),
      rules: state ? state.rules.length : 0,
      patterns: state ? state.patterns : [],
      // 退化清单非空 = 过滤范围是全量（只有这些规则需要全量，其余仍在精确范围内）。
      fallbackMatches,
      debugPass: state ? !!state.debugPass : INTERCEPT_LOG_PASS_DEFAULT,
      stats: state ? Object.assign({}, state.stats) : emptyInterceptStats(),
      log: log.slice(-INTERCEPT_LOG_STATUS_LIMIT),
      logCount: log.length
    };
  }

  if (action === 'clearlog') {
    if (session.intercept) session.intercept.log = [];
    return { ok: true, action: 'clearlog', tabId, logCount: 0 };
  }

  if (action === 'stop') {
    const state = session.intercept;
    const stats = state ? Object.assign({}, state.stats) : emptyInterceptStats();
    // stop 只停「拦截」，不销毁数据：rules/stats/log 仍然保留，
    // 这样 `net stop` 之后 `net log` / `status` 还能读到刚才的现场（排障用）。
    // 这与 markInterceptDetached 的注释是同一意图——detach 后要能读 net log。
    const logCount = state ? interceptLogOf(state).length : 0;
    // Fetch.disable 与 attach/Fetch.enable 同一把 tab 锁，
    // 防止并发的 start 刚 enable 完就被这条 disable 关掉（而回应里说已 stopped）。
    await withDebugTabLock(tabId, async () => {
      if (session.attached) {
        try {
          await chrome.debugger.sendCommand({ tabId }, 'Fetch.disable', {});
        } catch (e) {
          // disable 失败意味着可能还有已暂停但无人认领的请求；至少把原因记下来。
          console.warn('[TMWD-FETCH] stop: Fetch.disable 失败:', e && e.message);
        }
      }
      if (state) {
        state.fetchEnabled = false;
        // patterns 已随 Fetch.disable 失效，继续报告会让人误以为「这些还在拦」。
        state.patterns = [];
      }
      markDebugTabTouched(tabId);
    });
    // 不再算「用途」→ 交给延迟 detach；同时清掉持久化记录，避免 SW 重启后又把拦截装回去。
    await persistDebugInterceptState();
    await detachDebugIfIdle(session);
    return {
      ok: true,
      action: 'stop',
      tabId,
      stats,
      logCount,
      rules: state ? state.rules.length : 0,
      retained: !!state
    };
  }

  if (action !== 'start' && action !== 'rules') {
    return { ok: false, error: 'Unknown netIntercept action: ' + action };
  }

  try {
    const rules = normalizeInterceptRules(msg.rules);
    const state = interceptStateOf(session);
    // attach + 规则替换 + Fetch.disable/enable 整体串行。
    // 否则两条并发的 netIntercept 会互相 Fetch.disable 掉对方刚 enable 的拦截，
    // 最后「浏览器侧只拦一半，内存里却是后一条的规则」。
    let patterns;
    let lockError = null;
    try {
      patterns = await withDebugTabLock(tabId, async () => {
        await ensureDebugAttachedLocked(session);
        // start = 新会话（统计与日志清零）；rules = 只换规则，统计/日志继续累计。
        if (action === 'start') {
          state.stats = emptyInterceptStats();
          state.log = [];
        }
        // 排查用开关：打开后放行（pass）也会进日志，默认关闭以保住环形缓冲。
        if (msg.debugPass !== undefined) state.debugPass = !!msg.debugPass;
        state.rules = rules;
        return await enableFetchInterception(session, rules);
      });
    } catch (e) {
      lockError = e;
    }
    // 落盘必须在 tab 锁**之外**。persistDebugInterceptState 走状态锁，而
    // restoreDebugSessionsLocked 持状态锁时会调 withDebugTabLock —— 在 tab 锁内 await
    // 状态锁就与它形成锁序交叉，daemon 重启 + 重下发 netIntercept 的重合窗口里会挂死。
    // 成功与失败都要写：Fetch.enable 失败时内存状态已被清空，快照必须跟着清，
    // 否则 SW 重启后会照着过期规则恢复。
    await persistDebugInterceptState();
    if (lockError) throw lockError;
    return {
      ok: true,
      action,
      tabId,
      rules: rules.length,
      patterns,
      // 退化到 `*` 的 match（正则等）：这些规则会让过滤范围变成全量，但命中判定仍然精确。
      fallbackMatches: fallbackMatchesOf(rules),
      debugPass: !!state.debugPass,
      fetchEnabled: !!state.fetchEnabled
    };
  } catch (e) {
    const message = (e && e.message) || String(e);
    // Fetch.enable 失败时 enableFetchInterception 已把状态清空；返回体必须与 status 一致，
    // 不能再带 rules:N 造成「规则还在」的假象。
    if (message.startsWith('fetch-enable-failed:')) {
      return {
        ok: false,
        action,
        tabId,
        enabled: false,
        rules: 0,
        patterns: [],
        fetchEnabled: false,
        error: message
      };
    }
    // attach 失败等要带原始 message，调用方才能知道是「别的扩展占着 debugger」还是真出错。
    return { ok: false, error: message };
  }
}

function isFetchResponseStage(params) {
  return params.responseStatusCode !== undefined || params.responseErrorReason !== undefined;
}

async function handleFetchPausedRequest(session, state, params, guard) {
  const requestId = params.requestId;
  const request = params.request || {};
  const url = String(request.url || '');
  const method = String(request.method || 'GET').toUpperCase();
  const resourceType = params.type || params.resourceType;
  const rules = state.rules.filter(interceptRuleHasRequestEffect);
  const rule = matchInterceptRule(rules, url, method);

  if (!rule) {
    await settleFetch(session, guard, 'Fetch.continueRequest', { requestId }, 'continued');
    recordInterceptLog(state, { requestId, url, method, resourceType, stage: 'request', action: 'pass' });
    return;
  }
  bumpInterceptStat(session, 'matched');

  if (rule.abort === true) {
    await settleFetch(session, guard, 'Fetch.failRequest', { requestId, errorReason: 'Failed' }, 'aborted');
    recordInterceptLog(state, { requestId, url, method, resourceType, stage: 'request', action: 'abort', match: rule.match });
    return;
  }

  if (isInterceptMockRule(rule)) {
    let body;
    if (rule.base64Body !== undefined) body = String(rule.base64Body);
    else body = textToBase64(interceptBodyText(rule.body));
    const explicit = headersToEntries(rule.headers);
    const hasType = explicit.some(entry => entry.name.toLowerCase() === 'content-type');
    const responseHeaders = hasType
      ? explicit
      : [{ name: 'content-type', value: 'application/json' }].concat(explicit);
    const responseCode = Number(rule.status) || 200;
    await settleFetch(session, guard, 'Fetch.fulfillRequest', {
      requestId,
      responseCode,
      responseHeaders,
      body
    }, 'mocked');
    recordInterceptLog(state, {
      requestId, url, method, resourceType, stage: 'request', action: 'mock', status: responseCode, match: rule.match,
      preview: rule.base64Body !== undefined ? '(base64 body)' : interceptBodyText(rule.body)
    });
    return;
  }

  if (rule.requestHeaders !== undefined || rule.requestBody !== undefined) {
    const payload = { requestId };
    if (rule.requestHeaders !== undefined) {
      payload.headers = mergeHeaderEntries(request.headers, rule.requestHeaders);
    }
    // GET/HEAD 不能带 body，与 page_net 的处理保持一致。
    if (rule.requestBody !== undefined && method !== 'GET' && method !== 'HEAD') {
      payload.postData = textToBase64(interceptBodyText(rule.requestBody));
    }
    await settleFetch(session, guard, 'Fetch.continueRequest', payload, 'continued');
    recordInterceptLog(state, { requestId, url, method, resourceType, stage: 'request', action: 'continue', match: rule.match });
    return;
  }

  await settleFetch(session, guard, 'Fetch.continueRequest', { requestId }, 'continued');
  recordInterceptLog(state, { requestId, url, method, resourceType, stage: 'request', action: 'pass', match: rule.match });
}

async function handleFetchPausedResponse(session, state, params, guard) {
  const requestId = params.requestId;
  const request = params.request || {};
  const url = String(request.url || '');
  const method = String(request.method || 'GET').toUpperCase();
  const resourceType = params.type || params.resourceType;
  const rule = matchInterceptRule(
    state.rules.filter(item => item && item.response && typeof item.response === 'object'),
    url,
    method
  );

  if (!rule) {
    await continueFetchResponse(session, requestId, guard);
    recordInterceptLog(state, {
      requestId, url, method, resourceType, stage: 'response', action: 'pass', status: params.responseStatusCode
    });
    return;
  }
  bumpInterceptStat(session, 'matched');

  const spec = rule.response;
  const responseCode = Number(spec.status) || Number(params.responseStatusCode) || 200;
  const mergedHeaders = mergeHeaderEntries(params.responseHeaders, rule.headers, spec.headers);
  const bodyReplaced = spec.base64Body !== undefined || spec.body !== undefined;
  recordInterceptLog(state, {
    requestId, url, method, resourceType, stage: 'response', action: 'rewrite', status: responseCode, match: rule.match,
    preview: bodyReplaced
      ? (spec.base64Body !== undefined ? '(base64 body)' : interceptBodyText(spec.body))
      : undefined
  });

  // 只改状态码 / 响应头：用 continueResponse 原地放行，不碰 body。
  // 走 getResponseBody + fulfillRequest 会把压缩响应重新编码一次，
  // content-encoding 与真实字节容易对不上（blob / 二进制响应尤其明显）。
  if (!bodyReplaced) {
    const sent = await settleFetch(session, guard, 'Fetch.continueResponse', {
      requestId,
      responseCode,
      responseHeaders: mergedHeaders
    }, 'rewritten');
    if (sent.ok) return;
    if (!isFetchMethodMissingError(sent.error)) {
      // 意外错误：continueFetchResponse 会再放行一次（失败也会退到 continueRequest）。
      await continueFetchResponse(session, requestId, guard);
      return;
    }
    // 老版本 Chrome 没有 continueResponse：退回「取体 + fulfill」，
    // 此时保持原始头不动（体没换，长度与编码仍然有效）。
    let passthrough;
    try {
      passthrough = await chrome.debugger.sendCommand({ tabId: session.tabId }, 'Fetch.getResponseBody', { requestId });
    } catch (e) {
      bumpInterceptStat(session, 'failed');
      await continueFetchResponse(session, requestId, guard);
      return;
    }
    await settleFetch(session, guard, 'Fetch.fulfillRequest', {
      requestId,
      responseCode,
      responseHeaders: mergedHeaders,
      body: passthrough && typeof passthrough.body === 'string' ? passthrough.body : ''
    }, 'rewritten');
    return;
  }

  let original;
  try {
    original = await chrome.debugger.sendCommand({ tabId: session.tabId }, 'Fetch.getResponseBody', { requestId });
  } catch (e) {
    // 拿不到原始体（请求已结束/被取消）就放行，别把请求挂死。
    bumpInterceptStat(session, 'failed');
    await continueFetchResponse(session, requestId, guard);
    return;
  }

  let body;
  if (spec.base64Body !== undefined) {
    body = String(spec.base64Body);
  } else if (spec.body !== undefined) {
    body = textToBase64(interceptBodyText(spec.body));
  } else {
    body = original && typeof original.body === 'string' ? original.body : '';
  }

  // 体换过之后 length/encoding 必然对不上，继续保留会让页面按旧长度截断。
  const responseHeaders = dropHeaderEntries(mergedHeaders, ['content-length', 'content-encoding']);

  await settleFetch(session, guard, 'Fetch.fulfillRequest', {
    requestId,
    responseCode,
    responseHeaders,
    body
  }, 'rewritten');
}

/// 所有 Fetch.requestPaused 的唯一入口。
/// 被暂停的请求必须被 continue/fail/fulfill 显式处置，否则会永久挂起，
/// 所以这里用 guard + finally 兜底：任何分支（包括 throw 出去）漏了放行，都会被补一次。
async function handleFetchPaused(session, params) {
  if (!params || !params.requestId) return;
  const requestId = params.requestId;
  const state = session.intercept;
  const guard = createFetchGuard();
  try {
    // 拦截已关闭（stop/detach 之后事件还在路上）时必须放行，否则请求会一直挂住。
    if (!state || !state.fetchEnabled) {
      // 成功即标记 settled，否则 finally 会再放行一次（重复 continue 会报 Invalid InterceptionId）。
      if (await releasePausedRequest(session, requestId, params)) guard.settled = true;
      return;
    }
    if (isFetchResponseStage(params)) await handleFetchPausedResponse(session, state, params, guard);
    else await handleFetchPausedRequest(session, state, params, guard);
  } finally {
    if (!guard.settled) await releasePausedRequest(session, requestId, params);
  }
}

function summarizeRequest(item) {
  return {
    requestId: item.requestId,
    url: item.url,
    method: item.method,
    status: item.status,
    mimeType: item.mimeType,
    resourceType: item.resourceType,
    completed: !!item.completed,
    failed: !!item.failed,
    errorText: item.errorText,
    timestamp: item.timestamp
  };
}

function rememberRequest(session, requestId) {
  if (!session.requests.has(requestId)) session.requestOrder.push(requestId);
  // 上限来自 daemon 下发的 max_entries（默认 5000），超出丢最旧。
  trimRequestBuffer(session);
  let item = session.requests.get(requestId);
  if (!item) {
    item = { requestId };
    session.requests.set(requestId, item);
  }
  return item;
}

function pushLog(session, item) {
  session.logs.push(item);
  // console 缓冲同样按 max_entries 裁剪。
  trimConsoleBuffer(session);
}

function remoteObjectText(arg) {
  if (!arg) return '';
  if ('value' in arg) {
    if (typeof arg.value === 'string') return truncateText(arg.value, 2000);
    try { return truncateText(JSON.stringify(arg.value), 2000); } catch (_) { return String(arg.value); }
  }
  return truncateText(arg.description || arg.type || '', 2000);
}

function truncateText(text, max) {
  const value = String(text || '');
  return value.length > max ? value.slice(0, max) : value;
}

/// SW 冷启动时 debugSessions 必然是空的，但浏览器侧的 Fetch 拦截仍然生效：
/// 此时送达的 Fetch.requestPaused 已经是一个被暂停的请求，**必须**放行。
/// 这里没有 session 对象可用，直接按阶段选 CDP 方法；失败只记日志，绝不再抛
/// （onDebuggerEvent 是同步监听器，抛出去只会变成未捕获异常，请求照样挂着）。
async function releaseOrphanFetchRequest(tabId, params) {
  const requestId = params && params.requestId;
  if (!requestId) return;
  const target = { tabId };
  const send = async (method, payload) => {
    try {
      await chrome.debugger.sendCommand(target, method, payload);
      return true;
    } catch (e) {
      console.warn('[TMWD-FETCH] 无会话放行失败:', method, requestId, e && e.message);
      return false;
    }
  };
  // 响应阶段的事件必须用 continueResponse；老 Chrome 没有该方法时退回 continueRequest。
  if (isFetchResponseStage(params)) {
    if (await send('Fetch.continueResponse', { requestId })) return;
  }
  await send('Fetch.continueRequest', { requestId });
}

// 只在「浏览器侧 enable 之后仍会送达、且我们会入库」的事件上惰性重建会话。
const DEBUG_COLLECTOR_EVENTS = new Set([
  'Network.requestWillBeSent',
  'Network.responseReceived',
  'Network.loadingFinished',
  'Network.loadingFailed',
  'Runtime.consoleAPICalled',
  'Runtime.exceptionThrown',
  'Log.entryAdded'
]);

/// 无 session 时惰性重建一个「只采集」会话。
/// Network.enable / Runtime.enable 是浏览器侧状态，不随 debugSessions 消失：SW 重启后
/// 事件照样到达，旧代码逐条 `return`，于是 networkList/consoleList 返回空、status 却报 stopped。
/// 只在 session 不存在时创建，因此**不会覆盖**已有 session 的拦截规则。
function adoptCollectorSession(tabId, method) {
  const session = getDebugSession(tabId);
  session.attached = true; // 事件能送达，说明浏览器侧确实还 attach 着
  if (String(method).startsWith('Network.')) session.network = true;
  else session.console = true;
  markDebugTabTouched(tabId);
  console.log('[TMWD-DEBUG] 无会话但事件到达，惰性重建采集会话:', tabId, method);
  // 重建出的状态也要落盘，否则下一次 SW 回收又会丢。
  persistDebugInterceptState().catch(() => null);
  return session;
}

/// 冷启动第一批 Fetch.requestPaused 先等恢复完成，再按规则处置。
/// 恢复完成 → 走正常路径，mock/abort 不会被跳过；超时/失败 → 按 orphan 放行，
/// 宁可放弃一次拦截也不能把请求挂死。
async function handleFetchPausedAfterRestore(tabId, params) {
  // 竞速结束后必须清掉兜底 timer，否则每个被暂停的请求都留一个 3s 的闭包。
  let timer = null;
  const outcome = await Promise.race([
    ensureDebugSessionsRestored().then(() => 'restored', () => 'failed'),
    // 兜底 timer：restore 若卡在 storage / CDP 上，这里也必须继续往下走。
    new Promise((resolve) => {
      timer = setTimeout(() => resolve('timeout'), DEBUG_RESTORE_WAIT_MS);
    })
  ]);
  if (timer) {
    try { clearTimeout(timer); } catch (_) {}
    timer = null;
  }
  const session = debugSessions.get(tabId);
  if (session) {
    applyDebuggerEvent(session, 'Fetch.requestPaused', params);
    return;
  }
  if (outcome === 'restored') {
    console.warn('[TMWD-FETCH] 恢复已完成但该 tab 没有可恢复的拦截规则，按无会话放行:', params && params.requestId);
  } else {
    console.warn('[TMWD-FETCH] 冷启动恢复未完成(' + outcome + ')，该请求按无会话放行:', params && params.requestId);
  }
  await releaseOrphanFetchRequest(tabId, params);
}

function onDebuggerEvent(source, method, params) {
  const tabId = source.tabId;
  if (!tabId) return;
  let session = debugSessions.get(tabId);
  if (!session) {
    // SW 冷启动（或状态已随 tabs.onRemoved 清掉）时没有内存会话：
    //   * Fetch.requestPaused 必须**先等恢复**再决定 mock/abort/放行；
    //   * 采集类事件说明浏览器侧 enable 还活着，惰性重建会话后正常入库。
    if (method === 'Fetch.requestPaused') {
      handleFetchPausedAfterRestore(tabId, params).catch((e) => {
        console.warn('[TMWD-FETCH] 恢复后处置 requestPaused 异常:', e && e.message);
        // 兜底：异步链路任何一环抛错都必须放行，否则请求永久挂住。
        releaseOrphanFetchRequest(tabId, params).catch(() => {});
      });
      return;
    }
    if (!DEBUG_COLLECTOR_EVENTS.has(method)) return;
    session = adoptCollectorSession(tabId, method);
  }
  applyDebuggerEvent(session, method, params);
}

/// 已拿到 session 的事件分发（Fetch 处置 + 各类采集入库）。
function applyDebuggerEvent(session, method, params) {
  if (method === 'Fetch.requestPaused') {
    if (!session.intercept) {
      // stop/detach 之后事件可能还在路上：必须放行，否则这个请求会一直挂住。
      // 阶段判断交给 releasePausedRequest（Response 阶段只能 continueResponse）。
      releasePausedRequest(session, params.requestId, params)
        .then((ok) => {
          if (!ok) console.warn('[TMWD-FETCH] 迟到 requestPaused 放行失败:', params.requestId);
        })
        .catch((e) => {
          console.warn('[TMWD-FETCH] 迟到 requestPaused 放行异常:', e && e.message);
        });
      return;
    }
    // onDebuggerEvent 是同步监听器，异步处理要自己兜住 rejection。
    handleFetchPaused(session, params).catch((e) => {
      console.warn('[TMWD-FETCH] requestPaused failed:', e && e.message);
      // 兜底：处理过程中抛出去时再放行一次，宁可重复也不要把请求挂死。
      releasePausedRequest(session, params.requestId, params);
    });
    return;
  }
  if (method === 'Network.requestWillBeSent') {
    const item = rememberRequest(session, params.requestId);
    item.url = params.request?.url || item.url;
    item.method = params.request?.method || item.method;
    item.resourceType = params.type || item.resourceType;
    item.timestamp = params.wallTime || params.timestamp || item.timestamp;
    item.requestHeaders = params.request?.headers;
  } else if (method === 'Network.responseReceived') {
    const item = rememberRequest(session, params.requestId);
    item.status = params.response?.status;
    item.statusText = params.response?.statusText;
    item.mimeType = params.response?.mimeType;
    item.responseHeaders = params.response?.headers;
    item.url = params.response?.url || item.url;
    item.resourceType = params.type || item.resourceType;
  } else if (method === 'Network.loadingFinished') {
    const item = rememberRequest(session, params.requestId);
    item.completed = true;
    item.encodedDataLength = params.encodedDataLength;
  } else if (method === 'Network.loadingFailed') {
    const item = rememberRequest(session, params.requestId);
    item.completed = true;
    item.failed = true;
    item.errorText = params.errorText;
  } else if (method === 'Runtime.consoleAPICalled') {
    pushLog(session, {
      level: params.type || 'log',
      text: (params.args || []).map(remoteObjectText).join(' '),
      timestamp: params.timestamp || Date.now(),
      url: params.stackTrace?.callFrames?.[0]?.url || '',
      line: params.stackTrace?.callFrames?.[0]?.lineNumber,
      column: params.stackTrace?.callFrames?.[0]?.columnNumber
    });
  } else if (method === 'Runtime.exceptionThrown') {
    pushLog(session, {
      level: 'error',
      text: params.exceptionDetails?.text || params.exceptionDetails?.exception?.description || 'exception thrown',
      timestamp: params.timestamp || Date.now(),
      url: params.exceptionDetails?.url || '',
      line: params.exceptionDetails?.lineNumber,
      column: params.exceptionDetails?.columnNumber
    });
  } else if (method === 'Log.entryAdded') {
    const e = params.entry || {};
    pushLog(session, {
      level: e.level || 'log',
      text: e.text || '',
      timestamp: e.timestamp || Date.now(),
      url: e.url || '',
      line: e.lineNumber,
      column: undefined
    });
  }
}

function onDebuggerDetach(source) {
  if (!source.tabId) return;
  // alarm 清理失败不能变成未处理拒绝（Chrome 会记成扩展错误）。
  Promise.resolve(chrome.alarms.clear(debugDetachAlarmName(source.tabId))).catch(() => null);
  const session = debugSessions.get(source.tabId);
  if (session) {
    // 与 detachDebugSessionLocked 同口径：先记「有没有状态」，清完标志再决定是否写快照。
    const hadPersistedState = !!(session.network || session.console || (session.intercept && session.intercept.fetchEnabled));
    session.attached = false;
    session.network = false;
    session.console = false;
    // 与 detachDebugSession 同一口径：保留 intercept 的规则与日志，只标记 Fetch 已关闭。
    // （我们自己调 detach 也会触发这个事件，所以两处必须一致，否则日志照样丢。）
    markInterceptDetached(session);
    session.detachReason = null;
    if (hadPersistedState) {
      markDebugTabTouched(source.tabId);
      // detach 后 Fetch 不再生效，持久化记录也必须跟着失效。
      persistDebugInterceptState().catch(() => null);
    }
  }
}

// 顶层注册必须全部走可选链：MV3 里 Service Worker 是在「脚本求值阶段」挂掉的，
// 任何一句 chrome.xxx.yyy.addListener 抛 TypeError 都会让整个扩展静默失效
// （popup 只显示「未连接」，没有任何线索）。
chrome.debugger?.onEvent?.addListener?.(onDebuggerEvent);
chrome.debugger?.onDetach?.addListener?.(onDebuggerDetach);



async function handleCloseTab(msg, sender) {
  try {
    const tabId = Number(msg.tabId || sender.tab?.id);
    if (!Number.isInteger(tabId) || tabId <= 0) throw new Error('tabId is required');
    const session = debugSessions.get(tabId);
    // 不再用 session?.attached 当闸门——SW 重启后这个标志可能是 false，
    // 而浏览器侧仍然 attach（横幅常驻）。detachDebugSession 内部会用 getTargets 复核。
    if (session) await detachDebugSession(session, 'close-tab');
    debugSessions.delete(tabId);
    markDebugTabTouched(tabId);
    await persistDebugInterceptState();
    await chrome.tabs.remove(tabId);
    return { ok: true, data: { status: 'success', tabId } };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function handleDebugClearAll() {
  const tabIds = [...debugSessions.keys()];
  await detachAllDebugSessions('debugClearAll');
  return { ok: true, status: 'cleared', tabs: tabIds.length };
}

/// 用 chrome.tabs.captureVisibleTab 截图：**不附加调试器**，因此不会出现
/// 「已开始调试此浏览器」横幅。代价是只能截当前激活标签页的可见区域，
/// 整页/元素截图仍走 CDP（daemon 会按需回落）。
async function handleCaptureVisible(msg) {
  try {
    if (!chrome.tabs?.captureVisibleTab) {
      throw new Error('chrome.tabs.captureVisibleTab 不可用');
    }
    const tabId = Number(msg.tabId);
    if (!Number.isInteger(tabId) || tabId <= 0) throw new Error('tabId is required');
    const tab = await chrome.tabs.get(tabId);
    if (!tab?.active) {
      throw new Error('目标标签页不是当前激活页，非调试器截图不可用');
    }
    const format = msg.format === 'jpeg' ? 'jpeg' : 'png';
    const options = { format };
    if (format === 'jpeg' && Number.isFinite(Number(msg.quality))) {
      options.quality = Math.min(100, Math.max(1, Number(msg.quality)));
    }
    const dataUrl = await captureVisibleTabThrottled(tab.windowId, options);
    return { ok: true, data: { data_url: dataUrl, format, via: 'captureVisibleTab' } };
  } catch (e) {
    return {
      ok: false,
      error_code: (e && e.code) || 'capture_failed',
      error: e && e.message ? e.message : String(e),
      hint: (e && e.hint) || null
    };
  }
}

// --- 整页截图拼接（纯页面通道，不碰 chrome.debugger）---
// 背景：本机别的扩展给所有 http(s) 页面注入了扩展 iframe，Chrome 因此拒绝
// chrome.debugger.attach，Page.captureScreenshot 全废。但 captureVisibleTab 不受影响，
// 于是用「滚一屏 → 截一屏 → OffscreenCanvas 按 y 拼接」绕过调试器。
const CAPTURE_FULLPAGE_DEFAULT_MAX_HEIGHT = 20000;
const CAPTURE_FULLPAGE_MAX_SLICES = 40;
// 整页拼接的内存预算。画布是 width × canvasHeight（设备像素）× 4 字节，
// 4K 屏 + 长页很容易到 200~300MB，MV3 的 SW 会被直接 OOM 杀掉，连滚动位置都来不及恢复。
// 默认 8000 万像素（约 320MB 上限附近仍偏高，但保持了历史可用范围）；1M..500M 可配。
const CAPTURE_MAX_PIXELS_DEFAULT = 80000000;
const CAPTURE_MAX_PIXELS_MIN = 1000000;
const CAPTURE_MAX_PIXELS_MAX = 500000000;

async function loadCaptureMaxPixels() {
  try {
    const stored = await chrome.storage.local.get({ captureMaxPixels: CAPTURE_MAX_PIXELS_DEFAULT });
    const raw = Number(stored && stored.captureMaxPixels);
    if (!Number.isFinite(raw)) return CAPTURE_MAX_PIXELS_DEFAULT;
    return Math.min(CAPTURE_MAX_PIXELS_MAX, Math.max(CAPTURE_MAX_PIXELS_MIN, Math.floor(raw)));
  } catch (_) {
    return CAPTURE_MAX_PIXELS_DEFAULT;
  }
}
// 每片滚动后要等两帧 + 120ms：两帧保证合成器已提交新滚动偏移，120ms 给懒加载/图片解码留余量。
// 少等会出现「上一屏内容被画到这一屏位置」的错位（尤其带 sticky 头部的页面）。
const CAPTURE_FULLPAGE_SETTLE_MS = 120;
// Chrome 对 captureVisibleTab 有硬限流：MAX_CAPTURE_VISIBLE_TAB_CALLS_PER_SECOND = 2。
// 只等 120ms 的话连续滑动时一秒能打 5~8 次，直接抛 quota 错误让整页截图崩在第二片。
// 所以再叠一个「两次调用至少隔 550ms」，留 10% 余量给窗口边界。20 片 ≈ 11s，可以接受。
const CAPTURE_VISIBLE_MIN_GAP_MS = 550;

/// 带限流的 captureVisibleTab：保证任意 1 秒内不超过 2 次。
/// 单张 captureVisibleTab（handleCaptureVisible）与整页拼接**共用**这一个
/// 时间戳，否则「先单张、再整页」连续两次调用会各自认为自己是第一个，
/// 合起来超过 Chrome 的每秒 2 次硬限流，整页拼接在第 1~2 片就抛 quota 错误。
/// check-then-set 不是原子的 —— 并发调用会一起读到旧时间戳、算出同一个
/// waitMs 同时醒来，实际间隔 ≈0ms。改成 promise 链排队：每次调用都排在上一**次
/// 调用（含 sleep）之后**，串行保证「相邻两次真正发起 capture 的时刻」≥ 550ms。
/// 链上一步失败不能卡死后续（否则一次 quota 错误会让整页拼接永久停摆）。
let lastCaptureVisibleAt = 0;
let lastCaptureGate = Promise.resolve();

function captureVisibleTabThrottled(windowId, options) {
  const run = lastCaptureGate.then(async () => {
    const waitMs = lastCaptureVisibleAt + CAPTURE_VISIBLE_MIN_GAP_MS - Date.now();
    if (waitMs > 0) await new Promise((resolve) => setTimeout(resolve, waitMs));
    lastCaptureVisibleAt = Date.now();
    return await chrome.tabs.captureVisibleTab(windowId, options);
  });
  lastCaptureGate = run.then(() => undefined, () => undefined);
  return run;
}

/// 量页面尺寸。返回值非法（导航中、页面冻结）一律抛错，由调用方转成 {ok:false}。
async function measureFullPage(tabId) {
  const result = await chrome.scripting.executeScript({
    target: { tabId },
    func: () => ({
      innerH: window.innerHeight,
      innerW: window.innerWidth,
      scrollH: Math.max(
        document.documentElement ? document.documentElement.scrollHeight : 0,
        document.body ? document.body.scrollHeight : 0
      ),
      dpr: window.devicePixelRatio || 1,
      startY: window.scrollY
    })
  });
  const value = result && result[0] && result[0].result;
  if (!value) throw new Error('无法读取页面尺寸（executeScript 未返回结果，页面可能正在导航或已冻结）');
  return value;
}

/// 滚到指定位置并等待渲染稳定。注入函数返回 Promise，executeScript 会等它 resolve。
async function scrollFullPageTo(tabId, top, settleMs) {
  await chrome.scripting.executeScript({
    target: { tabId },
    func: async (y, waitMs) => {
      window.scrollTo(0, y);
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
      await new Promise((resolve) => setTimeout(resolve, waitMs));
      return window.scrollY;
    },
    args: [top, settleMs]
  });
}

/// 把 Uint8Array 转 base64。不能直接 `String.fromCharCode(...bytes)`：
/// 整页 PNG 动辄几 MB，展开成参数会打爆调用栈（RangeError: Maximum call stack size）。
/// 0x8000 一片是保守值，既能控制参数量，也不会产生过多中间字符串。
function bytesToBase64(bytes) {
  const CHUNK = 0x8000;
  const parts = [];
  for (let i = 0; i < bytes.length; i += CHUNK) {
    parts.push(String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK)));
  }
  return btoa(parts.join(''));
}

async function decodeDataUrlBitmap(dataUrl) {
  const blob = await (await fetch(dataUrl)).blob();
  return await createImageBitmap(blob);
}

/// 按 y 偏移把各片贴到同一张 OffscreenCanvas 上，再编码回 data URL。
///
/// 两处硬约束：
// *：画布宽度与**纵向比例**都必须以第一片的真实像素为准。captureVisibleTab 的输出
///     与 innerW/innerH 之间可能因为 dpr 取整、页面缩放（浏览器 zoom）差几个像素，
///     用 `y * dpr` 会把每片按错误步长叠上去，整页系统性错位且完全不报错。
///     这里用 `realScale = bmp.height / innerH` 反推真实比例，后续每片都校验高度一致。
// *：画布面积必须有上限，超了抛结构化错误，绝不让 SW 被 OOM 干掉。
///
/// `offsets[i]` 的 offset 是 **CSS 像素**的滚动 y（不是设备像素）。
async function stitchFullPageSlices(offsets, format, quality, { innerH, coveredCssH, maxHeightPx, maxPixels }) {
  if (typeof OffscreenCanvas !== 'function') {
    throw structuredError('capture_unsupported', 'OffscreenCanvas 不可用，无法拼接整页截图', '请升级浏览器到 Chrome 120+');
  }
  if (!offsets.length) throw structuredError('capture_no_slices', '没有可拼接的截图分片');
  let canvas = null;
  let ctx = null;
  let width = 0;
  let realScale = 0;
  let canvasHeight = 0;
  for (const item of offsets) {
    const bmp = await decodeDataUrlBitmap(item.dataUrl);
    try {
      if (!canvas) {
        width = bmp.width;
        if (!width || !bmp.height) throw structuredError('capture_empty_slice', '截图分片尺寸为 0');
        // 首片才是「真实设备像素」的唯一可信来源：反推纵向比例。
        realScale = innerH > 0 ? bmp.height / innerH : (bmp.height / Math.max(1, coveredCssH));
        if (!Number.isFinite(realScale) || realScale <= 0) realScale = 1;
        canvasHeight = Math.max(1, Math.min(Math.round(coveredCssH * realScale), maxHeightPx || Infinity));
        // 面积预算（设备像素），在真正分配画布之前判。
        const pixels = width * canvasHeight;
        if (Number.isFinite(maxPixels) && maxPixels > 0 && pixels > maxPixels) {
          throw structuredError(
            'capture_too_large',
            `整页截图超出像素预算：${width}x${canvasHeight} = ${pixels} 像素 > 上限 ${maxPixels}`,
            '减小 maxHeightPx / 缩小窗口或页面缩放后重试，或调高 storage 里的 captureMaxPixels（100万..5亿）'
          );
        }
        try {
          canvas = new OffscreenCanvas(width, canvasHeight);
          ctx = canvas.getContext('2d');
        } catch (e) {
          throw structuredError('capture_canvas_failed', '创建整页画布失败（内存不足？）: ' + ((e && e.message) || e), '降低 maxHeightPx 或缩小窗口后重试');
        }
        if (!ctx) throw structuredError('capture_canvas_failed', 'OffscreenCanvas 2d 上下文不可用');
        // JPEG 没有 alpha 通道：不铺底色时透明区域会变成黑色。
        if (format === 'jpeg') {
          ctx.fillStyle = '#ffffff';
          ctx.fillRect(0, 0, canvas.width, canvas.height);
        }
      } else {
        // 后续分片的高度必须与首片一致（允许 2px 取整误差）；不一致说明
        // dpr/缩放在截图过程中变了，按首片比例继续（至少在视觉上是连续的）并留痕。
        const expected = Math.round(innerH * realScale);
        if (Math.abs(bmp.height - expected) > 2) {
          console.warn('[TMWD-CAPTURE] 分片高度与首片比例不一致（expected≈' + expected + ', got ' + bmp.height +
            '），按首片 realScale=' + realScale.toFixed(4) + ' 继续拼接');
        }
      }
      ctx.drawImage(bmp, 0, Math.round(item.offset * realScale));
    } finally {
      if (typeof bmp.close === 'function') bmp.close();
    }
  }
  const blob = await canvas.convertToBlob(
    format === 'jpeg'
      ? { type: 'image/jpeg', quality: Number.isFinite(quality) ? quality / 100 : 0.92 }
      : { type: 'image/png' }
  );
  const buffer = await blob.arrayBuffer();
  return { width, height: canvas.height, scale: realScale, base64: bytesToBase64(new Uint8Array(buffer)) };
}

async function handleCaptureFullPage(msg) {
  let restore = null; // { tabId, startY }：量到 startY 之后才允许恢复，否则没有可信目标值
  try {
    if (!chrome.scripting?.executeScript) {
      throw new Error('chrome.scripting.executeScript 不可用：整页拼接需要 "scripting" 权限与站点访问权限');
    }
    if (!chrome.tabs?.captureVisibleTab) {
      throw new Error('chrome.tabs.captureVisibleTab 不可用');
    }
    const tabId = Number(msg.tabId);
    if (!Number.isInteger(tabId) || tabId <= 0) throw new Error('tabId is required');
    const tab = await chrome.tabs.get(tabId);
    if (!tab) throw new Error('找不到标签页 ' + tabId);
    // captureVisibleTab 只能截「窗口的激活标签页」，非激活页截出来是别人的画面，必须提前拒绝。
    if (tab.active !== true) {
      throw new Error('目标标签页不是当前激活页，整页拼接需要 --tab 指向当前激活页（或先切换）');
    }

    const format = msg.format === 'jpeg' ? 'jpeg' : 'png';
    const captureOptions = { format };
    let quality = null;
    if (format === 'jpeg' && Number.isFinite(Number(msg.quality))) {
      quality = Math.min(100, Math.max(1, Number(msg.quality)));
      captureOptions.quality = quality;
    }
    const maxHeightPxRaw = Number(msg.maxHeightPx);
    const maxHeightPx = Number.isFinite(maxHeightPxRaw) && maxHeightPxRaw > 0
      ? Math.floor(maxHeightPxRaw)
      : CAPTURE_FULLPAGE_DEFAULT_MAX_HEIGHT;
    const maxPixels = await loadCaptureMaxPixels();

    const measured = await measureFullPage(tabId);
    const startY = Number.isFinite(measured.startY) ? measured.startY : 0;
    restore = { tabId, startY };

    const innerH = Math.max(1, Math.floor(Number(measured.innerH) || 0));
    const scrollH = Math.max(innerH, Math.floor(Number(measured.scrollH) || 0));
    const dpr = Number.isFinite(measured.dpr) && measured.dpr > 0 ? measured.dpr : 1;

    // 上限按「设备像素总高」算：scrollH * dpr 才是真正要分配的画布高度。
    let truncated = false;
    let effectiveScrollH = scrollH;
    if (scrollH * dpr > maxHeightPx) {
      truncated = true;
      effectiveScrollH = Math.max(1, Math.floor(maxHeightPx / dpr));
      if (effectiveScrollH > scrollH) effectiveScrollH = scrollH;
    }

    const neededSlices = Math.max(1, Math.ceil(effectiveScrollH / innerH));
    const totalSlices = Math.min(CAPTURE_FULLPAGE_MAX_SLICES, neededSlices);
    // 切片数被硬上限截断时，最后一屏不可能对齐到 effectiveScrollH 底部，
    // 只能覆盖 totalSlices * innerH，画布必须跟着缩短，否则底部是一条空白。
    if (neededSlices > totalSlices) truncated = true;
    const coveredCssH = Math.min(effectiveScrollH, totalSlices * innerH);
    const canvasHeight = Math.max(1, Math.min(Math.round(coveredCssH * dpr), maxHeightPx));
    // 预算要在「截 40 片之前」就判一次（用 innerW*dpr 估宽）。
    // 否则 4K 长页会先把几十张分片读进内存、最后一步建画布时才 OOM，滚动位置都恢复不了。
    const estWidth = Math.max(1, Math.ceil((Number(measured.innerW) || innerW) * dpr));
    if (estWidth * canvasHeight > maxPixels) {
      throw structuredError(
        'capture_too_large',
        `整页截图超出像素预算：约 ${estWidth}x${canvasHeight} = ${estWidth * canvasHeight} 像素 > 上限 ${maxPixels}`,
        '减小 maxHeightPx / 缩小窗口或页面缩放后重试，或调高 storage 里的 captureMaxPixels（100万..5亿）'
      );
    }

    const offsets = [];
    for (let i = 0; i < totalSlices; i++) {
      // 最后一屏对齐到底部（min(i*innerH, scrollH - innerH)），避免末屏和前一屏大段重叠后
      // 底部多出一片重复内容。effectiveScrollH < innerH 时 clamp 到 0。
      const y = Math.max(0, Math.min(i * innerH, effectiveScrollH - innerH));
      await scrollFullPageTo(tabId, y, CAPTURE_FULLPAGE_SETTLE_MS);
      const dataUrl = await captureVisibleTabThrottled(tab.windowId, captureOptions);
      if (typeof dataUrl !== 'string' || !dataUrl.startsWith('data:')) {
        throw structuredError('capture_empty_slice', 'captureVisibleTab 返回空数据（第 ' + (i + 1) + ' 片）');
      }
      // offset 保持 CSS 像素：真实设备像素比例由拼接阶段按首片反推。
      offsets.push({ offset: y, dataUrl });
    }

    const stitched = await stitchFullPageSlices(offsets, format, quality, {
      innerH,
      coveredCssH,
      maxHeightPx,
      maxPixels
    });
    return {
      ok: true,
      data: {
        data_url: 'data:image/' + format + ';base64,' + stitched.base64,
        format,
        width: stitched.width,
        height: stitched.height,
        slices: offsets.length,
        truncated,
        dpr,
        // 实际用于拼图的比例（可能因页面缩放 ≠ dpr）；排障时看这个而不是 dpr。
        scale: stitched.scale
      }
    };
  } catch (e) {
    return {
      ok: false,
      error_code: (e && e.code) || 'capture_failed',
      error: e && e.message ? e.message : String(e),
      hint: (e && e.hint) || null
    };
  } finally {
    // 不论成功失败都要把页面滚回用户原来的位置：截完图停在页底是最容易被投诉的副作用。
    // 恢复本身失败只记日志，不能覆盖主流程的返回值。
    if (restore) {
      try {
        await chrome.scripting.executeScript({
          target: { tabId: restore.tabId },
          func: (top) => { window.scrollTo(0, top); },
          args: [restore.startY]
        });
      } catch (restoreError) {
        console.warn('[TMWD-CAPTURE] 恢复滚动位置失败:', restoreError && restoreError.message);
      }
    }
  }
}

/// 找出注入到页面里的 chrome-extension:// iframe，反查扩展 id。
/// 用途：CDP 被拒时直接告诉用户「关掉哪个扩展」，而不是让他去猜。
/// 这是诊断辅助，任何失败都返回空列表——不能让排查工具本身变成新的故障点。
/// 失败必须显式降级（degraded + warnings），否则「查不到」会被误读成「没有阻塞扩展」。
/// 不再调用 chrome.management.get 反查名字（权限已移除），只回 id + src。
const BLOCKING_EXTENSION_LIMIT = 10;
async function handleBlockingExtensions(msg) {
  const warnings = [];
  try {
    if (!chrome.scripting?.executeScript) {
      return {
        ok: true,
        degraded: true,
        warnings: ['chrome.scripting.executeScript 不可用（manifest 缺少 "scripting" 权限）'],
        data: { blocking: [] }
      };
    }
    const tabId = Number(msg.tabId);
    if (!Number.isInteger(tabId) || tabId <= 0) {
      return { ok: true, degraded: true, warnings: ['tabId 非法，未执行检测'], data: { blocking: [] } };
    }
    const result = await chrome.scripting.executeScript({
      target: { tabId },
      func: () => [...document.querySelectorAll('iframe')].map((f) => f.src || '')
    });
    const sources = result && result[0] && Array.isArray(result[0].result) ? result[0].result : [];
    const blocking = [];
    const seen = new Set();
    for (const src of sources) {
      if (typeof src !== 'string') continue;
      // 扩展 ID 固定 32 位、字符集 a-p，用它把普通 http iframe 和扩展 iframe 区分开。
      const match = /^chrome-extension:\/\/([a-p]{32})\//i.exec(src);
      if (!match) continue;
      const id = match[1].toLowerCase();
      if (seen.has(id)) continue; // 同一个扩展可能注入多个 iframe，只报一条
      seen.add(id);
      if (blocking.length >= BLOCKING_EXTENSION_LIMIT) break;
      // 不再查询扩展名（需 management 权限）：保留 id，用户仍可在 chrome://extensions 定位。
      blocking.push({ id, name: null, src });
    }
    return { ok: true, warnings, degraded: warnings.length > 0, data: { blocking } };
  } catch (e) {
    const message = (e && e.message) || String(e);
    console.warn('[TMWD-CAPTURE] 检测阻塞扩展失败:', message);
    return { ok: true, degraded: true, warnings: [message], data: { blocking: [] } };
  }
}

async function handleOpenTab(msg) {
  try {
    const url = normalizeOpenUrl(msg.url);
    const active = msg.active !== false;
    if (msg.window === true) {
      // window 模式默认不聚焦，避免新窗口打断用户当前工作区。
      const win = await chrome.windows.create({ url, focused: msg.allowFocus === true });
      const tab = Array.isArray(win.tabs) && win.tabs.length ? win.tabs[0] : null;
      if (tab?.id && msg.keepAlive !== false) await keepTabAlive(tab.id);
      const group = tab?.id ? await groupTabIfRequested(tab.id, msg.groupTitle) : { ok: false, skipped: true, reason: 'window created without tab info' };
      return { ok: true, data: { id: tab?.id, url: tab?.url || url, title: tab?.title || '', active: !!tab?.active, windowId: win.id, window: true, group } };
    }
    const tab = await chrome.tabs.create({ url, active });
    // 受控标签页禁止被 Chrome「内存节省」自动丢弃，否则后台长任务会中途失去上下文。
    if (tab.id && msg.keepAlive !== false) await keepTabAlive(tab.id);
    const group = await groupTabIfRequested(tab.id, msg.groupTitle);
    // 默认创建/激活标签页但不聚焦浏览器窗口，避免打断当前工作区。
    if (active && msg.allowFocus === true && tab.windowId) await chrome.windows.update(tab.windowId, { focused: true });
    return { ok: true, data: { id: tab.id, url: tab.url || url, title: tab.title || '', active: tab.active, windowId: tab.windowId, window: false, group } };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function keepTabAlive(tabId) {
  try {
    await chrome.tabs.update(tabId, { autoDiscardable: false });
  } catch (e) {
    console.log('[TMWD] keepAlive failed:', tabId, e.message);
  }
}

async function groupTabIfRequested(tabId, title) {
  const cleanTitle = String(title || '').trim();
  if (!cleanTitle) return null;
  if (!chrome.tabs?.group || !chrome.tabGroups?.update) {
    return { ok: false, skipped: true, reason: 'tabGroups API unavailable' };
  }
  try {
    const existing = await findTabGroupByTitle(cleanTitle);
    const groupId = await chrome.tabs.group(existing ? { tabIds: tabId, groupId: existing.id } : { tabIds: tabId });
    await chrome.tabGroups.update(groupId, { title: cleanTitle });
    return { ok: true, id: groupId, title: cleanTitle };
  } catch (e) {
    // 分组只是整理标签，不影响打开页面的主流程。
    return { ok: false, skipped: true, reason: e.message };
  }
}

async function findTabGroupByTitle(title) {
  if (!chrome.tabGroups?.query) return null;
  const groups = await chrome.tabGroups.query({});
  return groups.find(group => group.title === title) || null;
}

function normalizeOpenUrl(url) {
  const raw = String(url || '').trim();
  if (!raw) throw new Error('url is required');
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(raw)) return raw;
  return 'https://' + raw;
}

async function handleBatch(msg, sender) {
  const R = [];
  const touchedSessions = new Set();
  // `$N.path` 引用只能**整串精确匹配**时才替换。旧实现是对 JSON 文本做正则替换，
  // 于是 "前$0.url后" 这种普通字符串也会被改写（拼出 "前https://…后"），
  // 而路径取不到时会插入字符串 "undefined"，把参数悄悄换成非法值。
  const REF_RE = /^\$(\d+)\.([\w.]+)$/;
  const resolveRefs = (value) => {
    if (typeof value === 'string') {
      const m = REF_RE.exec(value);
      if (!m) return value;
      let v = R[Number(m[1])];
      for (const k of m[2].split('.')) v = v == null ? undefined : v[k];
      return v === undefined ? null : v;
    }
    if (Array.isArray(value)) return value.map(resolveRefs);
    if (value && typeof value === 'object') {
      const out = {};
      for (const k of Object.keys(value)) out[k] = resolveRefs(value[k]);
      return out;
    }
    return value;
  };
  const resolve$N = (params) => resolveRefs(params || {});
  if (!Array.isArray(msg.commands)) {
    return { ok: false, error: 'commands must be an array', results: [], count: 0, failed: 0 };
  }
  const commands = msg.commands;
  try {
    for (let index = 0; index < commands.length; index += 1) {
      const c = commands[index] || {};
      if (c.tabId === undefined && msg.tabId !== undefined) c.tabId = msg.tabId;
      // 每条命令都带 `index`，并且**单条失败只记在这一条上**：
      // 以前 cdp 抛错会直接跳出整个循环，前面的结果一起丢掉，调用方看不出是哪一步坏了。
      try {
        if (c.cmd === 'cookies') {
          R.push(Object.assign({ index }, await handleCookies(c, sender)));
        } else if (c.cmd === 'tabs') {
          const tabs = await queryScriptableTabs();
          R.push({ index, ok: true, data: tabs.map(t => ({ id: t.id, url: t.url, title: t.title, active: t.active, windowId: t.windowId })) });
        } else if (c.cmd === 'cdp') {
          const tabId = c.tabId || msg.tabId || sender.tab?.id;
          if (!tabId) throw new Error('no tabId');
          if (c.method === 'Page.bringToFront' && c.allowFocus !== true && msg.allowFocus !== true) {
            R.push({ index, ok: true, data: { skipped: true, reason: 'Page.bringToFront requires allowFocus=true' } });
            continue;
          }
          const session = getDebugSession(tabId);
          await ensureDebugAttached(session);
          touchedSessions.add(session);
          const out = await chrome.debugger.sendCommand({ tabId }, c.method, resolve$N(c.params));
          // `data` 是统一后的规范位置；同时把 CDP 结果的顶层字段并进条目，
          // 兼容 daemon 里既有的 `results[N].result.value` / `$N.result.objectId` 读法
          //（daemon 侧不在本次改动范围，不能只留 data 把那些读法打断）。
          R.push(Object.assign({}, out, { index, ok: true, data: out }));
        } else {
          R.push({ index, ok: false, error: 'unknown cmd: ' + c.cmd });
        }
      } catch (e) {
        R.push({ index, ok: false, error: (e && e.message) || String(e) });
      }
    }
    for (const session of touchedSessions) scheduleDebugDetach(session, 'batch-idle');
    return { ok: true, results: R, count: R.length, failed: R.filter((item) => !item.ok).length };
  } catch (e) {
    for (const session of touchedSessions) scheduleDebugDetach(session, 'batch-error');
    return { ok: false, error: e.message, results: R, count: R.length, failed: R.filter((item) => !item.ok).length };
  }
}

async function handleCDP(msg, sender) {
  const tabId = msg.tabId || sender.tab?.id;
  if (!tabId) return { ok: false, error: 'no tabId' };
  if (msg.method === 'Page.bringToFront' && msg.allowFocus !== true) {
    return { ok: true, data: { skipped: true, reason: 'Page.bringToFront requires allowFocus=true' } };
  }
  const session = getDebugSession(tabId);
  try {
    await ensureDebugAttached(session);
    const result = await chrome.debugger.sendCommand({ tabId }, msg.method, msg.params || {});
    scheduleDebugDetach(session, 'cdp-idle');
    return { ok: true, data: result };
  } catch (e) {
    scheduleDebugDetach(session, 'cdp-error');
    return { ok: false, error: e.message };
  }
}
// Filter out chrome:// and other internal tabs that can't be scripted.
const isScriptable = (url, fileSchemeAccess = false) => !!url && (/^https?:/i.test(url) || (fileSchemeAccess && /^file:/i.test(url)));

/// chrome.extension 在少数 Chromium 分支/精简构建里可能缺失，缺了就当没有文件访问权。
async function isAllowedFileSchemeAccess() {
  try {
    return !!(await chrome.extension?.isAllowedFileSchemeAccess?.());
  } catch (_) {
    return false;
  }
}

async function queryScriptableTabs() {
  const fileSchemeAccess = await isAllowedFileSchemeAccess();
  return (await chrome.tabs.query({})).filter(tab => isScriptable(tab.url, fileSchemeAccess));
}

/// 上报给 daemon 的 tab 列表统一走这里。
/// streamlit 内部页不是用户可操作的浏览目标：ext_ready 的初始列表与 tabs_update
/// 必须用同一套过滤，否则 daemon 会先看到它、随后在全量更新里「消失」（等同于被关闭）。
/// 判定只用 URL 上的 `/_stcore/`（Streamlit 自己的内部端点），不看 title：
/// 标题里带 "streamlit" 的普通网页以前会被整条隐藏，用户看到的就是「标签页开着却查不到」。
function filterReportableTabs(tabs) {
  return (tabs || []).filter(t => !/\/_stcore\//i.test(String((t && t.url) || '')));
}

/// 一次取回所有标签组标题：`chrome.tabGroups.query` 不能按 tab 批量查，
/// 逐个 `get` 在几十个标签页时会有明显开销。
async function queryTabGroupTitles() {
  try {
    if (!chrome.tabGroups?.query) return null;
    const groups = await chrome.tabGroups.query({});
    return new Map(groups.map(group => [group.id, group.title || '']));
  } catch (_) {
    return null;
  }
}

function tabGroupTitle(groupTitles, tab) {
  if (!groupTitles || !tab || !tab.groupId || tab.groupId < 0) return null;
  const title = groupTitles.get(tab.groupId);
  return title ? title : null;
}

async function injectContentScriptsIntoExistingTabs() {
  // manifest 已对新建页面注入 content_scripts；这里只需给「安装扩展前就打开着的标签页」补一次。
  // 之前每次 Service Worker 唤醒都全量重注入，多标签用户会有可感知的卡顿。
  try {
    const { injectedVersion } = await chrome.storage.local.get({ injectedVersion: null });
    if (injectedVersion === extensionVersion) return;
  } catch (_) {}
  const tabs = await queryScriptableTabs();
  for (const tab of tabs) {
    try {
      await chrome.scripting.executeScript({
        target: { tabId: tab.id, allFrames: true },
        // config.js 已删除（TID 全项目无消费者），只注入 content.js。
        files: ['content.js']
      });
    } catch (e) {
      console.log('[TMWD-WS] Inject content script failed:', tab.id, e.message);
    }
  }
  try {
    await chrome.storage.local.set({ injectedVersion: extensionVersion });
  } catch (_) {}
}

function buildDialogSuppressionScript(enabled) {
  if (!enabled) {
    return `(() => {
      const state = window.__TMWD_DIALOG_SUPPRESSION__;
      if (!state) return;
      state.count = Math.max(0, Number(state.count || 1) - 1);
      if (state.count > 0) return;
      try {
        window.alert = state.alert;
        window.confirm = state.confirm;
        window.prompt = state.prompt;
      } finally {
        delete window.__TMWD_DIALOG_SUPPRESSION__;
      }
    })()`;
  }
  return `(() => {
    const existing = window.__TMWD_DIALOG_SUPPRESSION__;
    if (existing) {
      existing.count = Number(existing.count || 1) + 1;
      return;
    }
    const state = {
      count: 1,
      alert: window.alert,
      confirm: window.confirm,
      prompt: window.prompt
    };
    window.__TMWD_DIALOG_SUPPRESSION__ = state;
    const toast = (type, msg) => {
      try { console.log('[TMWD] ' + type + ' suppressed during CLI command:', msg); } catch (_) {}
      try {
        const d = document.createElement('div');
        d.textContent = '[' + type + '] ' + msg;
        Object.assign(d.style, {
          position:'fixed', top:'12px', right:'12px', zIndex:'2147483647',
          background:'#222', color:'#fff', padding:'10px 18px', borderRadius:'8px',
          fontSize:'14px', maxWidth:'420px', wordBreak:'break-all',
          boxShadow:'0 4px 16px rgba(0,0,0,.3)', opacity:'1',
          transition:'opacity .5s', pointerEvents:'none'
        });
        (document.body || document.documentElement).appendChild(d);
        setTimeout(() => { d.style.opacity = '0'; }, 3000);
        setTimeout(() => { d.remove(); }, 3600);
      } catch (_) {}
    };
    window.alert = function(msg) { toast('alert', msg); };
    window.confirm = function(msg) { toast('confirm', msg); return true; };
    window.prompt = function(msg, def) { toast('prompt', msg); return def || null; };
  })()`;
}

async function setDialogSuppressionByScripting(tabId, enabled) {
  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      world: 'MAIN',
      func: async (script) => await eval(script),
      args: [buildDialogSuppressionScript(enabled)]
    });
    return true;
  } catch (e) {
    console.log('[TMWD-WS] dialog suppression scripting failed:', e.message);
    return false;
  }
}

async function setDialogSuppressionByCdp(tabId, enabled) {
  try {
    await chrome.debugger.sendCommand({ tabId }, 'Runtime.evaluate', {
      expression: buildDialogSuppressionScript(enabled),
      awaitPromise: true,
      returnByValue: true
    });
    return true;
  } catch (e) {
    console.log('[TMWD-WS] dialog suppression CDP failed:', e.message);
    return false;
  }
}

// --- Shared page/CDP script builder core ---
function buildExecScript(code, errorHandler) {
  return `(async () => {
    // 序列化失败必须能传到外层（见下面的 return），不能伪装成正常的字符串结果。
    // 用 Symbol 当哨兵，避免与用户真实返回的 {message:...} 对象混淆。
    const __abcSerializeError = Symbol('abcSerializeError');
    // DOM 节点 / Window / Function / Error 不是 JSON 安全的返回值。
    // 以前依赖 JSON.stringify 的隐式行为（元素变 outerHTML、函数直接炸掉），
    // 智能体拿不到 tag/id/class/rect。这里统一换成带 __type 的结构化摘要。
    function __abcDescribe(value) {
      if (value === null || value === undefined) return value;
      if (typeof value === 'function') return { __type: 'function', name: String(value.name || '') };
      if (typeof value !== 'object') return value;
      try { if (value instanceof Error) return { __type: 'error', name: String(value.name || 'Error'), message: String(value.message || '').slice(0, 500) }; } catch (_) {}
      try { if (value.window === value && value.document) return { __type: 'window', url: String((value.location && value.location.href) || ''), title: String((value.document && value.document.title) || '') }; } catch (_) {}
      if (value.nodeType === 1) {
        let rect = { x: 0, y: 0, width: 0, height: 0 };
        try { const r = value.getBoundingClientRect(); rect = { x: r.x, y: r.y, width: r.width, height: r.height }; } catch (_) {}
        return {
          __type: 'element',
          tag: String(value.tagName || '').toLowerCase(),
          id: String(value.id || ''),
          class: typeof value.className === 'string' ? value.className : String((value.getAttribute && value.getAttribute('class')) || ''),
          rect,
          text: String(value.innerText || value.textContent || '').trim().slice(0, 200)
        };
      }
      if (value.nodeType === 3) return { __type: 'text', text: String(value.textContent || '').slice(0, 200) };
      return value;
    }
    function smartProcessResult(result) {
      if (result === null || result === undefined) return result;
      if (typeof result !== 'object') return __abcDescribe(result);
      try { if (result.window === result && result.document) return __abcDescribe(result); } catch(_){}
      if (typeof jQuery !== 'undefined' && result instanceof jQuery) {
        const elements = []; for (let i = 0; i < result.length; i++) { if (result[i] && result[i].nodeType === 1) elements.push(result[i].outerHTML); } return elements;
      }
      if (result instanceof NodeList || result instanceof HTMLCollection) {
        const elements = []; for (let i = 0; i < result.length; i++) { if (result[i] && result[i].nodeType === 1) elements.push(result[i].outerHTML); } return elements;
      }
      if (result.nodeType === 1 || result.nodeType === 3 || result instanceof Error) return __abcDescribe(result);
      if (!Array.isArray(result) && typeof result === 'object' && 'length' in result && typeof result.length === 'number') {
        const firstElement = result[0];
        if (firstElement && firstElement.nodeType === 1) {
          const elements = []; const length = Math.min(result.length, 100);
          for (let i = 0; i < length; i++) { const elem = result[i]; if (elem && elem.nodeType === 1) elements.push(elem.outerHTML); } return elements;
        }
      }
      try { return JSON.parse(JSON.stringify(result, function(key, value) { if (typeof value === 'object' && value !== null) { if (value.nodeType === 1 || value.nodeType === 3 || value instanceof Error) return __abcDescribe(value); if (value === window || value === document) return '[Object]'; try { if (value.window === value && value.document) return __abcDescribe(value); } catch(_){} } return value; })); } catch (e) { return { [__abcSerializeError]: true, message: String((e && e.message) || e) }; }
    }
    // executed 必须在 catch 里可读 —— 决定「脚本抛错」还是「注入本身失败」。
    // 以前声明在 try 块内，catch 看不到。
    let __abcEvalStarted = false;
    try {
      const jsCode = ${JSON.stringify(code)}.trim();
      const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
      function _air(c) { const ls = c.split(/\\r?\\n/); let i = ls.length - 1; while (i >= 0 && !ls[i].trim()) i--; if (i < 0) return c; const t = ls[i].trim(); if (/^(return |return;|return$|let |const |var |if |if\\(|for |for\\(|while |while\\(|switch|try |throw |class |function |async |import |export |\\/\\/|})/.test(t)) return c; ls[i] = ls[i].match(/^(\\s*)/)[1] + 'return ' + t; return ls.join('\\n'); }
      // 先按原样求值：daemon 会把用户脚本包成自执行的 async 函数表达式，
      // 这种写法只有 eval 能拿到 IIFE 的返回值；一旦按「函数体」编译，
      // 末尾没有 return，整个 exec 就会永远得到 null。
      // 只有当 eval 因语句级 return / 顶层 await 报 SyntaxError 时，才退回函数体执行。
      let r;
      // 只有「编译期失败」才允许换通道。用探针区分：eval 的代码先执行
      // __abcEvalStarted = true，用户脚本自己 throw new SyntaxError('await ...')
      // 时探针已置位，必须原样上抛，绝不能再跑一次（副作用会翻倍）。
      try {
        r = eval('__abcEvalStarted = true;\\n' + jsCode);
        if (r instanceof Promise) r = await r;
      } catch (e) {
        if (e instanceof SyntaxError && !__abcEvalStarted) {
          // 编译期 SyntaxError（语句级 return / 顶层 await）⇒ 一行都没执行，
          // 换函数体通道是安全的。有语句级 return 时按原样当函数体；否则补一个
          // return，不然 await 的结果会被丢掉（返回 undefined）。
          //
          // **只有构造期**失败才允许换写法重编。旧实现把「构造 + 调用」写在
          // 同一个 try 里，用户脚本执行期 throw 也会命中 catch → 整段重跑一遍，
          // 点击/写入/insertAdjacentHTML 这类副作用直接翻倍。执行期异常必须原样上抛。
          const hasStatementReturn = /^\s*return\b/m.test(jsCode);
          const primaryBody = hasStatementReturn ? jsCode : _air(jsCode);
          let runner;
          try {
            runner = new AsyncFunction(primaryBody);
          } catch (compileError) {
            const fallbackBody = _air(jsCode);
            // 两种写法完全等价时不再重编一份同样的东西（结果只会再抛一次）。
            if (fallbackBody === primaryBody) throw compileError;
            runner = new AsyncFunction(fallbackBody);
          }
          // 编译成功才置位：执行期异常属于「脚本已执行」，SyntaxError 属于「没执行」。
          __abcEvalStarted = true;
          r = await runner();
        } else {
          throw e;
        }
      }
      const processed = smartProcessResult(r);
      // 序列化失败必须报错：以前返回字符串 '[无法序列化: ...]' + 外层 ok:true，
      // daemon 会把它当成「成功的返回值」，把错误文案写进结果里。
      if (processed && typeof processed === 'object' && processed[__abcSerializeError]) {
        return { ok: false, error: '结果无法序列化: ' + processed.message };
      }
      return { ok: true, data: processed };
    } catch (e) {
      ${errorHandler}
    }
  })()`;
}

function buildPageScript(code) {
  return buildExecScript(code, `
      const errMsg = e.message || String(e);
      return { ok: false, error: { name: e.name || 'Error', message: errMsg, stack: e.stack || '' },
        csp: errMsg.includes('Refused to evaluate') || errMsg.includes('unsafe-eval') || errMsg.includes('Content Security Policy'),
        // true = 用户脚本已经开始执行后抛错（副作用可能已发生）；
        // false = 编译期 SyntaxError / CSP 拦截，一行都没执行。
        executed: __abcEvalStarted === true };
  `);
}

function buildCdpScript(code) {
  return buildExecScript(code, `
      return { ok: false, error: { name: e.name || 'Error', message: e.message || String(e), stack: e.stack || '' },
        executed: __abcEvalStarted === true };
  `);
}

// --- WebSocket Client for TMWebDriver ---
let ws = null;

// 每次 Service Worker 求值（冷启动 / alarm 唤醒）+1，随 ext_ready 上报。
// 【事实基线】daemon 侧（src/server/ws.rs）在原始文本上先取一次 restartSeq
// （WsIncoming 没声明该字段，反序列化会丢弃），交给 note_extension_restart 记录；
// 该值**变化**意味着扩展换了一代 SW，daemon 把持有的 net_hooks 标记为待重建。
// 它不会据此重发 networkStart / consoleStart —— 三态恢复由扩展自己完成
// （ensureDebugSessionsRestored：从 storage.session 读回拦截规则 + network/console，
// 对仍 attached 的 tab 重发 Fetch.enable / Network.enable / Runtime.enable）。
// 字段本身只用于「变化检测 + 排障观测」，不要指望它携带具体状态。
// 注意：manifest.json 里 minimum_chrome_version = 120 的依据见文件顶部注释块。
const RESTART_SEQ_KEY = 'extRestartSeq';
let restartSeq = 0;
let restartSeqPromise = null;

function ensureRestartSeq() {
  if (!restartSeqPromise) {
    restartSeqPromise = (async () => {
      try {
        const data = await chrome.storage.local.get({ [RESTART_SEQ_KEY]: 0 });
        const next = (Number(data[RESTART_SEQ_KEY]) || 0) + 1;
        await chrome.storage.local.set({ [RESTART_SEQ_KEY]: next });
        restartSeq = next;
      } catch (e) {
        // 读失败就上报 0：daemon 即使不消费也能从日志看出这次启动没读到序号。
        console.warn('[TMWD-WS] restartSeq 自增失败:', e && e.message);
        restartSeq = 0;
      }
      return restartSeq;
    })();
  }
  return restartSeqPromise;
}

// 断线→重连窗口内的待发消息。
// 旧实现非 OPEN 直接返回 false，而所有调用方都不看返回值：断线期间产生的 tabs_update
// 就永久丢掉了，daemon 的标签列表会一直停在旧状态。这里改为入队、上限 200 条
// （超过丢最旧的，只告警一次），新连接 onopen 之后按序 flush。
const WS_SEND_QUEUE_LIMIT = 200;
let wsSendQueue = [];
let wsSendQueueWarned = false;

function enqueueWsSend(text, kind) {
  // 命令响应类消息（ack/result/error）**不入队**。队列是为「状态类推送」设计的，
  // 断线期间攒下的响应在重连后补发只会喂给 daemon 一个早已超时、甚至已被新命令复用的 id
  // —— 轻则被当成迟到响应丢弃，重则错配到另一条命令上。
  if (kind === 'result' || kind === 'error' || kind === 'ack') {
    console.log('[TMWD-WS] 丢弃断线期间的命令结果', kind);
    return;
  }
  // ext_ready 不入队：每次 onopen 都会重发一份带最新 tabs/restartSeq 的握手，
  // 补发一份旧握手只会让 daemon 把「过期快照」当成新连接的首批标签。
  if (kind === 'ext_ready') return;
  // 同类状态推送只保留最新一条：断线期间标签页反复变化时，堆 200 份旧 tab 快照
  // 既浪费又会让 daemon 先收到一串过期状态（真正的最新状态由 onopen 后的补发兜底）。
  if (kind === 'tabs_update') {
    wsSendQueue = wsSendQueue.filter((item) => item.kind !== 'tabs_update');
  }
  wsSendQueue.push({ text, kind });
  while (wsSendQueue.length > WS_SEND_QUEUE_LIMIT) {
    wsSendQueue.shift();
    if (!wsSendQueueWarned) {
      wsSendQueueWarned = true;
      console.warn('[TMWD-WS] 待发队列超过上限', WS_SEND_QUEUE_LIMIT, '条，已丢弃最旧的消息');
    }
  }
}

/// 新连接建立后按序补发；中途断开发送失败时，未发出的按原序留在队列里等下一次。
function flushWsSendQueue(socket) {
  if (!socket || socket.readyState !== WebSocket.OPEN || !wsSendQueue.length) return 0;
  const pending = wsSendQueue;
  wsSendQueue = [];
  wsSendQueueWarned = false;
  let sent = 0;
  for (const item of pending) {
    if (socket.readyState !== WebSocket.OPEN) {
      wsSendQueue = pending.slice(sent).concat(wsSendQueue);
      break;
    }
    try {
      socket.send(item.text);
      sent += 1;
    } catch (e) {
      console.log('[TMWD-WS] flush failed:', e.message);
      wsSendQueue = pending.slice(sent).concat(wsSendQueue);
      break;
    }
  }
  return sent;
}

/**
 * 安全发送：只向指定 socket 写；socket 不可用时入待发队列，不再静默丢弃。
 *
 * 之前所有发送都读写全局 `ws`：daemon 空闲退出 / 重连时 `onclose` 会把它置 null，
 * 正在执行的命令在 await 之后 `ws.send(...)` 就会抛
 * "Cannot read properties of null (reading 'send')"，命令也永远收不到回复。
 */
function socketSend(socket, payload, kindHint) {
  let text;
  try {
    text = typeof payload === 'string' ? payload : JSON.stringify(payload);
  } catch (e) {
    console.log('[TMWD-WS] payload 无法序列化:', e && e.message);
    return false;
  }
  // kind 决定断线时能不能入队：对象消息默认用自身 type，
  // 结果类消息的调用点显式传 'result'/'error'，避免依赖 payload 形状。
  const kind = kindHint || (payload && typeof payload === 'object' ? payload.type : null);
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    enqueueWsSend(text, kind);
    return false;
  }
  try {
    socket.send(text);
    return true;
  } catch (e) {
    console.log('[TMWD-WS] send failed:', e.message);
    enqueueWsSend(text, kind);
    return false;
  }
}

function sendWs(payload) {
  return socketSend(ws, payload);
}

// 应用层心跳（契约基线）：扩展每 30s 发 {"type":"ping"}，daemon 回 {"type":"pong"}；
// daemon 对「90s 内没有任何入站帧」的连接主动断开，所以浏览器空闲时必须自己保活。
// 30s 一次，与 90s 阈值留足两拍余量。ping/pong 都要记录时间：只发 ping 不知道对端
// 是否真的活着（半开连接 send 不报错），只有 pong 回来才算一次有效的 RTT 采样。
// 连接换代时旧 timer 必须清掉，否则旧 socket 上的 send 会抛错刷日志。
const WS_HEARTBEAT_MS = 30000;
function startWsHeartbeat(socket) {
  if (!socket || typeof socket.send !== 'function') return;
  if (socket.__atbHeartbeat) {
    try { clearInterval(socket.__atbHeartbeat); } catch (_) {}
    socket.__atbHeartbeat = null;
  }
  socket.__atbHeartbeat = setInterval(() => {
    if (ws !== socket || socket.readyState !== WebSocket.OPEN) {
      try { clearInterval(socket.__atbHeartbeat); } catch (_) {}
      socket.__atbHeartbeat = null;
      return;
    }
    try {
      lastPingAt = Date.now();
      socket.send(JSON.stringify({ type: 'ping' }));
    } catch (_) {
      // 发送失败说明连接已不可用，onclose 会接手重连。
    }
  }, WS_HEARTBEAT_MS);
}

// setInterval 的心跳有个结构性漏洞：MV3 的 Service Worker 空闲 ~30s 就会被回收，
// interval 跟着一起没了，而 socket 对象还挂在 daemon 那边「看起来活着」。
// chrome.alarms 是唯一能在 SW 冷启动时把它唤醒的机制，所以再加一层兜底 ping。
// 0.5 分钟是 Chrome 120+ 的最小粒度（更早版本会被 clamp 到 1 分钟），只做兜底不需要更密。
const WS_HEARTBEAT_ALARM = 'tmwd-ws-heartbeat';
const WS_HEARTBEAT_ALARM_PERIOD_MINUTES = 0.5;

/// 只在建连成功后 create 一次。同名 create 会覆盖已有 alarm（无害），
/// 但没必要每次 SW 启动都写一遍 storage 侧的调度状态。
function ensureWsHeartbeatAlarm() {
  try {
    chrome.alarms?.create?.(WS_HEARTBEAT_ALARM, { periodInMinutes: WS_HEARTBEAT_ALARM_PERIOD_MINUTES });
  } catch (e) {
    console.warn('[TMWD-WS] 心跳 alarm 创建失败:', e && e.message);
  }
}

/// 断线 / 主动换代时必须把常驻心跳 alarm 清掉，否则连通性早已消失，
/// 它仍每 30s 把 SW 唤醒一次空转（只做一次 readyState 判断就返回）。
async function clearWsHeartbeatAlarm() {
  try {
    await chrome.alarms?.clear?.(WS_HEARTBEAT_ALARM);
  } catch (e) {
    console.warn('[TMWD-WS] 心跳 alarm 清理失败:', e && e.message);
  }
}

// --- 自更新检测 ---
// 扩展是装载目录里的文件副本：磁盘改了 Chrome 仍执行旧 Service Worker，
// 表现就是「代码明明修好了，控制台报错照旧」。这里让扩展自己盯住 background.js
// 的内容哈希，发现变化就在空闲时自我重载，不用每次手动点「重新加载」。
const SELF_CHECK_ALARM = 'tmwd-self-check';
const SELF_BUILD_KEY = 'appliedBuildHash';
let commandInFlight = 0;

async function computeOwnBuildHash() {
  // 优先读部署脚本写下的整包指纹（.atb-build.json）：它覆盖 manifest、popup、
  // options、ui/*.css 等**所有**文件。只哈希 background.js 时，改界面文件浏览器
  // 永远不会重载，用户看到的就是「改了没生效」（2026-11-05 连续踩了三次）。
  try {
    const stampUrl = `${chrome.runtime.getURL('.atb-build.json')}?ts=${Date.now()}`;
    const stampRes = await fetch(stampUrl, { cache: 'no-store' });
    if (stampRes.ok) {
      const stamp = await stampRes.json();
      if (stamp && typeof stamp.digest === 'string' && stamp.digest) {
        return {
          hash: stamp.digest,
          length: Number(stamp.files) || 0,
          version: typeof stamp.version === 'string' ? stamp.version : '',
          builtAt: typeof stamp.builtAt === 'string' ? stamp.builtAt : '',
        };
      }
    }
  } catch (_) {
    // 老部署目录没有指纹文件：退回到 background.js 内容哈希，行为与旧版一致。
  }
  const url = `${chrome.runtime.getURL('background.js')}?ts=${Date.now()}`;
  const res = await fetch(url, { cache: 'no-store' });
  if (!res.ok) return null;
  const text = await res.text();
  return { hash: await sha256Hex(text), length: text.length, version: '', builtAt: '' };
}

async function sha256Hex(text) {
  const buf = new TextEncoder().encode(text);
  const digest = await crypto.subtle.digest('SHA-256', buf);
  return Array.from(new Uint8Array(digest))
    .map(byte => byte.toString(16).padStart(2, '0'))
    .join('');
}

async function handleSelfCheck() {
  const state = await chrome.storage.local.get({ [SELF_BUILD_KEY]: null, selfCheckAt: 0 });
  const alarm = await chrome.alarms.get(SELF_CHECK_ALARM).catch(() => null);
  const warnings = [];
  const out = {
    storedHash: state[SELF_BUILD_KEY],
    lastCheckAt: state.selfCheckAt || 0,
    alarmScheduled: !!alarm,
    alarmPeriodMinutes: alarm?.periodInMinutes ?? null,
    commandInFlight,
    lastCommandAt,
    manifestVersion: chrome.runtime.getManifest().version,
  };
  try {
    const fetched = await computeOwnBuildHash();
    out.fetchedHash = fetched?.hash ?? null;
    out.fetchedLength = fetched?.length ?? null;
    // 指纹文件里带版本与构建时间：能直接看出「跑的是哪一版、什么时候部署的」。
    out.fetchedVersion = fetched?.version || null;
    out.fetchedBuiltAt = fetched?.builtAt || null;
    out.changed = !!state[SELF_BUILD_KEY] && state[SELF_BUILD_KEY] !== out.fetchedHash;
  } catch (e) {
    // 自检失败不能伪装成「一切正常」——显式降级并带上原因。
    out.fetchError = (e && e.message) || String(e);
    warnings.push('buildHash: ' + out.fetchError);
  }
  out.warnings = warnings;
  out.degraded = warnings.length > 0;
  return { ok: true, data: out };
}

/// 「现在能不能重载」——自更新与遗留 self-reload alarm 共用。
/// MV3 里 chrome.runtime.reload() 会立刻换掉整个 Service Worker：在途的 pageOp /
/// netIntercept / CDP 命令全被截断，daemon 侧只看到超时，重试还可能重复点击。
/// 忙的时候每 5s 再来一次，最多 3 次；3 次都忙就放弃这一轮，交给下一分钟的自检
/// alarm（不无限重试，避免页面一直在跑长轮询时永远重载不掉）。
const RELOAD_RETRY_DELAY_MS = 5000;
const RELOAD_MAX_RETRIES = 3;
let reloadRetries = 0;

function reloadBusy() {
  return commandInFlight > 0 || (lastCommandAt > 0 && Date.now() - lastCommandAt < 5000);
}

/// `beforeReload` 在真正 reload 前一刻执行（用来写「已应用版本」基线）：
/// 只有在确实要重载时才写，延后重试的每一轮也会带上同一个回调。
function reloadWhenIdle(reason, beforeReload) {
  if (reloadBusy()) {
    if (reloadRetries < RELOAD_MAX_RETRIES) {
      reloadRetries += 1;
      console.log('[TMWD] ' + reason + '：仍有命令在途，' + (RELOAD_RETRY_DELAY_MS / 1000) +
        's 后重试（' + reloadRetries + '/' + RELOAD_MAX_RETRIES + '）');
      setTimeout(() => { reloadWhenIdle(reason, beforeReload); }, RELOAD_RETRY_DELAY_MS);
    } else {
      console.warn('[TMWD] ' + reason + '：重试 ' + RELOAD_MAX_RETRIES + ' 次仍在忙，放弃本轮重载');
      reloadRetries = 0;
    }
    return false;
  }
  reloadRetries = 0;
  if (typeof beforeReload === 'function') {
    Promise.resolve().then(beforeReload).catch(() => {}).then(() => { chrome.runtime.reload(); });
  } else {
    chrome.runtime.reload();
  }
  return true;
}

async function checkSelfUpdate({ force = false } = {}) {
  // 命令执行中不查、更不重载：重载会掐断正在跑的操作。
  // 旧实现直接 return 等下一轮 60s 的自检 —— 用户刚改完文件时要等一分钟才生效，
  // 很像「改了没反应」。这里延后 5s 把**检查**本身重排一次（最多 3 次）；
  // 注意重排的是检查，不是无条件的 reload —— 哈希没变就不该重载。
  if (reloadBusy()) {
    if (reloadRetries < RELOAD_MAX_RETRIES) {
      reloadRetries += 1;
      setTimeout(() => { checkSelfUpdate({ force: true }).catch(() => {}); }, RELOAD_RETRY_DELAY_MS);
    }
    return;
  }
  reloadRetries = 0;
  const now = Date.now();
  const state = await chrome.storage.local.get({ [SELF_BUILD_KEY]: null, selfCheckAt: 0 });
  if (!force && now - (state.selfCheckAt || 0) < 55000) return;
  await chrome.storage.local.set({ selfCheckAt: now });
  let hash = null;
  try {
    const fetched = await computeOwnBuildHash();
    hash = fetched?.hash ?? null;
  } catch (e) {
    // 自更新检查失败以前直接 return，用户完全看不到「这轮没查成」。
    console.warn('[TMWD] 自更新检查失败（本轮跳过）:', (e && e.message) || e);
    return;
  }
  if (!hash) return;
  if (!state[SELF_BUILD_KEY]) {
    // 手动重载后的第一轮只记录基线，不触发重载，避免来回抖动。
    await chrome.storage.local.set({ [SELF_BUILD_KEY]: hash });
    return;
  }
  if (state[SELF_BUILD_KEY] === hash) return;
  // 复核 in-flight：算哈希是异步的（fetch + sha256），这段窗口里完全可能
  // 进来一条 pageOp。必须再判一次，而且要在**写基线之前**判 —— 先把新哈希写进
  // storage 再延后重载的话，下一轮会认为「哈希没变」，这次更新就永久丢了。
  if (reloadBusy()) {
    console.log('[TMWD] 检测到新版本，但仍有命令在途，延后重载');
    if (reloadRetries < RELOAD_MAX_RETRIES) {
      reloadRetries += 1;
      setTimeout(() => { checkSelfUpdate({ force: true }).catch(() => {}); }, RELOAD_RETRY_DELAY_MS);
    }
    return;
  }
  console.log('[TMWD] background.js 已更新，自动重载扩展');
  // 基线只在**真正要重载**时写入：延后期间若又变忙，下一轮仍会判定「版本已变」，
  // 不会因为提前落盘而把这版更新永久漏掉。
  reloadWhenIdle('self-update', async () => {
    await chrome.storage.local.set({ [SELF_BUILD_KEY]: hash });
  });
}


function normalizePort(port) {
  const value = Number(port);
  if (!Number.isInteger(value) || value < 1 || value > 65535) {
    throw new Error('端口必须是 1-65535');
  }
  if (value === CLI_API_PORT) {
    throw new Error('18767 是 atb API 端口，请换一个插件端口');
  }
  return value;
}

async function loadWsPort() {
  const data = await chrome.storage.local.get({ wsPort: DEFAULT_WS_PORT });
  wsPort = normalizePort(data.wsPort);
  return wsPort;
}

async function saveWsPort(port) {
  wsPort = normalizePort(port);
  await chrome.storage.local.set({ wsPort });
}

async function loadWsToken() {
  const data = await chrome.storage.local.get({ wsToken: '' });
  // daemon 侧 `token_value()` 会 trim，这里保持同一口径：
  // 否则「文件末尾多一个换行」这种最常见的手抄错误会变成 401，且完全看不出原因。
  wsToken = String(data.wsToken || '').trim();
  return wsToken;
}

/// 信任档位：默认 compat = 历史行为，不做任何能力限制。
async function loadWsTrustMode() {
  const data = await chrome.storage.local.get({ wsTrustMode: 'compat' });
  const raw = String(data.wsTrustMode || 'compat').trim().toLowerCase();
  wsTrustMode = WS_TRUST_MODES.has(raw) ? raw : 'compat';
  return wsTrustMode;
}

function getWsUrl() {
  return `ws://127.0.0.1:${wsPort}`;
}

/// 真正用于握手的 URL。
/// **只有配置了 token 才追加查询串**；没有 token 时返回值与 getWsUrl() 完全一致，
/// 这样日志、status、既有测试、popup 展示都不受影响（默认行为零变化）。
function getWsConnectUrl() {
  return wsToken ? `${getWsUrl()}/?token=${encodeURIComponent(wsToken)}` : getWsUrl();
}

/// 脱敏：token 绝不能进 console / status / popup —— 日志经常被贴进 issue。
function maskWsUrl(url) {
  return String(url || '').replace(/([?&]token=)[^&]*/gi, '$1***');
}

const WS_LAST_ERROR_KEY = 'lastConnectError';

/// 记录「最后一次连接失败」，供 popup 显示。
/// 同一 (kind, message, hadToken) 连续出现时只更新计数与时间，不重复落盘
/// —— daemon 没起来时 probe 是 ~1s 一次，否则会把 storage 写爆。
async function recordWsConnectError(kind, message, extra) {
  const entry = Object.assign({
    at: Date.now(),
    kind,
    message: String(message || ''),
    hadToken: !!wsToken,
    url: maskWsUrl(getWsConnectUrl())
  }, extra || {});
  const fingerprint = `${entry.kind}|${entry.message}|${entry.hadToken}`;
  const repeated = lastConnectError && lastConnectError.fingerprint === fingerprint;
  entry.fingerprint = fingerprint;
  entry.repeats = repeated ? (Number(lastConnectError.repeats) || 0) + 1 : 0;
  lastConnectError = entry;
  wsConnectErrorHydrated = true;
  if (repeated) return;
  try {
    await chrome.storage.local.set({ [WS_LAST_ERROR_KEY]: entry });
  } catch (_) {}
}

/// 连上之后清掉失败记录，避免 popup 里挂着一条已经过期的错误。
async function clearWsConnectError() {
  // 内存为空但还没从 storage 恢复过 → storage 里可能有旧记录，照样要清。
  const maybeStored = lastConnectError || !wsConnectErrorHydrated;
  lastConnectError = null;
  wsConnectErrorHydrated = true;
  if (!maybeStored) return;
  try {
    await chrome.storage.local.set({ [WS_LAST_ERROR_KEY]: null });
  } catch (_) {}
}

/// SW 重启后内存里的 lastConnectError 是空的，从 storage 恢复一次（只读一次）。
async function hydrateWsConnectError() {
  if (wsConnectErrorHydrated) return lastConnectError;
  wsConnectErrorHydrated = true;
  try {
    const data = await chrome.storage.local.get({ [WS_LAST_ERROR_KEY]: null });
    if (!lastConnectError && data[WS_LAST_ERROR_KEY]) lastConnectError = data[WS_LAST_ERROR_KEY];
  } catch (_) {}
  return lastConnectError;
}

/// 对外（status / popup）只暴露可读字段，内部用来去重的 fingerprint 不外泄。
function publicWsConnectError(entry) {
  if (!entry) return null;
  const out = Object.assign({}, entry);
  delete out.fingerprint;
  return out;
}

/// 把 WebSocket close 事件翻译成一句人话，重点区分「token 不匹配」这种静默失败。
function describeWsClose(event) {
  const code = event && event.code;
  const reason = String((event && event.reason) || '').trim();
  if (reason) return reason;
  if (code === 1006) {
    return wsToken
      ? '握手被拒绝（HTTP 401/403 的典型表现）：token 可能与 ws-trust.json 不一致，或该扩展 ID 不在 extension_ids 白名单；也可能是 daemon 未启动'
      : '握手被拒绝或连接被异常关闭（daemon 未启动 / 扩展 ID 未被信任）；若 daemon 已启用 token，请在扩展里填写 token';
  }
  if (code === 1000) return '连接被正常关闭（daemon 退出或重启）';
  return code ? `连接关闭（code ${code}）` : '连接关闭';
}

/// 上一次真正开始尝试建连的时刻（connectWS 里写）。probe 用它避免重复排期。
let lastConnectAttemptAt = 0;
/// 单次握手最长等多久。超过就当这条 socket 已经废了（见 keepalive 的三态处理）。
const WS_CONNECT_TIMEOUT_MS = 15000;
/// alarms 的硬下限：Chrome 120+ 会把 delayInMinutes/periodInMinutes 抬到 ≥30s，
/// 传 0.017（~1s）也只会变成 30s。所以 probe 的真实间隔是 30~60s，不是注释里曾宣称的 1s。
const ALARM_MIN_MINUTES = 0.5;
/// 心跳的「静默上限」：保活周期 × 2.5。超过这么久**没有任何入站消息**
/// 就判定这条 socket 已经半开（对端进程已死但 TCP 没发 RST：send 不报错、pong 永远
/// 不来），主动丢弃重连。旧实现只 ping 不跟踪 pong，半开连接会一直挂着 —— 扩展
/// 以为自己连着、daemon 侧却早已没有这条连接，命令全部超时。
const WS_KEEPALIVE_STALE_MS = Math.round(ALARM_MIN_MINUTES * 60 * 1000 * 2.5);
/// 最近一次收到**入站消息**的时刻。daemon 对应用层 ping 会回 pong（src/server/ws.rs
/// 的 is_app_ping 分支），所以空闲链路也能靠 pong 刷新；其它 result / 通知同样算活。
let lastPongAt = 0;
// 最近一次**发出** ping 的时刻，与 lastPongAt 配对算 RTT（：
/// 旧实现只 ping 不跟踪 pong，静默判定实际退化成「有没有业务消息」）。
let lastPingAt = 0;
/// 最近一次 ping→pong 的往返毫秒数（单次采样，够排障用；不做滑窗统计）。
let lastPongRttMs = null;
/// 「静默但还能连上 daemon」只提醒一次，别每 30s 刷一条日志。
let wsIdleWarned = false;

/// onclose / daemon 未启动之后的**快速重连**。
///
/// 为什么要有它：扩展的重连只能靠 chrome.alarms，而 alarms 的硬下限是 30s
/// （Chrome 120+ 会把更小的值抬到 ≥30s）。daemon 一旦重启（升级二进制、手动
/// restart、或上一次空闲退出），扩展最长要等一分钟才重新出现，CLI 侧的表现就是
/// 「第一条命令特别慢」甚至「没有可用的浏览器标签页」。
///
/// 【事实修正 】MV3 **不保证**「有未执行的 setTimeout 时 SW 不被回收」：短 timer 只是
/// 尽力而为的快路径，可能随 SW 回收一起丢失；alarms 才是跨回收失效的保底。
/// 这里用一个 ~20 秒的退避窗口覆盖「daemon 启动那几秒」，丢了也不影响最终重连。
let fastRetryTimer = null;
let fastRetryIndex = 0;
const FAST_RETRY_DELAYS = [300, 600, 1200, 2500, 5000, 5000, 5000];

function scheduleFastReconnect() {
  if (ws && ws.readyState <= 1) return;
  if (fastRetryTimer) return;
  if (fastRetryIndex >= FAST_RETRY_DELAYS.length) return;
  const delay = FAST_RETRY_DELAYS[fastRetryIndex];
  fastRetryIndex += 1;
  fastRetryTimer = setTimeout(() => {
    fastRetryTimer = null;
    if (ws && ws.readyState <= 1) return;
    connectWS().catch(() => {});
    scheduleFastReconnect();
  }, delay);
}

function cancelFastReconnect() {
  if (fastRetryTimer) {
    try { clearTimeout(fastRetryTimer); } catch (_) {}
    fastRetryTimer = null;
  }
  fastRetryIndex = 0;
}

function scheduleProbe() {
  // chrome.alarms 能跨 SW 回收存活，这是「长期不连也得自愈」的唯一载体
  // （短窗口的密集重连由 scheduleFastReconnect 负责）。
  // 真实间隔由 Chrome 的 30s 最小粒度决定（实测 30~60s），不是下面这个数字：
  //   0.25min(15s) / 0.5min(30s) 都会被抬到 ≥30s，注释里不再宣称「1s 一次」。
  // 保护：距上次建连尝试不到 25s 时说明刚试过（keepalive / onStartup / popup 触发），
  // 这一轮不再请求更短的延迟，避免「刚建连又排一个新的 probe」在同一时刻各建一条
  // 连接（daemon 侧双连接）。同名 alarm 的 create 是覆盖语义，不会叠出两个 probe。
  const recentlyAttempted = !!lastConnectAttemptAt && Date.now() - lastConnectAttemptAt < 25000;
  chrome.alarms.create('tmwd-ws-probe', {
    delayInMinutes: recentlyAttempted ? ALARM_MIN_MINUTES : ALARM_MIN_MINUTES / 2
  });
}

function scheduleKeepalive() {
  // 保活 + 断线检测。0.5min 是 alarms 的下限，已经无法更短：
  // 想要真正「<30s 一次」的保活只能靠 offscreen document 或 runtime.connect 端口
  //（本批不改架构，见报告里的建议）。
  chrome.alarms.create('tmwd-ws-keepalive', { delayInMinutes: ALARM_MIN_MINUTES });
}

/// socket 事件监听器的统一模板：
///   1) 首行校验「还是当前 socket」——旧 socket 换代后迟到的 onopen/onmessage
///      会把旧帧当成新命令执行（重复执行 / 状态分裂），必须直接丢弃；
///   2) 顶层 catch——监听器是 async，任何未被捕获的 rejection 在 MV3 里都可能
///      让处理链断在半路（例如 ext_ready 之后再也没人发），只记日志、不改变业务。
function guardSocketHandler(label, socket, handler) {
  return async (event) => {
    if (ws !== socket) return;
    try {
      await handler(event);
    } catch (e) {
      console.warn('[TMWD-WS] ' + label + ' 处理异常（已兜底）:', (e && e.message) || e);
    }
  };
}

/// 主动放弃一条 socket：必须先摘掉回调再 close，否则被丢弃的 socket 之后仍可能
/// 触发 onopen/onmessage，闭包继续用 `ws` 之外的引用发消息 —— 那就是「实际连着
/// 却发不出响应」的来源。
function discardSocket(socket, reason) {
  if (!socket) return;
  try {
    socket.onopen = null;
    socket.onmessage = null;
    socket.onclose = null;
    socket.onerror = null;
  } catch (_) { /* 已关闭的 socket 赋值可能抛，忽略 */ }
  try { socket.close(); } catch (_) { /* 同上 */ }
  console.log('[TMWD-WS] 丢弃 socket:', reason);
}

async function isServerAlive() {
  let timer = null;
  try {
    const ctrl = new AbortController();
    // timer 必须在 finally 里清掉，否则每次探活都留一个 2s 后触发的闭包。
    timer = setTimeout(() => ctrl.abort(), 2000);
    await fetch(`http://127.0.0.1:${wsPort}`, { signal: ctrl.signal });
    return true; // Got HTTP response → port is listening
  } catch (e) {
    return false; // Network error (connection refused) or timeout → server not alive
  } finally {
    if (timer) { try { clearTimeout(timer); } catch (_) {} }
  }
}

chrome.alarms?.onAlarm?.addListener?.((alarm) => {
  // onAlarm 是 async 监听器，任何一步 await 抛错都会变成未处理拒绝
  // （表现为这条 alarm 静默失效，probe/keepalive 再也不排期）。统一收敛成日志。
  bgTask('alarm:' + (alarm && alarm.name), () => handleAlarm(alarm));
});

async function handleAlarm(alarm) {
  if (!alarm || !alarm.name) return;
  if (alarm.name === 'tmwd-self-reload') {
    // 兼容旧版本残留的 alarm。
    // 显式 reload 同样不能截断在途命令，交给 reloadWhenIdle 等空闲。
    reloadWhenIdle('self-reload');
    return;
  }
  if (alarm.name === SELF_CHECK_ALARM) {
    checkSelfUpdate();
    // 顺手清理上一代 SW 遗留的「纯 CDP」attach（只 attach、无持久化状态，
    // 恢复路径覆盖不到）。失败只记日志，不能打断自检。
    sweepOrphanDebugSessions().catch((e) => {
      console.log('[TMWD-DEBUG] sweep 失败:', e && e.message);
    });
    return;
  }
  if (alarm.name.startsWith(DEBUG_DETACH_ALARM_PREFIX)) {
    const tabId = Number(alarm.name.slice(DEBUG_DETACH_ALARM_PREFIX.length));
    const session = debugSessions.get(tabId);
    // 延迟期间可能又被 network/console/Fetch 重新占用，这里必须再判一次用途集合。
    if (session?.attached && !debugSessionInUse(session)) {
      await detachDebugSession(session, session.detachReason || 'delayed-idle');
    }
    return;
  }
  if (alarm.name === WS_HEARTBEAT_ALARM) {
    // SW 被回收后由 alarm 唤醒：这里只补一次 ping，不抢 keepalive 的职责
    // （重连/半开判定仍然只由 tmwd-ws-keepalive 那个分支做，避免两处同时 discard）。
    const socket = ws;
    if (socket && socket.readyState === WebSocket.OPEN) {
      try {
        lastPingAt = Date.now();
        socket.send(JSON.stringify({ type: 'ping' }));
      } catch (_) {}
    }
    return;
  }
  if (alarm.name === 'tmwd-ws-keepalive') {
    // 三态处理，不能再用「!== OPEN 就算断」：
    //   OPEN(1)       → 真连着：ping + 继续保活；
    //   CONNECTING(0) → 握手还没回来。既不是「断了」也不是「可用」：以前这里直接
    //                   ws=null + detachAllDebugSessions，把这条 socket 变成孤儿
    //                   —— 它的 onopen/onmessage 闭包仍在跑，daemon 也能收到 ext_ready，
    //                   但全局 ws 已经没了 → 「实际连着却发不出任何响应」，下一次 probe
    //                   还会再建一条并行连接。所以这里只观察、不清空、不 detach；
    //                   握手超时（WS_CONNECT_TIMEOUT_MS）才当失败处理。
    //   CLOSING/CLOSED/null → 真的没了：清空并重连。
    const socket = ws;
    const readyState = socket ? socket.readyState : WebSocket.CLOSED;
    if (readyState === WebSocket.OPEN) {
      // 先 ping，再看静默：链路是否活着只由「pong 是否回来」证明。
      // 静默超过 2.5 个保活周期 = 连 pong 都没有，高度可疑；但**不能只看静默就 close**：
      // 宿主机被挂起 / daemon 卡在长任务时 pong 会迟到，盲目拆会让所有 CDP 会话
      // （含 Fetch 拦截）空窗。所以静默只「触发复核」：再用一次 HTTP 探活确认进程是否还在。
      // 进程没了（-9 / 崩溃 / 宿主机挂起）→ 这条 socket 一定废了，丢弃重连；
      // 进程还在 → 只记一条日志，继续 ping 等 pong 自愈。
      const silenceMs = lastPongAt ? Date.now() - lastPongAt : 0;
      if (silenceMs > WS_KEEPALIVE_STALE_MS) {
        const silentSec = Math.round(silenceMs / 1000);
        const alive = await isServerAlive();
        if (!alive) {
          console.warn('[TMWD-WS] keepalive: ' + silentSec +
            's 未收到 pong 且 daemon 已不在监听，判定为半开连接，主动重连');
          discardSocket(socket, 'keepalive-stale');
          ws = null;
          await clearWsHeartbeatAlarm();
          // 与 keepalive-lost 同口径：只在浏览器侧释放 CDP，持久化规则快照保留。
          await detachAllDebugSessions('keepalive-lost').catch(() => {});
          scheduleProbe();
          scheduleFastReconnect();
          return;
        }
        if (!wsIdleWarned) {
          wsIdleWarned = true;
          console.log('[TMWD-WS] keepalive: ' + silentSec +
            's 未收到 pong，但 daemon 仍在监听（可能卡在长任务），先保留连接继续等 pong');
        }
      }
      lastPingAt = Date.now();
      sendWs('{"type":"ping"}');
      scheduleKeepalive();
      return;
    }
    if (readyState === WebSocket.CONNECTING) {
      const waited = Date.now() - (lastConnectAttemptAt || 0);
      if (waited < WS_CONNECT_TIMEOUT_MS) {
        // 只延长一次等待：保活周期本身就是 30s，下一拍再看即可。
        console.log('[TMWD-WS] keepalive: 仍在握手，继续等待', waited + 'ms');
        scheduleKeepalive();
        return;
      }
      console.warn('[TMWD-WS] keepalive: 握手超时，丢弃这条 socket', waited + 'ms');
    }
    // 到这里可能是：CLOSING/CLOSED、null，或握手超时的 CONNECTING。
    discardSocket(socket, readyState === WebSocket.CONNECTING ? 'connect-timeout' : 'keepalive-lost');
    ws = null;
    clearWsHeartbeatAlarm().catch(() => null);
    detachAllDebugSessions('keepalive-lost').finally(() => scheduleProbe());
  }
  if (alarm.name === 'tmwd-ws-probe') {
    if (ws && ws.readyState <= 1) return; // Already connected/connecting
    if (await isServerAlive()) {
      console.log('[TMWD-WS] Server detected, connecting...');
      connectWS();
    } else {
      scheduleProbe(); // Server not up, keep probing
    }
  }
}

/**
 * 注入返回 null 时，区分「页面根本不许注入」和「能注入但 eval 被 CSP 禁掉」。
 * 这两种情况的修复方式完全不同，笼统报一句 CSP 只会让人瞎猜。
 */
async function describeInjectFailure(tabId) {
  try {
    const probe = await chrome.scripting.executeScript({
      target: { tabId },
      world: 'MAIN',
      func: () => 1
    });
    if (probe?.[0]?.result === 1) {
      return '页面 CSP 禁止 eval（unsafe-eval 未放行），exec 无法在 MAIN world 执行；需要 CDP';
    }
    return 'executeScript 能调用但页面脚本没有返回结果（页面可能冻结或导航中）';
  } catch (e) {
    return 'executeScript 被拒绝: ' + e.message;
  }
}

// --- 命令幂等去重---
// daemon 侧的 60s 回放窗口、HTTP 超时后的重试，都会把同一个 id 再下发一次。
// 扩展侧没有去重时，click / submit / inject 这类副作用会执行两遍。
// 登记项：running（已开始）→ done（回放原结果，不再执行）。
// 上限 256 / TTL 60s；丢弃迟到响应时**不删**登记，正是为了让重试命中 done。
const WS_COMMAND_TTL_MS = 60000;
const WS_COMMAND_LIMIT = 256;
// 重复命中 running 时挂起等原命令终态（而不是立即回 duplicate_inflight）。
// 长命令（awaitPromise 可以远超 60s）在 daemon 超时后用同一 id 重试时，
// 立即拒绝等于把「慢但会成功」变成确定性失败。超时兜底仍回 duplicate_inflight。
const WS_COMMAND_DUPLICATE_WAIT_MS = 120000;
const wsCommandLedger = new Map();

function wsCommandSweep(now) {
  for (const [key, entry] of wsCommandLedger) {
    if (!entry) {
      wsCommandLedger.delete(key);
      continue;
    }
    // running 不能按 TTL 清：它还在等自己的终态，清掉会让 daemon 的重试变成
    // 「重新执行」（副作用翻倍），也会让挂起的等待者永远等不到结果。
    // 这类条目的数量由 WS_COMMAND_LIMIT 与 wsCommandReleaseIfRunning 共同兜住。
    if (entry.state === 'running') continue;
    if (now - entry.at > WS_COMMAND_TTL_MS) wsCommandLedger.delete(key);
  }
  while (wsCommandLedger.size > WS_COMMAND_LIMIT) {
    let oldest = null;
    for (const [key, entry] of wsCommandLedger) {
      if (entry && entry.state === 'running') continue;
      oldest = key;
      break;
    }
    // 全是 running：宁可超限一次，也不能清掉正在跑的命令（否则重试会重跑副作用）。
    if (oldest === null) break;
    wsCommandLedger.delete(oldest);
  }
}

function wsCommandBegin(id) {
  const key = String(id);
  const now = Date.now();
  wsCommandSweep(now);
  const prior = wsCommandLedger.get(key);
  if (prior) {
    // 仍新鲜 → 重复下发。running 等它结束后回放，done 直接回放。
    prior.at = now;
    return { duplicate: true, prior };
  }
  wsCommandLedger.set(key, { state: 'running', at: now, payload: null, waiters: [] });
  return { duplicate: false };
}

/// 挂起等待某条命令的终态。返回 null = 等不到（超时 / 命令没有回包被释放），
/// 调用方应回 duplicate_inflight。超时兜底是必须的：原命令可能因为 SW 回收永远不回包。
function wsCommandWaitForResult(id, timeoutMs) {
  const key = String(id);
  const entry = wsCommandLedger.get(key);
  if (!entry) return Promise.resolve(null);
  if (entry.state !== 'running') return Promise.resolve(entry.payload || null);
  return new Promise((resolve) => {
    let settled = false;
    const finish = (payload) => {
      if (settled) return;
      settled = true;
      try { clearTimeout(timer); } catch (_) {}
      resolve(payload || null);
    };
    const timer = setTimeout(() => finish(null), Math.max(1000, Number(timeoutMs) || WS_COMMAND_DUPLICATE_WAIT_MS));
    if (!Array.isArray(entry.waiters)) entry.waiters = [];
    entry.waiters.push(finish);
  });
}

/// 登记并发送终态响应（ack 不算终态）。发送失败也照样登记：
/// 断线期间的结果会被 enqueueWsSend 丢弃，但 daemon 的重试应当命中登记而不是重跑。
function sendWsCommandResponse(socket, id, payload) {
  if (!payload) return;
  const key = String(id);
  const entry = wsCommandLedger.get(key);
  if (entry) {
    entry.state = 'done';
    entry.payload = payload;
    entry.at = Date.now();
  } else {
    wsCommandLedger.set(key, { state: 'done', payload, at: Date.now() });
  }
  // 先唤醒重复请求的等待者，再清理过期条目：等待者拿的是同一个 payload 引用，
  // 顺序反了（sweep 先删）也不会丢，但语义上「结果已就绪」应先于「记账清理」。
  if (entry && Array.isArray(entry.waiters) && entry.waiters.length) {
    for (const finish of entry.waiters.splice(0)) {
      try { finish(payload); } catch (_) {}
    }
  }
  wsCommandSweep(Date.now());
  socketSend(socket, payload, payload.type === 'result' ? 'result' : 'error');
}

/// 只清理「还在 running 且从未产生终态响应」的登记（命令根本没执行成功），
/// 让 daemon 的重试可以真正重跑。挂起的等待者一并释放（它们会回 duplicate_inflight）。
function wsCommandReleaseIfRunning(id) {
  const key = String(id);
  const entry = wsCommandLedger.get(key);
  if (entry && entry.state === 'running') {
    wsCommandLedger.delete(key);
    if (Array.isArray(entry.waiters)) for (const finish of entry.waiters.splice(0)) {
      try { finish(null); } catch (_) {}
    }
  }
}

// 正在执行的 exec 命令 → tabId。取消帧（id 带 #cancel 后缀）按 execId 找到
// 对应的 tab，提前把只为这条 exec 建立的 debugger attach 释放掉（让 awaitPromise 的
// Runtime.evaluate 立刻失败），不必等到 daemon 侧超时后干等。
const wsRunningExecs = new Map();

// （报告方案 C）：strict 档位下「无 token 连接」只放行只读命令。
// `cancel` 不在其中：它会让 debugger attach 提前释放（可被用来打断别人正在跑的诊断），
// strict 档位下必须和 exec 同等信任级别。
const WS_READONLY_COMMANDS = new Set([
  'status', 'tabs', 'frames', 'networkList', 'networkDetail', 'consoleList', 'selfCheck'
]);

function wsTrustAllowsCommand(cmdName) {
  if (wsTrustMode !== 'strict') return true;
  if (wsToken) return true; // 配了 token = daemon 侧已启用共享密钥，能力不降级
  return WS_READONLY_COMMANDS.has(String(cmdName || ''));
}

async function handleWsExec(data, socket = ws) {
  const tabId = data.tabId;
  console.log('[TMWD-WS] Exec request', data.id, 'on tab', tabId);
  if (!socketSend(socket, { type: 'ack', id: data.id })) {
    console.log('[TMWD-WS] socket closed before ack, drop command', data.id);
    return;
  }
  if (!tabId) {
    sendWsCommandResponse(socket, data.id, { type: 'error', id: data.id, error: 'No tabId provided' });
    return;
  }
  // 登记「这条 exec 正在哪个 tab 上跑」，取消帧据此定位要提前释放的 attach。
  const execKey = String(data.id);
  wsRunningExecs.set(execKey, Number(tabId));
  // 极端情况下命令永远不返回会留下登记：按 FIFO 限长，避免 Map 无界增长。
  while (wsRunningExecs.size > 128) {
    const oldest = wsRunningExecs.keys().next().value;
    if (oldest === undefined) break;
    wsRunningExecs.delete(oldest);
  }
  const codeText = String(data.code || '');
  // 只在脚本可能触发原生弹窗/新标签页时才付额外开销：
  // 之前每条 exec 都固定做 2 次弹窗抑制注入 + 200ms 新标签宽限，隐藏标签页里尤其浪费。
  // 指定了 frame 时禁止 CDP 回退：CDP 那条路径永远跑在主框架，
  // 会把「frame 不存在」的失败悄悄变成「在主框架执行成功」。
  const hasFrameTarget = !!(data.frameId !== undefined || data.frame_id !== undefined || data.allFrames === true || data.all_frames === true);
  const needsDialogGuard = /\b(alert|confirm|prompt)\s*\(/.test(codeText);
  const mayOpenTab = /window\.open\s*\(|target\s*=\s*["']?_blank|\.click\s*\(/.test(codeText);
  const execStarted = Date.now();
  // Use onCreated listener to reliably capture new tabs (avoids race condition with query-diff)
  const newTabIds = new Set();
  const onCreated = (tab) => { newTabIds.add(tab.id); };
  chrome.tabs?.onCreated?.addListener?.(onCreated);
  if (needsDialogGuard) await setDialogSuppressionByScripting(tabId, true);
  try {
    let res;
    let injectError = null;
    try {
      const result = await chrome.scripting.executeScript({
        target: buildScriptTarget(tabId, data),
        // 同 runUserCodeInTab —— 小写 isolated 不能落 MAIN；exec 默认仍是 MAIN。
        world: normalizeWorld(data.world) || 'MAIN',
        func: async (s) => await eval(s),
        args: [buildPageScript(codeText)]
      });
      res = result[0]?.result;
      if (res === null || res === undefined) {
        console.log('[TMWD-WS] executeScript returned null/undefined, treating as CSP issue');
        injectError = await describeInjectFailure(tabId);
        res = { ok: false, error: { name: 'Error', message: injectError, stack: '' }, csp: true, executed: false };
      }
    } catch (e) {
      console.log('[TMWD-WS] scripting.executeScript failed:', e.message);
      injectError = e.message || String(e);
      res = { ok: false, error: { name: e.name || 'Error', message: injectError, stack: e.stack || '' }, csp: true, executed: false };
    }
    // CDP fallback for CSP-restricted pages.
    // executed=true 说明脚本已经在页面里跑过（用户自己抛了带 CSP 字样的错误），
    // 再走 CDP 会二次执行有副作用的脚本，必须排除。
    if (res && !res.ok && res.csp && res.executed !== true && !hasFrameTarget) {
      console.log('[TMWD-WS] CDP fallback for tab', tabId);
      const wrappedCode = buildCdpScript(codeText);
      try {
        const session = getDebugSession(tabId);
        await ensureDebugAttached(session);
        if (needsDialogGuard) await setDialogSuppressionByCdp(tabId, true);
        const cdpRes = await chrome.debugger.sendCommand({ tabId }, 'Runtime.evaluate', {
          expression: wrappedCode, awaitPromise: true, returnByValue: true
        });
        if (needsDialogGuard) await setDialogSuppressionByCdp(tabId, false);
        scheduleDebugDetach(session, 'ws-exec-idle');
        if (cdpRes.exceptionDetails) {
          const desc = cdpRes.exceptionDetails.exception?.description || 'CDP Error';
          // CDP 表达式已编译并执行，exceptionDetails 代表脚本抛错。
          res = { ok: false, error: { name: 'Error', message: desc, stack: desc }, executed: true };
        } else {
          res = cdpRes.result.value;
        }
      } catch (cdpErr) {
        if (needsDialogGuard) { try { await setDialogSuppressionByCdp(tabId, false); } catch (_) {} }
        scheduleDebugDetach(getDebugSession(tabId), 'ws-exec-error');
        // 把首次注入的真实错误一起带出来：只报"CDP fallback failed"会让人误以为是 CDP 的问题，
        // 实际根因往往在 executeScript 那一步。
        const parts = ['CDP fallback failed: ' + cdpErr.message];
        if (injectError) parts.push('inject failed: ' + injectError);
        res = { ok: false, error: { name: 'Error', message: parts.join(' | '), stack: '' }, executed: false };
      }
    }
    // 只有脚本里出现"可能新建标签页"的动作时才等待，避免每条命令白付 200ms。
    if (newTabIds.size === 0 && mayOpenTab) await new Promise(r => setTimeout(r, 120));
    chrome.tabs?.onCreated?.removeListener?.(onCreated);
    // Get full info for captured new tabs
    const newTabs = [];
    for (const id of newTabIds) {
      try { const t = await chrome.tabs.get(id); newTabs.push({id: t.id, url: t.url, title: t.title}); } catch (_) {}
    }
    if (res?.ok) {
      const resultValue = describeValue(res.data ?? null);
      // 只新增字段，`result` 本身仍是用户脚本的返回值。
      sendWsCommandResponse(socket, data.id, {
        type: 'result',
        id: data.id,
        result: resultValue,
        newTabs,
        executed: true,
        threw: null,
        value_type: typeof resultValue,
        duration_ms: Math.max(0, Date.now() - execStarted)
      });
    } else {
      console.log(res);
      // 失败同样带上 executed/threw：调用方据此区分「脚本已执行后抛错」与
      // 「注入/编译失败、一行都没跑」。旧 daemon 忽略这些顶层字段。
      const classified = classifyUserCodeResult(res, execStarted);
      sendWsCommandResponse(socket, data.id, {
        type: 'error',
        id: data.id,
        error: res?.error || 'Unknown error',
        newTabs,
        executed: classified.executed === true,
        threw: classified.threw,
        value_type: 'undefined',
        duration_ms: classified.duration_ms
      });
    }
  } catch (e) {
    sendWsCommandResponse(socket, data.id, { type: 'error', id: data.id, error: { name: e.name || 'Error', message: e.message || String(e), stack: e.stack || '' } });
  } finally {
    wsRunningExecs.delete(execKey);
    if (needsDialogGuard) await setDialogSuppressionByScripting(tabId, false);
    chrome.tabs?.onCreated?.removeListener?.(onCreated);
  }
}

// daemon 在 exec 超时/取消时下发 `{ id: "<exec_id>#cancel", cmd: "cancel", execId, tabId, reason }`。
/// 语义是「把这条 exec 为它建立的 debugger attach 提前释放」，让挂着的 Runtime.evaluate
/// 立刻失败，而不是等 daemon 侧的 HTTP 超时结束。
///   - 有采集/拦截用途（network/console/Fetch）的 tab **不能**拆：返回 cancelled:false + reason:'in_use'，
///     否则会连带打断用户正在用的 net log / mock 规则（比等一个超时更糟）。
///   - 找不到会话 / 已经释放 → `ok:true, cancelled:false, reason:'not_found'`（绝不回 Unknown cmd）。
/// 回包必须原样带回 msg.id（带 `#cancel` 后缀），daemon 靠它匹配等待中的取消请求。
async function handleCancelCommand(msg) {
  const execId = msg.execId === undefined || msg.execId === null ? '' : String(msg.execId);
  let tabId = execId ? wsRunningExecs.get(execId) : undefined;
  if (tabId === undefined) {
    const raw = Number(msg.tabId);
    if (Number.isInteger(raw) && raw > 0) tabId = raw;
  }
  if (!Number.isInteger(tabId) || tabId <= 0) {
    return { id: msg.id, ok: true, cancelled: false, reason: 'not_found' };
  }
  const session = debugSessions.get(tabId);
  if (!session || !session.attached) {
    return { id: msg.id, ok: true, cancelled: false, reason: 'not_found', tabId };
  }
  if (debugSessionInUse(session)) {
    return { id: msg.id, ok: true, cancelled: false, reason: 'in_use', tabId };
  }
  try {
    // 只有「纯 exec/CDP」的 attach 才拆得掉；detachDebugSession 内部会复核 getTargets。
    await detachDebugSession(session, 'exec-cancelled');
  } catch (e) {
    return { id: msg.id, ok: true, cancelled: false, reason: 'detach_failed', error: (e && e.message) || String(e), tabId };
  }
  if (execId) wsRunningExecs.delete(execId);
  return { id: msg.id, ok: true, cancelled: true, tabId, execId: execId || null, reason: msg.reason || null };
}

// ---------------------------------------------------------------------------
// 握手前的对端鉴权
//
// daemon 在 upgrade 成功后立刻下发 {"type":"ws_hello",v,nonce,proof,auth_required}，
// proof = HMAC-SHA256(key=wsToken, msg=nonce + "|" + extension_id) 的小写 hex。
// 扩展只有在算出同一 proof 之后才发 ext_ready —— 否则「谁绑上了端口谁就是 daemon」，
// 本机任意进程都能拿全量标签并下发 exec。
//
// wsToken 为空时保持历史行为：onopen 立即发 ext_ready，ws_hello 直接忽略（老 daemon
// 不发这一帧，也不能因此把兼容链路卡死）。
// ---------------------------------------------------------------------------
const WS_AUTH_WAIT_MS = 5000;

/// 当前这次 onopen 期间累积的非致命告警（心跳/保活等），随 ext_ready 一起上报。
/// 用模块级变量而不是闭包实参：sendExtReady 也可能由 ws_hello 校验通过后异步触发。
let wsOpenWarnings = [];

/// 正在等 ws_hello 的那条 socket。只认当前这条：换 socket 后迟到的 hello 必须作废。
let wsAuthPending = null;

function clearDaemonAuth() {
  if (wsAuthPending && wsAuthPending.timer) {
    try { clearTimeout(wsAuthPending.timer); } catch (_) {}
  }
  wsAuthPending = null;
}

function beginDaemonAuth(socket) {
  clearDaemonAuth();
  // 定时器用 setTimeout 而不是 alarm：5s 的窗口 alarm 根本表达不了（下限 30s）。
  // SW 在这 5s 内被回收的极端情况下 onclose 不会触发，此时由 probe/keepalive 兜底重连。
  const timer = setTimeout(() => {
    if (!wsAuthPending || wsAuthPending.socket !== socket) return;
    failDaemonAuth(socket, 'daemon_auth_timeout',
      `等待 daemon 握手证明超时（${WS_AUTH_WAIT_MS}ms）：对端可能不是真 daemon，或 daemon 版本不支持双向鉴权`)
      .catch((e) => console.warn('[TMWD-WS] 鉴权超时处理失败:', (e && e.message) || e));
  }, WS_AUTH_WAIT_MS);
  wsAuthPending = { socket, timer };
}

async function failDaemonAuth(socket, kind, message) {
  clearDaemonAuth();
  try { await recordWsConnectError(kind, message); } catch (_) {}
  // discardSocket 会把 onclose 一起摘掉（避免旧 socket 迟到事件干扰新连接），
  // 所以重连排期必须在这里自己补上，否则鉴权失败就等于永久断连。
  if (ws === socket) ws = null;
  discardSocket(socket, 'daemon-auth-failed');
  // 摘掉 onclose 之后它不会再跑，心跳 alarm 必须在这里清，否则每分钟都会空转一次唤醒。
  try { await clearWsHeartbeatAlarm(); } catch (_) {}
  try { scheduleProbe(); } catch (e) { console.warn('[TMWD-WS] scheduleProbe 失败:', e && e.message); }
  try { scheduleFastReconnect(); } catch (e) { console.warn('[TMWD-WS] scheduleFastReconnect 失败:', e && e.message); }
}

/// HMAC-SHA256 → 小写 hex。WebCrypto 在扩展页面（含 SW）恒定可用；
/// 不用扩展 API（chrome.runtime.id 是扩展 ID，正好作为消息的一部分）。
async function hmacSha256Hex(key, message) {
  const subtle = globalThis.crypto && globalThis.crypto.subtle;
  if (!subtle) throw new Error('WebCrypto subtle 不可用，无法校验 daemon 握手证明');
  const enc = new TextEncoder();
  const cryptoKey = await subtle.importKey('raw', enc.encode(String(key)), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const signature = await subtle.sign('HMAC', cryptoKey, enc.encode(String(message)));
  const bytes = new Uint8Array(signature);
  let hex = '';
  for (const byte of bytes) hex += byte.toString(16).padStart(2, '0');
  return hex;
}

/// 恒定时间比较：不 early-return，避免按「前几位对上了」的时序差反推 proof。
/// 长度不同直接判否（长度本身不是秘密）。
function constantTimeEqualHex(left, right) {
  const a = String(left || '').toLowerCase();
  const b = String(right || '').toLowerCase();
  if (!a || a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/// 处理 ws_hello：只有「本地有 token 且确实在等这一帧」时才校验。
async function handleDaemonAuthHello(socket, data) {
  const pending = wsAuthPending;
  if (!pending || pending.socket !== socket) return;
  // 等待期间用户清空了 token：此时没有可验证的密钥，回到无 token 的兼容路径。
  if (!wsToken) {
    clearDaemonAuth();
    await sendExtReady(socket);
    return;
  }
  // 本地有 token 但对端声明不需要鉴权 → 对端不认识我们的密钥（旧版或被冒充），按失败处理。
  if (data.auth_required === false) {
    await failDaemonAuth(socket, 'daemon_auth_failed',
      '本地已配置 token，但对端声明 auth_required=false：不是预期的 daemon（可能端口被其它进程占用）');
    return;
  }
  const nonce = String(data.nonce || '');
  const proof = String(data.proof || '');
  if (!nonce || !proof) {
    await failDaemonAuth(socket, 'daemon_auth_failed', 'ws_hello 缺少 nonce 或 proof');
    return;
  }
  let expected = '';
  try {
    expected = await hmacSha256Hex(wsToken, `${nonce}|${chrome.runtime.id}`);
  } catch (e) {
    await failDaemonAuth(socket, 'daemon_auth_failed', '计算握手证明失败：' + ((e && e.message) || e));
    return;
  }
  if (ws !== socket) return; // 校验期间已换代：这条 socket 的结论不再有人关心
  if (!constantTimeEqualHex(expected, proof)) {
    await failDaemonAuth(socket, 'daemon_auth_failed',
      'daemon 握手证明不匹配：对端不持有当前 wsToken（端口可能被其它进程占用）');
    return;
  }
  clearDaemonAuth();
  await sendExtReady(socket);
}

/// 发送 ext_ready 及其后续补发/恢复。抽成独立函数是必须的：鉴权模式下这条路径
/// 由 ws_hello 触发，与「无 token 时 onopen 直接发」共用同一份实现，避免两处分叉。
async function sendExtReady(socket) {
  if (ws !== socket) return;
  // daemon 在 accept 之后就在等 ext_ready：这里任何一步抛错都不能让它永不发出，
  // 否则 daemon 侧所有命令超时、扩展侧 popup 还显示「已连接」，故障极难定位。
  const warnings = wsOpenWarnings;
  wsOpenWarnings = [];
  let tabs = [];
  let groupTitles = null;
  let fileSchemeAccess = false;
  try {
    try { await clearWsConnectError(); } catch (e) { warnings.push('clearConnectError: ' + ((e && e.message) || e)); }
    try { await ensureBrowserMeta(); } catch (e) { warnings.push('browserMeta: ' + ((e && e.message) || e)); }
    try { await ensureClientIdentity(); } catch (e) { warnings.push('clientIdentity: ' + ((e && e.message) || e)); }
    try { tabs = await queryScriptableTabs(); } catch (e) { warnings.push('queryTabs: ' + ((e && e.message) || e)); }
    // 分组标题要和 tabs_update 用同一套字段：以前这里漏了声明，ext_ready 直接抛
    // ReferenceError，副作用是 daemon 拿不到首批标签（连接显示已建、标签数为 0）。
    try { groupTitles = await queryTabGroupTitles(); } catch (e) { warnings.push('tabGroups: ' + ((e && e.message) || e)); }
    try { fileSchemeAccess = await isAllowedFileSchemeAccess(); } catch (e) { warnings.push('fileSchemeAccess: ' + ((e && e.message) || e)); }
    try { await ensureRestartSeq(); } catch (e) { warnings.push('restartSeq: ' + ((e && e.message) || e)); }
    tabs = filterReportableTabs(tabs);
  } finally {
    // finally 里无论如何都发一次 ext_ready；拿不到的信息用默认值/空数组。
    socketSend(socket, withClientIdentity({
      type: 'ext_ready',
      protocol: EXTENSION_PROTOCOL,
      extension_version: extensionVersion,
      file_scheme_access: fileSchemeAccess,
      tabs: tabs.map(t => ({ id: t.id, window_id: t.windowId, url: t.url, title: t.title, active: t.active, group_title: groupTitles ? tabGroupTitle(groupTitles, t) : null })),
      restartSeq: restartSeq || 0,
      warnings
    }));
    console.log('[TMWD-WS] Sent ext_ready with', tabs.length, 'tabs',
      warnings.length ? ('warnings: ' + warnings.join(' | ')) : '');
  }
  // ext_ready 必须先发（daemon 靠它建会话表），再按序补发断线期间积累的消息；
  // 队列里可能有断线时的旧 tab 快照，所以最后再排一次 tabs_update 保证以最新状态收尾。
  flushWsSendQueue(socket);
  // 断线时快照被保留（detach 走 keepPersisted），这里补一次恢复，
  // 兜住「冷启动时 debugger/daemon 还没就绪，首次 restore 失败」的情况。
  ensureDebugSessionsRestoredOnReconnect();
  scheduleTabsUpdate();
}

/// 建连的单飞（E1-02）：connectWS 内部有多个 await（读端口 / 读 token / isServerAlive），
/// 而调用点有五处（模块求值、onStartup、onInstalled、probe、fastRetry）。
/// 旧写法只在 await 之后才检查 `ws && readyState <= 1`，于是并发两次调用会各自
/// `new WebSocket(...)` 建出两条连接 —— daemon 侧看到两个扩展会话，命令与
/// tabs_update 会在两条连接之间来回漂，表现为「状态时好时坏」。
/// 这里在**入口**就把并发的第二次调用折叠到同一条 promise 上。
let connectInFlight = null;

function connectWS() {
  if (connectInFlight) return connectInFlight;
  // 已有 CONNECTING/OPEN 的 socket：直接返回已 settle 的 promise。
  // 不能 return undefined：调用点普遍写成 `connectWS().catch(...)`（见文件末尾），
  // 返回 undefined 会在那里抛 "Cannot read properties of undefined (reading 'catch')"。
  if (ws && ws.readyState <= 1) return Promise.resolve();
  connectInFlight = connectWSInner().finally(() => { connectInFlight = null; });
  return connectInFlight;
}

async function connectWSInner() {
  // loadWsPort 内部 normalizePort 会 throw：storage 里的 wsPort 一旦被写坏（比如手工
  // 改成 "abc"），旧代码会让 connectWS 直接 reject，而所有调用点只 catch 记日志 →
  // 没有任何路径重新 scheduleProbe → 永久断连且不自愈。这里必须自己兜住。
  try {
    await loadWsPort();
  } catch (e) {
    console.warn('[TMWD-WS] wsPort 配置无效，回退默认端口', DEFAULT_WS_PORT, ':', e && e.message);
    wsPort = DEFAULT_WS_PORT;
    try {
      await recordWsConnectError('bad-port', `wsPort 配置无效，已回退 ${DEFAULT_WS_PORT}: ${(e && e.message) || e}`);
    } catch (_) {}
    // 顺手把脏值修正掉，避免每次启动都在降级路径上打转。
    try { await chrome.storage.local.set({ wsPort: DEFAULT_WS_PORT }); } catch (_) {}
    // 兜底：即使后续任何一步再失败，probe 循环也不会断。
    scheduleProbe();
  }
  try {
    await loadWsToken();
  } catch (e) {
    // token 读失败按「未配置」处理：连接本身仍可继续（兼容模式）。
    console.warn('[TMWD-WS] wsToken 读取失败，按未配置处理:', e && e.message);
    wsToken = '';
  }
  try {
    await loadWsTrustMode();
  } catch (e) {
    // 读不到信任档位就退回 compat：默认行为与历史完全一致。
    console.warn('[TMWD-WS] wsTrustMode 读取失败，按 compat 处理:', e && e.message);
    wsTrustMode = 'compat';
  }
  if (ws && ws.readyState <= 1) return; // CONNECTING or OPEN
  // 记录「真正开始尝试建连」的时刻：scheduleProbe 用它避免刚试过又立刻排一轮，
  // keepalive 用它判断一次握手是否已经等太久（CONNECTING 分支）。
  lastConnectAttemptAt = Date.now();
  if (!(await isServerAlive())) {
    console.warn('[TMWD-WS] Server not ready, retrying later:', maskWsUrl(getWsConnectUrl()));
    ws = null;
    await recordWsConnectError('server-down', 'daemon 未在该端口监听（或探测超时）');
    // daemon 刚被 CLI 拉起时会有 100~500ms 的空窗：先密集重试，alarms 只做兜底。
    scheduleProbe();
    scheduleFastReconnect();
    return;
  }
  const wsUrl = getWsConnectUrl();
  console.log('[TMWD-WS] Connecting to', maskWsUrl(wsUrl));
  let socket;
  try {
    socket = new WebSocket(wsUrl);
  } catch (e) {
    console.error('[TMWD-WS] Constructor error:', e);
    ws = null;
    await recordWsConnectError('constructor', (e && e.message) || String(e));
    scheduleProbe();
    scheduleFastReconnect();
    return;
  }
  ws = socket;
  // keepalive 以前只在 onopen 里排期。socket 卡在 CONNECTING（daemon accept 了
  // 但 upgrade 不回、或中间有代理吞掉握手）时 onopen 永远不来，于是没有任何 alarm 去
  // 判 WS_CONNECT_TIMEOUT_MS 并丢弃这条死 socket，`ws` 会一直挡着后续 connectWS。
  // 拿到 socket 后立刻排期，CONNECTING 分支就在冷启动第一次建连时也可达。
  try {
    scheduleKeepalive();
  } catch (e) {
    console.warn('[TMWD-WS] scheduleKeepalive 失败:', e && e.message);
  }
  socket.onopen = guardSocketHandler('ws-onopen', socket, async () => {
    console.log('[TMWD-WS] Connected!');
    // 握手成功即视为「对端活着」的起点：否则刚连上、还没收到任何消息时，
    // lastPongAt 会是上一代连接留下的旧值（或 0），静默判定会立刻误触发。
    if (ws === socket) { lastPongAt = Date.now(); wsIdleWarned = false; }
    cancelFastReconnect();
    // 心跳 / 保活 / alarm 与鉴权无关，必须在 onopen 就排期：等 ws_hello 的那几秒里
    // 若对端是半开连接，这是唯一能发现它的机制。失败只记进 ext_ready 的 warnings。
    wsOpenWarnings = [];
    try { scheduleKeepalive(); } catch (e) { wsOpenWarnings.push('scheduleKeepalive: ' + ((e && e.message) || e)); }
    try { startWsHeartbeat(socket); } catch (e) { wsOpenWarnings.push('wsHeartbeat: ' + ((e && e.message) || e)); }
    try { ensureWsHeartbeatAlarm(); } catch (e) { wsOpenWarnings.push('wsHeartbeatAlarm: ' + ((e && e.message) || e)); }
    // 契约 §3.1：本地配了 token 时**不得**立即发 ext_ready。任何本机进程都能在
    // daemon 未监听的窗口绑定 127.0.0.1:端口；先验 daemon 是否真持有同一 token，
    // 否则对方能直接拿到全量标签并下发 exec（本地提权到完全接管浏览器）。
    if (wsToken) {
      beginDaemonAuth(socket);
      return;
    }
    await sendExtReady(socket);
  });
  socket.onmessage = guardSocketHandler('ws-onmessage', socket, async (event) => {
    let data;
    try {
      data = JSON.parse(event.data);
    } catch (e) {
      console.error('[TMWD-WS] message parse error', e);
      return;
    }
    if (!data) return;
    // 任何入站帧都刷新「对端活着」的时间戳；只认当前 socket，
    // 过期 socket 的迟到消息不能给新连接续命（guardSocketHandler 已挡一层）。
    if (ws === socket) { lastPongAt = Date.now(); wsIdleWarned = false; }
    // 契约 §3.1：ws_hello 是握手帧，没有 id，必须在「无 id 直接丢」之前拦下来。
    // 只有正在等它的 socket 才消费；无 token 的连接按历史行为忽略这一帧。
    if (data.type === 'ws_hello') {
      await handleDaemonAuthHello(socket, data);
      return;
    }
    // 应用层心跳的回应（{type:'pong'}，无 id）。：这是唯一的 RTT 证据，
    // 旧实现把「任何入站消息」当 pong 刷新，链路半开时永远判不出来。
    if (data.type === 'pong') {
      if (lastPingAt) lastPongRttMs = Date.now() - lastPingAt;
      return;
    }
    if (!data.id) return;
    if (!data.code) {
      // 有 id 但没有 code：旧代码直接 return → daemon 空等到超时，扩展侧毫无痕迹。
      // 必须回一条显式失败。用协议里既有的 {type:'error', id, error} 形状：
      // daemon 的 WsIncoming::Result 要求 result 字段必填，只带 ok:false 的 "result"
      // 会被 serde 归到 Other，等于还是没回复。
      sendWsCommandResponse(socket, data.id, { type: 'error', id: data.id, error: 'code 缺失或无效' });
      return;
    }
    // 同一条命令 id 只执行一次。daemon 侧的 60s 回放窗口 / HTTP 超时重试
    // 会把**同一条**命令再下发一次；旧实现没有去重，副作用（点击、提交、注入）会翻倍。
    const ledger = wsCommandBegin(data.id);
    if (ledger.duplicate) {
      const prior = ledger.prior;
      if (prior.state === 'running') {
        // 不立即回 duplicate_inflight —— 长命令可能还要几十秒才结束，
        // 直接拒绝会把「慢但会成功」变成确定性失败。挂起等原命令的终态再回放同一条结果；
        // 只有等不到（超时 / 原命令没回包被释放）才退化为 duplicate_inflight。
        // 注意这里**不改登记状态**：原命令仍是 running，它稍后产生的真实结果才是终态。
        const payload = await wsCommandWaitForResult(data.id, WS_COMMAND_DUPLICATE_WAIT_MS);
        if (payload) {
          console.log('[TMWD-WS] 重复命令等到了原结果，回放:', data.id);
          socketSend(socket, payload, payload.type === 'result' ? 'result' : 'error');
        } else {
          socketSend(socket, {
            type: 'error',
            id: data.id,
            error: { name: 'DuplicateCommand', message: '命令正在执行（duplicate_inflight）', code: 'duplicate_inflight' }
          }, 'error');
        }
      } else {
        // 已完成的命令：原样回放上次的结果，不再执行副作用。
        console.log('[TMWD-WS] 命中已完成的命令 id，回放结果:', data.id);
        socketSend(socket, prior.payload, prior.payload.type === 'result' ? 'result' : 'error');
      }
      return;
    }
    commandInFlight += 1;
    try {
      lastCommandAt = Date.now();
      let code = data.code;
      // If code is a JSON string representing an object, parse it
      if (typeof code === 'string') {
        try { const p = JSON.parse(code); if (p && typeof p === 'object') code = p; } catch (_) {}
      }
      // 报告方案 C：strict 档位下，无 token 的连接视为「对端身份未确认」，
      // 只放行只读命令。默认 compat 不做任何拦截（既有部署零变化）。
      const cmdName = (typeof code === 'object' && code && code.cmd) ? String(code.cmd) : (typeof code === 'string' ? 'exec' : '');
      if (!wsTrustAllowsCommand(cmdName)) {
        sendWsCommandResponse(socket, data.id, {
          type: 'error',
          id: data.id,
          error: {
            name: 'UntrustedConnection',
            message: `wsTrustMode=strict 且未配置 wsToken，拒绝高危命令 ${cmdName}`,
            code: 'untrusted_connection'
          }
        });
        return;
      }
      if (typeof code === 'object' && code !== null && code.cmd) {
        // Custom protocol message → route to handleExtMessage
        if (code.tabId === undefined && data.tabId !== undefined) code.tabId = data.tabId;
        const res = await handleExtMessage(code, {});
        // inject 的失败回包带结构化 data（executed/threw/error_code）：
        // 旧的 `{type:'error'}` 会把 data 整个丢掉，daemon 只剩一句字符串。
        // 这里改走 result 帧；旧 daemon 的 extension_control 仍会因 data.ok=false 报错，
        // 新 daemon 则能保留字段（协议只加不改）。
        if (code.cmd === 'inject' && res && res.ok === false && res.data) {
          sendWsCommandResponse(socket, data.id, { type: 'result', id: data.id, result: res.data });
        } else {
          sendWsCommandResponse(socket, data.id, { type: res.ok ? 'result' : 'error', id: data.id, result: res.data ?? res.results ?? res, error: res.error });
        }
      } else if (typeof code === 'string') {
        // Plain JS code（把当前 socket 传进去，避免 await 之后读到已被置空的全局 ws）
        await handleWsExec(data, socket);
      } else if (typeof code === 'object' && code !== null) {
        // Object without cmd → legacy extension message
        const msg = code.tabId === undefined && data.tabId !== undefined ? { ...code, tabId: data.tabId } : code;
        const res = await handleExtMessage(msg, {});
        sendWsCommandResponse(socket, data.id, { type: res.ok ? 'result' : 'error', id: data.id, result: res.data ?? res.results ?? res, error: res.error });
      } else {
        sendWsCommandResponse(socket, data.id, { type: 'error', id: data.id, error: 'code 必须是对象或字符串' });
      }
    } catch (e) {
      console.error('[TMWD-WS] message handler error', e);
      sendWsCommandResponse(socket, data.id, {
        type: 'error',
        id: data.id,
        error: { name: e.name || 'Error', message: e.message || String(e), stack: e.stack || '' }
      });
    } finally {
      commandInFlight = Math.max(0, commandInFlight - 1);
      // 没有产生响应（例如 ack 都没发出去）的命令不能让登记永久停在 running：
      // 超时后由 sweep 清掉；这里只在确实没登记结果时放开，允许 daemon 重试。
      wsCommandReleaseIfRunning(data.id);
    }
  });
  socket.onclose = async (event) => {
    // ：整个 onclose 都必须用 `ws === socket` 守卫。旧 socket 迟到的 close
    // 曾经也会执行 detachAllDebugSessions('ws-close')，把**新连接**刚重建的
    // Fetch/Network 会话和 attach 全拆掉（表现为「刚连上就丢拦截、横幅又消失」）。
    if (ws !== socket) {
      console.log('[TMWD-WS] 忽略过期 socket 的 close 事件');
      return;
    }
    try {
      await onSocketClosed(socket, event);
    } catch (e) {
      // onclose 里的任何兜底动作（detach / persist / clear alarm）都不允许
      // 把重连调度一起带走 —— 单次异常不能变成「永久断连、只能手动重载扩展」。
      console.warn('[TMWD-WS] onclose 处理异常（已兜底，恢复调度不受影响）:', (e && e.message) || e);
    } finally {
      try { scheduleProbe(); } catch (e) { console.warn('[TMWD-WS] scheduleProbe 失败:', e && e.message); }
      // 重启场景（daemon 升级 / 手动 restart）下这条路径最常见，必须快重连。
      try { scheduleFastReconnect(); } catch (e) { console.warn('[TMWD-WS] scheduleFastReconnect 失败:', e && e.message); }
    }
  };
  socket.onerror = (e) => {
    console.warn('[TMWD-WS] Connection warning:', e);
    // onclose will fire after this, which triggers reconnect
  };
}

/// onclose 的实质处理，每一步各自兜底：任何一步失败都不影响其它步骤，
/// 真正的恢复调度由调用方的 finally 保证。
async function onSocketClosed(socket, event) {
  console.log('[TMWD-WS] Disconnected');
  ws = null;
  // 这条 socket 已经没了：等 ws_hello 的定时器必须一起撤掉，否则 5s 后会去
  // 对一条已关闭的 socket 做鉴权失败处理（重复排一次 probe）。
  clearDaemonAuth();
  forgetDebugRestoreMemo();
  // 连接没了，常驻心跳 alarm 必须停掉（否则每次唤醒都是空转）。
  try {
    await clearWsHeartbeatAlarm();
  } catch (e) {
    console.warn('[TMWD-WS] 清理心跳 alarm 失败:', e && e.message);
  }
  // 1006 基本上是握手阶段就被拒（token 不对 / 扩展未信任 / daemon 重启中），
  // 这条信息是用户唯一的线索，务必记下来（URL 已脱敏）。
  try {
    await recordWsConnectError('closed', describeWsClose(event), {
      code: event && event.code !== undefined ? event.code : null,
      wasClean: !!(event && event.wasClean)
    });
  } catch (e) {
    console.warn('[TMWD-WS] 记录断开原因失败:', e && e.message);
  }
  // daemon crash/kill 时 server.rs 无法发送 debugClearAll，扩展必须自行释放 CDP attach。
  try {
    await detachAllDebugSessions('ws-close');
  } catch (e) {
    console.warn('[TMWD-WS] 断线后释放调试会话失败（重连调度不受影响）:', e && e.message);
  }
  // 断线保留快照（detach 走 keepPersisted），这里再把「本 SW 动过的 tab」落一次盘，
  // 避免下次冷启动读到半截状态。失败只影响重启自愈。
  try {
    await persistDebugInterceptState();
  } catch (e) {
    console.warn('[TMWD-FETCH] 断线后持久化拦截状态失败:', e && e.message);
  }
  try {
    await chrome.alarms.clear('tmwd-ws-keepalive');
  } catch (e) {
    console.warn('[TMWD-WS] 清理 keepalive alarm 失败:', e && e.message);
  }
}

function reconnectWS() {
  const previous = ws;
  // 必须走 discardSocket（摘掉 onopen/onmessage/onclose 再 close）。
  // 旧实现只置空 onclose，旧 socket 残留的 onmessage 仍会把迟到帧当新命令执行，
  // 与 onclose 里已有的 `ws !== socket` 守卫形成不对称。
  ws = null;
  // 换代时旧 socket 的 ws_hello 定时器同样作废，否则它会误判新连接鉴权失败。
  clearDaemonAuth();
  if (previous) discardSocket(previous, 'reconnect');
  // 主动换代时把心跳 / 保活 / probe 的排期一起作废，
  // 旧 socket 的 timer 与 alarm 不该在新连接上继续空转。
  clearWsHeartbeatAlarm().catch(() => null);
  cancelFastReconnect();
  for (const name of ['tmwd-ws-probe', 'tmwd-ws-keepalive']) {
    try { chrome.alarms?.clear?.(name)?.catch?.(() => null); } catch (_) {}
  }
  connectWS().catch((e) => console.warn('[TMWD-WS] reconnect failed:', e && e.message));
}

// 配置变更（token / 端口 / 信任档位）后立即重建连接。
// 走 storage.onChanged 是刻意的：popup、控制台、未来的 CLI 都写同一个 storage，
// 只有这里统一响应，才不会出现「某个入口改了配置却不生效」。
// 以前只监听 wsToken，wsPort 改了必须等下一次 SW 重启才生效。
chrome.storage?.onChanged?.addListener?.((changes, area) => {
  if (area !== 'local' || !changes) return;
  bgTask('storage.onChanged', () => {
    let reconnect = false;
    if (changes.wsToken) {
      const next = String(changes.wsToken.newValue || '').trim();
      if (next !== wsToken) {
        wsToken = next;
        console.log('[TMWD-WS] wsToken 已' + (wsToken ? '设置' : '清空') + '，重建连接');
        reconnect = true;
      }
    }
    if (changes.wsPort) {
      const raw = Number(changes.wsPort.newValue);
      if (Number.isInteger(raw) && raw > 0 && raw !== wsPort) {
        wsPort = raw;
        console.log('[TMWD-WS] wsPort 已改为', wsPort, '，重建连接');
        reconnect = true;
      }
    }
    if (changes.wsTrustMode) {
      const next = String(changes.wsTrustMode.newValue || '').trim().toLowerCase();
      if (WS_TRUST_MODES.has(next) && next !== wsTrustMode) {
        wsTrustMode = next;
        console.log('[TMWD-WS] wsTrustMode 已改为', wsTrustMode);
      }
    }
    if (reconnect) reconnectWS();
  });
});

// Initial connect + wake-up hooks
// 这些都是「启动顺手做的事」，失败不能把 SW 整个带下去：统一吞掉 rejection 只记日志。
const swallow = (label) => (e) => {
  // 记录到 storage：Service Worker 启动期抛错时，浏览器只在扩展页显示
  // "Service worker registration failed"，用户看不到具体原因（Edge 尤其常见）。
  // popup 会读这个键，把最近一次启动错误显示出来。
  try {
    chrome.storage?.local?.set?.({
      lastServiceWorkerError: {
        at: new Date().toISOString(),
        label,
        message: (e && (e.message || String(e))) || 'unknown',
      },
    });
  } catch (_) {}
  console.warn(`[TMWD] ${label} failed:`, e && e.message);
};

// 用 globalThis 而不是裸 self：Service Worker 里有 self，测试沙箱里没有。
const swGlobal = typeof globalThis !== 'undefined' ? globalThis : undefined;
swGlobal?.addEventListener?.('error', (event) => {
  try {
    chrome.storage?.local?.set?.({
      lastServiceWorkerError: {
        at: new Date().toISOString(),
        label: 'unhandled',
        message: (event && (event.message || String(event.error))) || 'unknown',
      },
    });
  } catch (_) {}
});
swGlobal?.addEventListener?.('unhandledrejection', (event) => {
  try {
    chrome.storage?.local?.set?.({
      lastServiceWorkerError: {
        at: new Date().toISOString(),
        label: 'unhandledrejection',
        message: (event && (event.reason && (event.reason.message || String(event.reason)))) || 'unknown',
      },
    });
  } catch (_) {}
});
// SW 每次求值都 +1：随 ext_ready 上报，仅供 daemon 做「换代检测」（见上方 RESTART_SEQ 注释）。
ensureRestartSeq().catch(swallow('restartSeq'));
// 冷启动恢复：debugSessions 是内存态，但浏览器侧的 Fetch 拦截 / Network / Runtime 采集
// 仍然生效。恢复由扩展自己完成（单飞 promise，requestPaused 也会 await 它，见），
// 必须在 connectWS 之外独立跑，且失败不影响后续启动流程。
// alarm 唤醒也会重新求值本模块，这里用「本浏览器会话已恢复过」标记避免反复 attach。
ensureDebugSessionsRestoredOnBoot().catch(swallow('restoreDebugSessions'));
connectWS().catch(swallow('connectWS'));
syncPersistBoot().catch(swallow('syncPersistBoot'));
injectContentScriptsIntoExistingTabs().catch(swallow('injectContentScripts'));
// 每分钟比对一次 background.js 的内容哈希：磁盘有改动就在空闲时自我重载。
chrome.alarms?.create?.(SELF_CHECK_ALARM, { periodInMinutes: 1 });
checkSelfUpdate({ force: true }).catch(swallow('checkSelfUpdate'));
chrome.runtime?.onStartup?.addListener?.(() => {
  connectWS().catch(swallow('connectWS'));
  injectContentScriptsIntoExistingTabs().catch(swallow('injectContentScripts'));
});
chrome.runtime?.onInstalled?.addListener?.(() => {
  connectWS().catch(swallow('connectWS'));
  injectContentScriptsIntoExistingTabs().catch(swallow('injectContentScripts'));
});

// Sync tab list on changes
async function sendTabsUpdate() {
  // 不再因为「此刻没连上」就 return —— 断线期间的标签变化会随 socketSend 入队，
  // 重连后按序补发；onopen 之后还会再排一次，保证最终状态一定是最新的。
  const socket = ws;
  // 与 ext_ready 用同一个过滤函数：两处口径不一致会让 daemon 看到 tab「先出现后消失」。
  const tabs = filterReportableTabs(await queryScriptableTabs());
  // 分组标题只在有 tabGroups API 时批量取一次，避免每个标签页各查一次。
  const groupTitles = await queryTabGroupTitles();
  await ensureClientIdentity();
  socketSend(socket, withClientIdentity({
    type: 'tabs_update',
    protocol: EXTENSION_PROTOCOL,
    extension_version: extensionVersion,
    file_scheme_access: await isAllowedFileSchemeAccess(),
    tabs: tabs.map(t => ({ id: t.id, window_id: t.windowId, url: t.url, title: t.title, active: t.active, group_title: groupTitles ? tabGroupTitle(groupTitles, t) : null }))
  }));
}
// Sync tab list on changes。
// 这里**不做 setTimeout 去抖**：定时器只在内存里，SW 一被回收这次更新就
// 永久丢失，daemon 侧的 tab 列表会一直停在旧状态（表现为「刚打开的标签页看不到」）。
// tabs 事件本身不密集（一次导航最多 onCreated/onUpdated/onRemoved 几条），用
// 「同时只跑一个、期间有新事件就在结束后补一次」替代，既不刷屏也不会丢。
let tabsUpdateRunning = false;
let tabsUpdatePending = false;
function scheduleTabsUpdate() {
  if (tabsUpdateRunning) {
    tabsUpdatePending = true;
    return;
  }
  tabsUpdateRunning = true;
  sendTabsUpdate()
    .catch(swallow('sendTabsUpdate'))
    .finally(() => {
      tabsUpdateRunning = false;
      if (tabsUpdatePending) {
        tabsUpdatePending = false;
        scheduleTabsUpdate();
      }
    });
}
// 持久注入：每次主文档提交后重放（MV3 不能注册内联代码，只能这样"补钩子"）。
// webNavigation 缺失时整段跳过：没有它只是「补钩子晚一点」，不该让 SW 求值阶段崩掉。
if (chrome.webNavigation?.onCommitted?.addListener) {
  chrome.webNavigation.onCommitted.addListener((details) => {
    if (details.frameId !== 0) return;
    // 这个监听器整段是异步的（持久注入重放），任何一步抛错都必须兜住，
    // 否则会变成未处理拒绝，且失败点完全不可见。
    bgTask('persistReinject', async () => {
      // boot 脚本已经覆盖 document_start 时不再重复执行，避免 hook 被套两层。
      if (persistBootReady) return;
      // persistBootReady 只是内存标志，SW 冷启动窗口内它是 false，而 boot 脚本可能
      // 已经在跑 → 这里再重放一次就会执行两遍。用「本次导航」的一次性去重挡住。
      if (!(await claimPersistInjection(details.tabId, details.frameId, details.url))) return;
      // 先清过期条目，避免把死配置当成「这页有注入」反复重放。
      let map = {};
      try {
        const pruned = await pruneExpiredPersistInjections();
        map = pruned.map;
      } catch (e) {
        const store = await chrome.storage.local.get({ persistInjections: {} });
        map = store.persistInjections || {};
      }
      for (const [name, entry] of Object.entries(map)) {
        if (isPersistEntryExpired(entry)) continue;
        if (!matchesPersistPattern(entry.matches, details.url)) continue;
        try {
          const result = await applyPersistEntry(details.tabId, entry, { tabId: details.tabId });
          const outcome = result.outcome;
          if (!outcome.ok) console.log('[TMWD] persist reinject failed:', name, outcome.error);
        } catch (e) {
          console.log('[TMWD] persist reinject failed:', name, e.message);
        }
      }
    });
  });
} else {
  console.warn('[TMWD] webNavigation.onCommitted 不可用：持久注入只能靠 document_start 的 persist_boot.js（manifest 需要 "webNavigation" 权限）');
}

chrome.tabs?.onUpdated?.addListener?.((_, changeInfo) => {
  if (changeInfo.status === 'complete') scheduleTabsUpdate();
});
chrome.tabs?.onRemoved?.addListener?.((tabId) => {
  // 标签关闭时 Chrome 会自动 detach（onDetach 会到），这里只需要把会话状态从表里摘掉：
  // 否则 debugSessions 会为每个关闭过的标签留一份 requests/logs，越积越大。
  chrome.alarms?.clear?.(debugDetachAlarmName(tabId));
  debugSessions.delete(tabId);
  // 同步删掉持久化条目：tab 已经不在了，restore 只会白试一次（增量写入口）。
  markDebugTabTouched(tabId);
  persistDebugInterceptState().catch(() => null);
  scheduleTabsUpdate();
});
chrome.tabs?.onCreated?.addListener?.(() => scheduleTabsUpdate());
// 切换标签也必须刷新。`tabs[].active` 只由 daemon 侧的快照提供，缺这条监听时
// 用户切到别的标签后 daemon 仍认为旧标签是 active，`atb --tab current` 会打到上一个标签。
// sendTabsUpdate 每次都是全量快照（含 active 字段），所以不存在「增量不覆盖」的问题。
chrome.tabs?.onActivated?.addListener?.(() => scheduleTabsUpdate());

// 启动自检：可选 API 缺失时明确报出来，避免「popup 只显示未连接、毫无线索」。
warnMissingApis();

function warnMissingApis() {
  const missing = [
    ['debugger', chrome.debugger?.attach],
    ['webNavigation', chrome.webNavigation?.onCommitted],
    ['scripting', chrome.scripting?.executeScript],
    ['downloads', chrome.downloads?.download],
    ['tabs', chrome.tabs?.query],
    ['alarms', chrome.alarms?.create],
    ['tabGroups', chrome.tabGroups?.update]
  ].filter(([, present]) => !present).map(([name]) => name);
  if (missing.length) {
    console.warn('[TMWD] 以下 chrome API 不可用，相关命令会降级或失败：' + missing.join(', '));
  }
  return missing;
}
