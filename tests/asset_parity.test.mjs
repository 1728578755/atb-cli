// 资源一致性守卫。
//
// assets/page_*.js 是「源文件」，assets/atb_bridge/ops/page_*.js 是扩展里
// 真正被 scripting.executeScript 注入的那一份。两份靠复制同步，历史上就出现过
// 只改一边、线上跑的是旧逻辑的问题（TOKEN_SELECTORS 被删掉就是这么漏出去的）。
// 这里做逐字节比对，并给出差异偏移，方便定位。
//
// 页面清单不再手工维护：`assets/page_*.js` 出现一个新脚本就自动纳入比对，
// 避免「新增 page_xxx.js 却忘了加进 PAGES」的静默漏洞。
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const ASSETS = join(ROOT, 'assets');
const OPS = join(ASSETS, 'atb_bridge', 'ops');

const PAGES = readdirSync(ASSETS)
  .filter((name) => /^page_.*\.js$/.test(name))
  .map((name) => name.replace(/^page_/, '').replace(/\.js$/, ''))
  .sort();
// 清单为空说明目录结构变了，后续循环会一条断言都不产生（假绿），必须显式判红。
const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : '\n        → ' + detail}`);
}
check('assets/page_*.js 页面清单非空', PAGES.length > 0, `扫描到 ${PAGES.length} 个页面脚本`);

function firstDiffOffset(a, b) {
  const limit = Math.min(a.length, b.length);
  for (let i = 0; i < limit; i += 1) {
    if (a[i] !== b[i]) return i;
  }
  return limit;
}

function describeMismatch(sourceBuf, copyBuf, offset) {
  const lineOf = (buf, at) => {
    let line = 1;
    for (let i = 0; i < at && i < buf.length; i += 1) if (buf[i] === 0x0a) line += 1;
    return line;
  };
  const snippet = (buf, at) => {
    const from = Math.max(0, at - 30);
    const to = Math.min(buf.length, at + 30);
    return JSON.stringify(buf.slice(from, to).toString('utf8'));
  };
  return [
    `首个差异偏移 ${offset}（源第 ${lineOf(sourceBuf, offset)} 行 / 副本第 ${lineOf(copyBuf, offset)} 行）`,
    `  源 : ${snippet(sourceBuf, offset)}`,
    `  副本: ${snippet(copyBuf, offset)}`,
  ].join('\n        ');
}

for (const page of PAGES) {
  const sourcePath = join(ASSETS, `page_${page}.js`);
  const copyPath = join(OPS, `page_${page}.js`);
  const label = `page_${page}.js 与 ops/page_${page}.js 逐字节一致`;

  if (!existsSync(sourcePath)) {
    check(label, false, `缺少源文件 assets/page_${page}.js`);
    continue;
  }
  if (!existsSync(copyPath)) {
    check(label, false, `缺少副本 assets/atb_bridge/ops/page_${page}.js`);
    continue;
  }

  const sourceBuf = readFileSync(sourcePath);
  const copyBuf = readFileSync(copyPath);

  if (sourceBuf.equals(copyBuf)) {
    check(label, true);
    continue;
  }

  const offset = firstDiffOffset(sourceBuf, copyBuf);
  const newer =
    statSync(sourcePath).mtimeMs >= statSync(copyPath).mtimeMs ? 'assets（源）' : 'ops（副本）';
  check(
    label,
    false,
    [
      `字节数  源=${sourceBuf.length}  副本=${copyBuf.length}  差=${copyBuf.length - sourceBuf.length}`,
      `较新的一边：${newer}`,
      describeMismatch(sourceBuf, copyBuf, offset),
      '修复方式：node scripts/sync-extension-assets.mjs（以 assets/ 为唯一真相源覆盖 ops/，幂等）',
    ].join('\n        '),
  );
}

// 防止有人再次把 token 抓取逻辑整段删掉：TOKEN_SELECTORS 是 challenge 绕过
// 的关键数据，删了不会报错，只会静默失效。
const challengePath = join(ASSETS, 'page_challenge.js');
if (!existsSync(challengePath)) {
  check('page_challenge.js 含 TOKEN_SELECTORS', false, '文件不存在');
} else {
  const source = readFileSync(challengePath, 'utf8');
  check('page_challenge.js 含 TOKEN_SELECTORS 定义', /const TOKEN_SELECTORS\s*=\s*\[/.test(source));
  check('page_challenge.js 使用 TOKEN_SELECTORS', /TOKEN_SELECTORS\.(forEach|map|filter|some|length)|for\s*\([^)]*TOKEN_SELECTORS/.test(source));
  const opsChallenge = join(OPS, 'page_challenge.js');
  if (existsSync(opsChallenge)) {
    check(
      'ops/page_challenge.js 含 TOKEN_SELECTORS',
      /TOKEN_SELECTORS/.test(readFileSync(opsChallenge, 'utf8')),
    );
  }
}

// 扩展版本漂移守卫：manifest.json 是唯一真相源。
//
// v6 的实锤：manifest 早已是 5.2.1，README 三处仍写「扩展 5.0.0」，用户按文档
// 判断「我装的是不是新的」会得到错误结论。这里直接拿 manifest 的版本去比对文案。
const manifest = JSON.parse(readFileSync(join(ASSETS, 'atb_bridge', 'manifest.json'), 'utf8'));
const extVersion = manifest.version;
const readmeZh = readFileSync(join(ROOT, 'README.md'), 'utf8');
const readmeEn = readFileSync(join(ROOT, 'README_EN.md'), 'utf8');
// SKILL.md 会被 `install-skill` 装进 ~/.agents/skills，是 Agent 判断「扩展要不要重载」
// 的依据；它曾经停在 5.0.0（实际 5.2.2），于是 Agent 会把新扩展当超前版本反复要求重载。
const skillMd = readFileSync(join(ROOT, 'skills', 'atb', 'SKILL.md'), 'utf8');
const featureStatus = readFileSync(join(ROOT, 'docs', 'FEATURE_STATUS.md'), 'utf8');
const versionChecks = [
  ['README 配套扩展版本', readmeZh.includes(`配套扩展：\`${extVersion}\``)],
  ['README 扩展文件版本', readmeZh.includes(`扩展文件版本（当前 **${extVersion}**）`)],
  ['README 本版本需要扩展版本', readmeZh.includes(`本版本需要扩展 ${extVersion}`)],
  ['README_EN companion extension', readmeEn.includes(`Companion extension: \`${extVersion}\``)],
  ['README_EN extension file version', readmeEn.includes(`extension file version (currently **${extVersion}**)`)],
  ['README_EN release requires extension', readmeEn.includes(`requires extension ${extVersion}`)],
  ['SKILL.md 扩展文件版本', skillMd.includes(`扩展文件版本（当前 **${extVersion}**）`)],
  ['FEATURE_STATUS 扩展版本', featureStatus.includes(`扩展 ${extVersion} ATB`)],
];
for (const [label, ok] of versionChecks) {
  check(`${label} = manifest ${extVersion}`, ok, `README 与 manifest.json（${extVersion}）不一致`);
}

// 更强的守卫：**当前文档**里出现的任何 `5.x.y` 都必须等于 manifest 版本。
// 只查固定句式会漏掉「安装文档里 7 处 5.2.5」这种整篇漂移（v11 实测踩到）。
// 历史记录（CHANGELOG_ARCHIVE）不在此列。
//
// 清单来源：
//   1. scripts/version-docs.json 的 extension[] —— 打包脚本与文档共用同一份落点，
//      新增文档自动纳入；文件缺失时回落到内置清单（离线 / 局部检出也能跑）。
//   2. 内置清单必须包含 SECURITY.md / PRIVACY.md：这两份是用户与审计判断扩展
//      版本的主要依据，曾经整篇停在 5.2.5 而守卫全绿。
const FALLBACK_DOCS = [
  'README.md',
  'README_EN.md',
  'AI_INSTALL.md',
  'SECURITY.md',
  'PRIVACY.md',
  'docs/FEATURE_STATUS.md',
  'docs/BEST_PRACTICES.md',
  'skills/atb/SKILL.md',
  'skills/atb/references/commands.md',
  'skills/atb/references/recipes.md',
  'skills/atb/references/troubleshooting.md',
  'CHANGELOG.md',
];
let docsSpec = FALLBACK_DOCS.map((file) => ({ file, exclude: [] }));
try {
  const parsed = JSON.parse(readFileSync(join(ROOT, 'scripts', 'version-docs.json'), 'utf8'));
  const fromSpec = Array.isArray(parsed.extension)
    ? parsed.extension
        .filter((entry) => entry && typeof entry.file === 'string')
        .map((entry) => ({
          file: entry.file,
          exclude: Array.isArray(entry.exclude) ? entry.exclude : [],
        }))
    : [];
  if (fromSpec.length > 0) docsSpec = fromSpec;
} catch {
  // 清单不存在/不可解析：继续用内置清单，守卫不能因此静默消失。
}

// 历史能力门槛（「扩展低于 5.0.0」「5.0.0 起」）说的是旧版本边界，不是「当前版本」。
// 这些行必须豁免，否则升级扩展会逼着文档篡改历史。
const HISTORICAL_THRESHOLD =
  /低于\s*5\.\d+\.\d+|older than\s*5\.\d+\.\d+|≥\s*5\.0\.0|5\.0\.0\s*(起|之前|的改动)/;
// CHANGELOG 的历史段落必然包含旧版本号，只扫文件头（当前版本表格/最新一节）。
const HEADER_LINES = { 'CHANGELOG.md': 24 };

const seen = new Set();
for (const spec of docsSpec) {
  if (seen.has(spec.file)) continue;
  seen.add(spec.file);
  const abs = join(ROOT, ...spec.file.split('/'));
  if (!existsSync(abs)) {
    check(`${spec.file} 存在（版本落点清单）`, false, '清单里的文档不存在');
    continue;
  }
  const text = readFileSync(abs, 'utf8');
  const headerLimit = HEADER_LINES[spec.file];
  const lines = text.split(/\r?\n/);
  const scanned = headerLimit ? lines.slice(0, headerLimit) : lines;
  const excludes = spec.exclude.map((pattern) => new RegExp(pattern));
  const versions = [];
  for (const line of scanned) {
    if (HISTORICAL_THRESHOLD.test(line)) continue;
    if (excludes.some((pattern) => pattern.test(line))) continue;
    for (const match of line.matchAll(/\b5\.\d+\.\d+\b/g)) versions.push(match[0]);
  }
  // 一条都没命中说明句式变了、文件被搬走或清单与文档脱节，守卫已经失效。
  check(
    `${spec.file} 能定位到扩展版本串（守卫未脱节）`,
    versions.length > 0,
    '文档里没有任何 5.x.y 版本串——落点清单可能已过期',
  );
  const stale = [...new Set(versions)].filter((version) => version !== extVersion);
  check(
    `${spec.file} 不含过期的扩展版本`,
    stale.length === 0,
    `出现了 ${stale.join(', ')}（manifest 是 ${extVersion}）——升级扩展后同步这些文档`,
  );
}

const failed = results.filter((item) => !item.ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
if (failed.length) {
  console.log('failed:');
  for (const item of failed) console.log('  - ' + item.name);
  process.exit(1);
}
process.exit(0);
