#!/usr/bin/env node
// 平台清单读取器：`scripts/platforms.json` 是唯一真相源，本文件只做「读取 + 派生」，
// 不持有任何平台数据。
//
// 为什么要有这一层：
//   平台清单此前在多个脚本与 npm/bin/atb.js、
//   publish.yml 的 matrix、mirror-sync 的 packages 数组里各写一遍，5+ 处手工同步。
//   漏一处的后果不是报错而是「静默错配」：package.json 有该平台包、matrix 不构建它，
//   用户 npm 装到的 optionalDependency 永远 404。
//
// 用法（CI 用，避免在 YAML 里内联 node -e 的引号地狱）：
//   node scripts/platforms.mjs --json          # 原样输出 platforms.json
//   node scripts/platforms.mjs --matrix        # GitHub Actions matrix 的 include 数组
//   node scripts/platforms.mjs --npm-packages  # 主包 + 全部平台子包名（每行一个）
//   node scripts/platforms.mjs --keys          # 平台 key（每行一个）
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CONFIG_PATH = join(ROOT, 'scripts', 'platforms.json');

function loadConfig() {
  const config = JSON.parse(readFileSync(CONFIG_PATH, 'utf8'));
  const platforms = Array.isArray(config.platforms) ? config.platforms : [];
  if (platforms.length === 0) throw new Error(`${CONFIG_PATH} 里没有 platforms`);
  for (const item of platforms) {
    for (const field of ['name', 'os', 'cpu']) {
      if (!item[field]) throw new Error(`${CONFIG_PATH} 的 ${item.name ?? '(未命名)'} 缺少 ${field}`);
    }
  }
  return { registry: config.registry, platforms };
}

/// 主包名 → 平台子包名。作用域也从 package.json 读，不在这里写死。
function platformPackageNames(baseName, platforms) {
  return platforms.map((item) => `${baseName}-${item.name}`);
}

/// GitHub Actions 的 matrix 形状。os 用 runner，二进制名带上平台以便 artifact 不互相覆盖。
function matrixOf(platforms) {
  return {
    include: platforms.map((item) => ({
      name: item.name,
      os: item.runner,
      target: item.rustTarget,
      binary: item.binary,
    })),
  };
}

function main(argv) {
  const { registry, platforms } = loadConfig();
  if (argv.includes('--json')) {
    console.log(JSON.stringify({ registry, platforms }, null, 2));
    return 0;
  }
  if (argv.includes('--matrix')) {
    console.log(JSON.stringify(matrixOf(platforms)));
    return 0;
  }
  const pkgPath = join(ROOT, 'package.json');
  const baseName = existsSync(pkgPath) ? JSON.parse(readFileSync(pkgPath, 'utf8')).name : '@g1728578755/atb';
  if (argv.includes('--npm-packages')) {
    for (const name of [baseName, ...platformPackageNames(baseName, platforms)]) console.log(name);
    return 0;
  }
  if (argv.includes('--keys')) {
    for (const item of platforms) console.log(item.name);
    return 0;
  }
  console.error('用法: node scripts/platforms.mjs [--json|--matrix|--npm-packages|--keys]');
  return 2;
}

const invokedDirectly =
  process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url));
if (invokedDirectly) process.exit(main(process.argv.slice(2)));

export { loadConfig, platformPackageNames, matrixOf, CONFIG_PATH };
