// Dashboard: colour tokens, typography and the shared inline-style objects
// every other file here builds on. Loads first; nothing above it exists yet.
//
// `iconBtn`/`inputSt`/`labelSt` live here rather than beside the icons they
// were originally declared next to: they are style tokens, not icons, and
// they depend only on `theme`/`mono`/`sans` defined above them.


const { useState, useEffect, useCallback, useRef } = React;

const API = window.location.origin + "/api";

const theme = {
  bg: "#0a0a0f", surface: "#12121a", surfaceHover: "#1a1a26", surfaceActive: "#22222e",
  border: "#2a2a3a", borderFocus: "#6366f1", text: "#e4e4ed", textSecondary: "#8888a4",
  textMuted: "#55556a", accent: "#6366f1", accentHover: "#818cf8",
  accentSubtle: "rgba(99,102,241,0.12)", danger: "#ef4444", dangerSubtle: "rgba(239,68,68,0.12)",
  success: "#22c55e", successSubtle: "rgba(34,197,94,0.12)",
  warning: "#f59e0b", warningSubtle: "rgba(245,158,11,0.12)",
  categoryColors: {
    general: "#6366f1", preference: "#f59e0b", fact: "#22c55e", project: "#06b6d4",
    person: "#ec4899", decision: "#8b5cf6", chat_import: "#64748b", observation: "#14b8a6",
  },
};
const mono = "'IBM Plex Mono', 'JetBrains Mono', monospace";
const sans = "'IBM Plex Sans', -apple-system, sans-serif";


// minWidth/minHeight (rather than just padding) give icon-only buttons a
// ~44x44 tap target per common mobile touch-target guidance (issue #199
// mobile audit) without inflating the visible icon itself -- the icon stays
// its normal small size, centered in an otherwise-invisible (background:
// none) hit area.
const iconBtn = { background:"none", border:"none", color:theme.textSecondary, cursor:"pointer", padding:4, borderRadius:4, display:"flex", alignItems:"center", justifyContent:"center", transition:"color 0.15s", minWidth:44, minHeight:44 };
const inputSt = { width:"100%", padding:"10px 12px", borderRadius:6, border:"1px solid "+theme.border, background:theme.bg, color:theme.text, fontSize:14, fontFamily:sans, outline:"none", transition:"border-color 0.15s", boxSizing:"border-box" };
const labelSt = { display:"block", fontSize:12, fontWeight:600, fontFamily:mono, color:theme.textSecondary, marginBottom:6, textTransform:"uppercase", letterSpacing:"0.04em" };
