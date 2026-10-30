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

  // TUI confirm dialog for hx-confirm: "Delete printer X? [y/N]".
  const ask = (question) =>
    new Promise((resolve) => {
      const dialog = document.getElementById("confirm");
      if (!dialog) return resolve(window.confirm(question));
      dialog.querySelector("#confirm-q").textContent = `${question} [y/N]`;
      dialog.returnValue = "";
      const onKey = (e) => {
        const k = e.key.toLowerCase();
        if (k === "y" || k === "n") {
          e.preventDefault();
          dialog.close(k === "y" ? "yes" : "no");
        }
      };
      dialog.addEventListener("keydown", onKey);
      dialog.addEventListener(
        "close",
        () => {
          dialog.removeEventListener("keydown", onKey);
          resolve(dialog.returnValue === "yes");
        },
        { once: true },
      );
      dialog.showModal();
    });

  document.addEventListener("htmx:confirm", (e) => {
    if (!e.detail.question) return;
    e.preventDefault();
    ask(e.detail.question).then((ok) => ok && e.detail.issueRequest(true));
  });
})();
