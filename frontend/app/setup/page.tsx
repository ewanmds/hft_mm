"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { api } from "@/lib/api";
import type { TokenInfo } from "@/lib/types";

const TIME_PRESETS = [
  { label: "15 min", secs: 900 },
  { label: "30 min", secs: 1800 },
  { label: "1 hour", secs: 3600 },
  { label: "2 hours", secs: 7200 },
  { label: "4 hours", secs: 14400 },
  { label: "8 hours", secs: 28800 },
  { label: "No limit", secs: 0 },
];

type Ec2State = "unknown" | "stopped" | "pending" | "running" | "stopping";

export default function SetupPage() {
  const router = useRouter();
  const [tokens, setTokens] = useState<TokenInfo[]>([]);
  const [selectedToken, setSelectedToken] = useState<string>("XYZ100");
  const [orderSize, setOrderSize] = useState<string>("");
  const [leverage, setLeverage] = useState<number>(20);
  const [timeLimitSecs, setTimeLimitSecs] = useState<number>(0);
  const [ec2State, setEc2State] = useState<Ec2State>("unknown");
  const [statusMsg, setStatusMsg] = useState<string>("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  // On mount: check EC2 state + load tokens if running
  useEffect(() => {
    checkEc2State();
  }, []);

  const checkEc2State = async () => {
    try {
      const { state } = await api.ec2Status();
      setEc2State(state as Ec2State);
      if (state === "running") {
        loadTokensAndBotStatus();
      }
    } catch {
      setEc2State("unknown");
    }
  };

  const loadTokensAndBotStatus = async () => {
    try {
      const [toks, status] = await Promise.all([
        api.tokens().catch(() => [] as TokenInfo[]),
        api.status().catch(() => null),
      ]);
      setTokens(toks);
      if (status?.running) router.push("/run");
    } catch {}
  };

  const startPollUntilRunning = (
    pendingConfig: { token: string; order_size_usd?: number; leverage: number; time_limit_secs?: number }
  ) => {
    let attempts = 0;
    pollRef.current = setInterval(async () => {
      attempts++;
      try {
        const { state } = await api.ec2Status();
        setEc2State(state as Ec2State);

        if (state === "running") {
          setStatusMsg("EC2 prêt, lancement du bot...");
          // Try to start bot (backend may take a few seconds after EC2 is running)
          let botStarted = false;
          for (let i = 0; i < 10; i++) {
            try {
              await api.start(pendingConfig);
              botStarted = true;
              break;
            } catch {
              await new Promise((r) => setTimeout(r, 3000));
            }
          }
          clearInterval(pollRef.current!);
          setLoading(false);
          if (botStarted) {
            router.push("/run");
          } else {
            setError("EC2 démarré mais le bot ne répond pas. Vérifie systemd sur le serveur.");
          }
        } else if (attempts > 40) {
          // 40 * 5s = 200s timeout
          clearInterval(pollRef.current!);
          setLoading(false);
          setError("Timeout : EC2 n'a pas démarré après 3 min.");
        } else {
          setStatusMsg(`EC2 en démarrage... (${attempts * 5}s)`);
        }
      } catch {}
    }, 5000);
  };

  const handleLaunch = async () => {
    setLoading(true);
    setError(null);
    const pendingConfig = {
      token: selectedToken,
      order_size_usd: parseFloat(orderSize) || undefined,
      leverage,
      time_limit_secs: timeLimitSecs > 0 ? timeLimitSecs : undefined,
    };

    try {
      if (ec2State === "stopped" || ec2State === "unknown") {
        setStatusMsg("Démarrage de l'EC2...");
        await api.ec2Start();
        setEc2State("pending");
        startPollUntilRunning(pendingConfig);
      } else if (ec2State === "running") {
        // EC2 already running, just start the bot
        await api.start(pendingConfig);
        router.push("/run");
        setLoading(false);
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Erreur");
      setLoading(false);
    }
  };

  const handleStop = async () => {
    setLoading(true);
    setError(null);
    try {
      await api.stop().catch(() => {});
      await api.ec2Stop();
      setEc2State("stopping");
      setStatusMsg("EC2 en arrêt...");
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Erreur");
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => () => { if (pollRef.current) clearInterval(pollRef.current); }, []);

  const selectedInfo = tokens.find((t) => t.name === selectedToken);
  const leveragePct = ((leverage - 1) / 49) * 100;
  const isPending = ec2State === "pending" || ec2State === "stopping";

  return (
    <div style={{ maxWidth: 680, margin: "0 auto" }}>

      {/* Header */}
      <div style={{ marginBottom: 32, textAlign: "center" }}>
        <h1 style={{
          fontSize: 28, fontWeight: 700, letterSpacing: -0.5,
          background: "linear-gradient(135deg, #fff 40%, rgba(255,255,255,0.5))",
          WebkitBackgroundClip: "text", WebkitTextFillColor: "transparent",
          marginBottom: 8,
        }}>
          Configure Session
        </h1>
        <p style={{ fontSize: 14, color: "rgba(255,255,255,0.3)" }}>
          Set your trading parameters before launching
        </p>
      </div>

      {/* Tab bar */}
      <div style={{ display: "flex", gap: 24, borderBottom: "1px solid rgba(255,255,255,0.07)", marginBottom: 32 }}>
        <span onClick={() => router.push("/setup")} className="tab-active" style={{ cursor: "pointer", fontSize: 14 }}>Setup</span>
        <div className="tab-inactive" onClick={() => router.push("/run")} style={{ fontSize: 14 }}>Run</div>
      </div>

      {/* EC2 status badge */}
      <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 20 }}>
        <div style={{
          width: 8, height: 8, borderRadius: "50%",
          background: ec2State === "running" ? "#4ade80" : ec2State === "stopped" ? "#f87171" : "#fbbf24",
          boxShadow: ec2State === "running" ? "0 0 8px #4ade80" : "none",
        }} />
        <span style={{ fontSize: 13, color: "rgba(255,255,255,0.4)" }}>
          Serveur : {ec2State === "running" ? "en ligne" : ec2State === "stopped" ? "éteint" : ec2State === "pending" ? "démarrage..." : ec2State === "stopping" ? "arrêt..." : "inconnu"}
        </span>
        {ec2State === "running" && (
          <button onClick={handleStop} disabled={loading} style={{
            marginLeft: "auto", background: "none",
            border: "1px solid rgba(248,113,113,0.3)", color: "#f87171",
            borderRadius: 8, padding: "4px 12px", fontSize: 12, cursor: "pointer",
          }}>
            Éteindre le serveur
          </button>
        )}
      </div>

      {/* Pending state */}
      {isPending && (
        <div className="glass" style={{
          padding: "20px", marginBottom: 20, textAlign: "center",
          border: "1px solid rgba(251,191,36,0.2)",
          background: "rgba(251,191,36,0.05)",
        }}>
          <div style={{ fontSize: 14, color: "#fbbf24", marginBottom: 6 }}>{statusMsg}</div>
          <div style={{ fontSize: 12, color: "rgba(255,255,255,0.3)" }}>
            {ec2State === "pending" ? "L'instance EC2 démarre, ça prend ~30-60 secondes..." : "Arrêt en cours..."}
          </div>
        </div>
      )}

      {/* Main config card */}
      <div className="glass card-3d" style={{ padding: 28, opacity: isPending ? 0.4 : 1, pointerEvents: isPending ? "none" : "auto" }}>

        {/* Section: Token */}
        <div style={{ marginBottom: 24 }}>
          <Label>Token</Label>
          <select
            value={selectedToken}
            onChange={(e) => {
              setSelectedToken(e.target.value);
              const info = tokens.find((t) => t.name === e.target.value);
              if (info) {
                setLeverage(Math.round(info.default_leverage));
                setOrderSize("");
              }
            }}
            className="glass-input"
          >
            {tokens.length > 0 ? tokens.map((t) => (
              <option key={t.name} value={t.name}>{t.name} — {t.symbol}</option>
            )) : (
              <option value="XYZ100">XYZ100 (serveur éteint)</option>
            )}
          </select>
          {selectedInfo && (
            <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
              <span className="pill pill-blue">Leverage {selectedInfo.default_leverage}x</span>
              <span className="pill pill-green">Size ${selectedInfo.default_order_size_usd}</span>
            </div>
          )}
        </div>

        {/* Divider */}
        <div style={{ height: 1, background: "rgba(255,255,255,0.06)", margin: "4px 0 24px" }} />

        {/* Section: Order Size + Time */}
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 20, marginBottom: 24 }}>
          <div>
            <Label>Order Size (USD)</Label>
            <input
              type="number"
              value={orderSize}
              onChange={(e) => setOrderSize(e.target.value)}
              placeholder={selectedInfo ? `default: $${selectedInfo.default_order_size_usd}` : "default"}
              className="glass-input"
            />
          </div>
          <div>
            <Label>Time Limit</Label>
            <select value={timeLimitSecs} onChange={(e) => setTimeLimitSecs(Number(e.target.value))} className="glass-input">
              {TIME_PRESETS.map((p) => (
                <option key={p.secs} value={p.secs}>{p.label}</option>
              ))}
            </select>
          </div>
        </div>

        {/* Divider */}
        <div style={{ height: 1, background: "rgba(255,255,255,0.06)", margin: "4px 0 24px" }} />

        {/* Section: Leverage */}
        <div style={{ marginBottom: 8 }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 12 }}>
            <Label>Leverage</Label>
            <div style={{
              background: "linear-gradient(135deg, rgba(74,222,128,0.15), rgba(74,222,128,0.05))",
              border: "1px solid rgba(74,222,128,0.25)",
              borderRadius: 8, padding: "3px 12px",
              fontSize: 15, fontWeight: 700, color: "#4ade80",
            }}>
              {leverage}x
            </div>
          </div>
          <input
            type="range" min={1} max={50} value={leverage}
            onChange={(e) => setLeverage(Number(e.target.value))}
            style={{ backgroundImage: `linear-gradient(to right, #4ade80 ${leveragePct}%, rgba(255,255,255,0.1) ${leveragePct}%)` }}
          />
          <div style={{ display: "flex", justifyContent: "space-between", fontSize: 11, color: "rgba(255,255,255,0.2)", marginTop: 6 }}>
            <span>1×</span><span>10×</span><span>20×</span><span>35×</span><span>50×</span>
          </div>
        </div>
      </div>

      {/* Error */}
      {error && (
        <div className="glass" style={{
          marginTop: 16, padding: "14px 18px",
          border: "1px solid rgba(248,113,113,0.25)",
          background: "rgba(248,113,113,0.06)",
          color: "#f87171", fontSize: 14,
        }}>
          {error}
        </div>
      )}

      {/* Launch button */}
      <button
        className="btn-primary"
        onClick={handleLaunch}
        disabled={loading || isPending}
        style={{ width: "100%", marginTop: 20, fontSize: 16 }}
      >
        {loading && ec2State !== "running"
          ? statusMsg || "Démarrage EC2..."
          : loading
          ? "Lancement..."
          : ec2State === "running"
          ? "▶  Lancer le bot"
          : "⚡  Démarrer le serveur & Lancer le bot"}
      </button>

    </div>
  );
}

function Label({ children }: { children: React.ReactNode }) {
  return (
    <div style={{ fontSize: 11, fontWeight: 600, letterSpacing: "0.08em", textTransform: "uppercase", color: "rgba(255,255,255,0.3)", marginBottom: 8 }}>
      {children}
    </div>
  );
}
