// 假扩展测试桩：不依赖 Chrome，直接实现 daemon 的 WebSocket 协议。
//
// 用途：
//   1. 作为 e2e 测试的浏览器替身，验证 open / exec / snapshot / job 等链路；
//   2. 作为二次开发时的协议回归工具（真实 Chrome 不方便进 CI）。
//
// 直接运行:  node tests/mock_extension.mjs
// 自测:      node tests/mock_extension.mjs --self-test
import http from "node:http";
import net from "node:net";
import crypto from "node:crypto";
import os from "node:os";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const DEFAULT_EXT_PORT = Number(process.env.EXT_PORT || 18765);
const ORIGIN = "chrome-extension://" + "a".repeat(32);
const BROWSER_ID = "browser-mock";
const PROFILE_ID = "profile-mock";
// 假扩展必须跟**磁盘上的真实清单**同版本：daemon 的 `extension.stale` 是用
// 「连接方上报的版本 vs 编译进二进制的 manifest 版本」比出来的，写死一个常量
// 会在清单升级后立刻变成「版本不一致」的假红（曾经写死 3.4，清单已经是 5.x）。
const EXT_VERSION = (() => {
  try {
    const here = path.dirname(fileURLToPath(import.meta.url));
    const manifest = JSON.parse(
      fs.readFileSync(path.join(here, "..", "assets", "atb_bridge", "manifest.json"), "utf8"),
    );
    return String(manifest.version || "0");
  } catch {
    return "0";
  }
})();

class WsClient {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.onmessage = null;
    this.onclose = null;
    socket.on("data", (chunk) => this.#consume(chunk));
    socket.on("close", () => this.onclose?.());
    socket.on("error", () => this.onclose?.());
  }

  #consume(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    for (;;) {
      if (this.buffer.length < 2) return;
      const first = this.buffer[0];
      const second = this.buffer[1];
      const opcode = first & 0x0f;
      const masked = (second & 0x80) !== 0;
      let length = second & 0x7f;
      let offset = 2;
      if (length === 126) {
        if (this.buffer.length < 4) return;
        length = this.buffer.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        if (this.buffer.length < 10) return;
        length = Number(this.buffer.readBigUInt64BE(2));
        offset = 10;
      }
      let mask = null;
      if (masked) {
        if (this.buffer.length < offset + 4) return;
        mask = this.buffer.subarray(offset, offset + 4);
        offset += 4;
      }
      if (this.buffer.length < offset + length) return;
      const payload = Buffer.from(this.buffer.subarray(offset, offset + length));
      if (mask) for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
      this.buffer = this.buffer.subarray(offset + length);
      if (opcode === 0x1) this.onmessage?.(payload.toString("utf8"));
      else if (opcode === 0x8) this.socket.end();
      else if (opcode === 0x9) this.#frame(payload, 0x0a);
    }
  }

  #frame(data, opcode = 0x1) {
    const payload = Buffer.isBuffer(data) ? data : Buffer.from(data);
    const length = payload.length;
    let header;
    if (length < 126) {
      header = Buffer.from([0x80 | opcode, 0x80 | length]);
    } else if (length < 65536) {
      header = Buffer.alloc(4);
      header[0] = 0x80 | opcode;
      header[1] = 0x80 | 126;
      header.writeUInt16BE(length, 2);
    } else {
      header = Buffer.alloc(10);
      header[0] = 0x80 | opcode;
      header[1] = 0x80 | 127;
      header.writeBigUInt64BE(BigInt(length), 2);
    }
    const mask = crypto.randomBytes(4);
    const masked = Buffer.alloc(length);
    for (let i = 0; i < length; i += 1) masked[i] = payload[i] ^ mask[i % 4];
    this.socket.write(Buffer.concat([header, mask, masked]));
  }

  send(value) {
    this.#frame(typeof value === "string" ? value : JSON.stringify(value));
  }
}

function connectOnce(port) {
  return new Promise((resolve, reject) => {
    const request = http.request({
      host: "127.0.0.1",
      port,
      path: "/",
      headers: {
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Key": crypto.randomBytes(16).toString("base64"),
        "Sec-WebSocket-Version": "13",
        Origin: ORIGIN,
      },
    });
    request.on("upgrade", (_response, socket) => resolve(new WsClient(socket)));
    request.on("error", reject);
    request.end();
  });
}

/**
 * 启动假扩展；daemon 未就绪时自动重试连接。
 * @returns {{ close: () => void, state: { tabs: Array, polls: number, commands: string[] } }}
 */
export function startMockExtension({ waitForReady = true, port = DEFAULT_EXT_PORT } = {}) {
  const state = {
    tabs: [{ id: 101, url: "https://mock.local/start", title: "Mock Start" }],
    nextTabId: 102,
    polls: 0,
    waitCalls: 0,
    waitAfter: Number(process.env.MOCK_WAIT_AFTER || 5),
    // challenge 场景：cleared（默认，无验证码）/ widget-cleared / token / need-human / timeout。
    challengeScenario: process.env.MOCK_CHALLENGE || "cleared",
    // 页面侧 net 规则（mock 的 __abcNet route/hook/reset 会读写它）。
    netRoutes: [],
    // challenge wait 每次下发的参数快照（need-human 场景用来看 focusAfterMs 是否递减）。
    challengeSlices: [],
    commands: [],
  };
  let ws = null;
  let closed = false;
  let retryTimer = null;

  const sendTabsUpdate = () => {
    if (!ws) return;
    ws.send({
      type: "tabs_update",
      browser_id: BROWSER_ID,
      profile_id: PROFILE_ID,
      extension_version: EXT_VERSION,
      file_scheme_access: true,
      tabs: state.tabs.map((tab) => ({ id: tab.id, url: tab.url, title: tab.title })),
    });
  };

  const handleUserCode = (code, tabId) => {
    const tab = state.tabs.find((item) => item.id === Number(tabId)) || state.tabs[0];
    // 结构化读取 / 查找 / 动作：返回与真实页面脚本同形的数据，
    // 用来覆盖 daemon 路由、参数映射与 CLI 输出，不依赖真实浏览器。
    if (/__abcRead\(/.test(code)) {
      return {
        ok: true,
        url: tab?.url || "https://example.com/mock",
        title: tab?.title || "Mock Page",
        lang: "en",
        description: "mock description",
        textLength: 42,
        text: "Mock page text for read",
        truncated: false,
        headings: [{ tag: "h1", text: "Mock Page" }],
        counts: { links: 2, images: 0, inputs: 1, buttons: 1, forms: 1 },
        forms: [{ action: "", method: "GET", fields: [{ name: "q", type: "text", value: "", placeholder: "search", required: false }] }],
      };
    }
    if (/__abcFind\(/.test(code)) {
      return {
        ok: true,
        matched: 2,
        returned: 2,
        items: [
          { tag: "a", text: "More", href: "https://example.com/more", selector: "a.more", rect: { x: 10, y: 20, w: 40, h: 16 }, disabled: false },
          { tag: "button", text: "Go", href: null, selector: "button.go", rect: { x: 10, y: 60, w: 60, h: 24 }, disabled: false },
        ],
      };
    }
    if (/__abcDescribe/.test(code)) {
      // `snapshot --js --details` 的补详情脚本：daemon 一次性下发全部选择器，
      // 页面内逐个 describe 后回 `{ok, details:{ref: info}}`。
      const targetsMatch = code.match(/const __abcTargets = (\[[\s\S]*?\]);/);
      let targets = [];
      try {
        targets = targetsMatch ? JSON.parse(targetsMatch[1]) : [];
      } catch {
        targets = [];
      }
      const details = {};
      for (const [ref, selector] of targets) {
        details[ref] = {
          tag: selector === "#q" ? "input" : "button",
          text: selector === "#q" ? "" : "Go",
          selector,
          rect: selector === "#q" ? { x: 1, y: 30, w: 120, h: 20 } : { x: 1, y: 2, w: 40, h: 20 },
          visible: true,
        };
      }
      return { ok: true, details };
    }
    if (/__abcSnapshot\(/.test(code)) {
      return {
        ok: true,
        url: tab?.url || "https://example.com/mock",
        title: tab?.title || "Mock Page",
        mode: "js",
        total: 2,
        truncated: false,
        items: [
          { ref: "@e1", role: "button", name: "Go", tag: "button", selector: "#go", rect: { x: 1, y: 2, w: 40, h: 20 } },
          { ref: "@e2", role: "textbox", name: "search", tag: "input", selector: "#q", rect: { x: 1, y: 30, w: 120, h: 20 } },
        ],
      };
    }
    if (/__abcWait\(/.test(code)) {
      // wait --js 现在是「一次注入 + 页面内反复求值」，用 waitCalls 盯住注入次数。
      state.waitCalls += 1;
      const call = code.match(/__abcWait\((\{[\s\S]*\})\);\s*$/);
      let waitOpts = {};
      try {
        waitOpts = call ? JSON.parse(call[1]) : {};
      } catch {
        waitOpts = {};
      }
      if (/__mockNever/.test(String(waitOpts.js || ""))) {
        return {
          ok: false,
          matched: false,
          checks: 3,
          polls: 3,
          mode: "js-observer",
          error: "timeout",
          reason: "timeout",
          elapsed_ms: Number(waitOpts.timeoutMs || 0),
        };
      }
      return {
        ok: true,
        matched: true,
        checks: 1,
        polls: 1,
        mode: waitOpts.js ? "js-observer" : "observer",
        value: true,
        elapsed_ms: 1,
      };
    }
    // 旧式「daemon 每轮注入一次谓词」的桩，保留给其他用例；
    // 必须放在 __abcWait 之后，否则会抢先匹配等待脚本里内嵌的 js 源码。
    if (/__mockWait/.test(code)) {
      state.polls += 1;
      return state.polls >= state.waitAfter;
    }
    if (/__abcNet\(/.test(code)) {
      // 取**最后一次** `__abcNet({...})` 的实参。
      //
      // 不能用 `match(...$/greedy)`：page_net.js 的注释里就写着 `return __abcNet({})`，
      // 贪婪匹配会从注释那处一路吞到文件末尾，JSON.parse 必然失败 → 所有 action 都
      // 退化成 fetch（自检里 net route/log 全部假红）。这里用非贪婪 + 取最后一次。
      const calls = [...code.matchAll(/__abcNet\((\{[\s\S]*?\})\)/g)];
      let netOpts = {};
      if (calls.length) {
        try {
          netOpts = JSON.parse(calls[calls.length - 1][1]);
        } catch {
          netOpts = {};
        }
      }
      const netAction = String(netOpts.action || "fetch");
      const isLog = netAction === "log";
      const isHook = netAction === "hook";
      const isRoute = netAction === "route";
      const isReset = netAction === "reset";
      if (isLog) {
        return {
          ok: true,
          action: "log",
          installed: true,
          count: 1,
          items: [{ id: 1, kind: "fetch", url: "https://example.com/mocked", method: "GET", status: 201, mocked: true, ok: true }],
        };
      }
      if (isHook) {
        // 真实 page_net 的 hook 回包同时给计数与真实数组（的回归点：
        // daemon 必须持久化 routes_list，而不是把计数当规则存进去）。
        if (Array.isArray(netOpts.routes)) state.netRoutes = netOpts.routes.slice();
        return {
          ok: true,
          action: "hook",
          installed: true,
          capture_bodies: false,
          routes: state.netRoutes.length,
          count: state.netRoutes.length,
          routes_list: state.netRoutes.slice(),
        };
      }
      if (isRoute) {
        const rule = netOpts.rule && typeof netOpts.rule === "object" ? netOpts.rule : { match: "/mocked", status: 201 };
        const at = state.netRoutes.findIndex((item) => item && item.match === rule.match);
        if (at >= 0) state.netRoutes[at] = rule;
        else state.netRoutes.push(rule);
        // 与 page_net.js 严格同形：route 只给 all，没有 routes_list
        // （routes_list 只属于 hook/routes，见 tests/page_net_contract.test.mjs）。
        return {
          ok: true,
          action: "route",
          routes: state.netRoutes.length,
          rule,
          all: state.netRoutes.slice(),
        };
      }
      if (isReset) {
        state.netRoutes = [];
        return { ok: true, action: "reset", cleared_log: 1 };
      }
      return {
        ok: true,
        action: "fetch",
        url: "https://example.com/mock",
        method: "GET",
        status: 200,
        status_text: "OK",
        headers: { "content-type": "application/json" },
        body: '{"mock":true}',
        encoding: "text",
        truncated: false,
        total_chars: 13,
        json: { mock: true },
        elapsed_ms: 1,
      };
    }
    if (/__abcAct\(/.test(code)) {
      const opMatch = code.match(/__abcAct\("([a-zA-Z]+)"/);
      return {
        ok: true,
        op: opMatch ? opMatch[1] : "action",
        tag: "a",
        text: "More",
        rect: { x: 10, y: 20, w: 40, h: 16 },
        hit: { tag: "a", matches: true, covered_by: null },
      };
    }
    if (/__abcChallenge\(/.test(code)) {
      // 反爬/验证码脚本（page_challenge.js）。返回契约与页面脚本定稿一致：
      //   - 页面脚本永不返回 passed；detected 只出现在 status 里；
      //   - daemon 的 solve 循环只认 `cleared === true`，缺字段会一直重注入到超时，
      //     所以每个分支都必须给出明确的 cleared。
      // 场景由 state.challengeScenario 选择（默认 cleared，等价「本来就没验证码」）。
      const scenario = state.challengeScenario || "cleared";
      let opts = {};
      try {
        const parsed = code.match(/__abcChallenge\((\{[\s\S]*\})\)/);
        if (parsed) opts = JSON.parse(parsed[1]);
      } catch {
        /* 用默认值 */
      }
      // 每个场景的 status 形状决定 daemon 走快路径还是进 wait 循环（src/server.rs 的
      // `!blocking && (token_ready || !interactive)`）：
      //   cleared         → 命中快路径，via='no-challenge'、injections=0、challenge=status payload
      //   widget-cleared  → 卡住快路径（interactive && !token_ready），循环里 wait 不带 via → 兜成 'clear'
      //   token           → 已有令牌，status 直接给出 token_ready，快路径返回 via='token'
      //   need-human / timeout → 永不 cleared，直到超时
      const PROFILES = {
        cleared: { detected: false, blocking: false, interactive: false, token: false, vendor: null, kind: "unknown" },
        "widget-cleared": { detected: true, blocking: false, interactive: true, token: false, vendor: "turnstile", kind: "widget" },
        token: { detected: true, blocking: true, interactive: true, token: true, vendor: "turnstile", kind: "widget" },
        "need-human": { detected: true, blocking: true, interactive: true, token: false, vendor: "turnstile", kind: "widget" },
        timeout: { detected: true, blocking: true, interactive: true, token: false, vendor: "turnstile", kind: "widget" },
      };
      const profile = PROFILES[scenario] || PROFILES.timeout;
      const base = {
        ok: true,
        detected: profile.detected,
        blocking: profile.blocking,
        interactive: profile.interactive,
        title: "Mock Page",
        url: tab?.url || "https://mock.local/",
      };
      const tokenFields = profile.token ? [{ name: "cf-turnstile-response", length: 982 }] : [];
      const action = opts.action === "detect" ? "status" : String(opts.action || "wait");

      if (action === "status") {
        return {
          ...base,
          action: "status",
          detected: profile.detected,
          pending: false,
          token_ready: profile.token,
          token_fields: tokenFields,
          vendor: profile.vendor,
          kind: profile.kind,
          blocking: profile.blocking,
          interactive: profile.interactive,
          auto: false,
          frames: [],
          signals: profile.detected ? ["turnstile-widget"] : [],
          hints: profile.detected ? ["页面里存在验证码组件"] : ["未检测到验证码"],
        };
      }

      if (scenario === "cleared" || scenario === "widget-cleared") {
        // 故意不带 via：daemon 会兜成 via='clear'（widget-cleared 的路径），
        // 而 cleared 场景根本不会走到 wait（快路径已提前返回）。
        return {
          ...base,
          action: "wait",
          cleared: true,
          vendor: profile.vendor,
          blocking: false,
          interactive: profile.interactive,
          checks: scenario === "cleared" ? 1 : 2,
          elapsed_ms: scenario === "cleared" ? 4 : 60,
        };
      }
      if (scenario === "token") {
        return {
          ...base,
          action: "wait",
          cleared: true,
          via: "token",
          vendor: "turnstile",
          token_fields: tokenFields,
          blocking: false,
          interactive: true,
          checks: 3,
          elapsed_ms: 12,
        };
      }
      if (scenario === "need-human") {
        // 复刻页面脚本的判定（page_challenge.js:392）：单片 wait 内部最多只过去
        // timeoutMs，所以只有当 focusAfterMs 落在这一片之内，才会报 need_human；
        // 否则这一片就是普通的「还没好」。
        const focusAfterMs = Number(opts.focusAfterMs ?? 0);
        const sliceMs = Number(opts.timeoutMs ?? 4000);
        // 记录每次下发的参数：daemon 是否正确做了「按已用时间递减」可以直接从这里看出来，
        // 不依赖墙钟（mock 不 sleep，8s 的真实累计在自检里无法发生）。
        state.challengeSlices.push({ focusAfterMs, timeoutMs: sliceMs });
        const canAskHuman = focusAfterMs > 0 && focusAfterMs <= sliceMs;
        if (!canAskHuman) {
          return {
            ...base,
            action: "wait",
            cleared: false,
            reason: "timeout",
            vendor: "turnstile",
            hints: ["页面里存在验证码组件：可正常浏览，提交前需要人完成"],
            checks: 4,
            elapsed_ms: sliceMs,
          };
        }
        return {
          ...base,
          action: "wait",
          cleared: false,
          reason: "need_human",
          need_human: true,
          vendor: "turnstile",
          hints: ["验证码需要人工完成"],
          checks: 2,
          elapsed_ms: Math.max(1, focusAfterMs),
        };
      }
      // 默认落到 timeout：验证码一直没完成。
      return {
        ...base,
        action: "wait",
        cleared: false,
        reason: "timeout",
        hints: ["页面里存在验证码组件：可正常浏览，提交前需要人完成"],
        checks: 8,
        elapsed_ms: 2000,
      };
    }
    // --monitor 的变更观察器（src/server.rs 的 monitor_start/stop_script）。
    // 装不上时 daemon 会判成 Unsupported，所以这里必须回 status='watching'。
    // state.monitorMode 用来模拟三种降级：unsupported（装不上）/ navigated / superseded。
    if (/__abcMonitor__/.test(code)) {
      const mode = state.monitorMode || "ok";
      if (/status:\s*'watching'/.test(code)) {
        return mode === "unsupported" ? { status: "unsupported" } : { status: "watching" };
      }
      if (mode === "navigated" || mode === "superseded") return { status: mode };
      return {
        status: "ok",
        watch: {
          changed: 2,
          added: 1,
          removed: 0,
          attributes: 1,
          text: 0,
          samples: [{ type: "childList", target: "div#mock", detail: "+span" }],
        },
      };
    }
    if (/document\.querySelector/.test(code)) return true;
    if (/location\.href/.test(code)) return tab?.url || "about:blank";
    if (/document\.title/.test(code)) return tab?.title || "";
    if (/optHTML|findMainList|document\.body/.test(code)) {
      return "<html><body><h1>Mock Page</h1><ul><li>a</li><li>b</li></ul></body></html>";
    }
    const literal = code.match(/^\s*return\s+(-?\d+|true|false|null|"([^"]*)"|'([^']*)')\s*;?\s*$/);
    if (literal) {
      const raw = literal[1];
      if (raw === "true") return true;
      if (raw === "false") return false;
      if (raw === "null") return null;
      if (raw.startsWith('"') || raw.startsWith("'")) return raw.slice(1, -1);
      return Number(raw);
    }
    return { ok: true, echo: code.slice(0, 60) };
  };

  // 页面 CSP 禁止 eval 时，daemon 改走 `pageOp`：扩展注入 ops/<op>.js 并调用
  // `__abc<Op>(args)`（见 src/server.rs 的 page_op_via_extension）。
  // 这里把 op 还原成同一份注入脚本的调用串，直接复用 handleUserCode，保证返回
  // 形状与 eval 通道完全一致。
  const PAGE_OP_ENTRY = {
    read: "__abcRead",
    find: "__abcFind",
    snapshot: "__abcSnapshot",
    wait: "__abcWait",
    net: "__abcNet",
    action: "__abcAct",
    challenge: "__abcChallenge",
  };

  const handlePageOp = (op, args, tabId) => {
    const entry = PAGE_OP_ENTRY[op];
    if (!entry) return { ok: true, op, mode: "mock" };
    const payload = args && typeof args === "object" ? args : {};
    const call =
      op === "action"
        ? `${entry}("${payload.op || "click"}", ${JSON.stringify(payload)})`
        : `${entry}(${JSON.stringify(payload)})`;
    const value = handleUserCode(`${call};`, tabId);
    // extension_control 的回包形状是 { ok, value }。
    return value && typeof value === "object" && "ok" in value ? value : { ok: true, value };
  };

  const handlePlainJs = (code, tabId) => {
    // daemon 会把用户脚本包成 async IIFE（`const __abcCode = "…"`），这里还原出原始脚本。
    const wrapped = code.match(/const __abcCode = ("(?:[^"\\]|\\.)*");/);
    if (wrapped) {
      try {
        return handleUserCode(JSON.parse(wrapped[1]), tabId);
      } catch {
        /* 交给下面的兜底 */
      }
    }
    return handleUserCode(code, tabId);
  };

  const handleCdp = (code) => {
    if (code.method === "Accessibility.getFullAXTree") {
      return {
        ok: true,
        data: {
          nodes: [
            {
              nodeId: "1",
              childIds: ["2"],
              backendDOMNodeId: 1,
              role: { value: "RootWebArea", type: "role" },
              name: { value: "Mock", type: "computedString" },
            },
            {
              nodeId: "2",
              childIds: [],
              backendDOMNodeId: 2,
              role: { value: "button", type: "role" },
              name: { value: "提交", type: "computedString" },
            },
          ],
        },
      };
    }
    if (code.method === "Page.captureScreenshot") {
      // 1x1 透明 PNG
      return {
        ok: true,
        data: {
          data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
        },
      };
    }
    if (code.method === "Runtime.evaluate" || code.method === "Runtime.callFunctionOn") {
      return { ok: true, data: { result: { value: true } } };
    }
    if (code.method === "Page.getLayoutMetrics") {
      return { ok: true, data: { contentSize: { x: 0, y: 0, width: 1280, height: 800 } } };
    }
    return { ok: true, data: {} };
  };

  const handleMessage = (text) => {
    let message;
    try {
      message = JSON.parse(text);
    } catch {
      return;
    }
    if (!message.id) return;
    const id = message.id;
    if (message.type === "ping") return;
    let code = message.code;
    if (typeof code === "string") {
      try {
        const parsed = JSON.parse(code);
        if (parsed && typeof parsed === "object") code = parsed;
      } catch {
        /* 仍然是普通 JS */
      }
    }

    ws.send({ type: "ack", id });

    const codeText = typeof code === "string" ? code : "";
    if (/__mockDisconnect/.test(codeText)) {
      // 模拟 daemon 重启 / 插件重载：命令执行中途连接断开。
      try {
        ws.socket.end();
      } catch {
        /* ignore */
      }
      return;
    }

    if (code && typeof code === "object") {
      state.commands.push(code.cmd || "?");
      if (code.cmd === "inject") {
        const action = String(code.action || "run");
        if (action === "persist") {
          state.injections = state.injections || {};
          state.injections[code.name] = { world: code.world || "MAIN", code: String(code.code || "") };
          ws.send({ type: "result", id, result: { action, name: code.name, applied: true, count: Object.keys(state.injections).length } });
          return;
        }
        if (action === "remove") {
          state.injections = state.injections || {};
          const existed = !!state.injections[code.name];
          delete state.injections[code.name];
          ws.send({ type: "result", id, result: { action, name: code.name, removed: existed, count: Object.keys(state.injections).length } });
          return;
        }
        if (action === "list") {
          state.injections = state.injections || {};
          const items = Object.entries(state.injections).map(([name, entry]) => ({ name, world: entry.world, code_length: entry.code.length }));
          ws.send({ type: "result", id, result: { action, count: items.length, items } });
          return;
        }
        ws.send({ type: "result", id, result: { action: "run", world: code.world || "MAIN", value: 2, elapsed_ms: 1 } });
        return;
      }
      if (code.cmd === "netIntercept") {
        // 网络层拦截：真实扩展走 chrome.debugger 的 Fetch 域，
        // 这里只记录 daemon 下发了什么，并回同形的结果。
        state.netIntercept = state.netIntercept || [];
        const action = String(code.action || "status");
        const rules = Array.isArray(code.rules) ? code.rules : [];
        state.netIntercept.push({ action, tabId: code.tabId, rules });
        if (action === "start" || action === "rules") state.netInterceptEnabled = true;
        if (action === "stop") state.netInterceptEnabled = false;
        if (action === "start") state.cdpLogCleared = false;
        if (action === "clearlog") state.cdpLogCleared = true;
        // 真实扩展会记「每个被处理过的请求」，这里固定给一条，够 daemon 验证合并逻辑。
        const cdpLog = state.cdpLogCleared
          ? []
          : [{ at: Date.now(), requestId: "cdp-1", url: "https://example.com/cdp-api", method: "GET", stage: "response", action: "rewrite", status: 201, match: "/cdp-api", source: undefined }];
        ws.send({
          type: "result",
          id,
          result: {
            ok: true,
            action,
            tabId: code.tabId,
            rules: rules.length,
            patterns: [{ urlPattern: "*", requestStage: "Request" }],
            fetchEnabled: state.netInterceptEnabled === true,
            ...(action === "status" ? { log: cdpLog, logCount: cdpLog.length } : {}),
            ...(action === "clearlog" ? { logCount: 0 } : {}),
          },
        });
        return;
      }
      if (code.cmd === "downloads") {
        state.downloads = state.downloads || [];
        const action = String(code.action || "list");
        if (action === "start") {
          const downloadId = state.downloads.length + 1;
          state.downloads.push({
            id: downloadId,
            filename: `C:////Users////mock////Downloads////${code.filename || "download.bin"}`,
            url: String(code.url || ""),
            state: "in_progress",
            bytes_received: 0,
            total_bytes: 1024,
            mime: "application/octet-stream",
          });
          ws.send({ type: "result", id, result: { action, id: downloadId, item: state.downloads[state.downloads.length - 1] } });
          return;
        }
        // 每次 list 把 in_progress 往前推一步，第二次起变成 complete
        state.downloadPolls = (state.downloadPolls || 0) + 1;
        for (const item of state.downloads) {
          if (item.state === "in_progress" && state.downloadPolls >= 2) {
            item.state = "complete";
            item.bytes_received = item.total_bytes;
            item.filename = item.filename.replace(/\\$/, "");
          }
        }
        ws.send({
          type: "result",
          id,
          result: { action: "list", count: state.downloads.length, limit: 20, items: state.downloads.slice().reverse() },
        });
        return;
      }
      if (code.cmd === "frames") {
        ws.send({ type: "result", id, result: { tabId: code.tabId, count: 2, frames: [{ frame_id: 0, parent_frame_id: -1, url: "https://mock.local/" }, { frame_id: 7, parent_frame_id: 0, url: "https://mock.local/child" }] } });
        return;
      }
      if (code.cmd === "openTab") {
        const tab = { id: state.nextTabId++, url: code.url, title: `Mock ${code.url}` };
        state.tabs.push(tab);
        ws.send({
          type: "result",
          id,
          result: {
            id: tab.id,
            url: tab.url,
            title: tab.title,
            active: code.active !== false,
            windowId: 1,
            window: false,
            group: null,
          },
          newTabs: [{ id: tab.id, url: tab.url, title: tab.title }],
        });
        setTimeout(sendTabsUpdate, 30);
        return;
      }
      if (code.cmd === "closeTab") {
        state.tabs = state.tabs.filter((tab) => tab.id !== code.tabId);
        ws.send({ type: "result", id, result: { ok: true, tabId: code.tabId } });
        setTimeout(sendTabsUpdate, 30);
        return;
      }
      if (code.cmd === "status") {
        ws.send({
          type: "result",
          id,
          result: {
            extensionVersion: EXT_VERSION,
            fileSchemeAccess: true,
            extensionId: "a".repeat(32),
          },
        });
        return;
      }
      if (code.cmd === "pageOp") {
        // 无 eval 通道（页面 CSP）：扩展侧注入 ops/<op>.js 再调用入口函数。
        // 线上 WS 回包是 `res.data ?? null`（background.js 的 socketSend），也就是
        // **已经剥掉 `{ok,data}` 外层** 的 `{ op, value }`。旧 mock 多包了一层，
        // 和真实扩展不一致，这里按真实形状发送。
        const op = String(code.op || "");
        const value = handlePageOp(op, code.args, message.tabId);
        ws.send({ type: "result", id, result: { op, value } });
        return;
      }
      if (code.cmd === "cdp") {
        // 同上：handleCDP 的 `{ok,data}` 在 WS 层被剥成裸 CDP 结果。
        const out = handleCdp(code);
        ws.send({ type: "result", id, result: out && out.ok === true ? out.data : out });
        return;
      }
      if (code.cmd === "batch") {
        // 按顺序执行，支持 daemon 侧 `$N.path` 引用（DOM.resolveNode → callFunctionOn）。
        const results = [];
        for (const command of code.commands || []) {
          if (command.method === "DOM.resolveNode") {
            results.push({ object: { objectId: `mock-object-${results.length}` } });
          } else if (command.method === "Runtime.callFunctionOn") {
            results.push({
              result: {
                value: {
                  tag: "BUTTON",
                  text: "提交",
                  selector: "#mock-submit",
                  visible: true,
                  rect: { x: 10, y: 20, width: 80, height: 24 },
                },
              },
            });
          } else {
            results.push({ ok: true });
          }
        }
        // 与 background.js 的 handleBatch 同形：{ ok, results, count, failed }
        // （这一条**不**经过 data 层，WS 直接发送整个对象）。
        ws.send({
          type: "result",
          id,
          result: { ok: true, results, count: results.length, failed: 0 },
        });
        return;
      }
      ws.send({ type: "result", id, result: { ok: true } });
      return;
    }

    ws.send({ type: "result", id, result: handlePlainJs(String(code ?? ""), message.tabId), newTabs: [] });
  };

  const attempt = async () => {
    if (closed) return;
    try {
      ws = await connectOnce(port);
      ws.onmessage = handleMessage;
      ws.onclose = () => {
        ws = null;
        if (!closed) retryTimer = setTimeout(attempt, 400);
      };
      ws.send({
        type: "ext_ready",
        browser_id: BROWSER_ID,
        profile_id: PROFILE_ID,
        profile_label: null,
        extension_version: EXT_VERSION,
        file_scheme_access: true,
        tabs: state.tabs.map((tab) => ({ id: tab.id, url: tab.url, title: tab.title })),
      });
    } catch {
      if (!closed) retryTimer = setTimeout(attempt, 400);
    }
  };

  const ready = attempt();
  if (!waitForReady) return { close: () => {}, state };

  return {
    state,
    close() {
      closed = true;
      if (retryTimer) clearTimeout(retryTimer);
      try {
        ws?.socket?.end();
      } catch {
        /* ignore */
      }
    },
    async waitConnected(timeoutMs = 8000) {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (ws) return true;
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      return false;
    },
    _ready: ready,
  };
}

// ---------------------------------------------------------------- self test
function runSelfTest() {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const repo = path.resolve(here, "..");
  // 默认用 debug 构建（迭代快）；设 ABC_CLI 可指向任意二进制，
  // 或建好 release 后想直接验 release 时用：ABC_CLI=target/release/... node tests/mock_extension.mjs --self-test
  const bin = process.env.ABC_CLI
    ? path.resolve(repo, process.env.ABC_CLI)
    : path.join(repo, "target", "debug", process.platform === "win32" ? "atb.exe" : "atb");
  if (!fs.existsSync(bin)) {
    console.error(`缺少调试二进制: ${bin}\n先运行 cargo build`);
    process.exit(2);
  }
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "abc-home-"));
  const mockPort = Number(process.env.MOCK_EXT_PORT || 18766);
  fs.mkdirSync(path.join(home, ".atb"), { recursive: true });
  fs.writeFileSync(
    path.join(home, ".atb", "config.json"),
    JSON.stringify({ extension_port: mockPort }),
  );
  // ATB_HOME 是 cfg 侧显式支持的配置目录覆盖：Windows 上改 HOME/USERPROFILE 无效
  // （dirs::home_dir 走 Known Folder API），不设它就会连到真实 daemon 的端口上。
  const env = { ...process.env, HOME: home, USERPROFILE: home, ATB_HOME: path.join(home, ".atb") };
  // 必须用异步 spawn：spawnSync 会阻塞 Node 事件循环，假扩展就无法响应 daemon 的命令，
  // 真实的智能体调用也是异步子进程，这里保持一致。
  const run = (args, timeout = 60000) =>
    new Promise((resolve) => {
      const child = spawn(bin, args, { env, stdio: ["ignore", "pipe", "pipe"] });
      let stdout = "";
      let stderr = "";
      child.stdout.on("data", (chunk) => {
        stdout += chunk;
      });
      child.stderr.on("data", (chunk) => {
        stderr += chunk;
      });
      const timer = setTimeout(() => child.kill(), timeout);
      child.on("close", (code) => {
        clearTimeout(timer);
        const trimmed = stdout.trim();
        let json = null;
        try {
          json = JSON.parse(trimmed.slice(trimmed.indexOf("{")));
        } catch {
          /* 非 JSON 输出（例如 help） */
        }
        resolve({ code, stdout: trimmed, stderr: stderr.trim(), json });
      });
    });
  const assert = (condition, label, extra) => {
    if (condition) {
      console.log(`  PASS  ${label}`);
      return;
    }
    console.error(`  FAIL  ${label}`, extra ?? "");
    process.exitCode = 1;
  };

  // 真 daemon 占着 18767/18765 时，测试会连到用户的真实浏览器上——
  // 既不稳定也可能误操作真实标签页，所以先做端口体检。
  const portBusy = (port) => new Promise((resolve) => {
    const socket = net.connect({ host: "127.0.0.1", port });
    socket.setTimeout(400);
    socket.on("connect", () => { socket.destroy(); resolve(true); });
    socket.on("error", () => resolve(false));
    socket.on("timeout", () => { socket.destroy(); resolve(false); });
  });
  const ensurePortsFree = async () => {
    for (const port of [18767, mockPort]) {
      if (await portBusy(port)) {
        console.error(
          `端口 ${port} 已被占用：大概有真实 daemon 在跑（可能连着你的浏览器）。\n` +
          `请先执行 atb stop，再重跑 node tests/mock_extension.mjs --self-test`,
        );
        process.exit(1);
      }
    }
  };

  const mock = startMockExtension({ port: mockPort });
  console.log(`mock extension: connecting ws://127.0.0.1:${mockPort} ...`);
  const waitTick = (ms = 300) => new Promise((resolve) => setTimeout(resolve, ms));

  // 收尾必须无条件执行：任一断言失败或抛错时也要停掉 daemon，
  // 否则它继续占着 18766/18767，下一次自检会直接「端口被占用」而整轮失败。
  const cleanup = async () => {
    try {
      await run(["stop"], 15000);
    } catch {
      /* 尽力而为 */
    }
    mock.close();
    try {
      fs.rmSync(home, { recursive: true, force: true });
    } catch {
      /* ignore */
    }
    console.log(process.exitCode ? "E2E FAILED" : "E2E PASSED");
  };

  (async () => {
    await ensurePortsFree();
    console.log("1) tabs（会拉起 daemon）");
    let result = await run(["tabs"]);
    // daemon 就绪后事件循环才空出来，假扩展这时才能完成握手并发送 ext_ready。
    await mock.waitConnected(8000);
    for (let i = 0; i < 20 && (result.json?.result?.metadata?.tabs_count ?? 0) < 1; i += 1) {
      await waitTick();
      result = await run(["tabs"]);
    }
    assert(result.json?.ok === true, "daemon 可访问", result.stdout || result.stderr);
    assert(result.json?.result?.metadata?.tabs_count >= 1, "至少一个标签页", result.json);

    // 身份校验：确认连上的是「本次自检拉起的 daemon + 自己的假扩展」。
    // 端口可能在自检启动后被别人的 daemon 抢占，那时命令会打到别人的实例上，
    // 表现为「tab 找不到 / 结果为空」这种莫名其妙的假红。
    const identity = await run(["browsers"]);
    const browsers = identity.json?.result?.browsers ?? [];
    // 兼容不同版本的字段名：先认 browser_id，退回到整串里找 mock 的 browser id。
    const isMine =
      browsers.some((item) => item && item.browser_id === BROWSER_ID)
      || JSON.stringify(browsers).includes(BROWSER_ID);
    assert(isMine, "daemon 连的是本次自检的假扩展（不是别人的 daemon）", JSON.stringify(browsers).slice(0, 400));
    if (!isMine) {
      await cleanup();
      return;
    }

    console.log("2) open --background（不抢焦点，但成为默认目标）");
    result = await run(["open", "https://example.com/mock", "--background"]);
    const opened = result.json?.result;
    assert(result.json?.ok === true, "open 成功", result.stdout);
    assert(opened?.active === false, "新标签页未被激活", opened);
    assert(typeof opened?.opened_session_key === "string", "返回 opened_session_key", opened);
    await waitTick();
    const sessions = (await run(["tabs"])).json?.result?.metadata?.tabs ?? [];
    const openedTab = sessions.find((tab) => tab.session_key === opened?.opened_session_key);
    assert(!!openedTab, "新标签页已注册为会话", sessions);

    console.log("3) 不带 --tab 的 exec 命中刚打开的后台标签页");
    result = await run(["exec", "return document.title"]);
    assert(result.json?.ok === true, "exec 成功", result.stdout);
    assert(
      String(result.json?.result?.js_return || "").includes("example.com/mock"),
      "默认会话已切到新标签页",
      result.json?.result,
    );

    console.log("3b) snapshot --details（批量 CDP 合并成一条 batch）");
    const snap = await run(["snapshot", "--limit", "10", "--details"]);
    const snapTree = JSON.stringify(snap.json?.result?.tree ?? []);
    assert(snap.json?.ok === true, "snapshot 成功", snap.stdout);
    const refCount = (snapTree.match(/"ref":"@e/g) || []).length;
    assert(refCount >= 1, "至少一个 @e 引用", snap.json?.result);
    assert(/"tag":"button"/i.test(snapTree), "详情已合并进树", snapTree.slice(0, 200));
    assert(!/dom_error/.test(snapTree), "详情无错误", snapTree.slice(0, 200));

    console.log("4) --async 提交后台任务并等待结果");
    result = await run(["--async", "exec", "return 1"]);
    const jobId = result.json?.job?.id;
    assert(result.json?.ok === true && typeof jobId === "string", "返回 job_id", result.stdout);
    const waited = await run(["job", "wait", jobId, "--timeout", "20"]);
    assert(waited.json?.job?.status === "succeeded", "任务成功", waited.stdout);
    assert(waited.json?.job?.result?.js_return === 1, "任务结果正确", waited.json?.job?.result);
    assert(typeof waited.json?.job?.elapsed_ms === "number", "包含耗时", waited.json?.job);

    console.log("5) --wait-js 一次注入、页面内求值（不再每 100ms 注入一次）");
    const waitCallsBefore = mock.state.waitCalls;
    result = await run([
      "exec",
      "__mockWait()",
      "--wait-js",
      "__mockWait()",
      "--wait-timeout",
      "8",
      "--wait-interval",
      "0.05",
    ]);
    const wait = result.json?.result?.js_return?.wait;
    assert(wait?.ok === true, "--wait-js 命中", result.json?.result);
    assert(wait?.injections === 1, "只注入一次", wait);
    assert(wait?.mode === "js-observer", "走页面内 js-observer 模式", wait);
    assert(mock.state.waitCalls - waitCallsBefore === 1, "页面脚本只被调用一次", {
      calls: mock.state.waitCalls - waitCallsBefore,
    });

    console.log("5a) wait --js 超时同样只注入一次");
    result = await run(["wait", "--js", "__mockNever()", "--timeout", "1"]);
    const waitTimedOut = result.json?.result || result.json;
    assert(waitTimedOut?.ok === false, "超时判定", waitTimedOut);
    assert(waitTimedOut?.injections === 1, "超时也只注入一次", waitTimedOut);
    assert(waitTimedOut?.error === "timeout", "错误原因是 timeout", waitTimedOut);

    console.log("2b) open 默认前台（默认不在后台操作）");
    const foreground = await run(["open", "https://example.com/foreground"]);
    assert(
      foreground.json?.result?.active === true,
      "默认 open 会切到前台",
      foreground.json?.result,
    );
    const foregroundTab = foreground.json?.result?.opened_tab_id;
    if (foregroundTab) await run(["close", "--tab", String(foregroundTab)]);

    console.log("5b) run 管道：一次进程执行多步");
    const stepsPath = path.join(home, "steps.json");
    fs.writeFileSync(
      stepsPath,
      JSON.stringify({ steps: [{ op: "exec", args: { script: "return 1" } }, { op: "exec", args: { script: "return 2" } }] }),
    );
    const pipeline = await run(["run", stepsPath]);
    assert(pipeline.json?.ok === true, "管道整体成功", pipeline.stdout);
    assert(pipeline.json?.steps?.length === 2, "两步都执行", pipeline.json?.steps);

    console.log("6) job list / health 暴露任务状态");
    result = await run(["job", "list"]);
    assert(result.json?.ok === true && result.json?.count >= 1, "job list 有记录", result.stdout);
    result = await run(["status"]);
    assert(typeof result.json?.jobs === "object", "health 暴露 jobs", result.json?.jobs);
    assert(
      result.json?.extension?.connected_versions?.includes(EXT_VERSION),
      "health 暴露连接中的扩展版本",
      result.json?.extension,
    );
    assert(
      result.json?.extension?.stale === false,
      "版本一致时不标记 stale",
      result.json?.extension,
    );

    console.log("6b) read / find / action / wait");
    const tabId = String(openedTab?.tab_id ?? "");
    result = await run(["read", "--tab", tabId, "--max-chars", "1000"]);
    assert(result.json?.ok === true, "read 成功", result.stdout);
    assert(
      String(result.json?.result?.page?.title || "").startsWith("Mock"),
      "read 返回标题",
      result.json?.result?.page?.title,
    );

    result = await run(["find", "a", "--tab", tabId, "--limit", "5"]);
    assert(
      result.json?.ok === true && result.json?.result?.find?.matched === 2,
      "find 命中 2 个元素",
      result.json?.result?.find,
    );

    result = await run(["hover", "a", "--tab", tabId]);
    assert(
      result.json?.ok === true && result.json?.result?.action?.op === "hover",
      "hover 成功",
      result.json?.result?.action,
    );

    result = await run(["wait", "--selector", "input", "--tab", tabId, "--timeout", "5"]);
    assert(result.json?.ok === true && result.json?.result?.matched === true, "wait 命中", result.json?.result);

    console.log("6c) snapshot --js（DOM 快照）+ @e 走页面内动作");
    result = await run(["snapshot", "--js", "--tab", tabId, "--limit", "5"]);
    assert(result.json?.ok === true, "JS 快照成功", result.stdout);
    assert(result.json?.result?.mode === "js", "标记为 js 模式", result.json?.result?.mode);
    const jsRef = result.json?.result?.tree?.[0]?.ref;
    assert(/^@e\d+$/.test(String(jsRef || "")), "返回 @e 引用", jsRef);
    result = await run(["click", jsRef, "--tab", tabId]);
    assert(result.json?.ok === true, "JS 引用的 click 成功", result.stdout);
    assert(
      result.json?.result?.operation?.op === "click",
      "click 走了页面内动作而不是 CDP",
      result.json?.result?.operation,
    );

    // click --monitor 的 change 必须与 js --monitor 同构（结构化摘要），
    // 不能再回传原始 HTML 片段（top_change）。断言只锁「回归点 + 关键字段类型」，
    // 不锁字段集合——以后新增字段不应该让测试红。
    console.log("6c2) click --monitor 的 change 形状");
    result = await run(["click", jsRef, "--monitor", "--tab", tabId]);
    const monitorChange = result.json?.result?.change;
    assert(result.json?.ok === true, "click --monitor 成功", result.stdout);
    assert(!!monitorChange && typeof monitorChange === "object", "返回结构化 change", result.json?.result);
    assert(typeof monitorChange?.changed === "number", "change.changed 是计数（number），不是 0|1 标志", monitorChange?.changed);
    assert(
      !("top_change" in (monitorChange || {})),
      "change 不再回传原始 HTML（top_change）",
      monitorChange && Object.keys(monitorChange),
    );
    if (monitorChange && "samples" in monitorChange) {
      assert(
        Array.isArray(monitorChange.samples) && monitorChange.samples.length <= 8,
        "change.samples 是数组且最多 8 条",
        monitorChange.samples,
      );
    }
    // 其余计数字段「存在则必须是 number」，不要求齐全，避免加/改字段时误红。
    for (const key of ["added", "removed", "attributes", "text"]) {
      if (monitorChange && key in monitorChange) {
        assert(typeof monitorChange[key] === "number", `change.${key} 是 number`, monitorChange[key]);
      }
    }

    // 三种降级路径：观察器装不上 / 动作导致导航 / 被新的观察顶掉。
    // 它们必须给出机器可读的 monitor.reason，而不是静默当成「没变化」。
    mock.state.monitorMode = "unsupported";
    result = await run(["click", jsRef, "--monitor", "--tab", tabId]);
    const unsupportedRun = result.json?.result || {};
    assert(
      unsupportedRun.monitor?.ok === false && unsupportedRun.monitor?.reason === "unsupported",
      "观察器装不上时回 monitor.reason=unsupported（不静默成功）",
      unsupportedRun.monitor,
    );
    assert(!("change" in unsupportedRun), "unsupported 时不应编造 change", unsupportedRun.change);

    mock.state.monitorMode = "navigated";
    result = await run(["click", jsRef, "--monitor", "--tab", tabId]);
    const navigatedRun = result.json?.result || {};
    assert(
      navigatedRun.change?.changed === 1 && navigatedRun.change?.navigated === true,
      "动作导致导航时给 changed=1 + navigated=true（导航本身就是变化）",
      navigatedRun.change,
    );
    assert(
      navigatedRun.monitor?.ok === false && navigatedRun.monitor?.reason === "navigated",
      "导航场景同时给出 monitor.reason=navigated",
      navigatedRun.monitor,
    );

    mock.state.monitorMode = "superseded";
    result = await run(["click", jsRef, "--monitor", "--tab", tabId]);
    const supersededRun = result.json?.result || {};
    assert(
      supersededRun.monitor?.ok === false && supersededRun.monitor?.reason === "superseded",
      "被新观察顶掉时回 monitor.reason=superseded（与 navigated 区分）",
      supersededRun.monitor,
    );
    mock.state.monitorMode = "ok";

    console.log("6d) inject / frames");
    result = await run(["inject", "return 1+1", "--tab", tabId]);
    assert(result.json?.ok === true && result.json?.result?.value === 2, "inject run 返回值", result.stdout);
    result = await run(["inject", "--persist", "demo", "--tab", tabId, "window.__x = 1"]);
    assert(result.json?.ok === true && result.json?.result?.applied === true, "persist 生效", result.json?.result);
    result = await run(["inject", "--list", "--tab", tabId]);
    assert(result.json?.result?.count === 1, "list 有一条", result.json?.result);
    result = await run(["inject", "--remove", "demo", "--tab", tabId]);
    assert(result.json?.result?.removed === true, "remove 生效", result.json?.result);
    result = await run(["frames", "--tab", tabId]);
    assert(result.json?.result?.count === 2, "frames 返回 2 个", result.json?.result);

    console.log("6f) 下载 / 页面网络 / 多浏览器");
    result = await run(["download", "https://example.com/file.bin", "--filename", "mock.bin", "--tab", tabId]);
    const dlId = result.json?.result?.id;
    assert(result.json?.ok === true && typeof dlId === "number", "download 返回 id", result.stdout);
    result = await run(["download-wait", "--id", String(dlId), "--timeout", "20", "--interval", "0.05", "--tab", tabId]);
    assert(result.json?.result?.state === "complete", "download-wait 完成", result.stdout);
    result = await run(["downloads", "--limit", "5", "--tab", tabId]);
    assert((result.json?.result?.count ?? 0) >= 1, "downloads 列表", result.stdout);

    result = await run(["net", "fetch", "https://example.com/x", "--tab", tabId]);
    assert(result.json?.result?.net?.status === 200, "net fetch 200", result.stdout);
    result = await run(["net", "mock", "/mocked", "--tab", tabId]);
    assert((result.json?.result?.net?.routes ?? 0) >= 1, "net mock 规则", result.stdout);
    result = await run(["net", "log", "--tab", tabId]);
    assert((result.json?.result?.net?.count ?? 0) >= 1, "net log 有记录", result.stdout);

    console.log("6g) net --engine cdp：规则按 profile 共享并下发到扩展");
    result = await run([
      "net", "mock", "/cdp-api",
      "--engine", "cdp",
      "--response-status", "201",
      "--response-body", '{"v":9}',
      "--tab", tabId,
    ]);
    assert(result.json?.result?.net?.engine === "cdp", "engine=cdp 生效", result.stdout);
    assert((result.json?.result?.net?.routes ?? 0) === 1, "cdp 规则入库", result.json?.result?.net);
    const pushed = (mock.state.netIntercept || []).filter((item) => item.action === "start").pop();
    assert(!!pushed && pushed.rules.length === 1, "扩展收到 netIntercept start", pushed);
    assert(pushed?.rules?.[0]?.response?.status === 201, "规则原样下发", pushed?.rules?.[0]);

    result = await run(["net", "routes", "--tab", tabId]);
    const cdpRoutes = result.json?.result?.net;
    assert(cdpRoutes?.cdp?.enabled === true && cdpRoutes?.cdp?.count === 1, "routes 显示网络层规则", cdpRoutes);
    assert(typeof cdpRoutes?.profile === "string" || cdpRoutes?.cdp?.applied, "routes 带 profile/下发信息", cdpRoutes);

    console.log("6h) net log 合并页面侧与 CDP 侧记录");
    await run(["net", "hook", "--tab", tabId]);
    result = await run(["net", "log", "--limit", "20", "--tab", tabId]);
    const mergedNet = result.json?.result?.net;
    assert((mergedNet?.sources?.page ?? 0) >= 1, "有页面侧记录", mergedNet?.sources);
    assert((mergedNet?.sources?.cdp ?? 0) >= 1, "有 CDP 侧记录", mergedNet?.sources);
    assert(
      (mergedNet?.items || []).some((item) => item.source === "cdp" && item.action === "rewrite"),
      "条目带 source=cdp 与 action",
      (mergedNet?.items || []).slice(-3)
    );

    result = await run(["net", "log", "--clear", "--tab", tabId]);
    result = await run(["net", "log", "--limit", "10", "--tab", tabId]);
    assert((result.json?.result?.net?.sources?.cdp ?? -1) === 0, "clear 同时清空 CDP 侧", result.json?.result?.net?.sources);

    result = await run(["net", "reset", "--tab", tabId]);
    assert(result.json?.result?.net?.cdp_stopped === true, "reset 停掉网络层拦截", result.json?.result?.net);
    const stopped = (mock.state.netIntercept || []).filter((item) => item.action === "stop").pop();
    assert(!!stopped, "扩展收到 netIntercept stop", stopped);

    result = await run(["browsers"]);
    assert((result.json?.result?.count ?? 0) >= 1, "browsers 列出目标", result.stdout);
    result = await run(["broadcast", "exec", "--script", "return location.host"]);
    assert(
      (result.json?.result?.count ?? 0) >= 1 && result.json?.result?.failed === 0,
      "broadcast 并发执行",
      result.stdout,
    );

    // hook --persist 之后 daemon 持久化的必须是真实规则数组（routes_list），
    // 而不是 hook 回包里的计数（routes: 1）。把计数当数组存，后续 net routes 会读回空。
    console.log("6j) hook --persist 后 net routes 仍能读回规则");
    await run(["net", "mock", "/persist-api", "--tab", tabId]);
    const hooked = await run(["net", "hook", "--persist", "--tab", tabId]);
    assert(hooked.json?.ok === true, "hook --persist 成功", hooked.stdout);
    result = await run(["net", "routes", "--tab", tabId]);
    const persistedRoutes = result.json?.result?.net;
    assert(
      Array.isArray(persistedRoutes?.routes) && persistedRoutes.routes.length >= 1,
      "持久化后 routes 读回 ≥1 条（不能把计数当规则）",
      persistedRoutes,
    );
    assert(
      (Array.isArray(persistedRoutes?.routes) ? persistedRoutes.routes : []).some((item) =>
        String(item?.match || "").includes("persist-api"),
      ),
      "读回的规则就是 hook 前注册的那条",
      persistedRoutes?.routes,
    );

    console.log("6e) batch 并发批量");
    const batchFile = path.join(home, "batch.json");
    fs.writeFileSync(
      batchFile,
      JSON.stringify({
        steps: [
          { op: "exec", args: { script: "return 1", switch_tab_id: tabId } },
          { op: "read", args: { max_chars: 100, switch_tab_id: tabId } },
          { op: "find", args: { selector: "a", limit: 2, switch_tab_id: tabId } },
          { op: "action", args: { op: "hover", target: "a", switch_tab_id: tabId } },
        ],
      }),
      "utf8",
    );
    result = await run(["batch", batchFile, "--concurrency", "4"]);
    assert(result.json?.ok === true && result.json?.count === 4, "batch 全部成功", result.stdout);
    assert(typeof result.json?.elapsed_ms === "number", "batch 返回耗时", result.json?.elapsed_ms);

    console.log("6i) challenge status / solve（返回契约，不允许假通过）");
    // mock 的 challenge 分支按 state.challengeScenario 应答：cleared / token / need-human / timeout。
    mock.state.challengeScenario = "cleared";
    result = await run(["challenge", "status", "--tab", tabId]);
    assert(result.json?.ok === true, "challenge status 成功", result.stdout);
    const chStatus = result.json?.result?.challenge || result.json?.result;
    assert(
      chStatus?.action === "status" && chStatus?.detected === false,
      "status 返回 action=status 且 detected=false",
      chStatus,
    );

    result = await run(["challenge", "solve", "--tab", tabId, "--timeout", "6"]);
    assert(result.json?.ok === true, "solve（无验证码）成功", result.stdout);
    const chSolveClear = result.json?.result || {};
    assert(chSolveClear.passed === true, "无验证码时 solve passed=true", chSolveClear);
    // status 的 blocking:false + interactive:false 命中快路径：via 必为 no-challenge、零注入、
    // 且 challenge 字段是 status 的 payload（不是 wait 的）。
    assert(
      chSolveClear.via === "no-challenge",
      "无验证码时 via=no-challenge（走快路径）",
      chSolveClear.via,
    );
    assert(chSolveClear.injections === 0, "快路径不注入页面脚本（injections=0）", chSolveClear.injections);
    assert(
      chSolveClear.challenge?.action === "status",
      "快路径的 challenge 字段是 status payload",
      chSolveClear.challenge,
    );

    // 非阻断 widget（interactive=true + token_ready=false）会卡住快路径，进循环后由
    // wait 的 cleared:true（无 via）兜成 via='clear' —— 与快路径的 no-challenge 是两条不同来源。
    mock.state.challengeScenario = "widget-cleared";
    result = await run(["challenge", "solve", "--tab", tabId, "--timeout", "8"]);
    const chSolveWidget = result.json?.result || {};
    assert(chSolveWidget.passed === true, "widget 场景 solve passed=true", chSolveWidget);
    assert(chSolveWidget.via === "clear", "非阻断 widget 走循环，via=clear", chSolveWidget.via);
    assert(chSolveWidget.injections >= 1, "循环路径至少注入一次", chSolveWidget.injections);

    mock.state.challengeScenario = "token";
    result = await run(["challenge", "solve", "--tab", tabId, "--timeout", "6"]);
    const chSolveToken = result.json?.result || {};
    assert(chSolveToken.passed === true && chSolveToken.via === "token", "拿到令牌时 via=token", chSolveToken);
    assert(chSolveToken.injections === 0, "已有令牌时直接放行，不再注入页面脚本", chSolveToken.injections);

    // 验证码一直没完成：必须 passed=false，且不得出现 passed=true。
    mock.state.challengeScenario = "timeout";
    result = await run(["challenge", "solve", "--tab", tabId, "--timeout", "2"]);
    const chSolveTimeout = result.json?.result || {};
    assert(
      chSolveTimeout.passed === false && chSolveTimeout.reason === "timeout",
      "超时时 passed=false 且 reason=timeout",
      chSolveTimeout,
    );

    // need_human 只是循环内部信号（daemon 收到后聚焦再 continue），所以顶层收尾仍是
    // timeout，真正的人工信号藏在嵌套的 challenge 字段里。这里用 --focus-after 0.3
    // 让它落在单片 wait（≤4s）之内，才能在当前实现下触发聚焦路径。
    mock.state.challengeScenario = "need-human";
    result = await run(["challenge", "solve", "--tab", tabId, "--timeout", "2", "--focus-after", "0.3"]);
    const chNeedHuman = result.json?.result || {};
    assert(chNeedHuman.passed === false, "需要人工时 solve 不报通过", chNeedHuman);
    assert(chNeedHuman.focused === true, "需要人工时请求把标签页带到前台", chNeedHuman);
    assert(
      ["need_human", "timeout"].includes(chNeedHuman.challenge?.reason),
      "嵌套 challenge 记录最后一片 wait 的原因",
      chNeedHuman.challenge,
    );

    // 时序守卫：默认 --focus-after 8s 远大于单片 wait 上限 4s。若 focusAfterMs 每片都
    // 从 0 重新计时（旧实现），它恒为 8000 > 4000，need_human 永不触发 → 需要人工的
    // 验证码页会一路等到超时，从不把标签页带到前台。
    //
    // mock 的 wait 立即返回，墙钟推进很慢，但 daemon 用的是真实 elapsed，只要递减口径
    // 正确，focusAfterMs 总会被消到 ≤ 单片上限，所以端到端的 focused 断言仍然成立。
    // 若日后实现改成「下发绝对 deadline」等其它形态，请改本条断言，而不是改实现。
    mock.state.challengeSlices.length = 0;
    result = await run(["challenge", "solve", "--tab", tabId, "--timeout", "12"]);
    const chDefaultFocus = result.json?.result || {};
    const slices = mock.state.challengeSlices.slice();
    const focusSeq = slices.map((item) => item.focusAfterMs);
    assert(slices.length >= 3, "至少下发 3 片 wait（时序样本足够）", slices.length);
    assert(focusSeq[0] > 0, "首片应带正的 focusAfterMs（0=禁用聚焦）", focusSeq.slice(0, 8));
    // 聚焦之后 daemon 按设计改发 0（禁用），所以 0 只能出现在尾部，且之后不再回正值。
    const firstZero = focusSeq.findIndex((value) => !(value > 0));
    assert(
      firstZero === -1 || focusSeq.slice(firstZero).every((value) => value === 0),
      "递减到 0 之后应一直保持 0（0=禁用聚焦，不能回到正值）",
      { firstZero, tail: focusSeq.slice(Math.max(0, firstZero - 2)).slice(0, 6) },
    );
    // 单调用 <=：相邻两片的墙钟差可能不足 1ms，取整后会相等，用 < 会误报。
    const monotonic = focusSeq.every((value, i) => i === 0 || value <= focusSeq[i - 1]);
    assert(
      monotonic,
      "focusAfterMs 必须随切片单调不增（抖动实现同样算错）",
      focusSeq.slice(0, 8),
    );
    assert(
      focusSeq[focusSeq.length - 1] < focusSeq[0],
      "末片 focusAfterMs 必须严格小于首片（恒等即「每片从 0 重新计时」）",
      { first: focusSeq[0], last: focusSeq[focusSeq.length - 1], count: focusSeq.length },
    );
    assert(
      chDefaultFocus.focused === true,
      "默认 focus-after 下必须把标签页带到前台（需要人工时不能静默等到超时）",
      chDefaultFocus.focused,
    );
    assert(chDefaultFocus.passed === false, "未放行时 solve 不报通过", chDefaultFocus.passed);

    mock.state.challengeScenario = "cleared";

    console.log("7) 关闭后台标签页");
    result = await run(["close", "--tab", tabId]);
    assert(result.json?.ok === true, "close 成功", result.stdout);

    console.log("8) 扩展连接中途断开：应立即失败而不是等超时");
    const started = Date.now();
    const dropped = await run(["exec", "__mockDisconnect()"], 20000);
    const cost = Date.now() - started;
    assert(dropped.json?.ok === false, "命令返回失败", dropped.json);
    assert(
      /extension_disconnected/.test(dropped.stdout || dropped.stderr || ""),
      "错误码可识别",
      dropped.stdout,
    );
    assert(cost < 6000, `没有等超时（${cost}ms）`, cost);

    await run(["stop"], 5000);
    await cleanup();
  })().catch((error) => {
    console.error(error);
    return cleanup().finally(() => process.exit(1));
  });
}

const isMain = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  if (process.argv.includes("--self-test")) runSelfTest();
  else {
    const mock = startMockExtension();
    console.log(`mock extension 已启动，等待 daemon 连接 ws://127.0.0.1:${DEFAULT_EXT_PORT}`);
    process.on("SIGINT", () => {
      mock.close();
      process.exit(0);
    });
  }
}
