// The Rust boundary. Every network or audio call goes through here; the UI
// never sees a packet. Types mirror src-tauri/src/{lib.rs,store.rs,
// airplay/session.rs, airplay/mdns.rs} one-to-one.
import { invoke } from "@tauri-apps/api/core";

export interface Speaker {
  name: string;
  host: string;
  ip: string | null;
  port: number;
  model: string;
  device_id: string;
  txt: Record<string, string>;
}

export interface LastSpeaker {
  name: string;
  ip: string;
  port: number;
}

export type Latency = { mode: "auto" } | { mode: "fixed"; ms: number };

export interface Settings {
  last_speaker: LastSpeaker | null;
  volume: number;
  latency: Latency;
  sync_offset_ms: number;
  autostart_seeded: boolean;
}

export interface Stats {
  packets_sent: number;
  retransmit_requests: number;
  timing_requests: number;
  starved_packets: number;
  rtt_last_ms: number;
  rtt_p95_ms: number;
  seconds: number;
  latency_ms: number;
}

export interface Connected {
  speaker: LastSpeaker;
  stats: Stats;
  capture: string;
}

export interface NowPlaying {
  playing: boolean;
  title: string;
  artist: string;
}

export interface Status {
  connected: Connected | null;
  settings: Settings;
  media: NowPlaying;
}

export type Transport = "previous" | "play_pause" | "next";

export const speakersScan = () => invoke<Speaker[]>("speakers_scan");
export const speakersCached = () => invoke<Speaker[]>("speakers_cached");
export const speakerConnect = (speaker: LastSpeaker) => invoke<void>("speaker_connect", { speaker });
export const speakerDisconnect = () => invoke<void>("speaker_disconnect");
export const status = () => invoke<Status>("status");
export const volumeSet = (pct: number) => invoke<void>("volume_set", { pct });
export const latencySet = (latency: Latency, syncOffsetMs: number) =>
  invoke<void>("latency_set", { latency, syncOffsetMs });
export const transport = (kind: Transport) => invoke<void>("transport", { kind });
export const autostartGet = () => invoke<boolean>("autostart_get");
export const autostartSet = (on: boolean) => invoke<boolean>("autostart_set", { on });
export const panelHide = () => invoke<void>("panel_hide");
export const appQuit = () => invoke<void>("app_quit");
