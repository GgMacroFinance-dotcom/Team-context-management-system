// 在浏览器里把审核页点一遍：登录、看提议、接受、改了再接受、拒绝、待复核、浏览、改单条。
// 对着 scripts/review-demo.sh 搭的演示数据点（会改数据，再点一遍前重新跑那个脚本）。要装 Playwright 和 Chromium：
//
//   node scripts/review-walk.js http://127.0.0.1:7351 tcms-demo/reviewer.txt tcms-demo/agent.txt [截图目录]
const { chromium } = require('playwright');
const fs = require('fs');
const [base, tokenFile, agentFile, shots] = process.argv.slice(2);
const reviewer = fs.readFileSync(tokenFile, 'utf8').trim();
const agent = fs.readFileSync(agentFile, 'utf8').trim();
const log = (...a) => console.log(...a);
function check(cond, msg) { if (!cond) { throw new Error('没通过：' + msg); } log('✓ ' + msg); }

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
  const errors = [];
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('pageerror', (e) => errors.push(String(e)));
  // 每张截图前查一下：页面上不该出现 null、undefined 这样的字。
  const shot = async (name) => {
    const text = await page.evaluate(() => document.body.innerText);
    if (/\bnull\b|\bundefined\b|NaN/.test(text)) errors.push(`${name} 页面上出现了 null/undefined/NaN`);
    if (shots) await page.screenshot({ path: `${shots}/${name}.png`, fullPage: true });
  };

  await page.goto(`${base}/review`);
  await page.waitForSelector('#token');
  await shot('01-login');

  // agent 的令牌登录不了。
  await page.fill('#token', agent);
  await page.click('button[type=submit]');
  await page.waitForSelector('.note.err');
  check((await page.textContent('.note.err')).includes('不是审核的人的令牌'), 'agent 的令牌登录不了审核页');
  // 上面那次 401 浏览器会记一条“加载资源失败”，是预期的；从这里开始不该再有报错。
  errors.length = 0;

  await page.fill('#token', reviewer);
  await page.click('button[type=submit]');
  await page.waitForSelector('.list li');
  check((await page.textContent('#who-name')) === '王审核', '用审核令牌登录，右上角显示名字');
  const rows = await page.$$eval('.list .row .title', (els) => els.map((e) => e.textContent));
  check(rows.length === 4, `待审核的提议列出 4 条：${rows.join('，')}`);
  await shot('02-proposals');

  // 修改的提议：差异。
  await page.click('text=宏观数据从哪里取');
  await page.waitForSelector('.diff');
  const removed = await page.$$eval('.diff .removed', (els) => els.map((e) => e.textContent));
  const added = await page.$$eval('.diff .added', (els) => els.map((e) => e.textContent));
  check(removed.includes('CPI 用国家统计局的数。') && added.includes('CPI、PPI 都用国家统计局的数，周报里注明。'), '修改的提议显示差异（删掉的、加上的）');
  await shot('03-update-diff');
  await page.click('#accept');
  await page.waitForSelector('.note.ok');
  check((await page.textContent('.note.ok')).includes('已接受'), '点“接受”，提议变成已接受');
  check((await page.textContent('.badges')).includes('已接受'), '页面上的状态跟着变');
  await shot('04-accepted');

  // 新建的提议，有相近的条目：对照着看，改了再接受。
  await page.click('text=← 提议列表');
  await page.waitForSelector('.list li');
  await page.click('text=宏观数据该从哪里取');
  await page.waitForSelector('details.conflict');
  await page.click('details.conflict summary');
  const compare = await page.textContent('details.conflict');
  check(compare.includes('CPI、PPI 都用国家统计局的数') && compare.includes('Finance Macro Data API'), '相近的条目展开后，提议和现在的正文并排对照');
  check(!(await page.isVisible('#edit-form')) && !(await page.isVisible('#reject-form')), '“改了再接受”和“拒绝”的表单默认收起');
  await shot('05-conflict');
  await page.click('#edit');
  check(await page.isVisible('#edit-form') && !(await page.isVisible('#reject-form')), '点“改了再接受”只展开它自己的表单');
  await page.fill('#edit-title', '宏观数据统一从 Finance Macro Data API 取');
  await page.fill('#edit-body', '统一走 Finance Macro Data API；统计局、FRED 的数都在里面。');
  await page.fill('#edit-file', 'role-macro/接口.md');
  await page.fill('#accept-note', '标题写清楚');
  await shot('06-edit-before-accept');
  await page.click('#edit-form button[type=submit]');
  await page.waitForSelector('.note.ok');
  check((await page.textContent('.note.ok')).includes('role-macro/接口.md'), '改了标题、正文、文件再接受，写进指定的文件');
  check((await page.textContent('h1')).includes('宏观数据该从哪里取'), '提议页保留提议原来的标题');

  // 拒绝。
  await page.click('text=← 提议列表');
  await page.waitForSelector('.list li');
  await page.click('text=周报用表格');
  await page.waitForSelector('#reject');
  await page.click('#reject');
  await page.fill('#reject-note', '周报还在用表格');
  await page.click('#reject-form button[type=submit]');
  await page.waitForSelector('.note.ok');
  check((await page.textContent('.badges')).includes('已拒绝'), '填了理由拒绝，提议变成已拒绝');
  await shot('07-rejected');

  await page.click('text=← 提议列表');
  await page.waitForSelector('.list li');
  const left = await page.$$eval('.list .row .title', (els) => els.map((e) => e.textContent));
  check(left.length === 1 && left[0] === 'CPI 发布日', '待审核的只剩 1 条');
  await page.selectOption('#status', 'all');
  await page.waitForFunction(() => document.querySelectorAll('.list li').length === 4);
  check(true, '切到“全部”能看到处理过的');

  // 待复核。
  await page.click('nav a[data-tab=due]');
  await page.waitForSelector('#within');
  await page.waitForSelector('.list li, .empty');
  const due = await page.$$eval('.list .row .title', (els) => els.map((e) => e.textContent));
  check(due.length === 2 && due[0] === 'PMI 的口径', `待复核列出过期的 2 条：${due.join('，')}`);
  await shot('08-due');
  await page.click('text=PMI 的口径');
  await page.waitForSelector('#item-form');
  await page.fill('#item-review', '2027-05-01');
  await page.fill('#item-body', 'PMI 指官方制造业 PMI；非制造业的要写明。');
  await shot('09-item-edit');
  await page.click('#item-form button[type=submit]');
  await page.waitForSelector('.note.ok');
  check((await page.textContent('.note.ok')).includes('第 2 版'), '改复核日期和正文保存，成了第 2 版');
  await page.click('nav a[data-tab=due]');
  await page.waitForFunction(() => document.querySelectorAll('.list li').length === 1);
  check(true, '复核过的那条不在待复核里了');

  // 浏览：按范围过滤、检索。（服务收到通知后换索引，等一下。）
  await page.waitForTimeout(1500);
  await page.click('nav a[data-tab=items]');
  await page.waitForSelector('#scope');
  await page.selectOption('#scope', 'role:macro');
  await page.waitForFunction(() => [...document.querySelectorAll('.list .row .title')].every((e) => e.closest('.row').textContent.includes('role:macro')));
  const macro = await page.$$eval('.list .row .title', (els) => els.map((e) => e.textContent));
  check(macro.includes('宏观数据统一从 Finance Macro Data API 取'), `按范围 role:macro 浏览，能看到刚接受的：${macro.join('，')}`);
  await page.selectOption('#scope', '');
  await page.fill('#q', '非制造业');
  await page.click('form.toolbar button[type=submit]');
  await page.waitForFunction(() => document.querySelectorAll('.list li').length === 1);
  check((await page.textContent('.list .row .title')) === 'PMI 的口径', '全文检索“非制造业”找到刚改过的那条');
  await shot('10-items');

  // 窄屏。
  await page.setViewportSize({ width: 390, height: 800 });
  await page.goto(`${base}/review#/proposals`);
  await page.waitForSelector('.list li, .empty');
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
  check(!overflow, '手机宽度下没有横向滚动');
  await shot('11-mobile');

  check(errors.length === 0, `浏览器控制台没有报错${errors.length ? '：' + errors.join(' | ') : ''}`);
  await browser.close();
})().catch((e) => { console.error(e.message); process.exit(1); });
