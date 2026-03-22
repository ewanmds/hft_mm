import type { Metadata } from "next";
import "./globals.css";
import Link from "next/link";

export const metadata: Metadata = {
  title: "HFT Market Maker",
  description: "Spatial trading control panel",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body style={{ minHeight: "100vh", position: "relative" }}>
        {/* Decorative orbs */}
        <div className="orb" style={{ width: 500, height: 500, background: "rgba(74,222,128,0.04)", top: -100, left: -100 }} />
        <div className="orb" style={{ width: 400, height: 400, background: "rgba(99,102,241,0.05)", bottom: -100, right: -80, animationDelay: "3s" }} />
        <div className="orb" style={{ width: 300, height: 300, background: "rgba(14,165,233,0.04)", top: "40%", right: "20%", animationDelay: "5s" }} />

        {/* Nav */}
        <nav style={{
          position: "sticky", top: 0, zIndex: 50,
          background: "rgba(2, 4, 8, 0.7)",
          backdropFilter: "blur(20px) saturate(180%)",
          WebkitBackdropFilter: "blur(20px) saturate(180%)",
          borderBottom: "1px solid rgba(255,255,255,0.07)",
          padding: "0 24px",
          height: 56,
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
        }}>
          {/* Logo */}
          <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
            <div style={{
              width: 32, height: 32,
              background: "linear-gradient(135deg, #4ade80, #22c55e)",
              borderRadius: 10,
              display: "flex", alignItems: "center", justifyContent: "center",
              fontSize: 16, fontWeight: 900, color: "#000",
              boxShadow: "0 0 20px rgba(74,222,128,0.35)",
            }}>
              ft
            </div>
            <span style={{ fontSize: 13, color: "rgba(255,255,255,0.3)", fontWeight: 500 }}>
              HFT Market Maker
            </span>
          </div>

          {/* Tabs */}
          <div style={{ display: "flex", gap: 2, alignItems: "center" }}>
            <NavLink href="/setup">Setup</NavLink>
            <NavLink href="/run">Run</NavLink>
          </div>

          {/* Status pill */}
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <div className="pulse-dot" />
            <span style={{ fontSize: 12, color: "rgba(255,255,255,0.4)" }}>Hyperliquid</span>
          </div>
        </nav>

        {/* Page content */}
        <main style={{
          maxWidth: 960,
          margin: "0 auto",
          padding: "32px 20px 64px",
          position: "relative",
          zIndex: 1,
        }}>
          {children}
        </main>
      </body>
    </html>
  );
}

function NavLink({ href, children }: { href: string; children: React.ReactNode }) {
  return (
    <Link href={href} className="nav-link" style={{
      padding: "6px 16px",
      borderRadius: 8,
      fontSize: 14,
      fontWeight: 500,
      color: "rgba(255,255,255,0.55)",
      textDecoration: "none",
    }}>
      {children}
    </Link>
  );
}
