#!/usr/bin/env node
// atb 端到端回归：本地受控页面 + 真实 atb 二进制。
//
// 为什么不用真实站点：站点改版/风控/网络抖动都会让回归随机红，而这套脚本要守的是
// 「CLI ↔ daemon ↔ 扩展」这条链路的契约（fill/click/exec/wait/screenshot/scrape/download），
// 页面本身应该是一个常量。所以页面放在 tests/e2e/pages/，由本脚本起一个 127.0.0.1
// 随机端口的静态服务，全程无第三方依赖。
//
// 用法：
//   node scripts/e2e.mjs                 # 跑全部场景
//   node scripts/e2e.mjs --only=scrape   # 只跑名字包含 scrape 的场景
//   node scripts/e2e.mjs --keep          # 保留 os.tmpdir() 里的临时文件（排障用）
//   ATB_E2E=smoke node scripts/e2e.mjs   # 只跑 3 个核心场景（CI 用，省时间）
//   ATB_E2E_BIN=/path/to/atb node scripts/e2e.mjs
//
// 退出码：0 = 全部 PASS（SKIP 不算失败）；1 = 有 FAIL；2 = 前置条件不满足（daemon 未运行等）。

import { execFile } from 'node:child_process';
import { createServer } from 'node:http';
import { createConnection } from 'node:net';
import {
  closeSync,
  existsSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  rmSync,
  statSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, extname, join, normalize, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = dirname(HERE);
const PAGES_DIR = join(ROOT, 'tests', 'e2e', 'pages');

const BIN =
  process.env.ATB_E2E_BIN ||
  join(ROOT, 'target', 'release', process.platform === 'win32' ? 'atb.exe' : 'atb');
// 每个进程一个 agent 身份：cleanup 只关本次脚本打开的标签页，不误伤用户自己的页面。
const AGENT = process.env.ATB_E2E_AGENT || `atb-e2e-${process.pid}`;

const ONLY = (process.argv.find((arg) => arg.startsWith('--only=')) || '').slice('--only='.length);
const KEEP = process.argv.includes('--keep');
// CI 只需要守住主链路，全量场景留给本地 / 发布前手动跑。
const SMOKE = String(process.env.ATB_E2E || '').toLowerCase() === 'smoke';
const SMOKE_NAMES = ['basic-flow', 'screenshot', 'spa-wait'];

// 临时目录延迟创建：前置条件不满足时直接退出，不该在 os.tmpdir() 里留垃圾。
let TMP_DIR = null;
function ensureTmpDir() {
  if (!TMP_DIR) TMP_DIR = mkdtempSync(join(tmpdir(), 'atb-e2e-'));
  return TMP_DIR;
}

// ---------------------------------------------------------------------------
// 断言与输出
// ---------------------------------------------------------------------------

class Checks {
  constructor(name) {
    this.name = name;
    this.total = 0;
    this.failures = [];
  }

  /** 值相等（按 JSON 语义比较，够用于 CLI 的标量/小对象）。 */
  eq(label, actual, expected) {
    this.total += 1;
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
      this.failures.push({ label, expected, actual });
    }
  }

  /** 自定义布尔断言；note 写清「期望什么」，失败时原样打印。 */
  truthy(label, value, note = '期望为真') {
    this.total += 1;
    if (!value) this.failures.push({ label, expected: note, actual: value });
  }
}

function short(value, limit = 400) {
  let text;
  try {
    text = typeof value === 'string' ? value : JSON.stringify(value);
  } catch {
    text = String(value);
  }
  if (text === undefined) text = 'undefined';
  return text.length > limit ? `${text.slice(0, limit)}…(共 ${text.length} 字符)` : text;
}

function printFailure(failure) {
  console.log(`   ✗ ${failure.label}`);
  console.log(`       expected: ${short(failure.expected)}`);
  console.log(`       actual:   ${short(failure.actual)}`);
}

// ---------------------------------------------------------------------------
// atb 调用
// ---------------------------------------------------------------------------

/** 调一次 atb：不抛异常，退出码/JSON/原始输出都交回给断言决定怎么判。 */
function atb(args, { timeout = 90000 } = {}) {
  return new Promise((resolve) => {
    const env = { ...process.env, ATB_AGENT: AGENT };
    // 这两个开关会把失败退出码压成 0，回归里必须屏蔽，否则断言全在骗自己。
    delete env.ATB_EXIT0;
    delete env.AGENT_BROWSER_CLI_EXIT0;
    execFile(
      BIN,
      args,
      { timeout, env, maxBuffer: 32 * 1024 * 1024, windowsHide: true },
      (err, stdout, stderr) => {
        const code = err && typeof err.code === 'number' ? err.code : err ? 1 : 0;
        resolve({
          args,
          code,
          timedOut: Boolean(err && err.killed),
          stdout: String(stdout || ''),
          stderr: String(stderr || ''),
          json: parseJson(stdout),
        });
      },
    );
  });
}

/** CLI 正常输出是单行紧凑 JSON；混进日志时退化为「从后往前找第一行可解析的」。 */
function parseJson(text) {
  const trimmed = String(text || '').trim();
  if (!trimmed) return null;
  try {
    return JSON.parse(trimmed);
  } catch {
    /* 落到逐行尝试 */
  }
  const lines = trimmed.split(/\r?\n/).filter((line) => line.trim());
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    try {
      return JSON.parse(lines[index]);
    } catch {
      /* 继续往前找 */
    }
  }
  return null;
}

/** 从任意层级的响应里找 tab_id（单 URL 的 open 与多 URL 的 targets 形状不同）。 */
function findTabId(value) {
  const keys = new Set(['tab_id', 'opened_tab_id', 'tabId']);
  const seen = new Set();
  const walk = (node) => {
    if (!node || typeof node !== 'object' || seen.has(node)) return null;
    seen.add(node);
    if (Array.isArray(node)) {
      for (const item of node) {
        const hit = walk(item);
        if (hit) return hit;
      }
      return null;
    }
    for (const [key, val] of Object.entries(node)) {
      if (keys.has(key) && typeof val === 'string' && val) return val;
    }
    for (const val of Object.values(node)) {
      const hit = walk(val);
      if (hit) return hit;
    }
    return null;
  };
  return walk(value);
}

// ---------------------------------------------------------------------------
// 本地静态服务
// ---------------------------------------------------------------------------

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.txt': 'text/plain; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.png': 'image/png',
};

function startServer() {
  const root = normalize(PAGES_DIR);
  const server = createServer((req, res) => {
    try {
      if (req.method === 'POST' && req.url === '/submit') {
        // form.html 的 action 目标：e2e 不测表单提交，明确拒绝而不是伪装成功。
        res.writeHead(405, { 'content-type': 'text/plain; charset=utf-8' });
        res.end('form submission is not part of the e2e suite');
        return;
      }
      const { pathname: rawPath } = new URL(req.url || '/', 'http://127.0.0.1');
      let pathname = decodeURIComponent(rawPath);
      if (pathname === '/') pathname = '/basic.html';
      // SPA 场景 pushState 到 /spa/detail：整页刷新时也要回到同一个页面。
      if (pathname.startsWith('/spa/')) pathname = '/spa.html';

      const target = normalize(join(root, pathname));
      if (target !== root && !target.startsWith(root + sep)) {
        res.writeHead(403, { 'content-type': 'text/plain; charset=utf-8' });
        res.end('forbidden');
        return;
      }
      if (!existsSync(target) || !statSync(target).isFile()) {
        res.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' });
        res.end('not found');
        return;
      }
      const body = readFileSync(target);
      res.writeHead(200, {
        'content-type': MIME[extname(target).toLowerCase()] || 'application/octet-stream',
        'cache-control': 'no-store',
        'content-length': body.length,
      });
      res.end(body);
    } catch (err) {
      res.writeHead(500, { 'content-type': 'text/plain; charset=utf-8' });
      res.end(String(err));
    }
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      resolve({ server, base: `http://127.0.0.1:${server.address().port}` });
    });
  });
}

// ---------------------------------------------------------------------------
// 场景
// ---------------------------------------------------------------------------

/** 每个场景共享的上下文：URL 拼装、临时文件、打开过的标签页登记。 */
function makeContext(base, checks) {
  const opened = [];
  return {
    checks,
    url: (name) => `${base}/${name}`,
    tmp: (name) => join(ensureTmpDir(), name),
    opened,
    atb,
    /** 打开一个受控页面并把 tab_id 记下来；返回 null 表示要先断言的失败。 */
    async openPage(name) {
      const res = await atb(['open', `${base}/${name}`]);
      const tab = findTabId(res.json);
      checks.truthy(
        `open ${name}`,
        res.json && res.json.ok === true && Boolean(tab),
        `exit=${res.code} stderr=${res.stderr.trim()} stdout=${short(res.stdout)}`,
      );
      if (tab) opened.push(tab);
      return tab;
    },
  };
}

const SCENARIOS = [
  {
    name: 'basic-flow',
    title: 'open → read → fill → click → exec',
    async run(ctx) {
      const tab = await ctx.openPage('basic.html');
      if (!tab) return;

      // daemon 的 read 返回 `{ok, result:{tab_id, session_key, page:{title,…}}}`：
      // 页面脚本结果统一收在 `page` 键下（tree.rs::page_payload）。
      const read = await ctx.atb(['read', '--tab', tab, '--max-chars', '4000']);
      ctx.checks.eq(
        'read 的标题',
        read.json && read.json.result && read.json.result.page && read.json.result.page.title,
        'ATB E2E Basic',
      );

      const fill = await ctx.atb(['fill', '#name', 'hello', '--tab', tab]);
      ctx.checks.truthy('fill 成功', fill.json && fill.json.ok === true, `exit=${fill.code} ${short(fill.stdout)}`);

      const click = await ctx.atb(['click', '#btn', '--tab', tab]);
      ctx.checks.truthy('click 成功', click.json && click.json.ok === true, `exit=${click.code} ${short(click.stdout)}`);

      const exec = await ctx.atb([
        'exec',
        'return document.querySelector("#count").textContent',
        '--tab',
        tab,
      ]);
      const count = exec.json && exec.json.result ? exec.json.result.js_return : undefined;
      ctx.checks.eq('点击后 #count', String(count ?? '').trim(), '1');

      // fill 的值必须真的进了 DOM（而不是只回了个 ok）。
      // 注意不能拿 `read` 判定：`<input>` 的 value 不属于页面文本，read 只返回正文。
      const after = await ctx.atb([
        'exec',
        'return document.querySelector("#name").value',
        '--tab',
        tab,
      ]);
      const value = after.json && after.json.result && after.json.result.js_return;
      ctx.checks.eq('fill 的值真的写进了 DOM', String(value ?? '').trim(), 'hello');

      // 正文路径单独验一次：标题文字必须出现在 read 的正文里。
      const text = await ctx.atb(['read', '--tab', tab, '--max-chars', '4000']);
      ctx.checks.truthy(
        'read 返回正文',
        JSON.stringify(text.json || {}).includes('Basic'),
        'read 的正文里应出现页面标题文字',
      );
    },
  },

  {
    name: 'find-redaction',
    title: 'password 字段脱敏（find → items[].redacted）',
    async run(ctx) {
      const tab = await ctx.openPage('form.html');
      if (!tab) return;

      // find 的结果同样收在 `find` 键下：{ok, result:{tab_id, session_key, find:{matched,items}}}。
      const pass = await ctx.atb(['find', '#pass', '--tab', tab]);
      const passFind = pass.json && pass.json.result ? pass.json.result.find : undefined;
      const item = passFind && Array.isArray(passFind.items) ? passFind.items[0] : undefined;
      ctx.checks.eq('find #pass 命中数', passFind && passFind.matched, 1);
      ctx.checks.eq('#pass 标记 redacted', item && item.redacted, true);
      ctx.checks.eq('#pass 的值打码', item && item.value, '•••');

      // 负向对照：普通文本框不该被脱敏，否则「脱敏」就成了对所有字段的噪声。
      const name = await ctx.atb(['find', '#name', '--tab', tab]);
      const nameFind = name.json && name.json.result ? name.json.result.find : undefined;
      const nameItem = nameFind && Array.isArray(nameFind.items) ? nameFind.items[0] : undefined;
      ctx.checks.truthy('#name（文本框）不脱敏', Boolean(nameItem) && nameItem.redacted !== true, nameItem);
    },
  },

  {
    name: 'screenshot',
    title: 'screenshot --out 落盘且是合法 PNG',
    async run(ctx) {
      const tab = await ctx.openPage('basic.html');
      if (!tab) return;

      const out = ctx.tmp('shot.png');
      const shot = await ctx.atb(['screenshot', '--tab', tab, '--out', out]);
      ctx.checks.truthy('screenshot 成功', shot.json && shot.json.ok === true, `exit=${shot.code} ${short(shot.stdout)}`);
      ctx.checks.truthy('截图文件存在且非空', existsSync(out) && statSync(out).size > 0, out);

      const head = Buffer.alloc(8);
      const fd = openSync(out, 'r');
      try {
        readSync(fd, head, 0, 8, 0);
      } finally {
        closeSync(fd);
      }
      ctx.checks.eq('PNG 魔数', head.toString('hex'), '89504e470d0a1a0a');
    },
  },

  {
    name: 'spa-wait',
    title: 'pushState + 异步渲染 → wait --selector',
    async run(ctx) {
      const tab = await ctx.openPage('spa.html');
      if (!tab) return;

      const click = await ctx.atb(['click', '#go', '--tab', tab]);
      ctx.checks.truthy('click #go 成功', click.json && click.json.ok === true, `exit=${click.code} ${short(click.stdout)}`);

      const wait = await ctx.atb(['wait', '--selector', '#list li', '--timeout', '5', '--tab', tab]);
      ctx.checks.truthy(
        'wait --selector #list li 成功',
        wait.json && wait.json.ok === true,
        `exit=${wait.code} ${short(wait.stdout)}`,
      );

      const path = await ctx.atb(['exec', 'return location.pathname', '--tab', tab]);
      ctx.checks.eq('pushState 后的 pathname', path.json && path.json.result && path.json.result.js_return, '/spa/detail');

      const items = await ctx.atb(['exec', 'return document.querySelectorAll("#list li").length', '--tab', tab]);
      ctx.checks.eq('列表渲染条数', items.json && items.json.result && items.json.result.js_return, 3);
    },
  },

  {
    name: 'scrape',
    title: 'scrape --urls 单 URL 走完整链路',
    async run(ctx) {
      const out = ctx.tmp('scrape.jsonl');
      const res = await ctx.atb([
        'scrape',
        '--urls',
        ctx.url('basic.html'),
        '--out',
        out,
        '--concurrency',
        '1',
        '--batch',
        '1',
        '--retry',
        '1',
        '--timeout',
        '20',
      ]);
      ctx.checks.truthy('scrape 整体 ok', res.json && res.json.ok === true, `exit=${res.code} ${short(res.stdout)}`);
      ctx.checks.eq('scrape success', res.json && res.json.success, 1);
      ctx.checks.eq('scrape failed', res.json && res.json.failed, 0);

      const rows = existsSync(out)
        ? readFileSync(out, 'utf8').split(/\r?\n/).filter((line) => line.trim())
        : [];
      ctx.checks.eq('jsonl 行数', rows.length, 1);
      let row = null;
      try {
        row = rows.length ? JSON.parse(rows[0]) : null;
      } catch {
        row = null;
      }
      const title = row && row.data ? row.data.title : undefined;
      ctx.checks.truthy('jsonl 里 data.title 非空', typeof title === 'string' && title.trim().length > 0, title);
    },
  },

  {
    name: 'download',
    title: 'download 事件 → download-wait（无事件则 SKIP）',
    async run(ctx) {
      const tab = await ctx.openPage('download.html');
      if (!tab) return;

      const click = await ctx.atb(['click', '#dl', '--tab', tab]);
      ctx.checks.truthy('click 下载链接', click.json && click.json.ok === true, `exit=${click.code} ${short(click.stdout)}`);

      const wait = await ctx.atb(['download-wait', '--timeout', '15', '--interval', '0.5', '--tab', tab], {
        timeout: 45000,
      });
      const state = wait.json && wait.json.result ? wait.json.result.state : undefined;
      if (state === 'complete') {
        ctx.checks.eq('下载状态', state, 'complete');
        return {};
      }
      // Chrome 可能直接落盘或弹「另存为」，扩展拿不到下载事件——这是环境问题，
      // 判 skip 而不是 fail，避免套件在别人的机器上无意义地红。
      // 注意：只有「本轮断言没有失败」时才允许标 skip，否则 click 失败会被 skip 掩盖。
      if (
        ctx.checks.failures.length === 0 &&
        (wait.code === 3 || state === 'timeout' || state === 'in_progress')
      ) {
        return {
          skip: `15s 内没有等到 complete（state=${state ?? 'undefined'}, exit=${wait.code}）：Chrome 可能弹了另存为对话框，或扩展未接管这次下载`,
        };
      }
      ctx.checks.eq('下载状态', state, 'complete');
      return {};
    },
  },
];

// ---------------------------------------------------------------------------
// 前置条件
// ---------------------------------------------------------------------------

function printPrerequisites(reason) {
  console.error('');
  console.error(`✗ 前置条件不满足：${reason}`);
  console.error('');
  console.error('这套 e2e 需要真实链路，前置条件：');
  console.error('  1. 已构建 atb 二进制：cargo build --release（或设 ATB_E2E_BIN 指向已有二进制）');
  console.error(`     当前使用：${BIN}`);
  console.error('  2. daemon 已就绪：atb status 返回 running:true（未运行时可 atb restart 或先跑 atb status）');
  console.error('  3. Chrome/Edge 已安装并加载本扩展（assets/atb_bridge），扩展需连上 daemon');
  console.error('  4. 端口 18767 未被旧 daemon 占用（占用且 token 失配时用 atb restart --force）');
  console.error('');
  console.error('只想跑某一条：node scripts/e2e.mjs --only=scrape');
  console.error('');
}

/** 探测端口是否有人 LISTEN（不判断是谁）——用来区分「daemon 没起来」和「端口被占」。 */
function probePort(port, timeout = 1500) {
  return new Promise((resolve) => {
    const socket = createConnection({ port, host: '127.0.0.1' });
    const finish = (state) => {
      socket.destroy();
      resolve(state);
    };
    socket.setTimeout(timeout);
    socket.once('connect', () => finish('open'));
    socket.once('timeout', () => finish('timeout'));
    socket.once('error', () => finish('closed'));
  });
}

/** 返回 null 表示前置条件 OK，否则返回退出码。 */
async function preflight() {
  if (!existsSync(BIN)) {
    printPrerequisites(`找不到 atb 二进制：${BIN}`);
    return 2;
  }
  const status = await atb(['status'], { timeout: 30000 });
  const json = status.json;
  if (!json) {
    printPrerequisites(`atb status 没有输出可解析的 JSON（exit=${status.code}）：${short(status.stdout || status.stderr)}`);
    return 2;
  }
  if (json.running !== true) {
    const summary = json.summary || json.error_code || `exit=${status.code}`;
    printPrerequisites(`daemon 未就绪（${summary}）`);
    return 2;
  }

  // 扩展必须真的连上。旧实现写成 `!connected && activeTabs === 0`，于是只要浏览器里
  // 还开着一个页面（active_tabs 是上一次连接的残留计数）就放行，随后每个场景都在
  // 「扩展没连上」上失败 20 次——比直接报前置条件不满足难读得多。
  //
  // active_tabs 只用于报错信息、不再作为放行条件：本脚本自己会 open 页面，
  // 一个标签页都没有不影响跑（`atb status` 会因此 ok:false/summary=no_active_tabs，
  // 那是「当前没有页面」而不是「跑不了」，所以下面单独放行这个 summary）。
  const connected = Boolean(json.connection && json.connection.extension_connected);
  const activeTabs = Number((json.connection && json.connection.active_tabs) || 0);
  if (!connected) {
    printPrerequisites(
      `daemon 在跑，但扩展没有连上（extension_connected=false，active_tabs=${activeTabs}，` +
        `summary=${json.summary || `exit=${status.code}`}）。` +
        '浏览器里打开 http/https 页面并确认扩展已启用后重试',
    );
    return 2;
  }

  // 扩展仍在跑旧版本时，e2e 会以「新 API 不存在」的形式失败，看起来像 daemon 的 bug。
  // status 的 extension.stale 是明确信号：false 才是「磁盘上的版本已在浏览器里生效」。
  const extensionStale = json.extension && json.extension.stale;
  if (extensionStale !== false) {
    const bundled = json.extension && json.extension.bundled_version;
    const connectedVersions = json.extension && json.extension.connected_versions;
    printPrerequisites(
      `浏览器里的扩展版本不是磁盘上的最新版（extension.stale=${String(extensionStale)}，` +
        `bundled=${bundled ?? '?'}，connected=${JSON.stringify(connectedVersions ?? [])}）。` +
        '打开 chrome://extensions 对本扩展点「重新加载」后重试',
    );
    return 2;
  }

  // daemon 说自己在跑，但 API 端口连不上：多半是残留进程/端口冲突，早点说清楚。
  const apiPort = Number((json.ports && json.ports.api && (json.ports.api.listening ?? json.ports.api.port)) || 18767);
  const portState = await probePort(apiPort);
  if (portState !== 'open') {
    printPrerequisites(
      `daemon 报告 running:true，但 ${apiPort} 端口无法连接（${portState}）：可能是残留 daemon 或端口被占用，` +
        '先 `atb restart --force` 再重试',
    );
    return 2;
  }
  return null;
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

async function main() {
  const selected = ONLY
    ? SCENARIOS.filter((scenario) => scenario.name.includes(ONLY))
    : SMOKE
      ? SCENARIOS.filter((scenario) => SMOKE_NAMES.includes(scenario.name))
      : SCENARIOS;
  if (selected.length === 0) {
    console.error(`✗ --only=${ONLY} 没有匹配任何场景`);
    console.error(`  可用场景：${SCENARIOS.map((scenario) => scenario.name).join(', ')}`);
    return 2;
  }

  const blocked = await preflight();
  if (blocked !== null) return blocked;

  const { server, base } = await startServer();
  const summary = [];
  try {
    console.log(`atb e2e：${selected.length} 个场景${SMOKE ? '（ATB_E2E=smoke）' : ''}，页面根 ${base}`);
    console.log(`binary: ${BIN}`);
    console.log(`agent:  ${AGENT}`);
    console.log(`tmpdir: ${ensureTmpDir()}`);
    console.log('');

    for (const scenario of selected) {
      const checks = new Checks(scenario.name);
      const ctx = makeContext(base, checks);
      process.stdout.write(`▶ ${scenario.name} — ${scenario.title}\n`);
      let outcome = {};
      try {
        outcome = (await scenario.run(ctx)) || {};
      } catch (err) {
        checks.failures.push({ label: '场景抛出异常', expected: '正常完成', actual: String(err && err.stack ? err.stack : err) });
        checks.total += 1;
      }

      if (outcome.skip) {
        summary.push({ name: scenario.name, status: 'SKIP', checks, reason: outcome.skip });
        console.log(`   ⤼ SKIP：${outcome.skip}\n`);
        continue;
      }
      if (checks.failures.length === 0) {
        summary.push({ name: scenario.name, status: 'PASS', checks });
        console.log(`   ✓ PASS（${checks.total} 条断言）\n`);
      } else {
        summary.push({ name: scenario.name, status: 'FAIL', checks });
        checks.failures.forEach(printFailure);
        console.log('');
      }
    }
  } finally {
    // 收尾：只关本 agent 打开的标签页（ATB_AGENT 隔离），不让回归留下垃圾页。
    const cleanup = await atb(['cleanup'], { timeout: 60000 });
    if (cleanup.json && cleanup.json.ok === true) {
      console.log('cleanup：已关闭本次打开的标签页');
    } else {
      console.log(`cleanup：未完成（exit=${cleanup.code}）${short(cleanup.stdout || cleanup.stderr)}`);
    }
    // `server.close()` 只停止接收新连接；scenario 里 CLI/页面留下的 keep-alive
    // 连接会让 Node 进程一直不退出（表现为「场景全 PASS，但脚本挂住」）。
    // Node 18.2+ 的 closeAllConnections 主动断开空闲连接。
    server.closeAllConnections?.();
    server.close();
    if (!KEEP && TMP_DIR) {
      rmSync(TMP_DIR, { recursive: true, force: true });
    } else if (TMP_DIR) {
      console.log(`临时文件保留在 ${TMP_DIR}`);
    }
  }

  const passed = summary.filter((item) => item.status === 'PASS').length;
  const failed = summary.filter((item) => item.status === 'FAIL');
  const skipped = summary.filter((item) => item.status === 'SKIP').length;

  console.log('');
  console.log(`—— 汇总：${passed} PASS / ${failed.length} FAIL / ${skipped} SKIP ——`);
  for (const item of summary) {
    const detail =
      item.status === 'SKIP'
        ? ` — ${item.reason}`
        : item.status === 'FAIL'
          ? ` — ${item.checks.failures.length} 条断言失败`
          : ` — ${item.checks.total} 条断言`;
    console.log(`  ${item.status.padEnd(4)} ${item.name}${detail}`);
  }
  if (failed.length > 0) {
    console.log('');
    console.log('失败明细：');
    for (const item of failed) {
      console.log(`  ${item.name}`);
      item.checks.failures.forEach(printFailure);
    }
    process.exitCode = 1;
    return 1;
  }
  return 0;
}

const code = await main();
process.exitCode = code;
