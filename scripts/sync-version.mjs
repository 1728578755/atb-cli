#!/usr/bin/env node
// 版本同步与校验。两条独立版本线，各自的真相源不同：
//   CLI/daemon  ← package.json 的 version（同步 Cargo.toml / Cargo.lock / package-lock.json / 文档）
//   扩展        ← assets/atb_bridge/manifest.json 的 version（同步文档，不同步 CLI 文件）
//
// 用法：
//   node scripts/sync-version.mjs                     # 两条线一起对齐（含 optionalDependencies 与 package-lock）
//   node scripts/sync-version.mjs --check             # 只校验，默认同时校验 CLI 与扩展
//   node scripts/sync-version.mjs --check --expect 0.3.7   # 再把 tag 版本一并校验
//   node scripts/sync-version.mjs --check --ext       # 只校验扩展版本串
//   node scripts/sync-version.mjs --check --cli       # 只校验 CLI 版本串与锁文件
//   node scripts/sync-version.mjs --deps              # 只对齐 optionalDependencies + package-lock.json
//   node scripts/sync-version.mjs --cargo native/Cargo.toml
//
// 文档里哪些字符串算「版本」由 scripts/version-docs.json 集中维护：
// 之前只认 README 的两种固定句式，README 表格行、AI_INSTALL、SECURITY、PRIVACY、
// FEATURE_STATUS、BEST_PRACTICES、skills/** 的版本串完全无守卫，实测已漂移到 5.2.5。
//
// 两条防线：
//   1. 规则层面：每条 pattern 必须至少命中一次，0 命中即失败（以前只打 stderr 提示、
//      --check 照样 exit 0，规则可以从半失效烂到全失效）；
//   2. 兜底层面：sweep.files 里出现的任何 x.y.z 都必须等于当前 CLI/扩展版本，或在
//      sweep.allow / allowByFile 显式登记。README 标题里的版本号就是靠这条覆盖的。
//
// 写入模式是事务式的：任何一步失败（含结尾复核发现的 package-lock 漂移）都会把**所有**
// 改过的文件按原内容写回，不会留下半成品。
//
// 用法/退出码不变：0 一致或已写入；1 不一致、规则失效或出现未登记版本串（CI 直接失败）。

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadConfig, platformPackageNames } from './platforms.mjs';

const CANDIDATES = ['Cargo.toml', 'native/Cargo.toml', 'rust/Cargo.toml'];
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function parseArgs(argv) {
  const args = { check: false, expect: null, cargo: null, cli: false, ext: false, deps: false };
  for (let i = 0; i < argv.length; i += 1) {
    const current = argv[i];
    if (current === '--check') args.check = true;
    else if (current === '--expect') args.expect = argv[++i] ?? null;
    else if (current === '--cargo') args.cargo = argv[++i] ?? null;
    else if (current === '--cli') args.cli = true;
    else if (current === '--ext') args.ext = true;
    else if (current === '--deps') args.deps = true;
    else if (current === '--help' || current === '-h') {
      console.log(
        '用法: sync-version.mjs [--check] [--cli|--ext] [--deps] [--expect <version>] [--cargo <path>]',
      );
      process.exit(0);
    } else throw new Error(`未知参数: ${current}`);
  }
  // 不指定范围时两条线一起处理：CI 只跑一条命令，不会漏掉扩展版本。
  if (!args.cli && !args.ext) {
    args.cli = true;
    args.ext = true;
  }
  if (args.deps) {
    args.cli = false;
    args.ext = false;
  }
  return args;
}

function findCargo(root, explicit) {
  if (explicit) {
    const path = resolve(root, explicit);
    if (!existsSync(path)) throw new Error(`找不到 Cargo.toml: ${explicit}`);
    return path;
  }
  for (const rel of CANDIDATES) {
    const path = join(root, rel);
    if (existsSync(path)) return path;
  }
  throw new Error(`在 ${CANDIDATES.join(' / ')} 中都找不到 Cargo.toml`);
}

// 只改 [package] 段内的 version，避免误伤依赖版本
function readPackageField(lines, field) {
  const sectionStart = lines.findIndex((line) => /^\[package\]\s*$/.test(line));
  if (sectionStart === -1) throw new Error('Cargo.toml 缺少 [package] 段');
  const sectionEnd = lines.findIndex((line, index) => index > sectionStart && /^\[/.test(line));
  const limit = sectionEnd === -1 ? lines.length : sectionEnd;
  const pattern = new RegExp(`^\\s*${field}\\s*=\\s*"([^"]+)"`);
  for (let i = sectionStart + 1; i < limit; i += 1) {
    const matched = lines[i].match(pattern);
    if (matched) return { value: matched[1], lineIndex: i };
  }
  return null;
}

function readPackageVersion(lines) {
  const field = readPackageField(lines, 'version');
  if (!field) throw new Error('Cargo.toml 的 [package] 段内缺少 version');
  return field;
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// ---------------------------------------------------------------------------
// 文档版本串（scripts/version-docs.json 是清单，代码只负责机械执行）
// ---------------------------------------------------------------------------

function loadDocRules() {
  const path = join(ROOT, 'scripts', 'version-docs.json');
  if (!existsSync(path)) throw new Error(`缺少 scripts/version-docs.json（文档版本清单）: ${path}`);
  const rules = JSON.parse(readFileSync(path, 'utf8'));
  for (const key of ['cli', 'extension']) {
    if (!Array.isArray(rules[key]) || rules[key].length === 0) {
      throw new Error(`scripts/version-docs.json 的 ${key} 清单为空`);
    }
  }
  return rules;
}

/// 把匹配文本里的版本替换成 target。不能用 `match.replace(version, target)`：
/// 版本号可能同时出现在同一行的别处，只有捕获组那一处该改。
function withVersion(match, version, target) {
  const at = match.indexOf(version);
  if (at === -1) return match;
  return match.slice(0, at) + target + match.slice(at + version.length);
}

/// 扫描一组文档规则。
/// 返回 { problems, changes, found }：problems 是「不一致 / 守卫失效」，changes 是待写入的新全文，
/// found 是命中的 { file, line, value }（用来给兜底扫描判断「哪些旧版本号该被改写」）。
function scanDocs(root, entries, target) {
  const problems = [];
  const changes = [];
  const found = [];
  for (const entry of entries) {
    const path = join(root, entry.file);
    if (!existsSync(path)) {
      problems.push(`文档 ${entry.file} 不存在：版本守卫无法执行（恢复文件，或从 scripts/version-docs.json 移除）`);
      continue;
    }
    const text = readFileSync(path, 'utf8');
    const exclude = (entry.exclude ?? []).map((source) => new RegExp(source));
    const patterns = (entry.patterns ?? []).map((source) => new RegExp(source, 'g'));
    const hits = patterns.map(() => 0);
    const entryFound = [];
    const lines = text.split('\n');
    const nextText = lines
      .map((line, index) => {
        if (exclude.some((pattern) => pattern.test(line))) return line;
        let next = line;
        for (let p = 0; p < patterns.length; p += 1) {
          next = next.replace(patterns[p], (match, version) => {
            hits[p] += 1;
            entryFound.push({ line: index + 1, value: version });
            return withVersion(match, version, target);
          });
        }
        return next;
      })
      .join('\n');

    // 守卫空转是这一批缺陷的通病：一条都匹配不上时必须报错，
    // 不能让「文档改了措辞」静默变成「版本永远一致」。
    if (entryFound.length === 0) {
      problems.push(`${entry.file} 里没有任何可校验的版本串（规则失效，见 scripts/version-docs.json）`);
    }
    // ：单条规则 0 命中同样是失守。以前只打一行 stderr 注意、--check 仍 exit 0，
    // 规则可以从「半失效」烂到「全失效」才被拦住。
    for (let p = 0; p < patterns.length; p += 1) {
      if (hits[p] === 0) {
        problems.push(
          `${entry.file} 的第 ${p + 1} 条规则（${entry.patterns[p]}）没有命中：文档措辞可能已改，版本守卫失效`,
        );
      }
    }
    for (const item of entryFound) {
      found.push({ file: entry.file, ...item });
      if (item.value !== target) {
        problems.push(`${entry.file}:${item.line} 的版本 = ${item.value} ≠ ${target}`);
      }
    }
    if (nextText !== text) changes.push({ file: entry.file, text: nextText, target });
  }
  return { problems, changes, found };
}

// 兜底扫描：规则清单之外出现的任何 `x.y.z` 都必须有归属。
///
/// 为什么规则清单不够：README / README_EN 的章节标题「### 0.7.2 / 扩展 5.4.2 的新增能力」
/// 从来没有任何 pattern 覆盖，升版本后它会安静地停在旧号上——`--check` 全绿，用户看到两个版本。
/// 这里把所有文档里的版本号捞出来，只有「等于当前版本」「显式登记为历史/外部版本」才放行；
/// 写入模式下，等于**旧版本号**（本次要升掉的那个）的会被一起改写。
function scanSweep(root, sweep, target) {
  if (!sweep || !Array.isArray(sweep.files) || sweep.files.length === 0) {
    return { problems: ['scripts/version-docs.json 缺少 sweep.files：版本兜底扫描失效'], changes: [] };
  }
  const problems = [];
  const changes = [];
  const allowed = new Set(sweep.allow ?? []);
  const allowPatterns = (sweep.allowPatterns ?? []).map((source) => new RegExp(source));
  const allowByFile = sweep.allowByFile ?? {};
  const rewriteable = new Map([
    ...[...(target.oldCliVersions ?? [])].map((version) => [version, target.cli]),
    ...[...(target.oldExtVersions ?? [])].map((version) => [version, target.ext]),
  ]);

  for (const rel of sweep.files) {
    const path = join(root, rel);
    if (!existsSync(path)) {
      problems.push(`兜底扫描：文档 ${rel} 不存在（恢复文件，或从 scripts/version-docs.json 的 sweep.files 移除）`);
      continue;
    }
    const fileAllowed = new Set(allowByFile[rel] ?? []);
    const text = readFileSync(path, 'utf8');
    let changed = false;
    const nextText = text
      .split('\n')
      .map((line, index) => {
        return line.replace(/\b(\d+\.\d+\.\d+)\b/g, (match, version, offset, whole) => {
          if (version === target.cli || version === target.ext) return match;
          if (allowed.has(version) || fileAllowed.has(version)) return match;
          if (allowPatterns.some((pattern) => pattern.test(version))) return match;
          const replacement = rewriteable.get(version);
          if (replacement) {
            changed = true;
            return withVersion(match, version, replacement);
          }
          problems.push(
            `${rel}:${index + 1} 的版本串 ${version} 既不是 CLI ${target.cli}、也不是扩展 ${target.ext}：` +
              '如果是历史/外部版本，请登记到 scripts/version-docs.json 的 sweep.allow 或 allowByFile；' +
              '如果是漏改的版本号，请补一条 cli/extension 规则',
          );
          return match;
        });
      })
      .join('\n');
    if (changed) changes.push({ file: rel, text: nextText, target: target.cli });
  }
  return { problems, changes };
}

// ---------------------------------------------------------------------------
// package-lock.json / Cargo.lock
// ---------------------------------------------------------------------------

function compareList(actual, expected) {
  if (!Array.isArray(actual)) return false;
  const left = [...actual].sort();
  const right = [...expected].sort();
  return left.length === right.length && left.every((item, index) => item === right[index]);
}

// package-lock 的一致性校验：npm ci 在 version 漂移时会直接 EUSAGE。
///
// v6 P3 只比版本号； 起连平台条目的 os/cpu/resolved 一起比——这几个字段错了
/// npm 会「安装成功」但把错误的架构装进来（或直接跳过该 optional 依赖），
/// 用户要等到运行时 native binary not found 才发现。
function checkLock(root, pkg, platforms, registry) {
  const lockPath = join(root, 'package-lock.json');
  if (!existsSync(lockPath)) {
    // 缺失不是「没问题」。发布门禁只跑 --check，这里静默放行等于门禁不存在。
    return ['package-lock.json 不存在：npm ci 与可选依赖解析都会失败（先 `npm install --package-lock-only`）'];
  }
  const version = pkg.version;
  const lock = JSON.parse(readFileSync(lockPath, 'utf8'));
  const problems = [];
  if (lock.version !== version) {
    problems.push(`package-lock.json 顶层 version = ${lock.version ?? '(缺失)'}，应为 ${version}`);
  }
  const rootPackage = lock.packages?.[''] ?? {};
  if (rootPackage.version !== version) {
    problems.push(`package-lock.json 中 packages[""].version = ${rootPackage.version ?? '(缺失)'}，应为 ${version}`);
  }

  const expected = platformPackageNames(pkg.name, platforms);
  const declared = pkg.optionalDependencies ?? {};
  const lockDeclared = rootPackage.optionalDependencies ?? {};
  for (const name of expected) {
    if (declared[name] === undefined) {
      problems.push(`package.json optionalDependencies 缺少平台包 ${name}`);
    } else if (declared[name] !== version) {
      problems.push(`package.json optionalDependencies["${name}"] = ${declared[name]}，应为 ${version}`);
    }
    if (lockDeclared[name] === undefined) {
      problems.push(`package-lock.json 顶层 optionalDependencies 缺少 ${name}`);
    } else if (lockDeclared[name] !== declared[name]) {
      problems.push(
        `package-lock.json optionalDependencies["${name}"] = ${lockDeclared[name]} ≠ package.json = ${declared[name]}`,
      );
    }
  }
  for (const name of Object.keys(declared)) {
    if (!expected.includes(name)) {
      problems.push(`package.json optionalDependencies 多出 scripts/platforms.json 未声明的平台包 ${name}`);
    }
  }
  for (const name of Object.keys(lockDeclared)) {
    if (!expected.includes(name)) {
      problems.push(`package-lock.json 中多出 scripts/platforms.json 未声明的平台包 ${name}`);
    }
  }

  for (const platform of platforms) {
    const name = `${pkg.name}-${platform.name}`;
    const entry = lock.packages?.[`node_modules/${name}`];
    if (!entry) {
      problems.push(`package-lock.json 缺少平台包条目 ${name}`);
      continue;
    }
    if (entry.version !== version) {
      problems.push(`${name} 在 package-lock.json 中的 version = ${entry.version ?? '(缺失)'}，应为 ${version}`);
    }
    if (!compareList(entry.os, [platform.os])) {
      problems.push(`${name} 的 os = ${JSON.stringify(entry.os ?? null)}，应为 ${JSON.stringify([platform.os])}`);
    }
    if (!compareList(entry.cpu, [platform.cpu])) {
      problems.push(`${name} 的 cpu = ${JSON.stringify(entry.cpu ?? null)}，应为 ${JSON.stringify([platform.cpu])}`);
    }
    const tarball = `${registry}/${name}/-/${name.split('/')[1]}-${version}.tgz`;
    if (entry.resolved !== tarball) {
      problems.push(`${name} 的 resolved = ${entry.resolved ?? '(缺失)'}，应为 ${tarball}`);
    }
    if (entry.integrity === undefined) {
      // 刚 bump 完还没重新 npm install 时，新 tarball 的 integrity 无法离线算出
      // （sync-version 会把过期的删掉），这是唯一合法缺失场景：resolved 已指向当前版本。
      // 其余情况缺失说明有人手改了 lock，必须报错（ATB_LOCK_ALLOW_MISSING_INTEGRITY=1 可降级）。
      const resolvedIsCurrent = typeof entry.resolved === 'string' && entry.resolved.includes(`-${version}.tgz`);
      if (!resolvedIsCurrent && process.env.ATB_LOCK_ALLOW_MISSING_INTEGRITY !== '1') {
        problems.push(`${name} 的 integrity 缺失（resolved 也不是当前版本 ${version}）`);
      }
    } else if (!/^sha(1|512)-[A-Za-z0-9+/=]+$/.test(entry.integrity)) {
      problems.push(`${name} 的 integrity 格式非法: ${entry.integrity}`);
    }
  }
  return problems;
}

/// Cargo.lock 也要跟着 package.json 走：`cargo build --locked` 以 lock 为准，
// 漏改会让下游构建直接失败。
///
/// `sourceLabel` 只影响报错措辞：写入前的自检比较的是「lock vs 当前 Cargo.toml」，
// 说成「≠ package.json」会把人引向错误的文件。
function checkCargoLock(cargoPath, packageName, version, sourceLabel = 'package.json') {
  const lockPath = join(dirname(cargoPath), 'Cargo.lock');
  if (!existsSync(lockPath)) {
    // --check 下静默放行等于没有门禁。
    return [`${lockPath} 不存在：cargo build --locked 会直接失败，先执行 cargo generate-lockfile`];
  }
  const text = readFileSync(lockPath, 'utf8');
  const blocks = text.split(/^\[\[package\]\]\s*$/m);
  const namePattern = new RegExp(`^name\\s*=\\s*"${escapeRegExp(packageName)}"\\s*$`, 'm');
  for (const block of blocks) {
    if (!namePattern.test(block)) continue;
    const matched = /^version\s*=\s*"([^"]+)"\s*$/m.exec(block);
    if (!matched) return [`Cargo.lock 中 ${packageName} 缺少 version`];
    if (matched[1] !== version) {
      return [`Cargo.lock 中 ${packageName} 的 version = ${matched[1]} ≠ ${sourceLabel} = ${version}`];
    }
    return [];
  }
  return [`Cargo.lock 中找不到根包 ${packageName}`];
}

function gitAdd(root, paths) {
  if (paths.length === 0) return;
  try {
    execFileSync('git', ['add', ...paths], { cwd: root, stdio: 'pipe' });
    console.log(`已 git add ${paths.join(' ')}`);
  } catch {
    console.log(`跳过 git add（${root} 不是 git 仓库或 git 不可用）`);
  }
}

// 改完 Cargo.toml 后让 cargo 自己把 Cargo.lock 的根包版本写回去。
//
// 手写 lock 看起来更快，但 lock 里还有 checksum / 依赖图等 cargo 自己维护的字段，
// 一旦格式跟着改版就会写出 cargo 不认的文件。这里只用「官方命令 + 事后校验」：
// `cargo update -p <包名> --precise <版本>` 是唯一能把根包钉到指定版本的写法。
// 失败返回错误摘要（调用方据此回滚），成功返回 null。
function refreshCargoLock(cargoPath, packageName, version) {
  const cwd = dirname(cargoPath);
  const attempts = [
    ['update', '-p', packageName, '--precise', version],
    // 无网环境（CI 容器、离线开发机）里第一次会卡在索引更新，退回离线模式再试一次。
    ['update', '-p', packageName, '--precise', version, '--offline'],
  ];
  let lastError = null;
  for (const args of attempts) {
    try {
      execFileSync('cargo', args, { cwd, stdio: 'pipe' });
      return null;
    } catch (error) {
      lastError = error;
    }
  }
  const detail = String(lastError?.stderr ?? lastError?.message ?? lastError).trim();
  return detail.split('\n').slice(0, 6).join('\n') || 'cargo 不可用';
}

// ---------------------------------------------------------------------------
// 写入：optionalDependencies + package-lock.json
// ---------------------------------------------------------------------------

/// 计算对齐后的 package.json / package-lock.json 文本；不落盘，由调用方统一提交/回滚。
function planOptionalDeps(pkg, lock, version, platforms, registry) {
  const nextPkg = { ...pkg, optionalDependencies: { ...(pkg.optionalDependencies ?? {}) } };
  for (const platform of platforms) {
    nextPkg.optionalDependencies[`${pkg.name}-${platform.name}`] = version;
  }

  const nextLock = lock ? { ...lock } : null;
  if (nextLock) {
    nextLock.version = version;
    nextLock.packages = { ...(nextLock.packages ?? {}) };
    const rootPackage = { ...(nextLock.packages[''] ?? {}) };
    rootPackage.version = version;
    rootPackage.optionalDependencies = { ...(rootPackage.optionalDependencies ?? {}) };
    for (const platform of platforms) {
      rootPackage.optionalDependencies[`${pkg.name}-${platform.name}`] = version;
    }
    nextLock.packages[''] = rootPackage;

    for (const platform of platforms) {
      const name = `${pkg.name}-${platform.name}`;
      const key = `node_modules/${name}`;
      // 保持键序：已有条目按原顺序覆盖，只有新条目才追加 `optional`，
      // 否则每次 --deps 都会把 package-lock.json 整段重排成假 diff。
      const entry = { ...(nextLock.packages[key] ?? {}) };
      const tarball = `${registry}/${name}/-/${name.split('/')[1]}-${version}.tgz`;
      // 版本变了 → resolved 指向的 tarball 变了 → 旧的 integrity 必然对不上，删掉由 npm 重算。
      if (entry.resolved !== tarball && entry.integrity) delete entry.integrity;
      entry.version = version;
      entry.resolved = tarball;
      entry.os = [platform.os];
      entry.cpu = [platform.cpu];
      entry.optional = true;
      nextLock.packages[key] = entry;
    }
  }
  return { nextPkg, nextLock };
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const root = ROOT;

  const pkgPath = join(root, 'package.json');
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));
  const packageVersion = pkg.version;
  if (!packageVersion) throw new Error('package.json 缺少 version');

  const { registry, platforms } = loadConfig();
  const lockPath = join(root, 'package-lock.json');

  // --deps：只对齐 optionalDependencies 与 package-lock.json（prepack / 旧脚本入口用）。
  if (args.deps) {
    const lock = existsSync(lockPath) ? JSON.parse(readFileSync(lockPath, 'utf8')) : null;
    const { nextPkg, nextLock } = planOptionalDeps(pkg, lock, packageVersion, platforms, registry);
    const nextPkgText = `${JSON.stringify(nextPkg, null, 2)}\n`;
    if (nextPkgText !== readFileSync(pkgPath, 'utf8')) {
      writeFileSync(pkgPath, nextPkgText);
      console.log(`已把 optionalDependencies（${platforms.length} 个平台）对齐到 ${packageVersion}`);
    }
    if (nextLock) {
      const nextLockText = `${JSON.stringify(nextLock, null, 2)}\n`;
      if (nextLockText !== readFileSync(lockPath, 'utf8')) {
        writeFileSync(lockPath, nextLockText);
        console.log(`已把 package-lock.json 的平台条目对齐到 ${packageVersion}`);
      }
    } else {
      console.log('package-lock.json 不存在，跳过（先 npm install --package-lock-only）');
    }
    return;
  }

  const manifestPath = join(root, 'assets', 'atb_bridge', 'manifest.json');
  if (!existsSync(manifestPath)) throw new Error(`缺少扩展清单 ${manifestPath}`);
  const extVersion = JSON.parse(readFileSync(manifestPath, 'utf8')).version;
  if (!extVersion) throw new Error('assets/atb_bridge/manifest.json 缺少 version');

  const docRules = loadDocRules();
  const problems = [];
  const changes = [];
  const collect = (result) => {
    problems.push(...result.problems);
    changes.push(...result.changes);
  };
  const cliFound = [];
  const extFound = [];

  let cargoPath = null;
  let cargoName = null;
  let cargoVersion = null;
  let cargoLineIndex = -1;
  let cargoLines = null;
  let cargoRel = null;

  if (args.cli) {
    cargoPath = findCargo(root, args.cargo);
    cargoRel = relative(root, cargoPath).split('\\').join('/');
    cargoLines = readFileSync(cargoPath, 'utf8').split('\n');
    const parsed = readPackageVersion(cargoLines);
    cargoVersion = parsed.value;
    cargoLineIndex = parsed.lineIndex;
    cargoName = readPackageField(cargoLines, 'name')?.value;
    if (!cargoName) throw new Error(`${cargoRel} 的 [package] 段内缺少 name`);

    if (cargoVersion !== packageVersion) {
      problems.push(`${cargoRel} 中的 version = ${cargoVersion} ≠ package.json = ${packageVersion}`);
    }
    if (args.expect && args.expect !== packageVersion) {
      problems.push(`期望版本 = ${args.expect} ≠ package.json = ${packageVersion}`);
    }
    problems.push(...checkLock(root, pkg, platforms, registry));
    problems.push(...checkCargoLock(cargoPath, cargoName, packageVersion));
    const cliScan = scanDocs(root, docRules.cli, packageVersion);
    collect(cliScan);
    cliFound.push(...cliScan.found);
  }
  if (args.ext) {
    const extScan = scanDocs(root, docRules.extension, extVersion);
    collect(extScan);
    extFound.push(...extScan.found);
  }

  // 兜底扫描：规则清单之外的版本串必须有归属。只在 --check 分支里算问题；
  // 写入模式在改完规则命中的版本串之后再扫一遍（否则两个改动会互相覆盖）。
  const sweepTarget = {
    cli: packageVersion,
    ext: extVersion,
    oldCliVersions: new Set(cliFound.filter((item) => item.value !== packageVersion).map((item) => item.value)),
    oldExtVersions: new Set(extFound.filter((item) => item.value !== extVersion).map((item) => item.value)),
  };

  if (args.check) {
    const sweepResult = scanSweep(root, docRules.sweep, sweepTarget);
    problems.push(...sweepResult.problems);
    if (problems.length === 0) {
      const scope = [args.cli ? `CLI ${packageVersion}` : null, args.ext ? `扩展 ${extVersion}` : null]
        .filter(Boolean)
        .join(' / ');
      console.log(`版本一致: ${scope}（文档 / package-lock.json / Cargo.lock 全部匹配）`);
      return;
    }
    for (const line of problems) console.error(`版本不一致: ${line}`);
    if (problems.some((line) => line.includes('Cargo.lock'))) {
      console.error('Cargo.lock 由 cargo 维护：先改 Cargo.toml，再执行 `cargo update -p <包名> --precise <版本>` 或 `cargo build` 后重试');
    }
    if (problems.some((line) => line.includes('package-lock.json') || line.includes('optionalDependencies'))) {
      console.error('运行 `node scripts/sync-version.mjs --deps` 重新对齐 package-lock.json 后重试');
    }
    if (problems.some((line) => /\.md:/.test(line) || line.includes('没有可校验的版本串'))) {
      console.error('文档版本串：运行 `node scripts/sync-version.mjs`（或 --ext）自动改写，规则在 scripts/version-docs.json');
    }
    if (problems.some((line) => line.includes('没有命中') || line.includes('既不是 CLI'))) {
      console.error(
        '文档守卫失效：脚本里新增了规则却没命中、或出现了未登记的版本串 —— ' +
          '补 cli/extension 规则，或把历史/外部版本登记到 scripts/version-docs.json 的 sweep.allow / allowByFile',
      );
    }
    process.exit(1);
  }

  // -------------------------------------------------------------------------
  // 写入模式
  // -------------------------------------------------------------------------

  const backups = new Map();
  const writeTracked = (path, next) => {
    const current = readFileSync(path, 'utf8');
    if (current === next) return false;
    if (!backups.has(path)) backups.set(path, current);
    writeFileSync(path, next);
    return true;
  };
  const rollback = () => {
    const restored = [];
    for (const [path, text] of backups) {
      try {
        writeFileSync(path, text);
        restored.push(relative(root, path).split('\\').join('/'));
      } catch {
        // 回滚失败也要继续尝试其余文件：能救一个是一个。
      }
    }
    return restored;
  };

  const written = [];

  if (args.cli) {
    // 写入前确认 Cargo.lock 与**当前** Cargo.toml 一致（否则分不清是新旧版本差异还是历史漂移）。
    const cargoLockPath = join(dirname(cargoPath), 'Cargo.lock');
    const hasCargoLock = existsSync(cargoLockPath);
    if (hasCargoLock) {
      const drift = checkCargoLock(cargoPath, cargoName, cargoVersion, cargoRel);
      if (drift.length > 0) {
        throw new Error(
          `写入前检查失败，Cargo.lock 与当前 ${cargoRel} 不一致：${drift.join('；')}\n` +
            '先在一个干净的版本上把 lock 修好（`cargo update -p <包名> --precise <版本>`），再执行版本同步',
        );
      }
    }

    if (cargoVersion !== packageVersion) {
      const indent = cargoLines[cargoLineIndex].match(/^\s*/)[0];
      const nextLines = [...cargoLines];
      nextLines[cargoLineIndex] = `${indent}version = "${packageVersion}"`;
      if (writeTracked(cargoPath, nextLines.join('\n'))) {
        console.log(`已把 ${cargoRel} 的版本从 ${cargoVersion} 改为 ${packageVersion}`);
        written.push(cargoRel);
      }
    }

    // optionalDependencies + package-lock 与版本同一次事务。
    const lock = existsSync(lockPath) ? JSON.parse(readFileSync(lockPath, 'utf8')) : null;
    const { nextPkg, nextLock } = planOptionalDeps(pkg, lock, packageVersion, platforms, registry);
    if (writeTracked(pkgPath, `${JSON.stringify(nextPkg, null, 2)}\n`)) {
      console.log(`已把 optionalDependencies（${platforms.length} 个平台）对齐到 ${packageVersion}`);
      written.push('package.json');
    }
    Object.assign(pkg, nextPkg);
    if (nextLock) {
      if (writeTracked(lockPath, `${JSON.stringify(nextLock, null, 2)}\n`)) {
        console.log(`已把 package-lock.json 的平台条目对齐到 ${packageVersion}`);
        written.push('package-lock.json');
      }
    }

    // Cargo.toml 改过之后 lock 必然落后一步：交给 cargo 刷新，再复核一遍。
    let cargoLockRefreshed = false;
    if (hasCargoLock && checkCargoLock(cargoPath, cargoName, packageVersion).length > 0) {
      if (!backups.has(cargoLockPath)) backups.set(cargoLockPath, readFileSync(cargoLockPath, 'utf8'));
      const failure = refreshCargoLock(cargoPath, cargoName, packageVersion);
      const remaining = failure ? null : checkCargoLock(cargoPath, cargoName, packageVersion);
      if (failure !== null || remaining.length > 0) {
        const restored = rollback();
        throw new Error(
          `Cargo.lock 刷新失败${failure ? `：${failure}` : `：${remaining.join('；')}`}\n` +
            `已回滚 ${restored.length ? restored.join(' / ') : '(无文件需要回滚)'}；` +
            `请手动执行 \`cargo update -p ${cargoName} --precise ${packageVersion}\` 后重试`,
        );
      }
      console.log(`已用 cargo 把 Cargo.lock 中 ${cargoName} 的版本更新为 ${packageVersion}`);
      cargoLockRefreshed = true;
    }
    if (hasCargoLock && cargoLockRefreshed) {
      const lockRel = relative(root, cargoLockPath).split('\\').join('/');
      if (!lockRel.startsWith('..') && !written.includes(lockRel)) written.push(lockRel);
    }
  }

  for (const change of changes) {
    const path = join(root, change.file);
    if (writeTracked(path, change.text)) {
      console.log(`已把 ${change.file} 的版本串改写为 ${change.target}`);
      written.push(change.file);
    }
  }

  // 规则命中改写完之后再跑兜底扫描：此时磁盘上是新内容，旧版本号可以安全地一并改掉，
  // 两轮改动不会互相覆盖。
  const sweepWrite = scanSweep(root, docRules.sweep, sweepTarget);
  for (const change of sweepWrite.changes) {
    const path = join(root, change.file);
    if (writeTracked(path, change.text)) {
      console.log(`已把 ${change.file} 里未登记的旧版本串改写为 ${change.target}`);
      written.push(change.file);
    }
  }

  // 结尾复核用**写完之后**的状态：前面的 problems 是写入前算的。
  // 这里必须也走 rollback：否则「Cargo.toml 升了、package-lock 漂了」会留下
  // 一个既不能用 `npm ci` 也不能用 `cargo build --locked` 的半成品工作区。
  const remainingProblems = [];
  if (args.cli) {
    remainingProblems.push(...checkLock(root, pkg, platforms, registry));
    remainingProblems.push(...checkCargoLock(cargoPath, cargoName, packageVersion));
    remainingProblems.push(...scanDocs(root, docRules.cli, packageVersion).problems);
  }
  if (args.ext) {
    remainingProblems.push(...scanDocs(root, docRules.extension, extVersion).problems);
  }
  // 写完后的兜底扫描不再有「旧版本可改写」，任何残留都是真问题。
  remainingProblems.push(
    ...scanSweep(root, docRules.sweep, { ...sweepTarget, oldCliVersions: new Set(), oldExtVersions: new Set() }).problems,
  );
  if (remainingProblems.length > 0) {
    const restored = rollback();
    for (const line of remainingProblems) console.error(`版本不一致: ${line}`);
    console.error(
      `已回滚 ${restored.length ? restored.join(' / ') : '(无文件需要回滚)'}；` +
        'lock 文件不由本脚本手写：package-lock.json 用 `node scripts/sync-version.mjs --deps`，Cargo.lock 用 cargo（--locked 构建以它为准）',
    );
    process.exit(1);
  }

  gitAdd(root, [...new Set(written)]);

  if (args.expect && args.expect !== packageVersion) {
    console.error(`注意: 期望版本 = ${args.expect} ≠ package.json = ${packageVersion}`);
  }
}

try {
  main();
} catch (error) {
  console.error(`版本同步失败: ${error.message}`);
  process.exit(1);
}
