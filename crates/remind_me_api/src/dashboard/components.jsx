// Dashboard: presentational components shared across views, plus the
// timestamp helpers the reminder UI needs.
//
// Nothing here fetches. Everything takes props and renders; the stores above
// own the data. That split is what makes a component reusable across the
// Browse, Reminders and Searches views without carrying a view's state.


function CategoryBadge({category}) {
  const c = theme.categoryColors[category] || theme.accent;
  return React.createElement("span", {style:{display:"inline-flex",alignItems:"center",gap:4,padding:"2px 8px",borderRadius:4,fontSize:11,fontWeight:600,fontFamily:mono,letterSpacing:"0.04em",textTransform:"uppercase",background:c+"18",color:c,border:"1px solid "+c+"30"}}, category);
}

function TagPill({tag, onClick, removable, onRemove}) {
  return React.createElement("span", {onClick, style:{display:"inline-flex",alignItems:"center",gap:3,padding:"1px 7px",borderRadius:3,fontSize:11,fontFamily:mono,background:theme.surfaceActive,color:theme.textSecondary,border:"1px solid "+theme.border,cursor:onClick?"pointer":"default",transition:"all 0.15s"}},
    React.createElement(Icons.Tag), tag,
    removable && onRemove && React.createElement("span", {onClick:e=>{e.stopPropagation();onRemove()}, style:{cursor:"pointer",marginLeft:2,opacity:0.6}}, "\u00d7")
  );
}

// --- Reminder timestamp helpers ---
// The API speaks RFC 3339 in UTC throughout; a human reads local time. These
// three are the only places that conversion happens, so a reminder shown as
// "3pm" is the same instant the one typed as "3pm" was stored as.

/// Whether an RFC 3339 timestamp has already passed. An unparseable value is
/// treated as not past, so a bad string renders as a plain reminder rather
/// than as a false alarm.
function isPast(iso) {
  const t = Date.parse(iso);
  return !isNaN(t) && t <= Date.now();
}

function formatWhen(iso) {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  return d.toLocaleString(undefined, {month:"short",day:"numeric",hour:"numeric",minute:"2-digit"});
}

/// An RFC 3339 timestamp as `<input type="datetime-local">` wants it:
/// `YYYY-MM-DDTHH:MM`, in the browser's own zone and with no suffix.
function toLocalInputValue(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "";
  const pad = n => String(n).padStart(2, "0");
  return d.getFullYear()+"-"+pad(d.getMonth()+1)+"-"+pad(d.getDate())+"T"+pad(d.getHours())+":"+pad(d.getMinutes());
}

function MemoryCard({memory:m, onEdit, onDelete, onTagClick, onRemind, expanded, onToggle}) {
  const [copied, setCopied] = useState(false);
  const handleCopy = () => { navigator.clipboard.writeText(m.content); setCopied(true); setTimeout(()=>setCopied(false),1500); };
  const isLong = m.content.length > 200;
  const display = expanded || !isLong ? m.content : m.content.slice(0,200) + "\u2026";
  const meta = Object.entries(m.metadata||{}).filter(([k])=>k!=="import_id");

  return React.createElement("div", {style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"16px 18px",transition:"all 0.2s"},
    onMouseEnter:e=>{e.currentTarget.style.borderColor=theme.borderFocus+"60";e.currentTarget.style.background=theme.surfaceHover},
    onMouseLeave:e=>{e.currentTarget.style.borderColor=theme.border;e.currentTarget.style.background=theme.surface}},
    // header
    React.createElement("div", {style:{display:"flex",justifyContent:"space-between",alignItems:"flex-start",marginBottom:8}},
      React.createElement("div", {style:{display:"flex",alignItems:"center",gap:8,flexWrap:"wrap"}},
        React.createElement(CategoryBadge, {category:m.category}),
        React.createElement("code", {style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, m.id),
        m.source==="chat_import" && m.metadata?.filename && React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}},"\u2190 "+m.metadata.filename)
      ),
      React.createElement("div", {style:{display:"flex",gap:4}},
        React.createElement("button", {onClick:handleCopy,title:"Copy",style:iconBtn}, copied ? React.createElement(Icons.Check) : React.createElement(Icons.Copy)),
        onRemind && React.createElement("button", {onClick:()=>onRemind(m),title:m.remind_at?"Change reminder":"Set reminder",style:{...iconBtn,color:m.remind_at?theme.warning:theme.textSecondary}}, React.createElement(Icons.Clock)),
        React.createElement("button", {onClick:()=>onEdit(m),title:"Edit",style:iconBtn}, React.createElement(Icons.Edit)),
        React.createElement("button", {onClick:()=>onDelete(m.id),title:"Delete",style:{...iconBtn,color:theme.danger}}, React.createElement(Icons.Trash))
      )
    ),
    // content
    React.createElement("div", {onClick:isLong?onToggle:undefined,style:{fontFamily:sans,fontSize:14,lineHeight:1.65,color:theme.text,whiteSpace:"pre-wrap",wordBreak:"break-word",cursor:isLong?"pointer":"default"}}, display),
    isLong && React.createElement("button", {onClick:onToggle,style:{background:"none",border:"none",color:theme.accent,fontSize:12,fontFamily:mono,cursor:"pointer",padding:"4px 0",marginTop:4}}, expanded?"Show less":"Show more"),
    // tags
    React.createElement("div", {style:{display:"flex",flexWrap:"wrap",gap:4,marginTop:10}},
      (m.tags||[]).map(t => React.createElement(TagPill, {key:t, tag:t, onClick:()=>onTagClick(t)}))
    ),
    meta.length > 0 && React.createElement("div",{style:{marginTop:6,fontSize:11,color:theme.textMuted,fontFamily:mono}}, meta.map(([k,v])=>k+": "+v).join(" \u00b7 ")),
    // A set reminder is stated on the card rather than only in the Reminders
    // view: "this memory will resurface" is a property of the memory, and a
    // card that hid it would make an already-scheduled reminder look unset.
    //
    // "Due" rather than "Overdue" for a past timestamp: whether a due reminder
    // was actually delivered is recorded server-side, and only the `overdue`
    // window can answer it. A card holding one memory row cannot, so it says
    // what it knows.
    m.remind_at && React.createElement("div",{style:{marginTop:8,display:"inline-flex",alignItems:"center",gap:5,padding:"2px 8px",borderRadius:4,fontSize:11,fontFamily:mono,background:isPast(m.remind_at)?theme.dangerSubtle:theme.warningSubtle,color:isPast(m.remind_at)?theme.danger:theme.warning}},
      React.createElement(Icons.Clock), (isPast(m.remind_at)?"Due ":"Reminder ")+formatWhen(m.remind_at)
    ),
    React.createElement("div", {style:{marginTop:8,fontSize:11,color:theme.textMuted,fontFamily:mono}},
      new Date(m.created_at).toLocaleDateString("en-US",{month:"short",day:"numeric",year:"numeric"}),
      m.updated_at !== m.created_at ? " \u00b7 edited "+new Date(m.updated_at).toLocaleDateString("en-US",{month:"short",day:"numeric"}) : ""
    )
  );
}

function StatCard({label, value, color, icon}) {
  return React.createElement("div", {style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"14px 16px",flex:"1 1 140px",minWidth:140}},
    React.createElement("div", {style:{display:"flex",alignItems:"center",gap:6,marginBottom:6}},
      React.createElement("span",{style:{color:color||theme.accent}}, icon),
      React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,textTransform:"uppercase",letterSpacing:"0.06em"}}, label)
    ),
    React.createElement("div", {style:{fontSize:28,fontWeight:700,fontFamily:mono,color:theme.text,lineHeight:1}}, value)
  );
}

function BarChart({data, colorMap, preserveOrder}) {
  const max = Math.max(...Object.values(data), 1);
  const entries = preserveOrder ? Object.entries(data) : Object.entries(data).sort((a,b)=>b[1]-a[1]);
  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:6}},
    entries.map(([label, count]) =>
      React.createElement("div", {key:label, style:{display:"flex",alignItems:"center",gap:8}},
        React.createElement("span", {style:{width:90,fontSize:12,fontFamily:mono,color:theme.textSecondary,textAlign:"right",flexShrink:0}}, label),
        React.createElement("div", {style:{flex:1,height:20,background:theme.surfaceActive,borderRadius:3,overflow:"hidden"}},
          React.createElement("div", {style:{height:"100%",width:(count/max*100)+"%",background:(colorMap&&colorMap[label])||theme.accent,borderRadius:3,transition:"width 0.4s ease",display:"flex",alignItems:"center",justifyContent:"flex-end",paddingRight:6}},
            React.createElement("span",{style:{fontSize:10,fontWeight:700,fontFamily:mono,color:"#fff"}}, count)
          )
        )
      )
    )
  );
}

// Simple inline-SVG line+area chart (issue #186) -- same "no charting
// library" posture as BarChart above, just a different mark shape. Plots
// one numeric field (valueKey) from a list of {captured_at, ...} snapshot
// dicts (oldest first, as GET /api/analytics/trend already returns them).
// A viewBox of a fixed logical width/height with preserveAspectRatio="none"
// lets the <svg> stretch to its container via plain CSS width:100% -- no
// resize-observer or JS layout math needed, mirroring BarChart's own
// percentage-width bars.
function TrendChart({data, valueKey, color}) {
  const height = 100;
  const width = 300;
  if (!data || data.length === 0) {
    return React.createElement("div", {style:{fontSize:13,color:theme.textMuted,fontFamily:sans,textAlign:"center",padding:"32px 0"}}, "No trend data yet — a daily snapshot is captured automatically; check back tomorrow.");
  }
  const values = data.map(d => Number(d[valueKey]) || 0);
  const max = Math.max(...values, 1);
  const min = Math.min(0, ...values);
  const range = (max - min) || 1;
  const stepX = data.length > 1 ? width / (data.length - 1) : 0;
  const coords = values.map((v, i) => [
    data.length > 1 ? i * stepX : width / 2,
    height - ((v - min) / range) * height,
  ]);
  const linePoints = coords.map(([x, y]) => x + "," + y).join(" ");
  const areaPoints = linePoints + " " + width + "," + height + " 0," + height;
  const stroke = color || theme.accent;
  return React.createElement("div", null,
    React.createElement("svg", {viewBox:"0 0 "+width+" "+height, preserveAspectRatio:"none", style:{width:"100%",height:120,display:"block",overflow:"visible"}},
      React.createElement("polygon", {points:areaPoints, fill:stroke+"22", stroke:"none"}),
      React.createElement("polyline", {points:linePoints, fill:"none", stroke:stroke, strokeWidth:1.5, vectorEffect:"non-scaling-stroke"})
    ),
    React.createElement("div", {style:{display:"flex",justifyContent:"space-between",marginTop:8,fontSize:11,fontFamily:mono,color:theme.textMuted}},
      React.createElement("span", null, (data[0].captured_at||"").slice(0,10)),
      React.createElement("span", null, (data[data.length-1].captured_at||"").slice(0,10))
    )
  );
}

// --- Wiki ---
// Lightweight rendering, not a full markdown parser: headings by leading
// '#' count, and [[Wikilink]] / [[Wikilink|alias]] spans made clickable so
// the cross-linking the wiki schema calls "the point" is actually navigable
// from the dashboard. Body text otherwise renders as-is (monospace,
// matching the rest of the app's raw-content aesthetic).
const WIKILINK_RE = /\[\[([^\[\]|]+?)(?:\|([^\[\]]+?))?\]\]/g;

function slugifyTitle(title) {
  return (title || "").trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "untitled";
}

function renderWikiLine(line, i, onNavigate) {
  const heading = line.match(/^(#{1,3})\s+(.*)$/);
  if (heading) {
    const size = heading[1].length === 1 ? 22 : heading[1].length === 2 ? 17 : 14;
    return React.createElement("div", {key:i, style:{fontSize:size,fontWeight:700,fontFamily:sans,letterSpacing:"-0.01em",margin:i===0?"0 0 10px":"18px 0 8px"}}, heading[2]);
  }
  const parts = [];
  let last = 0, m, idx = 0;
  WIKILINK_RE.lastIndex = 0;
  while ((m = WIKILINK_RE.exec(line))) {
    if (m.index > last) parts.push(React.createElement("span",{key:idx++}, line.slice(last, m.index)));
    const target = m[1].trim();
    const label = (m[2] || target).trim();
    parts.push(React.createElement("button",{
      key:idx++, onClick:() => onNavigate(slugifyTitle(target)),
      style:{background:"none",border:"none",padding:0,color:theme.accent,fontFamily:mono,fontWeight:600,cursor:"pointer",textDecoration:"underline",textUnderlineOffset:2,fontSize:"inherit"},
    }, label));
    last = m.index + m[0].length;
  }
  if (last < line.length) parts.push(React.createElement("span",{key:idx++}, line.slice(last)));
  if (parts.length === 0) return React.createElement("div",{key:i,style:{minHeight:8}});
  return React.createElement("div", {key:i, style:{marginBottom:2}}, parts);
}

function WikiPageBody({content, onNavigate}) {
  return React.createElement("div", {style:{fontFamily:mono,fontSize:14,color:theme.text,lineHeight:1.7}},
    content.split("\n").map((line, i) => renderWikiLine(line, i, onNavigate))
  );
}

function WikiLinkList({label, items, onNavigate}) {
  if (!items || !items.length) return null;
  return React.createElement("div", {style:{marginTop:16}},
    React.createElement("div",{style:labelSt}, label),
    React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:6}},
      items.map(it => React.createElement("button",{
        key:it.slug, onClick:() => onNavigate(it.slug),
        style:{display:"flex",alignItems:"center",gap:4,padding:"4px 10px",borderRadius:12,border:"1px solid "+theme.border,background:theme.surface,color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer"},
      }, React.createElement(Icons.Link), it.title))
    )
  );
}

// --- Entities ---
// Mirrors WikiLinkList's shape (pill buttons that navigate on click), keyed
// by entity id/name instead of slug/title — used for the "Related Entities"
// drill-down built from a 1-hop traversal.
function EntityLinkList({label, items, onNavigate}) {
  if (!items || !items.length) return null;
  return React.createElement("div", {style:{marginTop:16}},
    React.createElement("div",{style:labelSt}, label),
    React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:6}},
      items.map(it => React.createElement("button",{
        key:it.id, onClick:() => onNavigate(it.name),
        style:{display:"flex",alignItems:"center",gap:4,padding:"4px 10px",borderRadius:12,border:"1px solid "+theme.border,background:theme.surface,color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer"},
      }, React.createElement(Icons.Link), it.name))
    )
  );
}

function Modal({open, onClose, title, children, width}) {
  if (!open) return null;
  return React.createElement("div", {style:{position:"fixed",inset:0,zIndex:1000,display:"flex",alignItems:"center",justifyContent:"center",background:"rgba(0,0,0,0.65)",backdropFilter:"blur(4px)"}, onClick:onClose},
    React.createElement("div", {onClick:e=>e.stopPropagation(), style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:12,padding:24,width:width||480,maxWidth:"92vw",maxHeight:"85vh",overflowY:"auto",boxShadow:"0 24px 80px rgba(0,0,0,0.5)"}},
      React.createElement("div", {style:{display:"flex",justifyContent:"space-between",alignItems:"center",marginBottom:20}},
        React.createElement("h2",{style:{margin:0,fontSize:18,fontFamily:sans,fontWeight:600,color:theme.text}}, title),
        React.createElement("button",{onClick:onClose,style:{...iconBtn,color:theme.textMuted}}, React.createElement(Icons.X))
      ),
      children
    )
  );
}
