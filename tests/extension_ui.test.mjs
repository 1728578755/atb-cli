// 扩展界面与资源引用的静态守卫。
//
// 背景：popup / options 是纯静态页面，路径写错（少一个 `ui/` 前缀、图标改名忘了改
// manifest）不会让任何 JS 报错，只会在用户点开时看到空白或破图；浏览器扩展也没有
// 构建步骤会替我们检查。这里在 CI 里做四件事：
//   1. manifest 声明的 options 页面、图标文件必须真实存在；
//   2. 两个页面里所有本地 `href/src` 都必须能解析到文件（外链 / data: / # 除外）；
//   3. 不允许内联脚本（MV3 CSP 只允许 `script-src 'self'`，内联会被静默拦掉）；
//   4. popup 的分区、options 的分区、两边的设置键必须对得上，避免改了一边忘了另一边。
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXT = join(ROOT, 'assets', 'atb_bridge');
const read = (name) => readFileSync(join(EXT, name), 'utf8');
const manifest = JSON.parse(read('manifest.json'));

function localRefs(html) {
  return [...html.matchAll(/(?:href|src)\s*=\s*"([^"]+)"/g)]
    .map((m) => m[1])
    .filter((url) => !/^(?:https?:|data:|mailto:|#|\/\/)/i.test(url));
}

test('manifest 声明的 options 页面与四档图标都存在', () => {
  assert.ok(manifest.options_ui, '缺少 options_ui：管理页面无法从扩展详情打开');
  assert.equal(manifest.options_ui.open_in_tab, true, '管理页面应该在标签页里打开（弹窗太小）');
  assert.ok(existsSync(join(EXT, manifest.options_ui.page)), `options_ui.page 指向不存在的文件：${manifest.options_ui.page}`);

  const icons = new Set([...(manifest.icons ? Object.values(manifest.icons) : []), ...(manifest.action?.default_icon ? Object.values(manifest.action.default_icon) : [])]);
  assert.ok(icons.size >= 4, `图标尺寸不足：${[...icons].join(', ')}`);
  for (const icon of icons) {
    assert.ok(existsSync(join(EXT, icon)), `manifest 引用了不存在的图标：${icon}`);
  }
});

test('popup / options 的本地资源引用全部可解析', () => {
  for (const page of ['popup.html', 'options.html']) {
    const refs = localRefs(read(page));
    assert.ok(refs.length >= 4, `${page} 的本地资源太少（${refs.length}），守卫可能已脱节`);
    for (const ref of refs) {
      assert.ok(existsSync(join(EXT, ref)), `${page} 引用了不存在的文件：${ref}`);
    }
  }
});

test('扩展页面不含内联脚本（MV3 CSP 会静默拦掉）', () => {
  for (const page of ['popup.html', 'options.html']) {
    const html = read(page);
    assert.doesNotMatch(
      html,
      /<script(?![^>]*\bsrc\s*=)[^>]*>[\s\S]*?<\/script>/i,
      `${page} 存在内联 <script>，MV3 CSP 下不会执行`,
    );
  }
});

test('popup 的三个分区与 options 的七个分区各自可达', () => {
  const popup = read('popup.html');
  for (const tab of ['status', 'data', 'settings']) {
    assert.match(popup, new RegExp(`data-tab="${tab}"`), `popup 缺少分区按钮：${tab}`);
    assert.match(popup, new RegExp(`id="panel-${tab}"`), `popup 缺少分区面板：${tab}`);
  }

  const optionsHtml = read('options.html');
  const optionsJs = read('options.js');
  const sections = [...optionsHtml.matchAll(/data-section="([a-z]+)"/g)].map((m) => m[1]);
  const unique = [...new Set(sections)];
  assert.ok(unique.length >= 7, `options 分区不足（${unique.join(', ')}）`);
  // 每个分区都必须在 SECTIONS 里有标题：否则切过去标题与副标题会是 undefined。
  for (const name of unique) {
    assert.match(optionsJs, new RegExp(`\\b${name}:\\s*\\{`), `options.js 的 SECTIONS 缺少 ${name}`);
  }
});

test('两个入口共用同一批设置键，且都监听 storage 变更', () => {
  const popup = read('popup.js');
  const options = read('options.js');
  const common = read('ui/common.js');
  for (const key of ['themeMode', 'popupWidthMode', 'uiDensity', 'showBadge', 'clipboardTtlSec',
    'persistTtlSec']) {
    assert.match(common, new RegExp(`\\b${key}:`), `ui/common.js 的默认设置缺少 ${key}`);
    assert.match(popup + options, new RegExp(key), `popup/options 没有使用设置键 ${key}`);
  }
  assert.match(popup, /onSettingsChanged/, 'popup 必须跟随 storage 变更（否则另一个入口改了设置不生效）');
  assert.match(options, /onSettingsChanged/, 'options 必须跟随 storage 变更');
});

test('调试注入只保留 persistTtlSec，其余项是 CLI 只读提示', () => {
  const common = read('ui/common.js');
  assert.match(common, /persistTtlSec:\s*7200/, 'persistTtlSec 默认应为 7200 秒');
  assert.doesNotMatch(common, /\bwarnHiddenTab:/, 'warnHiddenTab 没有运行时消费方，不应保留为设置键');
  assert.doesNotMatch(common, /\bconsoleHistorySettleMs:/, 'consoleHistorySettleMs 没有运行时消费方，不应保留为设置键');

  const popup = read('popup.html');
  const options = read('options.html');
  assert.match(popup, /id="persistTtlSec"/, 'popup.html 缺少 persistTtlSec 控件');
  assert.match(options, /id="persistTtlSelect"/, 'options.html 缺少 persistTtlSec 控件');
  assert.doesNotMatch(popup + options, /id="warnHiddenTab|id="warnHiddenTabToggle|id="consoleHistorySettleMs|id="consoleSettleSelect/,
    '已删除的死键不应留下控件');

  for (const [page, html] of [['popup.html', popup], ['options.html', options]]) {
    assert.match(html, /ATB_HIDDEN_TAB_WARNING=off/, `${page} 缺少后台标签页的 CLI 提示`);
    assert.match(html, /atb console history --settle-ms 300/, `${page} 缺少 console history 的 CLI 提示`);
    assert.match(html, /ATB_CONSOLE_HISTORY_SETTLE_MS/, `${page} 缺少 console history 的环境变量提示`);
  }
});

test('management 只在 optional_permissions 里，授权由用户点击触发', () => {
  assert.ok(Array.isArray(manifest.optional_permissions), '缺少 optional_permissions');
  assert.ok(manifest.optional_permissions.includes('management'), 'management 应是可选权限');
  assert.ok(!(manifest.permissions || []).includes('management'), 'management 不能放进必需权限');
  const html = read('options.html');
  assert.match(html, /id="btnGrantManagement"/, '管理页缺少授权按钮');
  assert.match(html, /management 权限只用于列出已安装扩展/, '管理页缺少权限边界说明');
  const js = read('options.js');
  assert.match(js, /chrome\.permissions\.contains/, '需要读取当前授权状态');
  assert.match(js, /chrome\.permissions\.request\(\{\s*permissions:\s*\['management'\]/, '授权必须走 permissions.request');
  assert.doesNotMatch(js, /chrome\.management\.getAll/, '扩展页面不应直接枚举扩展');
});

test('popup 的「重新连接」走后台 reconnect 命令', () => {
  assert.match(read('popup.js'), /cmd:\s*'reconnect'/, 'popup 没有调用 reconnect');
  const background = read('background.js');
  assert.match(background, /msg\.cmd === 'reconnect'/, '后台没有处理 reconnect 命令');
  assert.match(background, /function handleReconnect\(/, '后台缺少 handleReconnect');
});

test('popup 页面模式由 ?page=1 显式触发，不再靠 getCurrent 猜上下文', () => {
  const js = read('popup.js');
  // 只检查代码，不检查注释：注释里会记录这段踩坑历史。
  const code = js.replace(/^\s*\/\/.*$/gm, '');
  // 血泪教训：chrome.tabs.getCurrent 在部分版本/场景会返回 tab，把工具栏弹窗误判成
  // 页面模式，宽度反而被压窄。页面模式必须由调用方显式打开。
  assert.doesNotMatch(code, /chrome\.tabs\.getCurrent/, '不要再用 getCurrent 判断上下文');
  assert.match(js, /URLSearchParams\(location\.search\)/, '页面模式应由 ?page=1 触发');
  assert.match(js, /dataset\.mode\s*=/, '需要给 html/body 打 data-mode');
  const css = read('popup.css');
  assert.match(css, /html\[data-mode="page"\]/, '缺少页面模式的外层背景');
  assert.match(css, /body\[data-mode="page"\]/, '缺少页面模式的布局覆盖');
  assert.match(css, /max-width:\s*780px/, '页面模式应有 780px 内容列');
  assert.match(read('options.js'), /popup\.html\?page=1/, '管理页预览应带 page=1');
});

test('弹窗与管理页都能显示浏览器型号与版本', () => {
  assert.match(read('ui/common.js'), /detectBrowser/, '公共层缺少浏览器识别');
  assert.match(read('popup.html'), /id="browserInfoText"/, 'popup 缺少浏览器行');
  assert.match(read('popup.js'), /detectBrowser\(\)/, 'popup 没有调用浏览器识别');
  assert.match(read('options.html'), /id="ovBrowser"/, '管理页概览缺少浏览器行');
  assert.match(read('options.html'), /id="aboutBrowser"/, '管理页关于缺少浏览器行');
  assert.match(read('options.js'), /detectBrowser\(\)/, '管理页没有调用浏览器识别');
});

test('扩展自检读整包指纹，改界面文件也会触发重载', () => {
  const bg = read('background.js');
  // 只哈希 background.js 时，改 popup/options/css 永远不会重载 —— 用户看到的就是
  // 「改了没生效」。部署脚本写 .atb-build.json，自检优先读它的 digest。
  assert.match(bg, /\.atb-build\.json/, '自检需要读部署指纹');
  assert.match(bg, /stamp\.digest/, '指纹里取 digest 作为版本标识');
  const deploy = readFileSync(new URL('../scripts/deploy-extension.mjs', import.meta.url), 'utf8');
  assert.match(deploy, /BUILD_STAMP/, '部署脚本需要写指纹文件');
  assert.match(deploy, /writeBuildStamp\(target/, '部署成功后要落指纹');
});

test('管理页面收成 1240 居中面板，正文列 960', () => {
  const css = read('options.css');
  const block = (selector) => {
    const start = css.indexOf(selector);
    assert.ok(start >= 0, '缺少规则 ' + selector);
    return css.slice(start, css.indexOf('}', start));
  };
  assert.ok(block('.shell {').includes('max-width: 1240px'), '管理页外壳应限宽 1240');
  assert.ok(block('.content {').includes('max-width: 960px'), '管理页正文列应限宽 960');
});

test('浏览器标识默认跨重载稳定，管理页可显式重置', () => {
  const bg = read('background.js');
  // 标识必须持久化：session_key = browser_id:profile_id:tab，一重载就换会导致寻址全部失联。
  assert.match(bg, /data\.browserId \|\| `browser-\$\{crypto\.randomUUID\(\)\}`/, 'browserId 应持久化后复用');
  assert.match(bg, /cmd === 'resetIdentity'/, '缺少显式重置命令');
  assert.match(bg, /remove\(\['browserId', 'profileId'\]\)/, '重置要清掉两个标识');
  assert.match(read('options.html'), /id="btnResetIdentity"/, '管理页缺少重置按钮');
  assert.match(read('options.html'), /id="aboutBuild"/, '关于页缺少磁盘构建行');
});
