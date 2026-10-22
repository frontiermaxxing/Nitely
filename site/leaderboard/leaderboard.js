// The public board, read from the Leaderboard service (server/leaderboard) through this site's
// own /v1/leaderboard/ path. The ranking choices and number formats follow the app's board
// (src/leaderboard/Board.tsx, stats.ts). Every value from the service is set as text, never as
// HTML, and a link is shown only when it is https.
(function () {
  "use strict";

  var METRICS = [
    { key: "landed", label: "Landed", score: function (e) { return e.landed; } },
    { key: "first_pass", label: "First-pass review", score: function (e) { return e.first_pass_rate; } },
    { key: "lead", label: "Lead time", low: true, score: function (e) { return e.lead_time ? e.lead_time.median : null; } },
    { key: "hours", label: "Agent hours", score: function (e) { return e.agent_hours; } },
    { key: "tokens", label: "Tokens used", score: function (e) { return e.tokens; } },
    { key: "reply", label: "Inbox reply", low: true, score: function (e) { return e.inbox_reply ? e.inbox_reply.median : null; } },
    { key: "steady", label: "Steadiest week", score: function (e) { return e.steadiest_week ? e.steadiest_week.active_days * 1000 + e.steadiest_week.landed : null; } }
  ];
  var VENDORS = { claude: ["Cl", "Claude"], codex: ["Cx", "Codex"], cursor: ["Cu", "Cursor"] };
  var SHOWN = 50;

  var state = { period: "week", metric: "landed", data: null, cache: {} };

  function duration(seconds) {
    var s = Math.max(0, Math.round(seconds));
    if (s < 60) return s + "s";
    if (s < 600) return Math.floor(s / 60) + "m" + (s % 60 ? " " + String(s % 60).padStart(2, "0") + "s" : "");
    if (s < 3600) return Math.round(s / 60) + "m";
    if (s < 86400) {
      var m = Math.round((s % 3600) / 60);
      return Math.floor(s / 3600) + "h" + (m ? " " + String(m).padStart(2, "0") + "m" : "");
    }
    var h = Math.round((s % 86400) / 3600);
    return Math.floor(s / 86400) + "d" + (h ? " " + h + "h" : "");
  }
  function compact(n) {
    if (n < 10000) return Math.round(n).toLocaleString("en-US");
    if (n < 1e6) return (n / 1000).toFixed(n < 1e5 ? 1 : 0) + "K";
    if (n < 1e9) return (n / 1e6).toFixed(n < 1e8 ? 1 : 0) + "M";
    return (n / 1e9).toFixed(1) + "B";
  }
  function hours(n) { return n < 100 ? (Math.round(n * 10) / 10).toLocaleString("en-US") : Math.round(n).toLocaleString("en-US"); }
  function percent(r) { return r === null || r === undefined ? "–" : Math.round(r * 100) + "%"; }
  function num(n) { return typeof n === "number" && isFinite(n) ? n : null; }

  function valueOf(e, key) {
    switch (key) {
      case "landed": return (num(e.landed) || 0).toLocaleString("en-US");
      case "first_pass": return percent(num(e.first_pass_rate));
      case "lead": return e.lead_time ? duration(e.lead_time.median) : "–";
      case "hours": return hours(num(e.agent_hours) || 0);
      case "tokens": return compact(num(e.tokens) || 0);
      case "reply": return e.inbox_reply ? duration(e.inbox_reply.median) : "–";
      case "steady": return e.steadiest_week ? e.steadiest_week.active_days + " days" : "–";
    }
    return "–";
  }

  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  }

  function crew(e) {
    var box = el("span", "crew");
    (e.vendors || []).slice(0, 4).forEach(function (v) {
      var known = VENDORS[v.vendor];
      var t = el("span", "tile " + (known ? v.vendor : "other"), known ? known[0] : "?");
      t.title = (known ? known[1] : String(v.vendor)) + ": " + Math.round((v.share || 0) * 100) + "% of landed";
      box.appendChild(t);
    });
    return box;
  }

  function links(e) {
    var l = e.links || {};
    var box = el("div", "lb-links");
    [["GitHub", l.github], ["X", l.x], ["Web", l.website]].forEach(function (pair) {
      var v = pair[1];
      if (typeof v !== "string" || v.indexOf("https://") !== 0) return;
      var a = el("a");
      a.href = v;
      a.rel = "noopener noreferrer nofollow";
      a.appendChild(el("span", "quiet", pair[0] + " "));
      a.appendChild(document.createTextNode(v.replace(/^https:\/\/(www\.)?(github\.com\/|x\.com\/)?/, "")));
      box.appendChild(a);
    });
    return box.childNodes.length ? box : null;
  }

  function bars(weekly) {
    var w = (weekly || []).slice(-12);
    var max = Math.max.apply(null, w.concat([1]));
    var box = el("div", "bars");
    box.setAttribute("role", "img");
    box.setAttribute("aria-label", "Landed per week, the last " + w.length + " weeks: " + w.join(", "));
    w.forEach(function (v) {
      var b = el("i");
      b.style.height = Math.max(2, Math.round((v / max) * 100)) + "%";
      box.appendChild(b);
    });
    return box;
  }

  function ranked() {
    var m = METRICS.filter(function (x) { return x.key === state.metric; })[0];
    var rows = (state.data.entries || []).slice();
    rows.sort(function (a, b) {
      var x = m.score(a), y = m.score(b);
      if (x === null && y === null) return (b.landed || 0) - (a.landed || 0);
      if (x === null) return 1;
      if (y === null) return -1;
      return m.low ? x - y : y - x;
    });
    return { m: m, rows: rows };
  }

  function render() {
    var status = document.getElementById("lb-status");
    var podium = document.getElementById("lb-podium");
    var wrap = document.getElementById("lb-table-wrap");
    var empty = document.getElementById("lb-empty");
    var tbody = document.getElementById("lb-rows");
    var entries = (state.data && state.data.entries) || [];

    podium.textContent = "";
    tbody.textContent = "";
    if (!entries.length) {
      status.hidden = true;
      podium.hidden = wrap.hidden = true;
      empty.hidden = false;
      return;
    }
    empty.hidden = true;
    var r = ranked();
    status.hidden = false;
    status.textContent = entries.length.toLocaleString("en-US") + (entries.length === 1 ? " person" : " people") +
      (state.data.week ? " · " + state.data.week : "") + " · updates weekly";

    r.rows.slice(0, 3).forEach(function (e, i) {
      var card = el("section", "lb-card");
      var head = el("div", "lb-card-head");
      head.appendChild(el("span", "rank" + (i === 0 ? " first" : ""), String(i + 1)));
      head.appendChild(el("span", "name", e.display_name));
      head.appendChild(crew(e));
      card.appendChild(head);
      var fig = el("div", "lb-figure");
      fig.appendChild(el("span", "big", valueOf(e, r.m.key)));
      fig.appendChild(el("span", "unit", r.m.label.toLowerCase()));
      card.appendChild(fig);
      card.appendChild(bars(e.weekly));
      var meta = el("div", "lb-meta");
      meta.appendChild(el("span", "", hours(num(e.agent_hours) || 0) + " agent hours"));
      meta.appendChild(el("span", "", compact(num(e.tokens) || 0) + " tokens"));
      meta.appendChild(el("span", "", percent(num(e.first_pass_rate)) + " first pass"));
      card.appendChild(meta);
      var ln = links(e);
      if (ln) card.appendChild(ln);
      podium.appendChild(card);
    });
    podium.hidden = false;

    document.querySelectorAll(".lb-table th[data-col]").forEach(function (th) {
      var on = th.getAttribute("data-col") === r.m.key;
      th.classList.toggle("sorted", on);
      if (on) th.setAttribute("aria-sort", r.m.low ? "ascending" : "descending"); else th.removeAttribute("aria-sort");
    });
    r.rows.slice(0, SHOWN).forEach(function (e, i) {
      var tr = el("tr");
      tr.appendChild(el("td", "num quiet", String(i + 1)));
      tr.appendChild(el("td", "name", e.display_name));
      var c = el("td"); c.appendChild(crew(e)); tr.appendChild(c);
      ["landed", "first_pass", "lead", "hours", "tokens", "reply"].forEach(function (k) {
        tr.appendChild(el("td", "num" + (k === r.m.key ? " sorted" : " quiet"), valueOf(e, k)));
      });
      tbody.appendChild(tr);
    });
    wrap.hidden = false;
  }

  function load() {
    var status = document.getElementById("lb-status");
    var p = state.period;
    if (state.cache[p]) { state.data = state.cache[p]; render(); return; }
    status.hidden = false;
    status.textContent = "Loading the board…";
    fetch("/v1/leaderboard/board/" + p + ".json", { headers: { Accept: "application/json" } })
      .then(function (res) { if (!res.ok) throw new Error(res.status); return res.json(); })
      .then(function (data) {
        state.cache[p] = data;
        if (state.period !== p) return;
        state.data = data;
        render();
      })
      .catch(function () {
        if (state.period !== p) return;
        status.textContent = "The board couldn't be loaded just now. Try again in a minute.";
      });
  }

  function chips() {
    var group = document.querySelector('[data-group="metric"]');
    METRICS.forEach(function (m) {
      var b = el("button", "", m.label);
      b.type = "button";
      b.setAttribute("aria-pressed", String(m.key === state.metric));
      b.addEventListener("click", function () {
        state.metric = m.key;
        group.querySelectorAll("button").forEach(function (x) { x.setAttribute("aria-pressed", String(x === b)); });
        if (state.data) render();
      });
      group.appendChild(b);
    });
    var periods = document.querySelector('[data-group="period"]');
    periods.querySelectorAll("button").forEach(function (b) {
      b.addEventListener("click", function () {
        state.period = b.getAttribute("data-period");
        periods.querySelectorAll("button").forEach(function (x) { x.setAttribute("aria-pressed", String(x === b)); });
        load();
      });
    });
  }

  chips();
  load();
})();
