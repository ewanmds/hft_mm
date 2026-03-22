import type { Config } from "tailwindcss";

const config: Config = {
  content: [
    "./app/**/*.{js,ts,jsx,tsx,mdx}",
    "./components/**/*.{js,ts,jsx,tsx,mdx}",
  ],
  theme: {
    extend: {
      colors: {
        bg: "#0a0a0a",
        panel: "#141414",
        border: "#1f1f1f",
        green: { DEFAULT: "#4ade80", dim: "#22c55e" },
        red: { DEFAULT: "#f87171", dim: "#ef4444" },
        gray: { muted: "#6b7280", light: "#9ca3af" },
      },
    },
  },
  plugins: [],
};

export default config;
