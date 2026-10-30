// printpit: keyboard shortcuts, dialogs, the dashboard's live stream and camera tiles.
// No framework; keep this file small.
"use strict";

(() => {
  const typing = (el) =>
    el instanceof HTMLElement &&
    (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName));

  // ── Keyboard ──────────────────────────────────────────────────────
  // Links with data-key="<key>" are clicked by that key; "r" clicks every [data-refresh].
  document.addEventListener("keydown", (e) => {
    if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
    if (document.querySelector("dialog[open]")) return;
    if (e.key === "r") {
      const buttons = document.querySelectorAll("[data-refresh]");
      if (buttons.length) {
        e.preventDefault();
        buttons.forEach((b) => b.click());
      }
      return;
    }
    const link = document.querySelector(`a[data-key="${CSS.escape(e.key)}"]`);
    if (link) {
      e.preventDefault();
      link.click();
    }
  });

  // ── Dialogs ───────────────────────────────────────────────────────
  // TUI confirm for hx-confirm: "Delete printer X? [y/N]".
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

  // Content swapped into #modal-body opens the modal; the server closes it via HX-Trigger.
  const modal = () => document.getElementById("modal");
  document.addEventListener("htmx:afterSwap", (e) => {
    if (e.detail.target.id === "modal-body" && !modal().open) modal().showModal();
  });
  document.addEventListener("printpit:close-modal", () => modal()?.open && modal().close());

  // ── Live stream ───────────────────────────────────────────────────
  // One EventSource while a [data-sse] element is on screen and the page is visible. Each
  // message is "<element id>\n<html>". Closing it lets the server drop the printer connection.
  let source = null;
  const syncStream = () => {
    const host = document.querySelector("[data-sse]");
    const want = host && document.visibilityState === "visible" ? host.dataset.sse : null;
    if (source && source.printpitUrl !== want) {
      source.close();
      source = null;
    }
    if (!source && want) {
      source = new EventSource(want);
      source.printpitUrl = want;
      source.onmessage = (e) => {
        const nl = e.data.indexOf("\n");
        const target = document.getElementById(e.data.slice(0, nl));
        if (target) htmx.swap(target, e.data.slice(nl + 1), { swapStyle: "innerHTML" });
      };
    }
  };
  document.addEventListener("visibilitychange", syncStream);
  document.addEventListener("htmx:afterSettle", syncStream);
  document.addEventListener("htmx:historyRestore", syncStream);
  document.addEventListener("DOMContentLoaded", syncStream);

  // ── Camera tiles ──────────────────────────────────────────────────
  const SNAPSHOT_EVERY_MS = 5000;
  const visible = new Set();
  const observer = new IntersectionObserver((entries) => {
    for (const e of entries) e.isIntersecting ? visible.add(e.target) : visible.delete(e.target);
  });
  const watchCams = () => {
    document.querySelectorAll("img.cam-img:not([data-watched])").forEach((img) => {
      img.dataset.watched = "1";
      observer.observe(img);
    });
  };
  document.addEventListener("htmx:afterSettle", watchCams);
  document.addEventListener("DOMContentLoaded", watchCams);

  // A 404/502 snapshot means no (reachable) camera: show the placeholder instead.
  document.addEventListener(
    "error",
    (e) => {
      if (e.target instanceof HTMLImageElement && e.target.matches("img.cam-img")) {
        e.target.closest(".cam")?.classList.add("cam-missing");
      }
    },
    true,
  );
  document.addEventListener(
    "load",
    (e) => {
      if (e.target instanceof HTMLImageElement && e.target.matches("img.cam-img")) {
        e.target.closest(".cam")?.classList.remove("cam-missing");
      }
    },
    true,
  );

  // "live" swaps the snapshot for the proxied MJPEG stream; clicking again stops it.
  document.addEventListener("click", (e) => {
    const button = e.target instanceof Element && e.target.closest("button[data-live]");
    if (!button) return;
    const img = button.closest(".cam")?.querySelector("img.cam-img");
    if (!img) return;
    if (img.dataset.streaming) {
      delete img.dataset.streaming;
      img.src = `${img.dataset.snapshot}?t=${Date.now()}`;
      button.textContent = "live";
    } else {
      img.dataset.streaming = "1";
      img.src = button.dataset.live;
      button.textContent = "stop";
    }
  });

  // Snapshots refresh only with auto-refresh on, the page visible and the tile on screen.
  setInterval(() => {
    if (document.visibilityState !== "visible") return;
    if (!document.querySelector('[data-auto="1"]')) return;
    for (const img of visible) {
      if (!img.isConnected) {
        visible.delete(img);
        continue;
      }
      if (img.dataset.streaming || img.closest(".cam-missing")) continue;
      img.src = `${img.dataset.snapshot}?t=${Date.now()}`;
    }
  }, SNAPSHOT_EVERY_MS);
})();
