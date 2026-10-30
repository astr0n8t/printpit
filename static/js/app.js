// printpit: keyboard shortcuts and small glue. No framework; keep this file small.
"use strict";

(() => {
  const typing = (el) =>
    el instanceof HTMLElement &&
    (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName));

  // Tab shortcuts: any link with data-key="<key>" is clicked when that key is pressed.
  document.addEventListener("keydown", (e) => {
    if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
    if (document.querySelector("dialog[open]")) return;
    const link = document.querySelector(`a[data-key="${CSS.escape(e.key)}"]`);
    if (link) {
      e.preventDefault();
      link.click();
    }
  });
})();
