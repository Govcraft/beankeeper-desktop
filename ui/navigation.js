// Embedded, same-origin enhancement. Ordinary links/forms also work without JS.
(async () => {
  "use strict";
  const csrf = document.querySelector('section[aria-labelledby="session"] input[name="csrf"]')?.value;
  if (!csrf) return;
  const notice = document.createElement("p");
  notice.setAttribute("role", "status");
  notice.className = "note";
  document.querySelector("main")?.prepend(notice);
  let tabId = sessionStorage.getItem("bk-tab");
  // Duplicating a tab can clone sessionStorage. Keep the active original's ID.
  const channel = typeof BroadcastChannel === "function" ? new BroadcastChannel("bk-tabs") : null;
  let probing = true;
  channel?.addEventListener("message", ({ data }) => {
    if (data.id !== tabId) return;
    if (data.kind === "probe" && !probing) channel.postMessage({ kind: "occupied", id: tabId });
    if (data.kind === "occupied" && probing) tabId = null;
  });
  if (tabId && channel) {
    channel.postMessage({ kind: "probe", id: tabId });
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  probing = false;
  const targets = () => [...document.querySelectorAll("main [id], main[id]")]
    .map(element => element.id).filter(id => id.length <= 256).slice(0, 128).join("\n");
  let highlight = "";
  const update = async () => {
    const body = new URLSearchParams({ csrf, url: location.pathname + location.search, title: document.title, targets: targets(), highlight });
    if (tabId) body.set("tab_id", tabId);
    const response = await fetch("/api/ui/tab", { method: "POST", headers: { "X-CSRF-Token": csrf }, body });
    if (!response.ok) throw new Error("Session unavailable");
    tabId = (await response.json()).tab_id;
    sessionStorage.setItem("bk-tab", tabId);
  };
  try {
    await update();
    const events = new EventSource(`/api/ui/events?tab_id=${encodeURIComponent(tabId)}`);
    let associated = null;
    events.addEventListener("open", () => {
      associated = null;
      notice.textContent = "Browser navigation channel connected.";
    });
    events.addEventListener("error", () => {
      associated = null;
      notice.textContent = "Browser navigation disconnected. Reload to reconnect if your session expired.";
    });
    events.addEventListener("ui", async ({ data }) => {
      try {
        const event = JSON.parse(data);
        if (typeof event.agent_associated === "boolean" && event.agent_associated !== associated) {
          associated = event.agent_associated;
          const connection = document.querySelector("[data-agent-connection]");
          if (connection) connection.textContent = associated
            ? "Your agent is connected to this browser."
            : "Your browser session is active. Open your agent's connection link here to let it navigate this workspace.";
          notice.textContent = associated
            ? "Agent linked. Browser navigation ready."
            : "Browser paired. Open your agent's connection link to enable navigation.";
        }
        if (event.kind === "navigate") {
          const url = new URL(event.url, location.origin);
          if (url.origin !== location.origin || !event.url.startsWith("/") || event.url.startsWith("//")) return;
          notice.textContent = "Your agent is opening a view.";
          location.assign(url.href);
        } else if (event.kind === "highlight") {
          document.querySelectorAll(".agent-highlight").forEach(element => element.classList.remove("agent-highlight"));
          const target = document.getElementById(event.target);
          highlight = target?.closest("main") ? event.target : "";
          if (highlight) {
            target.classList.add("agent-highlight");
            target.scrollIntoView({ block: "center", behavior: "instant" });
          }
          notice.textContent = highlight ? `Your agent highlighted ${highlight}.` : "The requested row or region is no longer in this view.";
          await update();
        }
      } catch { notice.textContent = "Navigation could not be applied. Reload this view to reconnect."; }
    });
    addEventListener("pagehide", () => { events.close(); channel?.close(); });
  } catch { notice.textContent = "Agent navigation unavailable. Reload after pairing this browser."; }
})();
