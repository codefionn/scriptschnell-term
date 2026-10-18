// Installed into every page the debug browser's tools read. Builds the
// accessibility snapshot agents see and keeps the refs it hands out, so a
// later click or type finds the same element.
(() => {
  if (window.__cmdterm && window.__cmdterm.v === 1) return;
  const refs = new Map();
  const ids = new WeakMap();
  let next = 0;
  const squash = (s) => (s || "").replace(/\s+/g, " ").trim();
  const landmark = "article,aside,main,nav,section";
  const implicit = (el) => {
    switch (el.localName) {
      case "a":
      case "area":
        return el.hasAttribute("href") ? "link" : null;
      case "button":
      case "summary":
        return "button";
      case "input": {
        const type = (el.getAttribute("type") || "text").toLowerCase();
        if (type === "hidden") return null;
        if (["button", "submit", "reset", "image"].includes(type)) return "button";
        if (type === "checkbox") return "checkbox";
        if (type === "radio") return "radio";
        if (type === "range") return "slider";
        if (type === "number") return "spinbutton";
        if (type === "search") return "searchbox";
        if (el.hasAttribute("list")) return "combobox";
        return "textbox";
      }
      case "select":
        return el.multiple || el.size > 1 ? "listbox" : "combobox";
      case "option":
        return "option";
      case "textarea":
        return "textbox";
      case "h1":
      case "h2":
      case "h3":
      case "h4":
      case "h5":
      case "h6":
        return "heading";
      case "img":
        return el.getAttribute("alt") === "" ? null : "img";
      case "ul":
      case "ol":
      case "menu":
        return "list";
      case "li":
        return "listitem";
      case "nav":
        return "navigation";
      case "main":
        return "main";
      case "header":
        return el.closest(landmark) ? null : "banner";
      case "footer":
        return el.closest(landmark) ? null : "contentinfo";
      case "aside":
        return "complementary";
      case "form":
        return "form";
      case "table":
        return "table";
      case "tr":
        return "row";
      case "td":
        return "cell";
      case "th":
        return "columnheader";
      case "dialog":
        return "dialog";
      case "p":
        return "paragraph";
      case "details":
      case "fieldset":
        return "group";
      case "article":
        return "article";
      case "section":
        return el.hasAttribute("aria-label") || el.hasAttribute("aria-labelledby") ? "region" : null;
      case "hr":
        return "separator";
      case "progress":
        return "progressbar";
      case "iframe":
        return "iframe";
    }
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) {
      return "textbox";
    }
    return null;
  };
  // Roles whose name is their text; their children are not listed again.
  const leaf = new Set([
    "button", "link", "heading", "option", "checkbox", "radio", "switch", "tab",
    "menuitem", "menuitemcheckbox", "menuitemradio", "treeitem", "img", "textbox",
    "searchbox", "combobox", "slider", "spinbutton", "progressbar", "separator",
  ]);
  const withValue = new Set(["textbox", "searchbox", "combobox", "spinbutton", "slider"]);
  const hidden = (el) => {
    if (el.hidden || el.getAttribute("aria-hidden") === "true") return true;
    const style = getComputedStyle(el);
    return style.display === "none" || style.visibility === "hidden";
  };
  const labelled = (el) => {
    const by = el.getAttribute("aria-labelledby");
    if (!by) return "";
    return squash(
      by.split(/\s+/)
        .map((id) => document.getElementById(id))
        .filter(Boolean)
        .map((e) => e.innerText || e.textContent)
        .join(" "),
    );
  };
  const name = (el, role) => {
    const aria = squash(el.getAttribute("aria-label"));
    if (aria) return aria;
    const by = labelled(el);
    if (by) return by;
    if (el.labels && el.labels.length) {
      const label = squash([...el.labels].map((l) => l.innerText || l.textContent).join(" "));
      if (label) return label;
    }
    if (["img", "area"].includes(el.localName) || (el.localName === "input" && el.type === "image")) {
      const alt = squash(el.getAttribute("alt"));
      if (alt) return alt;
    }
    if (el.localName === "input" && ["button", "submit", "reset"].includes(el.type)) {
      return squash(el.value) || (el.type === "submit" ? "Submit" : el.type === "reset" ? "Reset" : "");
    }
    if (leaf.has(role) && !withValue.has(role)) {
      const content = squash(el.innerText || el.textContent);
      if (content) return content.slice(0, 200);
    }
    return squash(el.getAttribute("title") || el.getAttribute("placeholder"));
  };
  const refOf = (el) => {
    let ref = ids.get(el);
    if (!ref) {
      ref = "e" + ++next;
      ids.set(el, ref);
    }
    refs.set(ref, el);
    return ref;
  };
  const children = (node) => {
    if (node.localName === "slot") return node.assignedNodes({ flatten: true });
    return [...(node.shadowRoot || node).childNodes];
  };
  const MAX_LINES = 4000;
  const walk = (node, depth, out) => {
    for (const child of children(node)) {
      if (out.length > MAX_LINES) return;
      const pad = "  ".repeat(depth);
      if (child.nodeType === 3) {
        const text = squash(child.textContent);
        if (text) out.push(pad + "- text: " + JSON.stringify(text));
        continue;
      }
      if (child.nodeType !== 1) continue;
      const el = child;
      if (["script", "style", "noscript", "template", "head", "meta", "link"].includes(el.localName)) continue;
      if (hidden(el)) continue;
      const explicit = (el.getAttribute("role") || "").split(" ")[0];
      const role = explicit || implicit(el);
      if (!role || ["generic", "presentation", "none"].includes(role)) {
        walk(el, depth, out);
        continue;
      }
      const label = name(el, role);
      let line = pad + "- " + role + (label ? " " + JSON.stringify(label) : "");
      if (role === "heading") line += " [level=" + (el.getAttribute("aria-level") || el.localName.slice(1)) + "]";
      if (el.checked === true || el.getAttribute("aria-checked") === "true") line += " [checked]";
      if (el.disabled === true || el.getAttribute("aria-disabled") === "true") line += " [disabled]";
      const expanded = el.getAttribute("aria-expanded");
      if (expanded) line += " [expanded=" + expanded + "]";
      if (el.selected === true || el.getAttribute("aria-selected") === "true") line += " [selected]";
      if (el.getAttribute("aria-pressed") === "true") line += " [pressed]";
      line += " [ref=" + refOf(el) + "]";
      if (withValue.has(role)) {
        const value = el.localName === "select"
          ? [...el.selectedOptions].map((o) => squash(o.textContent)).join(", ")
          : el.isContentEditable ? squash(el.innerText) : el.value;
        if (value) line += ": " + JSON.stringify(value);
      }
      if (leaf.has(role)) {
        out.push(line);
        if (role === "link" && el.href) out.push(pad + "  - /url: " + el.href);
        if (el.localName === "select") {
          for (const option of el.options) {
            out.push(pad + "  - option " + JSON.stringify(squash(option.textContent)) +
              (option.selected ? " [selected]" : "") + " [ref=" + refOf(option) + "]");
          }
        }
        continue;
      }
      const inner = [];
      walk(el, depth + 1, inner);
      const only = inner.length === 1 && inner[0].startsWith(pad + "  - text: ");
      if (only) {
        out.push(line + ": " + inner[0].slice(pad.length + 10));
      } else {
        out.push(line, ...inner);
      }
    }
  };
  window.__cmdterm = {
    v: 1,
    snapshot() {
      const out = [];
      walk(document.body || document.documentElement, 0, out);
      if (out.length > MAX_LINES) out.push("- text: \"(snapshot cut at " + MAX_LINES + " lines)\"");
      return out.join("\n");
    },
    el(ref) {
      const el = refs.get(ref);
      if (!el || !el.isConnected) throw new Error("no element " + ref + " on this page; take a new snapshot");
      return el;
    },
    point(ref) {
      const el = this.el(ref);
      el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      const rect = el.getBoundingClientRect();
      if (rect.width === 0 && rect.height === 0) throw new Error(ref + " is not visible");
      return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
    },
  };
})();
