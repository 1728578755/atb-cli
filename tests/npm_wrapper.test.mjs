// npm 包装层（npm/bin/atb.js）的行为测试。
//
// 为什么单独建这份：包装层是用户安装后唯一会执行的 JS，但此前 tests/ 对它零覆盖
// （`grep -rn "npm/bin" tests/` 无输出），所以「错误分支里引用不存在的变量 → 用户只看到
// ReferenceError」这类问题在 CI 完全不可见。
//
// 测试全部在子进程里跑，不装包、不起 daemon：
//   1. 把 wrapper 复制到临时目录，构造「预编译包缺失 + target/release 不存在」的环境；
//   2. 用 `node -r preload.cjs` 覆写 process.platform / process.arch，覆盖平台映射；
//   3. 用 ATB_BIN=<node> 让 wrapper 直接拉起 node，验证 ATB_PACKAGE_DIR 注入。
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..');
const WRAPPER = join(REPO_ROOT, 'npm', 'bin', 'atb.js');

/// 复制一份 wrapper 到临时目录：这样 `__dirname/../../target/release` 必然不存在，
/// 且 Node 不会从仓库的 node_modules 解析到已安装的平台包。
function isolatedWrapper() {
  const root = mkdtempSync(join(tmpdir(), 'atb-wrapper-'));
  const pkgDir = join(root, 'pkg');
  mkdirSync(join(pkgDir, 'npm', 'bin'), { recursive: true });
  copyFileSync(WRAPPER, join(pkgDir, 'npm', 'bin', 'atb.js'));
  writeFileSync(
    join(root, 'preload.cjs'),
    [
      "Object.defineProperty(process, 'platform', { value: process.env.FAKE_PLATFORM });",
      "Object.defineProperty(process, 'arch', { value: process.env.FAKE_ARCH });",
      'if (process.report) {',
      '  // 两个方向都要伪造：只伪造 glibc=1 时，Linux runner 上真实的',
      '  // glibcVersionRuntime 会漏进来，musl 用例被当成 glibc，断言随之失真。',
      '  process.report.getReport = () => (process.env.FAKE_GLIBC === "1"',
      "    ? { header: { glibcVersionRuntime: '2.31' } }",
      '    : { header: {} });',
      '}',
      '',
    ].join('\n'),
    'utf8',
  );
  return {
    root,
    entry: join(pkgDir, 'npm', 'bin', 'atb.js'),
    preload: join(root, 'preload.cjs'),
    cleanup: () => rmSync(root, { recursive: true, force: true }),
  };
}

/// 在伪造平台下运行 wrapper，返回 stderr/status。
function runAs(platform, arch, { glibc = false } = {}) {
  const sandbox = isolatedWrapper();
  try {
    const result = spawnSync(process.execPath, ['-r', sandbox.preload, sandbox.entry], {
      encoding: 'utf8',
      // wrapper 正常应在 1s 内退出；5s 是防挂死的硬上限。
      timeout: 5000,
      env: {
        ...process.env,
        FAKE_PLATFORM: platform,
        FAKE_ARCH: arch,
        FAKE_GLIBC: glibc ? '1' : '0',
        // 避免本机环境把测试引导到真实二进制/真实平台包。
        ATB_BIN: '',
        AGENT_BROWSER_CLI_BIN: '',
        NODE_PATH: '',
      },
    });
    return { status: result.status, stderr: result.stderr || '', stdout: result.stdout || '' };
  } finally {
    sandbox.cleanup();
  }
}

test('缺少二进制时是可读的 Error，而不是 ReferenceError', () => {
  const { status, stderr } = runAs('linux', 'x64', { glibc: true });
  assert.equal(status, 1, `预期退出码 1，实际 ${status}；stderr: ${stderr}`);
  assert.ok(!/ReferenceError/.test(stderr), `不应出现 ReferenceError：\n${stderr}`);
  assert.match(stderr, /atb 原生二进制缺失/);
  // 报错必须告诉用户缺的是哪个可选依赖，否则「--force 重装」无从下手。
  assert.match(stderr, /@g1728578755\/atb-linux-x64/);
  assert.match(stderr, /npm install @g1728578755\/atb/);
});

// 平台矩阵表驱动，覆盖 ×os ×arch ×libc 三轴；每条同时校验退出码与
// stderr 里的平台包名/自救提示。旧用例只 match 包名，即使 wrapper 以 0 退出
// （把错误吞掉）也会绿。
const PLATFORM_MATRIX = [
  { label: 'linux-x64 (glibc)', platform: 'linux', arch: 'x64', glibc: true, expect: /@g1728578755\/atb-linux-x64/ },
  { label: 'linux-arm64 (glibc)', platform: 'linux', arch: 'arm64', glibc: true, expect: /@g1728578755\/atb-linux-arm64/ },
  { label: 'darwin-x64', platform: 'darwin', arch: 'x64', glibc: false, expect: /@g1728578755\/atb-darwin-x64/ },
  { label: 'darwin-arm64', platform: 'darwin', arch: 'arm64', glibc: false, expect: /@g1728578755\/atb-darwin-arm64/ },
  { label: 'win32-x64', platform: 'win32', arch: 'x64', glibc: false, expect: /@g1728578755\/atb-win32-x64/ },
  {
    label: 'win32-arm64（无预编译包）',
    platform: 'win32',
    arch: 'arm64',
    glibc: false,
    expect: /未提供 win32-arm64 的预编译包/,
    also: /cargo build --release/,
    not: /可选依赖 @g1728578755\/atb-win32/,
  },
  {
    label: 'linux-x64 (musl/Alpine，无预编译包)',
    platform: 'linux',
    arch: 'x64',
    glibc: false,
    expect: /未提供 linux-x64 的预编译包/,
    also: /cargo build --release/,
    not: /@g1728578755\/atb-linux-x64/,
  },
];

for (const item of PLATFORM_MATRIX) {
  test(`平台矩阵：${item.label}`, () => {
    const { status, stderr } = runAs(item.platform, item.arch, { glibc: item.glibc });
    assert.equal(status, 1, `预期退出码 1，实际 ${status}；stderr: ${stderr}`);
    assert.match(stderr, item.expect, `stderr 未命中平台提示：
${stderr}`);
    if (item.also) assert.match(stderr, item.also, `缺少可操作提示：
${stderr}`);
    if (item.not) assert.doesNotMatch(stderr, item.not, `不应出现错误的安装引导：
${stderr}`);
  });
}

test('wrapper 把 ATB_PACKAGE_DIR 注入子进程 env', () => {
  const probe = 'console.log(JSON.stringify({pkg: process.env.ATB_PACKAGE_DIR, legacy: process.env.AGENT_BROWSER_CLI_PACKAGE_DIR}))';
  const result = spawnSync(process.execPath, [WRAPPER, '-e', probe], {
    encoding: 'utf8',
    env: { ...process.env, ATB_BIN: process.execPath },
  });
  assert.equal(result.status, 0, `预期退出码 0，实际 ${result.status}；stderr: ${result.stderr}`);
  const payload = JSON.parse((result.stdout || '').trim().split('\n').pop());
  assert.equal(payload.pkg, payload.legacy, '新旧两个环境变量必须指向同一目录');
  assert.ok(existsSync(join(payload.pkg, 'skills', 'atb', 'SKILL.md')), `ATB_PACKAGE_DIR 不是包根目录：${payload.pkg}`);
  // 包根目录应等于仓库根（npm/bin/atb.js 向上两级）。
  assert.equal(resolve(payload.pkg), REPO_ROOT);
});

test('wrapper 源码里不再有重复的二进制候选名', () => {
  const source = readFileSync(WRAPPER, 'utf8');
  const matched = /const\s+BINARY_NAMES\s*=\s*\[([^\]]+)\]/.exec(source);
  assert.ok(matched, '未找到 BINARY_NAMES 定义');
  const names = matched[1]
    .split(',')
    .map((item) => item.trim().replace(/^['"]|['"]$/g, ''))
    .filter(Boolean);
  assert.deepEqual(names, [...new Set(names)], `候选名存在重复：${names.join(', ')}`);
});

// `npm ci --dry-run` / `npm install --dry-run` 仍会执行 postinstall，但不装
// optionalDependencies —— 探测必然失败，把「只校验依赖树」误报成安装失败
// （ci.yml 的第一道锁文件门禁因此在干净 runner 上直接红）。dry-run 必须放行，
// 非 dry-run 仍要严格失败，否则 的安装期提示又失去意义。
const POSTINSTALL = join(REPO_ROOT, 'npm', 'postinstall.js');

function runPostinstall(extraEnv) {
  const env = { ...process.env };
  for (const key of ['ATB_BIN', 'AGENT_BROWSER_CLI_BIN', 'ATB_SKIP_POSTINSTALL_CHECK', 'npm_config_dry_run']) {
    delete env[key];
  }
  return spawnSync(process.execPath, [POSTINSTALL], {
    encoding: 'utf8',
    env: { ...env, ...extraEnv },
  });
}

test('dry-run 不因缺原生二进制把安装标成失败', () => {
  // ATB_PLATFORM_OVERRIDE 指向不存在的平台：保证「非 dry-run 分支必然找不到二进制」，
  // 这样这条断言在任何机器（含已装好平台包的开发机）上结论一致。
  const result = runPostinstall({ npm_config_dry_run: 'true', ATB_PLATFORM_OVERRIDE: 'plan9-x64' });
  assert.equal(result.status, 0, `dry-run 应 exit 0，实际 ${result.status}；stderr: ${result.stderr}`);
  assert.match(result.stdout, /dry-run/, `dry-run 应说明跳过了探测，实际 stdout: ${result.stdout}`);
});

test('非 dry-run 仍严格探测原生二进制', () => {
  const result = runPostinstall({ ATB_PLATFORM_OVERRIDE: 'plan9-x64' });
  assert.equal(result.status, 1, `缺二进制应 exit 1，实际 ${result.status}；stdout: ${result.stdout}`);
  assert.match(result.stderr, /没有找到可执行的 atb 原生二进制/);
});
