import { getCurrentWindow } from "@tauri-apps/api/window";
import { applyTheme, initTheme, type ThemeName } from "./theme";
import {
  appQuit,
  autostartGet,
  autostartSet,
  latencySet,
  panelHide,
  speakerConnect,
  speakerDisconnect,
  speakersCached,
  speakersScan,
  status,
  transport,
  volumeSet,
  type LastSpeaker,
  type Latency,
  type Speaker,
  type Status,
  type Transport,
} from "./api";

const appWindow = getCurrentWindow();

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const fmtTime = (s: number) => {
  const m = Math.floor(s / 60);
  const sec = s % 60;
  return m >= 60 ? `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}` : `${m}:${String(sec).padStart(2, "0")}`;
};

window.addEventListener("DOMContentLoaded", () => {
  initTheme();

  // ── toast ──
  const toast = $<HTMLParagraphElement>("toast");
  let toastTimer = 0;
  const notify = (msg: string, isError = false) => {
    toast.textContent = msg;
    toast.classList.toggle("toast--error", isError);
    toast.hidden = false;
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => (toast.hidden = true), isError ? 6000 : 2500);
  };

  // ── speakers ──
  const list = $<HTMLUListElement>("speaker-list");
  const scanBtn = $<HTMLButtonElement>("scan");
  let speakers: Speaker[] = [];
  let selected: LastSpeaker | null = null;
  let connectedName: string | null = null;

  const toLast = (s: Speaker): LastSpeaker | null => (s.ip ? { name: s.name, ip: s.ip, port: s.port } : null);

  const renderSpeakers = () => {
    if (speakers.length === 0) {
      const li = document.createElement("li");
      li.className = "speaker speaker--empty";
      li.textContent = scanning ? "Scanning…" : "No speakers found. Scan again?";
      list.replaceChildren(li);
      return;
    }
    list.replaceChildren(
      ...speakers.map((s) => {
        const li = document.createElement("li");
        li.className = "speaker";
        li.setAttribute("role", "option");
        const isSel = selected?.name === s.name;
        const isLive = connectedName === s.name;
        li.setAttribute("aria-selected", String(isSel));
        li.classList.toggle("is-live", isLive);
        if (!s.ip) li.classList.add("is-unreachable");
        li.innerHTML = `<span class="speaker__dot" aria-hidden="true"></span><span class="speaker__name"></span><span class="speaker__model"></span>`;
        li.querySelector(".speaker__name")!.textContent = s.name;
        li.querySelector(".speaker__model")!.textContent = s.ip ? s.model || "AirPlay" : "no address";
        li.title = s.ip ? `${s.ip}:${s.port}` : "mDNS answered without an A record";
        li.addEventListener("click", () => {
          if (busy || connectedName === s.name) return;
          selected = toLast(s);
          renderSpeakers();
          void connect();
        });
        return li;
      }),
    );
  };

  let scanning = false;
  const scan = async () => {
    if (scanning) return;
    scanning = true;
    scanBtn.classList.add("is-busy");
    renderSpeakers();
    try {
      speakers = await speakersScan();
      if (!selected && speakers.length === 1) selected = toLast(speakers[0]);
    } catch (e) {
      notify(String(e), true);
    } finally {
      scanning = false;
      scanBtn.classList.remove("is-busy");
      renderSpeakers();
    }
  };
  scanBtn.addEventListener("click", () => void scan());

  // ── connection ──
  const connStatus = $<HTMLElement>("conn-status");
  const connName = $<HTMLElement>("conn-title");
  const connLine = $<HTMLElement>("conn-line");
  const connectBtn = $<HTMLButtonElement>("connect");
  const disconnectBtn = $<HTMLButtonElement>("disconnect");
  const latencyMode = $<HTMLSelectElement>("latency-mode");
  const latencyMs = $<HTMLInputElement>("latency-ms");
  const latencyValue = $<HTMLElement>("latency-value");
  const syncOffset = $<HTMLInputElement>("sync-offset");
  const syncOffsetValue = $<HTMLElement>("sync-offset-value");
  const volume = $<HTMLInputElement>("volume");
  const volumeValue = $<HTMLElement>("volume-value");

  const setStatus = (state: "ok" | "probing" | "idle" | "missing", text: string) => {
    connStatus.dataset.status = state;
    connStatus.querySelector(".status__text")!.textContent = text;
  };

  let busy = false;
  const setBusy = (on: boolean) => {
    busy = on;
    connectBtn.classList.toggle("is-busy", on);
    disconnectBtn.classList.toggle("is-busy", on);
  };

  const connect = async () => {
    const target = selected ?? lastSpeaker;
    if (!target) {
      notify("Pick a speaker first", true);
      return;
    }
    setBusy(true);
    setStatus("probing", "Pairing…");
    connName.textContent = target.name;
    connLine.textContent = `${target.ip}:${target.port}`;
    try {
      await speakerConnect(target);
      notify(`Streaming to ${target.name}`);
    } catch (e) {
      setStatus("missing", "Failed");
      connLine.textContent = String(e);
      notify(String(e), true);
    } finally {
      setBusy(false);
      void refresh();
    }
  };
  const disconnect = async () => {
    setBusy(true);
    try {
      await speakerDisconnect();
    } catch (e) {
      notify(String(e), true);
    } finally {
      setBusy(false);
      void refresh();
    }
  };
  connectBtn.addEventListener("click", () => void connect());
  disconnectBtn.addEventListener("click", () => void disconnect());

  // Buffer + offset commit on change (a reconnect), reflect live on input.
  let lastSpeaker: LastSpeaker | null = null;
  const reflectLatency = () => {
    const fixed = latencyMode.value === "fixed";
    latencyMs.disabled = !fixed;
    latencyValue.textContent = fixed ? `${latencyMs.value} ms` : "Auto";
    syncOffsetValue.textContent = `+${syncOffset.value} ms`;
  };
  const commitLatency = async () => {
    const latency: Latency = latencyMode.value === "fixed" ? { mode: "fixed", ms: Number(latencyMs.value) } : { mode: "auto" };
    reflectLatency();
    if (connectedName) setStatus("probing", "Reconnecting…");
    try {
      await latencySet(latency, Number(syncOffset.value));
    } catch (e) {
      notify(String(e), true);
    } finally {
      void refresh();
    }
  };
  latencyMode.addEventListener("change", () => void commitLatency());
  latencyMs.addEventListener("input", reflectLatency);
  latencyMs.addEventListener("change", () => void commitLatency());
  syncOffset.addEventListener("input", reflectLatency);
  syncOffset.addEventListener("change", () => void commitLatency());

  // Volume goes live as you drag, throttled to the RTSP round trip.
  let volTimer = 0;
  volume.addEventListener("input", () => {
    volumeValue.textContent = `${volume.value}%`;
    window.clearTimeout(volTimer);
    volTimer = window.setTimeout(() => void volumeSet(Number(volume.value)).catch((e) => notify(String(e), true)), 120);
  });

  // ── transport ──
  const playPause = $<HTMLButtonElement>("play-pause");
  const npTitle = $<HTMLElement>("np-title");
  const npArtist = $<HTMLElement>("np-artist");
  document.querySelectorAll<HTMLButtonElement>("[data-transport]").forEach((b) => {
    b.addEventListener("click", () => {
      if (b === playPause) playPause.dataset.playing = String(playPause.dataset.playing !== "true"); // optimistic; the poll corrects it
      void transport(b.dataset.transport as Transport);
    });
  });

  // ── card order: the transport card rises to the top while streaming ──
  // FLIP: measure, reorder the DOM, play the delta back as a transform.
  const body = document.querySelector<HTMLElement>(".app-body")!;
  const cards = () => Array.from(body.querySelectorAll<HTMLElement>(":scope > .panel"));
  let transportOnTop = false;
  const moveCard = (card: HTMLElement, toTop: boolean) => {
    const before = new Map(cards().map((c) => [c, c.getBoundingClientRect().top]));
    if (toTop) body.prepend(card);
    else body.insertBefore(card, $("speakers"));
    const moving = cards().filter((c) => before.get(c) !== c.getBoundingClientRect().top);
    card.classList.add("is-front");
    for (const c of moving) {
      const dy = before.get(c)! - c.getBoundingClientRect().top;
      c.style.transform = `translateY(${dy}px)`;
    }
    void body.offsetHeight; // flush the start positions
    for (const c of moving) {
      c.classList.add("is-sliding");
      c.style.transform = "";
    }
    window.setTimeout(() => {
      for (const c of moving) c.classList.remove("is-sliding");
      card.classList.remove("is-front");
    }, 450);
  };

  // ── status poll ──
  let settingsApplied = false;
  const applyStatus = (s: Status) => {
    lastSpeaker = s.settings.last_speaker;
    if (!settingsApplied) {
      settingsApplied = true;
      volume.value = String(Math.round(s.settings.volume));
      volumeValue.textContent = `${volume.value}%`;
      latencyMode.value = s.settings.latency.mode;
      if (s.settings.latency.mode === "fixed") latencyMs.value = String(s.settings.latency.ms);
      syncOffset.value = String(s.settings.sync_offset_ms);
      reflectLatency();
      if (!selected && lastSpeaker) selected = lastSpeaker;
    }
    const c = s.connected;
    const wasLive = connectedName;
    connectedName = c ? c.speaker.name : null;
    if (c) {
      const st = c.stats;
      setStatus("ok", "Streaming");
      connName.textContent = c.speaker.name;
      const rtt = st.rtt_last_ms > 0 ? ` · rtt ${st.rtt_last_ms.toFixed(1)} ms` : "";
      const warn = st.retransmit_requests > 0 ? ` · ${st.retransmit_requests} resend req` : "";
      connLine.textContent = `buffer ${st.latency_ms} ms${rtt} · ${fmtTime(st.seconds)}${warn}`;
      connLine.title = `${c.capture} · ${st.packets_sent} packets · ${st.timing_requests} timing requests · ${st.starved_packets} starved`;
    } else if (!busy) {
      setStatus("idle", "Idle");
      if (wasLive) {
        connName.textContent = wasLive;
        connLine.textContent = "Disconnected.";
      } else if (!connName.textContent || connName.textContent === "Connection") {
        connName.textContent = lastSpeaker ? lastSpeaker.name : "Connection";
        connLine.textContent = lastSpeaker ? "Last used. Connect to resume." : "Click a speaker below.";
      }
    }
    connectBtn.hidden = !!c;
    disconnectBtn.hidden = !c;
    if (wasLive !== connectedName) renderSpeakers();
    if (!!c !== transportOnTop) {
      transportOnTop = !!c;
      moveCard($("transport"), transportOnTop);
    }
    playPause.dataset.playing = String(s.media.playing);
    npTitle.textContent = s.media.title || (s.media.playing ? "Playing" : "Nothing playing");
    npArtist.textContent = s.media.artist;
  };
  const refresh = async () => {
    try {
      applyStatus(await status());
    } catch (e) {
      console.warn("[status]", e);
    }
  };
  let poll = 0;
  const startPolling = () => {
    window.clearInterval(poll);
    poll = window.setInterval(() => void refresh(), 1000);
  };

  // ── chrome ──
  $("tl-close").addEventListener("click", () => void panelHide());
  $("quit").addEventListener("click", () => void appQuit());

  const autostartToggle = $("autostart-toggle");
  void autostartGet().then((on) => autostartToggle.setAttribute("aria-checked", String(on)));
  autostartToggle.addEventListener("click", async (e) => {
    e.stopPropagation();
    const next = autostartToggle.getAttribute("aria-checked") !== "true";
    try {
      autostartToggle.setAttribute("aria-checked", String(await autostartSet(next)));
    } catch (err) {
      notify(String(err), true);
    }
  });

  const trigger = $("settings-trigger");
  const menu = $("settings-menu");
  const closeMenu = () => {
    menu.hidden = true;
    trigger.setAttribute("aria-expanded", "false");
  };
  trigger.addEventListener("click", (e) => {
    e.stopPropagation();
    menu.hidden = !menu.hidden;
    trigger.setAttribute("aria-expanded", String(!menu.hidden));
  });
  document.addEventListener("click", (e) => {
    if (!menu.hidden && !menu.contains(e.target as Node)) closeMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (!menu.hidden) closeMenu();
      else void panelHide();
    }
  });
  document.querySelectorAll<HTMLElement>("[data-theme-choice]").forEach((el) => {
    el.addEventListener("click", () => {
      applyTheme(el.dataset.themeChoice as ThemeName);
      closeMenu();
    });
  });

  // Re-scan whenever the panel is shown, so a speaker that woke up appears.
  void appWindow.onFocusChanged(({ payload: focused }) => {
    if (focused) {
      void refresh();
      void scan();
    }
  });

  void speakersCached().then((s) => {
    speakers = s;
    renderSpeakers();
  });
  void refresh().then(startPolling);
  void scan();
});
