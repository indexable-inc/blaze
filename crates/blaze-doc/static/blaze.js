// blaze doc's script: the search box, the module list in the sidebar, copying an item's path,
// collapsing docs, and highlighting source lines. No framework, nothing loaded until it is
// used: the search index of each crate loads the first time the box is used.
"use strict";

(function () {
  const body = document.body;
  const root = body.dataset.root || "";
  const input = document.getElementById("search-input");
  const main = document.getElementById("main-content");
  const results = document.getElementById("search");

  function loadScript(src) {
    return new Promise((resolve) => {
      const script = document.createElement("script");
      script.src = src;
      script.onload = () => resolve(true);
      script.onerror = () => resolve(false);
      document.head.appendChild(script);
    });
  }

  function escapeHtml(text) {
    return text.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  }

  // The module list: the items of the module the page is in, from its sidebar-items.js.
  const KIND_TITLES = {
    keyword: "Keywords", primitive: "Primitive Types", mod: "Modules", macro: "Macros",
    struct: "Structs", enum: "Enums", constant: "Constants", static: "Statics", trait: "Traits",
    fn: "Functions", type: "Type Aliases", union: "Unions", attr: "Attribute Macros",
    derive: "Derive Macros", traitalias: "Trait Aliases",
  };
  const sidebarSrc = body.dataset.sidebar;
  const moduleBox = document.querySelector("#sidebar-module .sidebar-module-items");
  if (sidebarSrc && moduleBox) {
    const base = sidebarSrc.slice(0, sidebarSrc.length - "sidebar-items.js".length);
    loadScript(sidebarSrc).then(() => {
      const items = window.SIDEBAR_ITEMS || {};
      let html = "";
      for (const kind of Object.keys(KIND_TITLES)) {
        const names = items[kind];
        if (!names || names.length === 0) continue;
        html += `<h3>${KIND_TITLES[kind]}</h3><ul class="block">`;
        for (const name of names) {
          const href = kind === "mod" ? `${base}${name}/index.html` : `${base}${kind}.${name}.html`;
          html += `<li><a class="${kind}" href="${href}">${escapeHtml(name)}</a></li>`;
        }
        html += "</ul>";
      }
      moduleBox.innerHTML = html;
    });
  }

  // Copying the item's path.
  const copy = document.getElementById("copy-path");
  if (copy) {
    copy.addEventListener("click", () => {
      navigator.clipboard.writeText(copy.dataset.path).then(() => {
        copy.classList.add("copied");
        setTimeout(() => copy.classList.remove("copied"), 1200);
      });
    });
  }

  // Collapsing every doc block at once.
  const toggleAll = document.querySelector(".toggle-all");
  function setAll(open) {
    for (const details of document.querySelectorAll("details.toggle")) details.open = open;
    if (toggleAll) toggleAll.textContent = open ? "Collapse all" : "Expand all";
  }
  if (toggleAll) {
    toggleAll.addEventListener("click", () => setAll(toggleAll.textContent !== "Collapse all"));
  }

  // Source lines: `#10` or `#10-20` highlights them; shift-click extends.
  function highlightLines() {
    const lines = document.querySelectorAll(".src-line");
    if (lines.length === 0) return;
    for (const line of document.querySelectorAll(".src-line.highlighted")) line.classList.remove("highlighted");
    const match = /^#(\d+)(?:-(\d+))?$/.exec(location.hash);
    if (!match) return;
    const from = parseInt(match[1], 10);
    const to = match[2] ? parseInt(match[2], 10) : from;
    for (let n = Math.min(from, to); n <= Math.max(from, to) && n <= lines.length; n++) {
      lines[n - 1].classList.add("highlighted");
    }
    const first = lines[Math.min(from, to) - 1];
    if (first) first.scrollIntoView({ block: "center" });
  }
  let lastLine = null;
  document.addEventListener("click", (event) => {
    const link = event.target.closest && event.target.closest(".line-number");
    if (!link) return;
    event.preventDefault();
    const n = parseInt(link.id, 10);
    const hash = event.shiftKey && lastLine !== null ? `#${Math.min(lastLine, n)}-${Math.max(lastLine, n)}` : `#${n}`;
    if (!event.shiftKey) lastLine = n;
    history.replaceState(null, "", hash);
    highlightLines();
  });
  window.addEventListener("hashchange", highlightLines);
  highlightLines();

  // Search.
  if (!input || !main || !results) return;
  let index = null;
  let loading = null;
  window.blazeSearchLoad = (krate, entries) => {
    for (const [kind, name, parent, href, desc] of entries) {
      index.push({ kind, name, parent, href, desc, lower: name.toLowerCase(), full: `${parent}::${name}`.toLowerCase(), krate });
    }
  };
  function loadIndex() {
    if (loading) return loading;
    index = [];
    loading = loadScript(`${root}crates.js`).then(() => {
      const crates = window.BLAZE_CRATES || [body.dataset.crate];
      return Promise.all(crates.map((krate) => loadScript(`${root}search-index/${krate}.js`)));
    });
    return loading;
  }

  const KIND_RANK = { mod: 0, struct: 1, enum: 1, trait: 1, union: 1, type: 2, fn: 2, macro: 2, derive: 2, attr: 2, constant: 3, static: 3, primitive: 1 };
  function score(entry, query, words) {
    // Lower is better: an exact name, then a prefix, then a word of the name, then the path.
    const lastWord = words[words.length - 1];
    let s;
    if (entry.lower === lastWord) s = 0;
    else if (entry.lower.startsWith(lastWord)) s = 10;
    else if (entry.lower.includes(lastWord)) s = 20;
    else if (entry.full.includes(query)) s = 30;
    else return -1;
    for (const word of words.slice(0, -1)) {
      if (!entry.full.includes(word)) return -1;
    }
    s += (KIND_RANK[entry.kind] ?? 4) + entry.lower.length / 100;
    if (entry.krate === body.dataset.crate) s -= 1;
    return s;
  }

  let selected = -1;
  function render(query) {
    const q = query.trim().toLowerCase();
    if (!q) {
      results.classList.add("hidden");
      main.classList.remove("hidden");
      return;
    }
    const words = q.split(/::|\s+/).filter(Boolean);
    const found = [];
    for (const entry of index) {
      const s = score(entry, q, words);
      if (s >= 0) found.push([s, entry]);
    }
    found.sort((a, b) => a[0] - b[0] || a[1].full.localeCompare(b[1].full));
    const shown = found.slice(0, 200);
    let html = `<h1>Results for ${escapeHtml(query.trim())}</h1>`;
    html += `<p class="search-summary">${found.length === 0 ? "No results" : found.length === 1 ? "1 result" : `${found.length} results`}${found.length > shown.length ? `, the first ${shown.length} shown` : ""}</p>`;
    if (shown.length === 0) {
      html += `<div class="no-results">Nothing in the documented crates has that name. Try fewer letters, or a path such as <code>serde::de</code>.</div>`;
    } else {
      html += `<div class="search-results">`;
      for (const [, e] of shown) {
        html += `<a class="result" href="${root}${e.href}"><span class="path"><span class="parent">${escapeHtml(e.parent).replaceAll("::", "::<wbr>")}::<wbr></span><span class="kind-${e.kind}">${escapeHtml(e.name)}</span></span><span class="desc">${escapeHtml(e.desc)}</span></a>`;
      }
      html += `</div>`;
    }
    results.innerHTML = html;
    selected = -1;
    main.classList.add("hidden");
    results.classList.remove("hidden");
  }

  function updateUrl(query) {
    const url = new URL(location.href);
    if (query.trim()) url.searchParams.set("search", query.trim());
    else url.searchParams.delete("search");
    history.replaceState(null, "", url);
  }

  let timer = null;
  input.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      loadIndex().then(() => {
        render(input.value);
        updateUrl(input.value);
      });
    }, 60);
  });
  input.addEventListener("focus", () => loadIndex());
  input.addEventListener("keydown", (event) => {
    const items = results.querySelectorAll(".result");
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (items.length === 0) return;
      event.preventDefault();
      selected = event.key === "ArrowDown" ? Math.min(selected + 1, items.length - 1) : Math.max(selected - 1, 0);
      items.forEach((item, at) => item.classList.toggle("selected", at === selected));
      items[selected].scrollIntoView({ block: "nearest" });
    } else if (event.key === "Enter") {
      const pick = items[selected >= 0 ? selected : 0];
      if (pick) location.href = pick.href;
    } else if (event.key === "Escape") {
      input.value = "";
      render("");
      updateUrl("");
      input.blur();
    }
  });

  document.addEventListener("keydown", (event) => {
    if (event.target === input || event.ctrlKey || event.metaKey || event.altKey) return;
    if (event.target.tagName === "INPUT" || event.target.tagName === "TEXTAREA") return;
    if (event.key === "s" || event.key === "S" || event.key === "/") {
      event.preventDefault();
      input.focus();
      input.select();
    } else if (event.key === "-") {
      setAll(false);
    } else if (event.key === "+") {
      setAll(true);
    }
  });

  const asked = new URL(location.href).searchParams.get("search");
  if (asked) {
    input.value = asked;
    loadIndex().then(() => render(asked));
  }
})();
