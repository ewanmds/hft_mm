"use client";

import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { api } from "@/lib/api";
import type { FinishedSession, StatusSnapshot } from "@/lib/types";
import { costPerMillion, formatDuration, formatTimeAgo } from "@/lib/types";

const pnlColor = (v: number) => (v >= 0 ? "#4ade80" : "#f87171");
const fmt = (v: number) => (v >= 0 ? `+$${v.toFixed(4)}` : `-$${Math.abs(v).toFixed(4)}`);
const fmt2 = (v: number) => (v >= 0 ? `+$${v.toFixed(2)}` : `-$${Math.abs(v).toFixed(2)}`);
const fmtVol = (v: number) =>
  v >= 1_000_000 ? `$${(v / 1_000_000).toFixed(2)}M` :
  v >= 1_000 ? `$${(v / 1000).toFixed(1)}k` : `$${v.toFixed(0)}`;
const volPerHour = (vol: number, secs: number) =>
  secs > 0 ? fmtVol(vol / (secs / 3600)) : "$0";

export default function RunPage() {
  const router = useRouter();
  const [status, setStatus] = useState<StatusSnapshot | null>(null);
  const [sessions, setSessions] = useState<FinishedSession[]>([]);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [s, sess] = await Promise.all([api.status(), api.sessions()]);
      setStatus(s);
      setSessions(sess);
    } catch { /* keep last state */ }
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 1000);
    return () => clearInterval(id);
  }, [refresh]);

  const handleStop = async () => {
    setStopping(true);
    setError(null);
    try {
      await api.stop();
      await refresh();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Failed to stop");
    } finally {
      setStopping(false);
    }
  };

  const isRunning = status?.running === true;
  const cpm = status ? costPerMillion(status.stats) : 0;

  return (
    <div style={{ maxWidth: 960, margin: "0 auto" }}>

      {/* Header */}
      <div style={{ marginBottom: 32 }}>
        <h1 style={{
          fontSize: 28, fontWeight: 700, letterSpacing: -0.5,
          background: "linear-gradient(135deg, #fff 40%, rgba(255,255,255,0.5))",
          WebkitBackgroundClip: "text", WebkitTextFillColor: "transparent",
          marginBottom: 8,
        }}>
          Live Dashboard
        </h1>
        <p style={{ fontSize: 14, color: "rgba(255,255,255,0.3)" }}>
          Real-time market maker performance
        </p>
      </div>

      {/* Tabs */}
      <div style={{ display: "flex", gap: 24, borderBottom: "1px solid rgba(255,255,255,0.07)", marginBottom: 32 }}>
        <div className="tab-inactive" onClick={() => router.push("/setup")} style={{ fontSize: 14 }}>Setup</div>
        <div className="tab-active" style={{ fontSize: 14 }}>Run</div>
      </div>

      {/* Active session */}
      <div style={{ marginBottom: 28 }}>
        <SectionLabel>Active session</SectionLabel>
        <div className={`glass card-3d ${isRunning ? "glow-green" : ""}`} style={{ padding: 28 }}>
          {isRunning && status ? (
            <>
              {/* Top row: token + PnL */}
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-start", marginBottom: 20 }}>
                <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                  <div className="pulse-dot" />
                  <span style={{ fontSize: 22, fontWeight: 700, color: "#fff" }}>{status.token}</span>
                  <span className="pill pill-green">Live</span>
                </div>
                <div style={{ textAlign: "right" }}>
                  <div style={{ fontSize: 28, fontWeight: 800, color: pnlColor(status.stats.session_pnl), letterSpacing: -1 }}>
                    {fmt2(status.stats.session_pnl)}
                  </div>
                  <div style={{ fontSize: 12, color: "rgba(255,255,255,0.3)", marginTop: 2 }}>
                    net {fmt2(status.stats.session_pnl - status.stats.total_fees)}
                  </div>
                </div>
              </div>

              {/* Elapsed time — prominent */}
              <div style={{
                display: "flex", alignItems: "center", gap: 16,
                background: "rgba(255,255,255,0.03)",
                border: "1px solid rgba(255,255,255,0.07)",
                borderRadius: 12, padding: "12px 16px", marginBottom: 16,
              }}>
                <div>
                  <div style={{ fontSize: 10, fontWeight: 600, letterSpacing: "0.08em", textTransform: "uppercase", color: "rgba(255,255,255,0.25)", marginBottom: 3 }}>
                    Elapsed
                  </div>
                  <div style={{ fontSize: 22, fontWeight: 700, color: "#e2e8f0", fontVariantNumeric: "tabular-nums" }}>
                    {formatDuration(status.uptime_secs)}
                  </div>
                </div>
                <div style={{ width: 1, height: 36, background: "rgba(255,255,255,0.08)" }} />
                <div>
                  <div style={{ fontSize: 10, fontWeight: 600, letterSpacing: "0.08em", textTransform: "uppercase", color: "rgba(255,255,255,0.25)", marginBottom: 3 }}>
                    Orders
                  </div>
                  <div style={{ fontSize: 16, fontWeight: 600 }}>
                    <span style={{ color: "#4ade80" }}>{status.buy_orders}B</span>
                    <span style={{ color: "rgba(255,255,255,0.2)", margin: "0 6px" }}>/</span>
                    <span style={{ color: "#f87171" }}>{status.sell_orders}A</span>
                  </div>
                </div>
                <div style={{ width: 1, height: 36, background: "rgba(255,255,255,0.08)" }} />
                <div>
                  <div style={{ fontSize: 10, fontWeight: 600, letterSpacing: "0.08em", textTransform: "uppercase", color: "rgba(255,255,255,0.25)", marginBottom: 3 }}>
                    Spread
                  </div>
                  <div style={{ fontSize: 16, fontWeight: 600, color: "#e2e8f0" }}>
                    {status.spread_ticks}t
                  </div>
                </div>
              </div>

              {/* Stats grid */}
              <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 10, marginBottom: 20 }}>
                <StatChip label="Volume" value={fmtVol(status.stats.total_volume)} />
                <StatChip label="Vol / hr" value={volPerHour(status.stats.total_volume, status.uptime_secs)} />
                <StatChip label="Position" value={status.position.toFixed(4)} color={status.position > 0 ? "#4ade80" : status.position < 0 ? "#f87171" : undefined} />
                <StatChip label="Fees" value={`$${status.stats.total_fees.toFixed(4)}`} dim />
                <StatChip label="Fills" value={String(status.stats.fills_count)} />
                <StatChip label="Cost/1M" value={`$${cpm.toFixed(1)}`} color={cpm > 30 ? "#f87171" : cpm < 10 ? "#4ade80" : undefined} />
              </div>

              {/* Unrealized PnL */}
              {Math.abs(status.unrealized_pnl) > 0.001 && (
                <div className="glass-sm" style={{ padding: "10px 14px", marginBottom: 16, display: "flex", alignItems: "center", gap: 12 }}>
                  <span style={{ fontSize: 12, color: "rgba(255,255,255,0.35)", textTransform: "uppercase", letterSpacing: "0.06em" }}>Unrealized</span>
                  <span style={{ fontSize: 14, fontWeight: 600, color: pnlColor(status.unrealized_pnl) }}>
                    {fmt(status.unrealized_pnl)}
                  </span>
                  {status.entry_price && (
                    <span style={{ fontSize: 12, color: "rgba(255,255,255,0.3)" }}>
                      entry ${status.entry_price.toFixed(2)}
                    </span>
                  )}
                </div>
              )}

              {/* Status line */}
              <div className="glass-sm" style={{ padding: "9px 14px", marginBottom: 22, fontFamily: "monospace", fontSize: 12, color: "rgba(255,255,255,0.35)" }}>
                {status.last_status}
              </div>

              {/* Stop button */}
              <button className="btn-danger" onClick={handleStop} disabled={stopping} style={{ width: "100%" }}>
                {stopping ? "Stopping..." : "■  Stop Session"}
              </button>
            </>
          ) : (
            /* Empty state */
            <div style={{ textAlign: "center", padding: "48px 0" }}>
              <div style={{
                width: 64, height: 64,
                background: "rgba(255,255,255,0.04)",
                border: "1px solid rgba(255,255,255,0.08)",
                borderRadius: 18,
                display: "flex", alignItems: "center", justifyContent: "center",
                margin: "0 auto 20px", fontSize: 28,
              }}>
                🤖
              </div>
              <div style={{ fontSize: 18, fontWeight: 600, color: "rgba(255,255,255,0.8)", marginBottom: 8 }}>
                Ready to Start
              </div>
              <div style={{ fontSize: 14, color: "rgba(255,255,255,0.3)", marginBottom: 28 }}>
                No active session — configure and launch from Setup
              </div>
              <button className="btn-primary" onClick={() => router.push("/setup")} style={{ padding: "13px 40px" }}>
                ▶  Setup Session
              </button>
            </div>
          )}
        </div>
      </div>

      {/* Error */}
      {error && (
        <div className="glass" style={{
          marginBottom: 20, padding: "14px 18px",
          border: "1px solid rgba(248,113,113,0.25)",
          background: "rgba(248,113,113,0.05)",
          color: "#f87171", fontSize: 14,
        }}>
          {error}
        </div>
      )}

      {/* Finished sessions */}
      {sessions.length > 0 && (
        <div>
          <SectionLabel>Finished sessions</SectionLabel>
          <div className="glass" style={{ padding: 0, overflow: "hidden" }}>
            <table className="data-table">
              <thead>
                <tr>
                  {["Finished", "Token", "Exchange", "Volume", "PNL/million", "Gross PnL", "Fees", "Net PnL", "Trades"].map((h) => (
                    <th key={h}>{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {sessions.map((s, i) => {
                  const cpm = s.cost_per_million;
                  return (
                    <tr key={i}>
                      <td style={{ color: "rgba(255,255,255,0.35)" }}>{formatTimeAgo(s.end_ts_ms)}</td>
                      <td style={{ color: "#e2e8f0", fontWeight: 600 }}>{s.token}</td>
                      <td style={{ color: "rgba(255,255,255,0.4)" }}>{s.exchange_name}</td>
                      <td style={{ color: "#e2e8f0" }}>{fmtVol(s.volume)}</td>
                      <td style={{ color: cpm > 30 ? "#f87171" : "#9ca3af" }}>
                        {cpm > 0 ? `-$${cpm.toFixed(0)}` : `+$${Math.abs(cpm).toFixed(0)}`}
                      </td>
                      <td style={{ color: pnlColor(s.gross_pnl), fontWeight: 600 }}>{fmt2(s.gross_pnl)}</td>
                      <td style={{ color: "rgba(255,255,255,0.4)" }}>${s.fees.toFixed(4)}</td>
                      <td style={{ color: pnlColor(s.net_pnl), fontWeight: 700 }}>{fmt2(s.net_pnl)}</td>
                      <td style={{ color: "#e2e8f0" }}>{s.fills}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div style={{ fontSize: 11, fontWeight: 600, letterSpacing: "0.08em", textTransform: "uppercase", color: "rgba(255,255,255,0.25)", marginBottom: 12 }}>
      {children}
    </div>
  );
}

function StatChip({ label, value, dim, color }: { label: string; value: string; dim?: boolean; color?: string }) {
  return (
    <div className="stat-chip">
      <div style={{ fontSize: 10, fontWeight: 600, letterSpacing: "0.07em", textTransform: "uppercase", color: "rgba(255,255,255,0.25)", marginBottom: 5 }}>
        {label}
      </div>
      <div style={{ fontSize: 15, fontWeight: 700, color: color ?? (dim ? "rgba(255,255,255,0.3)" : "#e2e8f0") }}>
        {value}
      </div>
    </div>
  );
}

