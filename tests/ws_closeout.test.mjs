// WS / 页面脚本回归守卫
//
// 这两条是已判定、但需要持续守住的问题：
// * page_action.setValue 的内层 `base` 遮蔽外层结果对象 → fill 成功却丢 ok；
// * page_challenge 仅凭脚本标签判 interactive → challenge wait 必定耗满预算。
//
// 它们的共同点是"单点语义很难用 DOM stub 完整复现"，所以这里用源码级不变式守住
// 最容易回退的那一行；真正的行为验证放在 tests/page_script_closeout.test.mjs 与真机验收。
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ASSETS = join(HERE, '..', 'assets');
const source = (name) => readFileSync(join(ASSETS, name), 'utf8');

test('setValue 不得再用内层 base 遮蔽外层结果对象', () => {
  const text = source('page_action.js');
  assert.match(text, /const before = String\(el\.value/, 'setValue 的写入前值必须叫 before');
  assert.doesNotMatch(text, /const base = String\(el\.value/, '内层 const base 会遮蔽外层 base 对象');
  assert.match(text, /const want = params\.append \? before \+ value : value/);
  // 结果必须挂在外层对象上，否则 executeScript 序列化后只剩字符串、ok 丢失。
  assert.match(text, /return Object\.assign\(base, state, \{ ok: true \}\)/);
});

test('脚本标签只能进 signals，interactive 由可见挂载点决定', () => {
  const text = source('page_challenge.js');
  assert.match(text, /function visibleCaptchaMount\(selector\)/, '缺少可见挂载点判定');
  // 脚本标签不再直接 hit()（那样会把正常页面判成永远有待完成组件）。
  assert.doesNotMatch(text, /if \(has\('\.g-recaptcha'\) \|\| scriptHas\('recaptcha'\)\)/);
  assert.doesNotMatch(text, /if \(has\('\.h-captcha'\) \|\| scriptHas\('hcaptcha\.com'\)\)/);
  assert.match(text, /visibleCaptchaMount\('\.g-recaptcha'\)/);
  assert.match(text, /visibleCaptchaMount\('\.h-captcha'\)/);
  // 脚本线索仍要保留，排障时能看出"页面引过 SDK 但没有可见组件"。
  assert.match(text, /signals\.push\('recaptcha-script'\)/);
  assert.match(text, /signals\.push\('hcaptcha-script'\)/);
});
