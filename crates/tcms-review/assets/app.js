// 审核页。不用框架，也不拼 HTML 字符串：页面上的文字一律走 textContent，提议里写什么都只是文字。
"use strict";
(() => {
  const KEY = "tcms-review-token";
  const main = document.getElementById("main");
  let me = null;
  let proposalStatus = "open";
  let dueWithin = 0;
  const itemFilter = { scope: "", kind: "", q: "" };

  const ACTION = { create: "新建", update: "修改", supersede: "取代", archive: "归档" };
  const STATUS = { open: "待审核", accepted: "已接受", rejected: "已拒绝" };
  const KIND = {
    rule: "规则", decision: "决定", fact: "事实", term: "术语",
    procedure: "流程", reference: "资料", memory: "记忆",
  };
  const ITEM_STATUS = { active: "生效", superseded: "被取代", archived: "已归档" };

  function token() {
    try { return sessionStorage.getItem(KEY); } catch { return null; }
  }
  function setToken(t) {
    try {
      if (t) sessionStorage.setItem(KEY, t); else sessionStorage.removeItem(KEY);
    } catch { /* 存不了就每次重新登录 */ }
  }

  // 建一个元素。attrs 里 on 开头的是事件；kids 可以是字符串、元素、数组或空。
  function h(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else if (k === "class") e.className = v;
      else if (k === "value") e.value = v;
      else if (k === "checked") e.checked = !!v;
      else e.setAttribute(k, v === true ? "" : String(v));
    }
    const add = (c) => {
      if (c === null || c === undefined || c === false) return;
      if (Array.isArray(c)) c.forEach(add);
      else e.append(c instanceof Node ? c : document.createTextNode(String(c)));
    };
    kids.forEach(add);
    return e;
  }

  class ApiFail extends Error {
    constructor(status, body) {
      super((body && body.error) || `请求失败（${status}）`);
      this.status = status;
      this.body = body || {};
    }
  }

  async function api(method, path, body) {
    const headers = { Authorization: `Bearer ${token() || ""}` };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    let r;
    try {
      r = await fetch(path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
    } catch {
      throw new ApiFail(0, { error: "连不上服务" });
    }
    let data = null;
    try { data = await r.json(); } catch { /* 不是 JSON */ }
    if (r.status === 401) {
      setToken(null);
      me = null;
      showLogin((data && data.error) || "要重新登录");
      throw new ApiFail(401, data);
    }
    // 批准、改单条会和远端打交道，做完（成不成都算）更新页面顶上那一行。
    if (method === "POST") setTimeout(refreshRemote, 0);
    if (!r.ok) throw new ApiFail(r.status, data);
    return data;
  }

  const enc = encodeURIComponent;
  const render = (...nodes) => main.replaceChildren(...nodes.filter((n) => n));
  const failNote = (e) => h("div", { class: "note err", role: "alert" }, e.message);
  const kindWord = (k) => KIND[k] || k;
  const badge = (text, cls) => h("span", { class: `badge ${cls || ""}` }, text);
  const short = (c) => (c ? c.slice(0, 12) : "");
  // 配了远端时，批准、保存以后说一句推没推上去；先拉下来的新条目补了 id 时也说一句。
  const remoteNote = (r) => {
    if (!r) return "";
    const ids = r.ids ? `；远端新来的 ${r.ids.items.length} 条补了 id` : "";
    if (r.errors && r.errors.length) return `${ids}；没推到远端（${r.errors.join("；")}），提交在服务器上，下次同步时再推`;
    return ids + (r.pushed_new ? "；已推到远端" : "");
  };
  const fields = (pairs) =>
    h("dl", { class: "fields" }, pairs.filter((p) => p && p[1] !== null && p[1] !== undefined && p[1] !== "")
      .map(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));

  function daysBetween(from, to) {
    const a = Date.parse(`${from}T00:00:00Z`);
    const b = Date.parse(`${to}T00:00:00Z`);
    return Math.round((b - a) / 86400000);
  }

  // ---------- 远端 ----------

  // 页面顶上一行：配了远端时，最近一次同步的结果；合并不了时写明哪些文件、怎么合并。没配远端时不显示。
  const remoteBar = document.getElementById("remote");
  const WHAT = { sync: "同步", accept: "批准时同步", edit: "改单条时同步" };

  function ago(ms) {
    const s = Math.max(0, Math.round(ms / 1000));
    if (s < 60) return `${s} 秒前`;
    const m = Math.round(s / 60);
    if (m < 60) return `${m} 分钟前`;
    const hr = Math.round(m / 60);
    if (hr < 48) return `${hr} 小时前`;
    return `${Math.round(hr / 24)} 天前`;
  }

  async function refreshRemote() {
    if (!me) return;
    let s;
    try {
      s = await api("GET", "/api/remote");
    } catch {
      return;
    }
    if (!me || !s.remote) {
      remoteBar.hidden = true;
      return;
    }
    const when = (ms) => ago(s.now_ms - ms);
    const parts = [`远端 ${s.remote}`, s.sync_secs ? `每 ${s.sync_secs} 秒同步一次` : "只在批准、改单条时同步"];
    let cls = "";
    let detail = null;
    const last = s.last;
    if (s.needs_merge) {
      cls = "err";
      parts.push("合并不了，批准和改单条会停下来，合并好以后下次同步就恢复");
      detail = s.needs_merge;
    } else if (last && last.report.errors.length) {
      cls = "warn";
      parts.push(`${WHAT[last.what] || "同步"}没成（${when(last.at_ms)}）`);
      parts.push(s.last_ok_ms ? `上次正常是 ${when(s.last_ok_ms)}` : "启动以后还没成功过");
      detail = `${last.report.errors.join("；")}。提交在服务器上，下次同步时再推。`;
    } else if (last) {
      parts.push(`上次${WHAT[last.what] || "同步"} ${when(last.at_ms)}，正常`);
      if (last.report.ids) parts.push(`远端新来的 ${last.report.ids.items.length} 条补了 id`);
    } else {
      parts.push("启动以后还没同步过");
    }
    remoteBar.className = `remote-bar ${cls}`;
    remoteBar.replaceChildren(h("div", {}, parts.join(" · ")));
    if (detail) remoteBar.append(h("div", { class: "detail" }, detail));
    remoteBar.hidden = false;
  }
  setInterval(refreshRemote, 30000);

  // ---------- 登录 ----------

  function showLogin(msg) {
    document.getElementById("tabs").hidden = true;
    document.getElementById("who").hidden = true;
    remoteBar.hidden = true;
    const input = h("input", { type: "password", id: "token", autocomplete: "off", spellcheck: "false", placeholder: "tcms_…" });
    const form = h("form", {
      class: "stack",
      onsubmit: (ev) => {
        ev.preventDefault();
        const t = input.value.trim();
        if (!t) return;
        setToken(t);
        me = null;
        route();
      },
    },
    h("label", {}, "审核的人的令牌", input),
    h("div", { class: "actions" }, h("button", { type: "submit", class: "primary" }, "登录")));
    render(h("div", { class: "login panel" },
      h("h1", {}, "登录"),
      h("p", { class: "muted small" },
        "用管理员给你的审核令牌登录（tcms token add --reviewer 发的）。令牌只存在这个标签页里，关掉就要重新登录。agent 的令牌在这里不能用。"),
      msg ? h("div", { class: "note err", role: "alert" }, msg) : null,
      form));
    input.focus();
  }

  document.getElementById("logout").addEventListener("click", () => {
    setToken(null);
    me = null;
    showLogin();
  });

  function setTab(tab) {
    document.querySelectorAll("#tabs a").forEach((a) => a.classList.toggle("on", a.dataset.tab === tab));
  }

  async function route() {
    if (!token()) return showLogin();
    if (!me) {
      try {
        me = await api("GET", "/api/me");
      } catch (e) {
        if (e.status !== 401) render(failNote(e));
        return;
      }
      document.getElementById("tabs").hidden = false;
      document.getElementById("who").hidden = false;
      document.getElementById("who-name").textContent = me.name;
      refreshRemote();
    }
    const parts = location.hash.replace(/^#\/?/, "").split("/").filter(Boolean).map(decodeURIComponent);
    const tab = parts[0] || "proposals";
    setTab(tab);
    render(h("p", { class: "muted" }, "加载中…"));
    try {
      if (tab === "due") await showDue();
      else if (tab === "items") await (parts[1] ? showItem(parts[1]) : showItems());
      else if (tab === "proposals") await (parts[1] ? showProposal(parts[1]) : showProposals());
      else location.hash = "#/proposals";
    } catch (e) {
      if (e.status !== 401) render(failNote(e));
    }
  }
  window.addEventListener("hashchange", route);

  // ---------- 提议 ----------

  async function showProposals() {
    const data = await api("GET", `/api/proposals?status=${enc(proposalStatus)}`);
    const select = h("select", {
      id: "status",
      onchange: () => { proposalStatus = select.value; route(); },
    }, ["open", "accepted", "rejected", "all"].map((s) =>
      h("option", { value: s, selected: s === proposalStatus }, s === "all" ? "全部" : STATUS[s])));
    const rows = data.proposals.map((p) => h("li", {},
      h("a", { class: "row", href: `#/proposals/${enc(p.id)}` },
        h("div", { class: "title" }, p.title),
        h("div", { class: "meta" },
          badge(STATUS[p.status] || p.status, p.status),
          h("span", {}, `${ACTION[p.action] || p.action}${p.item_id ? " " + p.item_id : ""}`),
          h("span", {}, `${p.scope} · ${kindWord(p.kind)}`),
          h("span", {}, `${p.proposer_name} 提于 ${p.created_at} UTC`),
          p.conflicts ? h("span", {}, `相近的条目 ${p.conflicts} 条`) : null,
          p.redacted ? h("span", {}, `脱敏 ${p.redacted} 处`) : null))));
    render(
      h("h1", {}, "提议"),
      h("div", { class: "toolbar" }, h("label", {}, "看哪些", select)),
      h("div", { class: "panel" }, rows.length
        ? h("ul", { class: "list" }, rows)
        : h("div", { class: "empty" }, proposalStatus === "open" ? "没有待审核的提议。" : "没有提议。")));
  }

  function diffBlock(lines) {
    if (!lines.length) return h("p", { class: "muted" }, "没有差异。");
    return h("div", { class: "diff", "aria-label": "差异" },
      lines.map((l) => h("div", { class: l.op }, l.line)));
  }

  // flash：刚做完的操作的结果，显示在标题下面（处理完以后“处理”那一块就不在了）。
  async function showProposal(id, flash) {
    const d = await api("GET", `/api/proposals/${enc(id)}`);
    const p = d.proposal;
    const cur = d.current;
    const open = p.status === "open";
    const gone = p.item_id && (!cur || cur.status !== "active");
    const status = h("div", { id: "action-status" });

    const parts = [
      h("div", { class: "crumbs" }, h("a", { href: "#/proposals" }, "← 提议列表")),
      h("h1", {}, p.title),
      h("div", { class: "badges" },
        badge(STATUS[p.status] || p.status, p.status),
        badge(ACTION[p.action] || p.action),
        badge(p.scope),
        badge(kindWord(p.kind))),
      flash ? h("div", { class: "note ok", role: "status" }, flash) : null,
      h("div", { class: "panel" }, fields([
        ["提议人", `${p.proposer_name}（${p.proposer_token}，岗位 ${p.proposer_role}）`],
        ["提的时间", `${p.created_at} UTC`],
        ["目标", p.item_id ? h("a", { href: `#/items/${enc(p.item_id)}` }, p.item_id) : null],
        ["理由", p.reason],
        ["证据", p.evidence.length ? h("ul", {}, p.evidence.map((e) => h("li", {}, e))) : null],
        ["脱敏", p.redacted ? `抹掉了 ${p.redacted} 处像密钥的内容` : null],
        ["处理", open ? null : `${STATUS[p.status]}：${p.decided_by || "-"}，${p.decided_at || "-"} UTC${p.decision_note ? "，备注：" + p.decision_note : ""}`],
        ["写成", p.result_item ? h("a", { href: `#/items/${enc(p.result_item)}` }, p.result_item) : null],
        ["提交", p.result_commit ? h("code", {}, p.result_commit) : null],
      ])),
    ];
    if (open && gone) {
      parts.push(h("div", { class: "note err" },
        cur ? `目标条目现在是“${ITEM_STATUS[cur.status] || cur.status}”，这条提议只能拒绝。` : "目标条目已经不在了，这条提议只能拒绝。"));
    } else if (open && d.stale) {
      parts.push(h("div", { class: "note warn" },
        `这一条在提议之后被人改过：提议时是第 ${p.base_revision} 版，现在是第 ${cur.revision} 版。下面的差异是和现在的比，接受会写成提议的样子；要保留别人的改动，用“改了再接受”给出合并后的正文。`));
    }
    const diffTitle = !open ? "提议的内容（和现在的比）"
      : p.action === "create" ? "批准以后新加这一条"
        : p.action === "archive" ? "批准以后归档这一条" : "批准以后（和现在的比）";
    parts.push(h("h2", {}, diffTitle));
    parts.push(diffBlock(d.diff));

    if (d.conflicts.length) {
      parts.push(h("h2", {}, "相近的条目（可能重复或矛盾）"));
      for (const c of d.conflicts) {
        const now = c.current;
        parts.push(h("details", { class: "conflict" },
          h("summary", {}, `${c.title} · ${c.id} 第 ${c.revision} 版 · 相近 ${c.similarity}%`),
          h("div", { class: "compare" },
            h("div", {}, h("h3", {}, "这条提议"), h("pre", { class: "body" }, p.body || "（归档，没有正文）")),
            h("div", {},
              h("h3", {}, now ? `现在的“${now.title}”（第 ${now.revision} 版，${ITEM_STATUS[now.status] || now.status}）` : "这一条已经不在了"),
              now ? h("pre", { class: "body" }, now.body) : null,
              h("a", { class: "small", href: `#/items/${enc(c.id)}` }, "打开这一条")))));
      }
    }

    if (open) parts.push(h("h2", {}, "处理"), decide(d, gone, status), status);
    render(...parts);
  }

  // 接受、改了再接受、拒绝。
  function decide(d, gone, status) {
    const p = d.proposal;
    const cur = d.current;
    const over = d.stale && !gone
      ? h("input", { type: "checkbox", id: "over" })
      : null;
    const overValue = () => (over && over.checked ? cur.revision : undefined);
    const busy = (on) => box.querySelectorAll("button").forEach((b) => { b.disabled = on; });

    async function send(path, body, okText) {
      if (d.stale && !gone && path.endsWith("/accept") && !over.checked) {
        status.replaceChildren(h("div", { class: "note err", role: "alert" }, `先勾上“我看过现在的第 ${cur.revision} 版”。`));
        return;
      }
      busy(true);
      status.replaceChildren(h("p", { class: "muted" }, "正在处理…"));
      try {
        const r = await api("POST", path, body);
        const where = r.path ? `，写进 ${r.path}，提交 ${short(r.commit)}` : "";
        await showProposal(p.id, `${okText}${where}${r.already_applied ? "（之前已经写进去了，这次只记下接受）" : ""}${remoteNote(r.remote)}。`);
      } catch (e) {
        busy(false);
        if (e.status === 401) return;
        let msg = e.message;
        if (e.body.code === "stale") msg += "（刷新一下页面看现在的内容）";
        status.replaceChildren(h("div", { class: "note err", role: "alert" }, msg));
      }
    }

    // 改了再接受。
    const title = h("input", { type: "text", id: "edit-title", value: p.title });
    const text = p.action === "archive" ? null : h("textarea", { id: "edit-body", value: p.body });
    const file = d.default_file !== null
      ? h("input", { type: "text", id: "edit-file", placeholder: d.default_file })
      : null;
    const note = h("input", { type: "text", id: "accept-note", placeholder: "可以不写" });
    const editForm = h("form", {
      class: "stack", hidden: true, id: "edit-form",
      onsubmit: (ev) => {
        ev.preventDefault();
        const body = { note: note.value, over: overValue() };
        if (title.value !== p.title) body.title = title.value;
        if (text && text.value !== p.body) body.body = text.value;
        if (file && file.value.trim()) body.file = file.value.trim();
        send(`/api/proposals/${enc(p.id)}/accept`, body, "已改好并接受");
      },
    },
    h("label", {}, "标题", title),
    text ? h("label", {}, "正文（Markdown，不能有一级、二级标题）", text) : null,
    file ? h("label", {}, `写进哪个文件（不写时是 ${d.default_file}）`, file) : null,
    h("label", {}, "备注", note),
    h("div", { class: "actions" }, h("button", { type: "submit", class: "primary" }, "保存并接受")));

    // 拒绝。
    const why = h("input", { type: "text", id: "reject-note", placeholder: "比如：重复了、说法不对" });
    const rejectForm = h("form", {
      class: "stack", hidden: true, id: "reject-form",
      onsubmit: (ev) => {
        ev.preventDefault();
        send(`/api/proposals/${enc(p.id)}/reject`, { note: why.value }, "已拒绝");
      },
    },
    h("label", {}, "为什么拒绝（记进审计）", why),
    h("div", { class: "actions" }, h("button", { type: "submit", class: "danger" }, "拒绝这条提议")));

    const toggle = (show, hide) => { show.hidden = !show.hidden; hide.hidden = true; };
    const box = h("div", { class: "panel" },
      over ? h("label", { class: "inline" }, over, ` 我看过现在的第 ${cur.revision} 版，照样接受`) : null,
      h("div", { class: "actions" },
        gone ? null : h("button", {
          type: "button", class: "primary", id: "accept",
          onclick: () => send(`/api/proposals/${enc(p.id)}/accept`, { over: overValue() }, "已接受"),
        }, "接受"),
        gone ? null : h("button", { type: "button", class: "secondary", id: "edit", onclick: () => toggle(editForm, rejectForm) }, "改了再接受"),
        h("button", { type: "button", class: "danger", id: "reject", onclick: () => toggle(rejectForm, editForm) }, "拒绝")),
      gone ? null : editForm,
      rejectForm);
    return box;
  }

  // ---------- 待复核 ----------

  async function showDue() {
    const data = await api("GET", `/api/due?within=${dueWithin}`);
    const select = h("select", {
      id: "within",
      onchange: () => { dueWithin = Number(select.value); route(); },
    }, [[0, "已经到期的"], [30, "30 天内到期的"], [90, "90 天内到期的"]].map(([v, t]) =>
      h("option", { value: v, selected: v === dueWithin }, t)));
    const rows = data.items.map((it) => {
      const late = daysBetween(it.review_by, data.today);
      return h("li", {}, h("a", { class: "row", href: `#/items/${enc(it.id)}` },
        h("div", { class: "title" }, it.title),
        h("div", { class: "meta" },
          badge(late > 0 ? `过期 ${late} 天` : late === 0 ? "今天到期" : `${-late} 天后到期`, late >= 0 ? "open" : ""),
          h("span", {}, `复核日期 ${it.review_by}`),
          h("span", {}, `${it.scope} · ${kindWord(it.kind)}`),
          it.owner ? h("span", {}, `负责人 ${it.owner}`) : null,
          h("span", { class: "mono" }, it.path))));
    });
    render(
      h("h1", {}, "待复核的条目"),
      h("p", { class: "muted small" }, `今天是 ${data.today}（UTC）。打开一条，看过以后改复核日期或内容并保存。`),
      h("div", { class: "toolbar" }, h("label", {}, "看哪些", select)),
      h("div", { class: "panel" }, rows.length
        ? h("ul", { class: "list" }, rows)
        : h("div", { class: "empty" }, "没有到期要复核的条目。")));
  }

  // ---------- 浏览 ----------

  async function showItems() {
    const qs = new URLSearchParams();
    for (const k of ["scope", "kind", "q"]) if (itemFilter[k]) qs.set(k, itemFilter[k]);
    const data = await api("GET", `/api/items?${qs}`);
    const scope = h("select", { id: "scope" },
      h("option", { value: "" }, "全部范围"),
      data.scopes.map((s) => h("option", { value: s.scope, selected: s.scope === itemFilter.scope }, `${s.scope}（${s.count}）`)));
    const kind = h("select", { id: "kind" },
      h("option", { value: "" }, "全部类型"),
      data.kinds.map((k) => h("option", { value: k.kind, selected: k.kind === itemFilter.kind }, `${kindWord(k.kind)}（${k.count}）`)));
    const q = h("input", { type: "search", id: "q", value: itemFilter.q, placeholder: "全文检索" });
    const apply = () => {
      itemFilter.scope = scope.value;
      itemFilter.kind = kind.value;
      itemFilter.q = q.value.trim();
      route();
    };
    scope.addEventListener("change", apply);
    kind.addEventListener("change", apply);
    const rows = data.items.map((it) => h("li", {},
      h("a", { class: "row", href: `#/items/${enc(it.id)}` },
        h("div", { class: "title" }, it.title),
        h("div", { class: "meta" },
          h("span", {}, `${it.scope} · ${kindWord(it.kind)}`),
          h("span", {}, `第 ${it.revision} 版`),
          it.review_by ? h("span", {}, `复核 ${it.review_by}`) : null,
          it.tags.length ? h("span", {}, `标签 ${it.tags.join("、")}`) : null,
          h("span", { class: "mono" }, it.path)))));
    render(
      h("h1", {}, "条目"),
      h("form", { class: "toolbar", onsubmit: (ev) => { ev.preventDefault(); apply(); } },
        scope, kind, q, h("button", { type: "submit", class: "secondary" }, "找")),
      h("p", { class: "muted small" },
        `生效的条目共 ${data.total} 条${data.total > data.limit ? `，只列前 ${data.limit} 条` : ""}${data.commit ? `；索引到提交 ${short(data.commit)}` : ""}。只读的地方直接改上下文仓库里的 Markdown 也行。`),
      h("div", { class: "panel" }, rows.length
        ? h("ul", { class: "list" }, rows)
        : h("div", { class: "empty" }, "没有符合的条目。")));
  }

  async function showItem(id, flash) {
    const d = await api("GET", `/api/items/${enc(id)}`);
    const it = d.item;
    const status = h("div", { id: "edit-status" });
    const parts = [
      h("div", { class: "crumbs" }, h("a", { href: "#/items" }, "← 条目列表"), " · ", h("a", { href: "#/due" }, "待复核")),
      h("h1", {}, it.title),
      h("div", { class: "badges" }, badge(ITEM_STATUS[it.status] || it.status, it.status === "active" ? "accepted" : "rejected"), badge(it.scope), badge(kindWord(it.kind))),
      flash ? h("div", { class: "note ok", role: "status" }, flash) : null,
      h("div", { class: "panel" }, fields([
        ["id", h("code", {}, it.id)],
        ["版本", `第 ${it.revision} 版`],
        ["文件", h("code", {}, it.path)],
        ["提交", h("code", {}, it.commit_id)],
        ["标签", it.tags.length ? it.tags.join("、") : null],
        ["负责人", it.owner],
        ["复核日期", it.review_by],
        ["取代了", it.supersedes ? h("a", { href: `#/items/${enc(it.supersedes)}` }, it.supersedes) : null],
        ["被取代", d.superseded_by.length ? d.superseded_by.map((s, i) => [i ? "、" : "", h("a", { href: `#/items/${enc(s)}` }, s)]) : null],
      ])),
      h("h2", {}, "正文"),
      h("pre", { class: "body" }, it.body || "（空）"),
    ];
    if (it.status === "active") parts.push(h("h2", {}, "改这一条"), editItem(it, status), status);
    render(...parts);
  }

  // 改单条的纯文本：标题、标签、复核日期、正文。类型、范围不在这里改。
  function editItem(it, status) {
    const title = h("input", { type: "text", id: "item-title", value: it.title });
    const tags = h("input", { type: "text", id: "item-tags", value: it.tags.join("、"), placeholder: "用顿号或逗号隔开" });
    const review = h("input", { type: "date", id: "item-review", value: it.review_by || "" });
    const body = h("textarea", { id: "item-body", value: it.body });
    const save = h("button", { type: "submit", class: "primary" }, "保存");
    const form = h("form", {
      class: "stack panel", id: "item-form",
      onsubmit: async (ev) => {
        ev.preventDefault();
        const change = { base_revision: it.revision };
        if (title.value !== it.title) change.title = title.value;
        if (body.value !== it.body) change.body = body.value;
        const newTags = tags.value.split(/[,，、]/).map((t) => t.trim()).filter(Boolean);
        if (newTags.join("\u0000") !== it.tags.join("\u0000")) change.tags = newTags;
        if (review.value !== (it.review_by || "")) change.review_by = review.value;
        if (Object.keys(change).length === 1) {
          status.replaceChildren(h("div", { class: "note warn", role: "alert" }, "没有改动。"));
          return;
        }
        save.disabled = true;
        status.replaceChildren(h("p", { class: "muted" }, "正在保存…"));
        try {
          const r = await api("POST", `/api/items/${enc(it.id)}/edit`, change);
          await showItem(it.id, `已保存：第 ${r.revision} 版，写进 ${r.path}，提交 ${short(r.commit)}${remoteNote(r.remote)}。`);
        } catch (e) {
          save.disabled = false;
          if (e.status === 401) return;
          status.replaceChildren(h("div", { class: "note err", role: "alert" },
            e.body.code === "stale" ? `${e.message}（刷新一下页面看现在的内容）` : e.message));
        }
      },
    },
    h("label", {}, "标题", title),
    h("label", {}, "标签", tags),
    h("label", {}, "复核日期（清空表示不用复核）", review),
    h("label", {}, "正文（Markdown，不能有一级、二级标题）", body),
    h("div", { class: "actions" }, save),
    h("p", { class: "muted small" }, "保存会直接做成上下文仓库里的一个提交，提交者是你。"));
    return form;
  }

  route();
})();
