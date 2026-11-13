#!/usr/bin/env node
// 把 registry 上的平台包 dist.integrity 写回 package-lock.json。
//
// 为什么需要它：平台包的 tarball 由 publish 流水线现场构建，发布之前谁也不可能有它的
// 哈希，所以「刚 bump 完版本」的 package-lock.json 一定缺 integrity。而 npm ci 对
// optionalDependency 不做内容校验——这正是供应链里最该锁哈希的工件。
//
// 正确顺序（两段式发布）：
//   1. 先让平台包发布（publish.yml 的 publish-binaries）；
//   2. 本地执行 `node scripts/fill-lock-integrity.mjs`，把 integrity 写回 lock 并提交；
//   3. 重跑发布（平台包已存在会自动跳过），`check-lock --strict-integrity --registry` 通过后才发主包。
//
// 只读校验入口仍是 `scripts/check-lock.mjs`；本脚本只负责「补齐」，不改版本、不改 resolved。
//
// 用法：
//   node scripts/fill-lock-integrity.mjs            # 补缺失的 integrity
//   node scripts/fill-lock-integrity.mjs --check    # 只报告会改什么，不落盘（exit 1 表示有缺口）
//   node scripts/fill-lock-integrity.mjs --registry-url https://registry.npmmirror.com
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadConfig, platformPackageNames } from './platforms.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function parseArgs(argv) {
  const options = { check: false, registryUrl: null };
  for (let i = 0; i < argv.length; i += 1) {
    const current = argv[i];
    if (current === '--check') options.check = true;
    else if (current === '--registry-url') options.registryUrl = argv[++i] ?? null;
    else if (current.startsWith('--registry-url=')) options.registryUrl = current.slice('--registry-url='.length);
    else if (current === '--help' || current === '-h') {
      console.log('用法: fill-lock-integrity.mjs [--check] [--registry-url <url>]');
      process.exit(0);
    } else throw new Error(`未知参数: ${current}`);
  }
  return options;
}

function registryPackageUrl(registry, name) {
  const slash = name.indexOf('/');
  const encoded = slash === -1 ? name : `${name.slice(0, slash)}%2F${name.slice(slash + 1)}`;
  return `${registry.replace(/\/+$/, '')}/${encoded}`;
}

async function publishedIntegrity(registry, name, version) {
  const response = await fetch(registryPackageUrl(registry, name), {
    headers: { accept: 'application/json' },
    signal: AbortSignal.timeout(20000),
  });
  if (response.status === 404) return { missing: true };
  if (!response.ok) throw new Error(`${name}: HTTP ${response.status}`);
  const data = await response.json();
  const integrity = data?.versions?.[version]?.dist?.integrity;
  return integrity ? { integrity } : { missing: true };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const { registry, platforms } = loadConfig();
  const lockPath = join(ROOT, 'package-lock.json');
  if (!existsSync(lockPath)) throw new Error('package-lock.json 不存在');
  const pkg = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8'));
  const raw = readFileSync(lockPath, 'utf8');
  const lock = JSON.parse(raw);

  const problems = [];
  const filled = [];
  for (const name of platformPackageNames(pkg.name, platforms)) {
    const key = `node_modules/${name}`;
    const entry = lock.packages?.[key];
    if (!entry) {
      problems.push(`package-lock.json 缺少条目 ${key}（先运行 node scripts/sync-version.mjs --deps）`);
      continue;
    }
    const result = await publishedIntegrity(options.registryUrl ?? registry, name, entry.version);
    if (result.missing) {
      problems.push(
        `${name}@${entry.version} 在 registry 上还不存在（或没有 dist.integrity）：先发布平台包，再补 lock`,
      );
      continue;
    }
    if (entry.integrity === result.integrity) continue;
    if (entry.integrity && entry.integrity !== result.integrity) {
      problems.push(`${key} 的 integrity 与 registry 不一致：lock = ${entry.integrity}，registry = ${result.integrity}`);
      continue;
    }
    entry.integrity = result.integrity;
    filled.push(`${key} → ${result.integrity}`);
  }

  for (const line of filled) console.log(`${options.check ? '待补' : '已补'}: ${line}`);
  for (const line of problems) console.error(`问题: ${line}`);
  if (problems.length > 0) process.exit(1);
  if (options.check) {
    if (filled.length > 0) {
      console.error('lock 里还有平台包缺 integrity：去掉 --check 执行一次写入，然后提交 package-lock.json');
      process.exit(1);
    }
    console.log('lock 的平台包 integrity 与 registry 一致');
    return;
  }
  if (filled.length === 0) {
    console.log('lock 的平台包 integrity 已齐全，无需改动');
    return;
  }
  writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`);
  console.log(`已把 ${filled.length} 个平台包的 integrity 写入 package-lock.json，记得提交`);
}

try {
  await main();
} catch (error) {
  console.error(`补齐 integrity 失败: ${error.message}`);
  process.exit(1);
}
